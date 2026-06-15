# Quest flat-view capture — reverse-engineering notes

Goal: stream the Quest's **flat / non-lens view** (what casting/recording shows)
to the host and decode it with our Media Foundation H.264 decoder — no
client-side lens de-warp. Hard requirement: **must work on UNROOTED units**, so
the agent runs as the plain `adb shell` uid (2000), exactly like scrcpy.

## ✅ CHOSEN PATH (unrooted): MetaCam / SpatialMedia capture

The breakthrough: the `adb shell` uid (`com.android.shell`, a PRIVILEGED app)
**already holds the flat-capture permissions** — `com.oculus.permission.METACAM_SCREEN_CAPTURE`,
`horizonos.permission.METACAM_SCREEN_CAPTURE`, `CAPTURE_VIDEO_OUTPUT`,
`READ_FRAME_BUFFER`, `MANAGE_MEDIA_PROJECTION`, `ADD_TRUSTED_DISPLAY`. So an
unrooted shell agent is *permitted* to grab the flat stream (almost certainly
how MQDH records/casts flat from stock headsets). It does **not** hold
`XRS_STREAM`, so we use the capture path, not the XRSP broker.

```
agent (adb shell, uid 2000, scrcpy-style)
  └─ bind  spatialmedia : vros.spatial.media.ISpatialMediaManagerService   (service #295)
        host: /system_ext/bin/xrservice (system), engine: libOVRMrcLib.oculus.so (OVR MRC),
              encode: libstagefright_framecapture_utils.so, AIDL: capture_state_listener-aidl-cpp.so
        client reference impl: com.oculus.metacam (UI: record / cast / livestream)
  └─ start a flat video capture → receive H.264 (Surface/MediaCodec or buffer callback)
  └─ forward H.264 over adb-forwarded TCP → host → decoder.rs
```

IDA targets in `re/`: `xrservice`, `libOVRMrcLib.oculus.so`, `capture_state_listener-aidl-cpp.so`,
`metacam.apk` (caller), plus the XRSP set below (background).

### The capture API (reversed from MetaCam, C2)

Real service: **`com.oculus.aidl.IScreenCaptureService`** (bound service
`com.oculus.vrapi.ScreenCaptureService`, hosted by the VrRuntime/vrshell side;
gated by `METACAM_SCREEN_CAPTURE` which shell holds). NOT `libOVRMrcLib` — that's
the app-level green-screen MRC/VRCam path (`debug.oculus.mrc.tcpport`), a side road.

AIDL surface (`com.oculus.aidl.*`):
- `IScreenCaptureService` methods: **`startScreenCaptureAhb(...)`** (frames as
  AHardwareBuffers; "AHB capture requires VrRuntime AIDL version >= N"),
  `stopScreenCapture(...)`, `ScreenshotStartRequest` (stills), instant-replay.
- request/param parcelables: **`ScreenCaptureStartRequest`** (the captureConfig),
  `PrivacyParameters`, `FrameRateParameters`, `StabilizationParameters`,
  `GuideConfiguration`, `ScreenCaptureStopRequest`, `ScreenCaptureError`.
- frame metadata: `ImageHeaderMetadata{mProjectionType=…}` — projection type
  selects **panel (flat)** vs **spherical (360)**. We want panel/flat.

MetaCam client flow (the reference to copy):
`com.oculus.metacam.capture.ipc.ScreenCaptureClient.bindServiceSuspend()` →
`IScreenCaptureService` → `startScreenCaptureAhb(ScreenCaptureStartRequest)`.
MetaCam also has `SurfaceCapture` + `VideoCapture` + MediaCodec + a
`createVirtualDisplay` `PanelFrameSource` — i.e. it can capture into a **Surface**
(MediaCodec input surface) and encode to H.264, the scrcpy pattern.

### Agent design (C4)

A pushed `app_process` agent running as the **shell uid** (holds
`METACAM_SCREEN_CAPTURE`), like the scrcpy server jar:
1. Build a `FakeContext` (à la scrcpy) so we can `bindService` the bound
   `com.oculus.vrapi.ScreenCaptureService`.
2. MediaCodec H.264 encoder → `createInputSurface()`.
3. `startScreenCaptureAhb` / surface-capture with a **panel/flat** projection,
   targeting our encoder surface (or AHB→GL→surface if surface isn't accepted).
4. Drain MediaCodec → write Annex-B H.264 to an `adb forward` TCP socket.
5. Host: read the socket → `decoder.rs` → new "flat mode" source.

Open: exact `ScreenCaptureStartRequest` field order + whether the service accepts
a caller Surface or only AHB (→ decompile the `com.oculus.aidl` AIDL + MetaCam
`ScreenCaptureClient`/`PanelFrameSource`). Then confirm shell-uid is accepted (C3).

---

## Background: XRSP (the broker/streaming path — NOT the chosen route)

(Kept because it explains casting/Link internals; the unrooted path above avoids
it.) Living spec for the protocol Meta uses to carry the Quest's **flat / non-lens
"casting" view** over the network/USB.

Sources reversed (in `re/`, gitignored):
`libmagicislandnative.so` (casting client), `libossdk.oculus.so` (OSSDK / XRSP
stack + broker), `xrspd` (daemon), `libxrspdhelper.so`.

> Status: **transport + framing mapped; handshake/pairing/crypto/schema NOT yet
> mapped.** A working client is not possible until the connect handshake and the
> casting topic schema are reversed. See "Open questions" / "Milestones".

---

## Architecture (confirmed)

The flat view is **not an Android display**. It is an XRSP *topic* on a channel
brokered by the on-device daemon `xrspd`.

```
xrspd (system_ext/bin/xrspd)  ── binder service: oculus::internal::IXrspBroker
   ▲ pairing, streaming secret, transport credentials, transport endpoint
   │
client (casting svc, Link, MQDH) uses libossdk:
   createXrspBroker() → IXrspBroker (AServiceManager_getService)
     ├─ startPairing / getPairingCode / setPairingResult / getPairedCertFingerprint
     ├─ getStreamingSecret()        → AES-GCM session key (base64)
     ├─ getTransportCredentials()
     ├─ acquireUsbFDsForHighwind()  → USB (Link) transport fds   ["Highwind" = Link]
     └─ startStreamingSession(XrspStreamingClient)
           └─ obtainTransportInfo() BLOCKS until broker returns a transport:
                • TCP: builds "tcpclient,<addr>" → XrspTransportCreateFromDescription
                • FD : read/write fds → XrspFileDescTransport (sets XrspTransport.isUSB)
           └─ optional E2EE: AES-GCM (xrsp::crypto::aes::gcm), key from broker
                • backcompat path: "e2ee encryption not supported, skipping"
```

XRSP itself = a Cap'n Proto-based pub/sub:
- `XrspSession` / `XrspParticipant` / `Channel` (`xrsp::ReliableChannel`)
- `XrspTopic*` (Create/Read/Write/ReadNextMessage/StartBuiltinTopicConsumer)
- per-topic schemas exchanged at runtime (`XrspTopicSchemaInputStream`)
- `XrspParticipant::isTopicEncrypted(topicId)` — per-topic encryption flag
- invite/pairing state machine: `XrspInviteTransaction{Invite,Pairing,CodeGeneration,…}`

## Packet framing (confirmed — `XrspPacketHeaderInit`)

8-byte header, little-endian, packets padded to a 4-byte boundary:

| off | type | meaning |
|-----|------|---------|
| 0   | u16  | word0: version + flags. `0x08`=sized/has-payload, `0x10`=internal, version in high bits; two 6-bit subfields (msg type / priority) — **exact bit layout TODO-verify** |
| 2   | u16  | length: `total_packet_bytes = (value + 1) * 4` |
| 4   | u16  | topic id |
| 6   | u16  | reserved (0) |

`payload_num_bytes = total - 8 - num_padding_bytes`, `padding ≤ 255`.
Max packet = `(0xFFFF + 1) * 4` = 256 KiB.

## Transport — host paths CONFIRMED (M2)

`xrspd` (`/system_ext/bin/xrspd`) is a thin daemon; the real logic lives in
**`libxrspdhelper.so`** (`XrspdHelperStart` / `…StopAsync` / `…RequestRTTCalculation`
/ `PairingErrorToStr`). It has **no idle TCP listener** — transports come up
on-demand. Two host-reachable transports exist:

1. **USB (the clean host path).** xrspd holds `/dev/usb-ffs/xrsp/ep0` open and
   `/dev/usb-ffs/` exposes a dedicated **`xrsp` USB function** (FunctionFS gadget).
   On the host this is the `XrspLibusbTransportCreate` path: claim the Quest's
   `xrsp` USB interface and read/write its **bulk IN/OUT endpoints** (WinUSB or
   libusb/`nusb` on Windows). Only present when the USB config includes `xrsp`
   (`getprop sys.usb.config`) — i.e. plugged in with Link/the right composite.
2. **AirLink (network).** `DNSServiceBrowse/Resolve/QueryRecord` (mDNS/Bonjour)
   to discover the headset, then a socket + the secure transport below.

Transport constructors (from `libmagicislandnative.so`):
- `XrspSocketClientTransportCreate(type, "host:port", …)`
- `XrspSocketClientTransportSecureCreate(addr, x509_st*, evp_pkey_st*, …)` ← **TLS w/ client cert**
- `XrspFileDescTransportCreate` (FD), `XrspLibusbTransportCreate` (host USB)
- `XrspTransportCreateFromDescription("tcpclient,<addr>")`
- Daemon side: `XrspSocketServerTransportCreateOnAnyPort`, secure variants

Binder service is registered by `ServiceManager::start(const char*)` /
`android::defaultServiceManager` inside xrspd (descriptor
`oculus::internal::IXrspBroker`).

## Open questions (block a working client)

1. **Host transport**: does `xrspd` expose anything a Windows host can reach
   without the Oculus runtime? Candidates: (a) Link USB interface (libusb bulk),
   (b) AirLink TCP listener, (c) an adb-forwardable unix/abstract socket.
   → reverse `xrspd` + `libxrspdhelper.so` for its listener + socket name.
2. **Pairing**: cert-based (`getPairedCertFingerprint`, x509 secure transport).
   Can we pair as a new host, or reuse an existing PC pairing's cert/key?
3. **Session handshake**: the `XrspInviteTransaction` state machine + QoS.
4. **Crypto**: is the casting/video topic encrypted (`isTopicEncrypted`)? If so
   we need the AES-GCM session key (broker `getStreamingSecret`) and/or DTLS.
5. **Casting topic**: which topic id carries the flat H.264, its Cap'n Proto
   schema, and the codec/resolution negotiation.

## Milestones

Plan pivoted to the unrooted **MetaCam/SpatialMedia capture** path (above). XRSP
milestones M0–M2 (done) stay as background.

Capture-path milestones:
- [x] C0 Flat view is privileged capture, not a display; shell uid holds `METACAM_SCREEN_CAPTURE`
- [x] C1 Capture service identified: `spatialmedia` / `ISpatialMediaManagerService` on `xrservice`, engine `libOVRMrcLib.oculus.so`
- [ ] C2 Reverse the start-capture transaction + frame delivery (Surface vs callback) in `libOVRMrcLib` / how `metacam` calls it
- [ ] C3 Confirm a shell-uid caller is accepted (empirical: bind + start capture)
- [ ] C4 On-device agent (app_process/native, shell uid): start capture → H.264 → `adb forward` socket
- [ ] C5 Host: read the socket → `decoder.rs`; wire into the app as "flat mode"

## Reality check (Route B vs Route A)

Route B (this doc) = reimplement a paired, TLS/AES-GCM-encrypted, Cap'n
Proto-schema'd streaming protocol. The framing is easy; the **pairing + secure
transport + schema** is Link-class effort (the whole point of which is to keep
third parties out). It is also *fragile to OS updates* — Meta can rev the wire
format/crypto anytime.

Route A (on-device `app_process` agent that calls OSSDK `startStreamingSession`,
like scrcpy's own server) reuses Meta's client and skips all of that. Its only
unknown is whether a `shell`-uid caller passes `xrspd`'s server-side permission
check — a quick test. Recommend confirming that before committing to B.

# Quest scrcpy

A native, from-scratch [scrcpy](https://github.com/Genymobile/scrcpy) client written in Rust and tuned for the **Meta Quest 3** — it renders the headset view in its own window, with a live one-eye crop, lens (fisheye) flattening, audio, screenshots and MP4 clip recording.

It is **not** a GUI wrapper around `scrcpy.exe`. It talks the scrcpy client/server protocol directly, decodes H.264 with the built-in **Windows Media Foundation** decoder, and renders with **egui/wgpu**. No ffmpeg, no openh264 — the scrcpy server `.jar` is embedded straight into the executable, so it's a single self-contained `.exe` (you only need `adb` on your `PATH`).

## Why this exists

On Horizon OS 74+, plain scrcpy shows a **black screen / flicker** on Quest. The cause ([scrcpy#5913](https://github.com/Genymobile/scrcpy/issues/5913)) is that the Quest's `createVirtualDisplay()` throws an exception but starts mirroring anyway, so scrcpy's `SurfaceControl` fallback opens a *second* capture on the same surface. This client ships the **official scrcpy v4.0 server**, which detects Quest devices and skips that broken fallback — so it actually renders.

On top of that, the Quest mirrors the stereoscopic, lens-distorted view. This client lets you crop to one eye and **flatten the fisheye** into a clean, upright 2D image.

## Features

- 🎯 Device & display pickers over adb (shows every display the Quest exposes)
- 📶 **Wireless adb** — connect a Quest by `ip[:port]`; remembered addresses get one-click reconnect chips + "Connect all"
- 🥽 **One-click Quest 3 preset** — left eye, lens flattened, leveled
- 🔍 Live crop: drag to pan, scroll to zoom, eye/full presets
- 🪞 **Flatten lens**: live radial-distortion + tilt correction (de-fisheye the VR view)
- 🔊 Audio toggle (AAC via Media Foundation → cpal)
- 📷 Screenshot the current crop to PNG
- ⏺ **Record** to `.mp4` — captures exactly the processed view you see (cropped, lens-flattened, tilted); the CLI can also do a lossless full-panel passthrough
- ⚡ Low-latency pipeline: `MF_LOW_LATENCY` decode, no-vsync present, multi-threaded NV12→RGBA
- 💾 Settings (lens/tilt/crop/quality + remembered devices) auto-saved and restored between launches
- 🖥️ Both a GUI and a CLI

## Requirements

- Windows 10/11 (uses Media Foundation)
- `adb` on your `PATH` (Android platform-tools)
- A Quest in developer mode, connected over USB or wireless adb

## Usage

GUI (default):

```sh
quest-scrcpy            # launch the GUI
quest-scrcpy gui        # same
quest-scrcpy mirror --serial <SERIAL> --audio   # launch + auto-connect
```

CLI helpers:

```sh
quest-scrcpy list                       # list adb devices
quest-scrcpy connect 192.168.1.40       # adb connect over Wi-Fi (defaults to :5555)
quest-scrcpy displays --serial <SERIAL> # list device displays
quest-scrcpy record --serial <SERIAL> -n 15 -o clip.mp4               # 15s full-panel clip (lossless)
quest-scrcpy record --serial <SERIAL> --crop left --flatten -o eye.mp4 # one-eye, lens-flattened clip
quest-scrcpy shot --serial <SERIAL> -o frame.png                      # single-frame grab
```

In the GUI: pick your Quest, hit **Connect**, click **🥽 Quest 3** for the tuned view, then fine-tune **Flatten lens** (`curve`/`edge`) and `tilt°` if needed. Screenshots and clips land in a `captures/` folder next to the executable.

## Build

```sh
cargo build --release
# -> target/release/quest-scrcpy.exe
```

## How it works

- `adb.rs` — push the embedded server jar, set up the forward, launch `app_process`, list displays
- `server.rs` — scrcpy v4.0 wire protocol (handshake, session-meta resolution, packet framing)
- `decoder.rs` — H.264 → NV12 → RGBA via the Media Foundation H.264 MFT
- `recorder.rs` — H.264 → MP4 mux via the Media Foundation sink writer
- `audio.rs` / `audioplay.rs` — AAC → PCM via MF, played through cpal
- `stream.rs` — the streaming thread tying it together
- `app.rs` — the egui front-end (crop, lens flatten, capture controls)

## Third-party

This project embeds and launches the scrcpy server from [Genymobile/scrcpy](https://github.com/Genymobile/scrcpy) (Apache-2.0). See [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

## License

MIT — see [LICENSE](LICENSE).

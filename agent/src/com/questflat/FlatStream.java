package com.questflat;

import android.content.Context;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioPlaybackCaptureConfiguration;
import android.media.AudioRecord;
import android.media.MediaRecorder;
import android.hardware.display.VirtualDisplay;
import android.hardware.display.VirtualDisplayConfig;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaFormat;
import android.media.projection.MediaProjection;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.IBinder;
import android.os.Looper;
import android.util.Log;
import android.view.Surface;

import java.io.BufferedOutputStream;
import java.io.DataOutputStream;
import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.lang.reflect.Method;
import java.nio.ByteBuffer;

/**
 * On-device flat-view streamer. Runs as the (unrooted) shell uid via app_process,
 * like scrcpy's server jar — but it writes the H.264 stream to STDOUT, so the
 * host reads it with a single robust pipe (no adb forward / TCP loopback):
 *
 *   adb exec-out CLASSPATH=/data/local/tmp/qflat-probe.jar \
 *       app_process /system/bin com.questflat.FlatStream
 *
 * It mints a whole-view MediaProjection (MediaProjectionManagerExt.createProjectionToken(null)
 * + AUTO_MIRROR = the flat casting view), encodes to H.264, and writes:
 *   [u32 w][u32 h] then repeated [u32 len][Annex-B access unit].
 * All logging goes to STDERR so STDOUT stays pure video.
 */
public class FlatStream {
    static final String TAG = "QFLATSTREAM";
    // Log to logcat ONLY — stdout must stay pure H.264 (it's piped via exec-out).
    static void log(String s) { Log.i(TAG, s); }
    static int W = 1280, H = 720, BITRATE = 12_000_000, FPS = 60;
    static volatile boolean running = true;

    public static void main(String[] a) {
        for (int i = 0; i + 1 < a.length; i += 2) {
            switch (a[i]) {
                case "-w": W = Integer.parseInt(a[i + 1]); break;
                case "-h": H = Integer.parseInt(a[i + 1]); break;
                case "-b": BITRATE = Integer.parseInt(a[i + 1]); break;
                case "-f": FPS = Integer.parseInt(a[i + 1]); break;
            }
        }
        try { run(); } catch (Throwable t) { Log.e(TAG, "FATAL", t); System.exit(1); }
    }

    static void run() throws Exception {
        Looper.prepareMainLooper();
        Class<?> atC = Class.forName("android.app.ActivityThread");
        Object at = atC.getMethod("systemMain").invoke(null);
        // Wrap in FakeContext so getPackageName()=="com.android.shell" — required
        // so createVirtualDisplay's "packageName must match calling uid" passes
        // for the unrooted shell uid (2000).
        Context ctx = new FakeContext((Context) atC.getMethod("getSystemContext").invoke(at));

        // --- whole flat view projection: createProjectionToken(null) + AUTO_MIRROR ---
        IBinder b = (IBinder) Class.forName("android.os.ServiceManager")
                .getMethod("getService", String.class).invoke(null, "media_projection");
        Class<?> mpmIfc = Class.forName("android.media.projection.IMediaProjectionManager");
        Object mpm = Class.forName("android.media.projection.IMediaProjectionManager$Stub")
                .getMethod("asInterface", IBinder.class).invoke(null, b);
        Class<?> extC = Class.forName("horizonos.media.projection.MediaProjectionManagerExt");
        Object ext = extC.getConstructor(Context.class, mpmIfc).newInstance(ctx, mpm);
        MediaProjection mp = (MediaProjection) extC
                .getMethod("createProjectionToken", Class.forName("horizonos.view.VolumetricWindowToken"))
                .invoke(ext, new Object[]{null});
        log("mediaProjection=" + mp);

        // Match the capture to the real flat-view aspect ratio, fit inside the
        // requested WxH *box*. AUTO_MIRROR scales the source to fit our virtual
        // display preserving aspect, so a frame that isn't the source's shape
        // gets letterboxed (looks "cropped"/not maximized). Sizing the frame to
        // the source aspect makes the view fill it. Falls back to the requested
        // size if the query fails.
        try {
            Object dmgEarly = Class.forName("android.hardware.display.DisplayManagerGlobal")
                    .getMethod("getInstance").invoke(null);
            Object di = dmgEarly.getClass().getMethod("getDisplayInfo", int.class)
                    .invoke(dmgEarly, 0 /* Display.DEFAULT_DISPLAY */);
            int lw = di.getClass().getField("logicalWidth").getInt(di);
            int lh = di.getClass().getField("logicalHeight").getInt(di);
            if (lw > 0 && lh > 0) {
                double scale = Math.min((double) W / lw, (double) H / lh);
                int ow = ((int) Math.round(lw * scale)) & ~1;
                int oh = ((int) Math.round(lh * scale)) & ~1;
                if (ow >= 2 && oh >= 2) {
                    log("source display " + lw + "x" + lh + " -> capture " + ow + "x" + oh);
                    W = ow;
                    H = oh;
                }
            }
        } catch (Throwable t) {
            log("display-size query failed, using requested " + W + "x" + H + ": " + t);
        }

        // --- H.264 encoder fed by a Surface ---
        MediaFormat fmt = MediaFormat.createVideoFormat("video/avc", W, H);
        fmt.setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface);
        fmt.setInteger(MediaFormat.KEY_BIT_RATE, BITRATE);
        fmt.setInteger(MediaFormat.KEY_FRAME_RATE, FPS);
        fmt.setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 1);
        fmt.setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 100_000L);
        MediaCodec enc = MediaCodec.createEncoderByType("video/avc");
        enc.configure(fmt, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
        Surface surface = enc.createInputSurface();
        enc.start();

        HandlerThread ht = new HandlerThread("vd");
        ht.start();
        Handler h = new Handler(ht.getLooper());
        mp.registerCallback(new MediaProjection.Callback() {}, h);

        // Call DisplayManagerGlobal.createVirtualDisplay(context, projection, ...)
        // DIRECTLY, passing our FakeContext as `context`: that's the exact value
        // used for the "packageName must match calling uid" check, so it becomes
        // "com.android.shell" and matches the unrooted shell uid. (MediaProjection's
        // own createVirtualDisplay re-fetches the base system context, which fails.)
        VirtualDisplayConfig config = new VirtualDisplayConfig.Builder("flat_stream", W, H, 240)
                .setFlags(0x10 /*VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR*/)
                .setSurface(surface)
                .build();
        Object dmg = Class.forName("android.hardware.display.DisplayManagerGlobal")
                .getMethod("getInstance").invoke(null);
        Method cvd = null;
        for (Method m : dmg.getClass().getDeclaredMethods()) {
            Class<?>[] p = m.getParameterTypes();
            if (m.getName().equals("createVirtualDisplay") && p.length == 5 && p[0] == Context.class) {
                cvd = m;
                break;
            }
        }
        if (cvd == null) throw new IllegalStateException("DisplayManagerGlobal.createVirtualDisplay(Context,...) not found");
        // Arg 5 is an Executor on API 33+ (was a Handler before) — pass the right one.
        final Handler vh = h;
        Object arg5;
        if (cvd.getParameterTypes()[4].getName().equals("java.util.concurrent.Executor")) {
            arg5 = new java.util.concurrent.Executor() {
                public void execute(Runnable r) { vh.post(r); }
            };
        } else {
            arg5 = h;
        }
        VirtualDisplay vd = (VirtualDisplay) cvd.invoke(dmg, ctx, mp, config, new VirtualDisplay.Callback() {}, arg5);
        log("virtualDisplay=" + vd);
        if (vd == null) { log("FATAL: createVirtualDisplay returned null"); System.exit(3); }

        // --- pipe H.264 straight to stdout (adb exec-out) ---
        DataOutputStream out = new DataOutputStream(
                new BufferedOutputStream(new FileOutputStream(FileDescriptor.out), 1 << 16));
        // Magic so the host can skip any stdout preamble (linker warnings etc.).
        out.writeByte('Q'); out.writeByte('F'); out.writeByte('L'); out.writeByte('T');
        out.writeInt(W);
        out.writeInt(H);
        out.flush();
        log("streaming " + W + "x" + H + " to stdout");
        startAudioCapture(out, mp); // best-effort device audio -> AAC, interleaved

        MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
        long frames = 0, bytes = 0;
        try {
            while (true) {
                int idx = enc.dequeueOutputBuffer(info, 250_000);
                if (idx >= 0) {
                    if (info.size > 0) {
                        ByteBuffer buf = enc.getOutputBuffer(idx);
                        buf.position(info.offset);
                        buf.limit(info.offset + info.size);
                        byte[] data = new byte[info.size];
                        buf.get(data);
                        synchronized (out) {
                            out.writeByte(0); // 0 = video access unit
                            out.writeInt(data.length);
                            out.write(data);
                            out.flush();
                        }
                        frames++;
                        bytes += data.length;
                        if ((frames & 127) == 0) log("frames=" + frames + " bytes=" + bytes);
                    }
                    enc.releaseOutputBuffer(idx, false);
                    if ((info.flags & MediaCodec.BUFFER_FLAG_END_OF_STREAM) != 0) break;
                } else if (idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                    log("encoder format=" + enc.getOutputFormat());
                }
            }
        } catch (Exception e) {
            log("stream ended: " + e);
        } finally {
            running = false;
            try { enc.stop(); enc.release(); } catch (Throwable ignore) {}
            try { vd.release(); } catch (Throwable ignore) {}
        }
        log("done frames=" + frames);
        System.exit(0);
    }

    /// Best-effort: capture the device's playback audio (via the same projection),
    /// encode AAC, and interleave it into the stdout stream as kind 1 (frame) /
    /// kind 2 (codec config). If anything fails, video keeps streaming.
    static void startAudioCapture(final DataOutputStream out, final MediaProjection mp) {
        Thread t = new Thread(new Runnable() {
            public void run() {
                final int SR = 48000;
                try {
                    int minBuf = AudioRecord.getMinBufferSize(
                            SR, AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT);
                    int bufSize = Math.max(minBuf, 16384);

                    AudioRecord rec = openPlaybackCapture(mp, SR, bufSize);
                    if (rec == null || rec.getState() != AudioRecord.STATE_INITIALIZED) {
                        // Fall back to REMOTE_SUBMIX (the output mix) if playback
                        // capture isn't available for this projection.
                        log("audio: playback-capture unavailable, trying REMOTE_SUBMIX");
                        rec = new AudioRecord(
                                MediaRecorder.AudioSource.REMOTE_SUBMIX, SR,
                                AudioFormat.CHANNEL_IN_STEREO, AudioFormat.ENCODING_PCM_16BIT, bufSize);
                    }
                    if (rec.getState() != AudioRecord.STATE_INITIALIZED) {
                        log("audio: no source initialized — video only");
                        return;
                    }
                    rec.startRecording();

                    MediaFormat af = MediaFormat.createAudioFormat("audio/mp4a-latm", SR, 2);
                    af.setInteger(MediaFormat.KEY_AAC_PROFILE,
                            MediaCodecInfo.CodecProfileLevel.AACObjectLC);
                    af.setInteger(MediaFormat.KEY_BIT_RATE, 128_000);
                    af.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, 16384);
                    MediaCodec aenc = MediaCodec.createEncoderByType("audio/mp4a-latm");
                    aenc.configure(af, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE);
                    aenc.start();
                    log("audio: encoder started, reading PCM...");

                    byte[] pcm = new byte[4096];
                    MediaCodec.BufferInfo info = new MediaCodec.BufferInfo();
                    long reads = 0, pcmBytes = 0, aacFrames = 0;
                    while (running) {
                        int n = rec.read(pcm, 0, pcm.length);
                        if (n > 0) {
                            pcmBytes += n;
                            if (reads == 0) log("audio: first PCM read = " + n + " bytes");
                            reads++;
                            if ((reads & 255) == 0) {
                                log("audio: reads=" + reads + " pcmBytes=" + pcmBytes + " aacFrames=" + aacFrames);
                            }
                            int ii = aenc.dequeueInputBuffer(10_000);
                            if (ii >= 0) {
                                ByteBuffer ib = aenc.getInputBuffer(ii);
                                ib.clear();
                                ib.put(pcm, 0, n);
                                aenc.queueInputBuffer(ii, 0, n, System.nanoTime() / 1000, 0);
                            }
                        } else if (n < 0) {
                            log("audio: read error " + n + " — stopping audio");
                            break;
                        }
                        int oi;
                        while ((oi = aenc.dequeueOutputBuffer(info, 0)) >= 0) {
                            if (info.size > 0) {
                                ByteBuffer ob = aenc.getOutputBuffer(oi);
                                ob.position(info.offset);
                                ob.limit(info.offset + info.size);
                                byte[] d = new byte[info.size];
                                ob.get(d);
                                int kind = (info.flags & MediaCodec.BUFFER_FLAG_CODEC_CONFIG) != 0 ? 2 : 1;
                                if (kind == 1) aacFrames++;
                                synchronized (out) {
                                    out.writeByte(kind);
                                    out.writeInt(d.length);
                                    out.write(d);
                                    out.flush();
                                }
                            }
                            aenc.releaseOutputBuffer(oi, false);
                        }
                    }
                    try { rec.stop(); rec.release(); aenc.stop(); aenc.release(); } catch (Throwable ignore) {}
                } catch (Throwable e) {
                    log("audio capture failed (video continues): " + e);
                }
            }
        });
        t.setDaemon(true);
        t.start();
    }

    /// Build an AudioRecord that captures app PLAYBACK audio (media/game/unknown)
    /// routed through the projection — the correct way to grab device sound.
    /// Returns null if the platform/projection won't allow it (caller falls back).
    static AudioRecord openPlaybackCapture(MediaProjection mp, int sr, int bufSize) {
        try {
            AudioPlaybackCaptureConfiguration apc =
                    new AudioPlaybackCaptureConfiguration.Builder(mp)
                            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
                            .addMatchingUsage(AudioAttributes.USAGE_GAME)
                            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
                            .build();
            AudioFormat fmt = new AudioFormat.Builder()
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .setSampleRate(sr)
                    .setChannelMask(AudioFormat.CHANNEL_IN_STEREO)
                    .build();
            AudioRecord rec = new AudioRecord.Builder()
                    .setAudioFormat(fmt)
                    .setBufferSizeInBytes(bufSize)
                    .setAudioPlaybackCaptureConfig(apc)
                    .build();
            log("audio: AudioPlaybackCapture initialized (state=" + rec.getState() + ")");
            return rec;
        } catch (Throwable e) {
            log("audio: AudioPlaybackCapture init failed: " + e);
            return null;
        }
    }
}

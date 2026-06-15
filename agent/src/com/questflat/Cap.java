package com.questflat;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.PixelFormat;
import android.hardware.display.VirtualDisplay;
import android.media.Image;
import android.media.ImageReader;
import android.media.projection.MediaProjection;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;

import java.io.FileOutputStream;

/**
 * Moment-of-truth capture: mint a MediaProjection scoped to the GLOBAL
 * volumetric window via Quest's MediaProjectionManagerExt, mirror it into an
 * ImageReader, and save one frame — so we can eyeball flat vs warped.
 *   CLASSPATH=/data/local/tmp/qflat-probe.jar app_process /system/bin com.questflat.Cap
 */
public class Cap {
    static final String TAG = "QFLATCAP";
    static void log(String s) { System.out.println(TAG + ": " + s); }
    static final int W = 1280, H = 720;
    // "whole" = null token + AUTO_MIRROR = the whole flat composited view
    // (home env + panels), i.e. the casting view. "focused" = one panel.
    static String STRAT = "whole";

    public static void main(String[] a) {
        if (a.length > 0) STRAT = a[0];
        log("strategy=" + STRAT);
        try { run(); } catch (Throwable t) { log("FATAL " + t); t.printStackTrace(); System.exit(2); }
    }

    static void run() throws Exception {
        Looper.prepareMainLooper();
        Class<?> atC = Class.forName("android.app.ActivityThread");
        Object at = atC.getMethod("systemMain").invoke(null);
        final Context ctx = (Context) atC.getMethod("getSystemContext").invoke(at);
        log("uid=" + android.os.Process.myUid());

        IBinder b = (IBinder) Class.forName("android.os.ServiceManager")
                .getMethod("getService", String.class).invoke(null, "media_projection");
        Class<?> mpmIfc = Class.forName("android.media.projection.IMediaProjectionManager");
        Object mpm = Class.forName("android.media.projection.IMediaProjectionManager$Stub")
                .getMethod("asInterface", IBinder.class).invoke(null, b);

        Class<?> extC = Class.forName("horizonos.media.projection.MediaProjectionManagerExt");
        // MetaCam uses getSystemService(MediaProjectionManagerExt.class), but that
        // NPEs in a bare app_process (no Vros app info); the ctor works for us.
        Object ext;
        try { ext = ctx.getSystemService(extC); }
        catch (Throwable t) { log("getSystemService failed (" + t + "), using ctor"); ext = null; }
        if (ext == null) ext = extC.getConstructor(Context.class, mpmIfc).newInstance(ctx, mpm);
        log("ext=" + ext);

        // Get the FOCUSED volumetric window (what the user sees). Its token
        // references a real recordable window, unlike the global token (which
        // maps to a system overlay with no WindowContainerToken).
        Class<?> tokC = Class.forName("horizonos.view.VolumetricWindowToken");
        IBinder ivwB = (IBinder) Class.forName("android.os.ServiceManager")
                .getMethod("getService", String.class).invoke(null, "interoperable_volumetric_window");
        Object ivw = Class.forName("internal.horizonos.vwr.IInteroperableVolumetricWindowManager$Stub")
                .getMethod("asInterface", IBinder.class).invoke(null, ivwB);
        Object focused = ivw.getClass().getMethod("getFocusedVolumetricWindow").invoke(ivw);
        log("focused=" + focused + " (" + (focused == null ? "null" : focused.getClass().getName()) + ")");
        try { log("activeImmersivePkg=" + ivw.getClass().getMethod("getActiveImmersiveAppPackage").invoke(ivw)); } catch (Throwable t) { log("immPkg err " + t); }

        Object foc = (focused != null && !tokC.isInstance(focused))
                ? ivw.getClass().getMethod("getWindowToken", IBinder.class).invoke(ivw, focused) : focused;

        String strat = STRAT;
        Object token;
        if ("whole".equals(strat)) {
            token = null; // null token + AUTO_MIRROR mirrors the whole flat view
            log("whole view: null token + AUTO_MIRROR");
        } else if ("global".equals(strat)) {
            token = ivw.getClass().getMethod("getGlobalWindowToken", tokC).invoke(ivw, foc);
            log("getGlobalWindowToken -> " + token);
        } else if (strat.startsWith("vwt")) {
            IBinder vwB = (IBinder) Class.forName("android.os.ServiceManager")
                    .getMethod("getService", String.class).invoke(null, "volumetric_window");
            Object vw = Class.forName("internal.horizonos.vwr.IVolumetricWindowManager$Stub")
                    .getMethod("asInterface", IBinder.class).invoke(null, vwB);
            int n = Integer.parseInt(strat.substring(3));
            token = vw.getClass().getMethod("getVolumetricWindowToken", int.class).invoke(vw, n);
            log("getVolumetricWindowToken(" + n + ") -> " + token);
        } else {
            token = (foc != null) ? foc : tokC.getMethod("createGlobalVolumetricWindowToken").invoke(null);
        }
        log("token=" + token);

        MediaProjection mp = (MediaProjection) extC
                .getMethod("createProjectionToken", tokC).invoke(ext, token);
        log("mediaProjection=" + mp);

        final Handler h = new Handler(Looper.getMainLooper());
        final ImageReader reader = ImageReader.newInstance(W, H, PixelFormat.RGBA_8888, 2);
        final boolean[] saved = {false};
        reader.setOnImageAvailableListener(new ImageReader.OnImageAvailableListener() {
            public void onImageAvailable(ImageReader r) {
                if (saved[0]) return;
                Image img = null;
                try {
                    img = r.acquireLatestImage();
                    if (img == null) return;
                    Image.Plane p = img.getPlanes()[0];
                    int rowPixels = p.getRowStride() / p.getPixelStride();
                    Bitmap full = Bitmap.createBitmap(rowPixels, H, Bitmap.Config.ARGB_8888);
                    full.copyPixelsFromBuffer(p.getBuffer());
                    Bitmap out = Bitmap.createBitmap(full, 0, 0, W, H);
                    FileOutputStream fos = new FileOutputStream("/data/local/tmp/qflat.png");
                    out.compress(Bitmap.CompressFormat.PNG, 100, fos);
                    fos.close();
                    saved[0] = true;
                    log("RESULT: SAVED /data/local/tmp/qflat.png (" + W + "x" + H + ")");
                    System.exit(0);
                } catch (Throwable t) {
                    log("img err " + t);
                } finally {
                    if (img != null) img.close();
                }
            }
        }, h);

        mp.registerCallback(new MediaProjection.Callback() {}, h);
        // MetaCam uses name "panel_capture" + flags 0x10 (AUTO_MIRROR).
        VirtualDisplay vd = mp.createVirtualDisplay("panel_capture", W, H, 240, 0x10, reader.getSurface(), null, h);
        log("virtualDisplay=" + vd);

        h.postDelayed(new Runnable() {
            public void run() { log("RESULT: timeout — no frame (saved=" + saved[0] + ")"); System.exit(6); }
        }, 8000);
        Looper.loop();
    }
}

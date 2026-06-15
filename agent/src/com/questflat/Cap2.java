package com.questflat;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.PixelFormat;
import android.hardware.display.VirtualDisplay;
import android.media.Image;
import android.media.ImageReader;
import android.media.projection.MediaProjection;
import android.os.Binder;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.Parcel;
import android.os.Process;
import android.os.RemoteException;

import java.io.FileOutputStream;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * Capture using the HOST volumetric window token (the real content), obtained
 * from theaterview via a hand-rolled Binder callback (we can't compile against
 * the AIDL). Then MediaProjectionManagerExt.createProjectionToken -> capture.
 */
public class Cap2 {
    static final String TAG = "QFLATCAP2";
    static void log(String s) { System.out.println(TAG + ": " + s); }
    static final String SVC = "dev.vros.internal.app.theater.ITheaterViewService";
    static final String CB = "dev.vros.internal.app.theater.ITheaterViewHostSubscribeCallbackVolumetricWindow";
    static final String DATA = "dev.vros.internal.app.theater.TheaterViewDataParcelableVolumetricWindow";
    static volatile Object hostToken;
    static final CountDownLatch latch = new CountDownLatch(1);
    static final int W = 1280, H = 720;

    public static void main(String[] a) {
        try { run(); } catch (Throwable t) { log("FATAL " + t); t.printStackTrace(); System.exit(2); }
    }

    static void run() throws Exception {
        Looper.prepareMainLooper();
        Class<?> atC = Class.forName("android.app.ActivityThread");
        Object at = atC.getMethod("systemMain").invoke(null);
        final Context ctx = (Context) atC.getMethod("getSystemContext").invoke(at);
        log("uid=" + Process.myUid());

        IBinder theater = (IBinder) Class.forName("android.os.ServiceManager")
                .getMethod("getService", String.class).invoke(null, "theaterview");
        int txSub = txCode(SVC + "$Stub", "TRANSACTION_subscribeTheaterViewHostVolumetricWindow");
        final int txOnSet = txCode(CB + "$Stub", "TRANSACTION_onTheaterViewSet");
        log("txSub=" + txSub + " txOnSet=" + txOnSet);

        Binder cb = new Binder() {
            protected boolean onTransact(int code, Parcel data, Parcel reply, int flags) throws RemoteException {
                if (code == txOnSet) {
                    try {
                        data.enforceInterface(CB);
                        Object tv = null;
                        if (data.readInt() != 0) {
                            Class<?> dc = Class.forName(DATA);
                            Object creator = dc.getField("CREATOR").get(null);
                            tv = creator.getClass().getMethod("createFromParcel", Parcel.class).invoke(creator, data);
                        }
                        log("onTheaterViewSet -> " + tv);
                        Object tok = extractToken(tv);
                        log("host token = " + tok);
                        if (tok != null) { hostToken = tok; latch.countDown(); }
                    } catch (Throwable t) { log("cb err " + t); }
                    if (reply != null) reply.writeNoException();
                    return true;
                }
                return super.onTransact(code, data, reply, flags);
            }
        };
        cb.attachInterface(null, CB);

        Parcel d = Parcel.obtain(), r = Parcel.obtain();
        try {
            d.writeInterfaceToken(SVC);
            d.writeStrongBinder(cb);
            theater.transact(txSub, d, r, 0);
            try { r.readException(); } catch (Throwable ignore) {}
            log("subscribe sent");
        } finally { d.recycle(); r.recycle(); }

        boolean got = latch.await(5, TimeUnit.SECONDS);
        log("host token arrived=" + got);
        if (hostToken == null) {
            log("RESULT: no host token (is an app/2D panel focused in the headset?)");
            System.exit(7);
        }
        capture(ctx, hostToken);

        new Handler(Looper.getMainLooper()).postDelayed(new Runnable() {
            public void run() { log("RESULT: timeout, no frame"); System.exit(6); }
        }, 8000);
        Looper.loop();
    }

    static int txCode(String cls, String field) throws Exception {
        Field f = Class.forName(cls).getDeclaredField(field);
        f.setAccessible(true);
        return f.getInt(null);
    }

    static Object extractToken(Object data) throws Exception {
        if (data == null) return null;
        Class<?> tokC = Class.forName("horizonos.view.VolumetricWindowToken");
        for (Method m : data.getClass().getMethods())
            if (m.getParameterTypes().length == 0 && tokC.isAssignableFrom(m.getReturnType())) {
                Object v = m.invoke(data); if (v != null) return v;
            }
        for (Field f : data.getClass().getDeclaredFields())
            if (tokC.isAssignableFrom(f.getType())) {
                f.setAccessible(true); Object v = f.get(data); if (v != null) return v;
            }
        log("no token in " + data.getClass() + " fields=" + java.util.Arrays.toString(data.getClass().getDeclaredFields()));
        return null;
    }

    static void capture(Context ctx, Object token) throws Exception {
        IBinder b = (IBinder) Class.forName("android.os.ServiceManager")
                .getMethod("getService", String.class).invoke(null, "media_projection");
        Class<?> mpmIfc = Class.forName("android.media.projection.IMediaProjectionManager");
        Object mpm = Class.forName("android.media.projection.IMediaProjectionManager$Stub")
                .getMethod("asInterface", IBinder.class).invoke(null, b);
        Class<?> extC = Class.forName("horizonos.media.projection.MediaProjectionManagerExt");
        Object ext = extC.getConstructor(Context.class, mpmIfc).newInstance(ctx, mpm);
        MediaProjection mp = (MediaProjection) extC
                .getMethod("createProjectionToken", Class.forName("horizonos.view.VolumetricWindowToken"))
                .invoke(ext, token);
        log("mp=" + mp);

        final Handler h = new Handler(Looper.getMainLooper());
        final ImageReader reader = ImageReader.newInstance(W, H, PixelFormat.RGBA_8888, 2);
        final boolean[] saved = {false};
        reader.setOnImageAvailableListener(new ImageReader.OnImageAvailableListener() {
            public void onImageAvailable(ImageReader rd) {
                if (saved[0]) return;
                Image img = null;
                try {
                    img = rd.acquireLatestImage();
                    if (img == null) return;
                    Image.Plane p = img.getPlanes()[0];
                    int rw = p.getRowStride() / p.getPixelStride();
                    Bitmap full = Bitmap.createBitmap(rw, H, Bitmap.Config.ARGB_8888);
                    full.copyPixelsFromBuffer(p.getBuffer());
                    Bitmap out = Bitmap.createBitmap(full, 0, 0, W, H);
                    FileOutputStream fos = new FileOutputStream("/data/local/tmp/qflat.png");
                    out.compress(Bitmap.CompressFormat.PNG, 100, fos);
                    fos.close();
                    saved[0] = true;
                    log("RESULT: SAVED /data/local/tmp/qflat.png");
                    System.exit(0);
                } catch (Throwable t) { log("img err " + t); }
                finally { if (img != null) img.close(); }
            }
        }, h);
        mp.registerCallback(new MediaProjection.Callback() {}, h);
        VirtualDisplay vd = mp.createVirtualDisplay("qflat", W, H, 240, 0, reader.getSurface(), null, h);
        log("vd=" + vd);
    }
}

package com.questflat;

import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.ServiceConnection;
import android.content.pm.PackageInfo;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.content.pm.ServiceInfo;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;

import java.lang.reflect.Method;
import java.util.List;

/**
 * C3 de-risk probe. Runs via app_process as the (unrooted) shell uid:
 *   CLASSPATH=/data/local/tmp/probe.jar app_process /system/bin com.questflat.Probe
 *
 * Goal: prove whether a shell-uid caller can reach + bind
 * com.oculus.vrapi.ScreenCaptureService (the flat MetaCam capture service,
 * gated by METACAM_SCREEN_CAPTURE which shell holds). It self-discovers the
 * host package, attempts the bind, and logs permitted/denied + the binder
 * interface descriptor. No frames yet — this is the go/no-go gate.
 */
public class Probe {
    static final String TAG = "QFLATPROBE";
    static void log(String s) { System.out.println(TAG + ": " + s); }

    public static void main(String[] args) {
        try {
            run();
        } catch (Throwable t) {
            log("FATAL " + t);
            t.printStackTrace();
            System.exit(2);
        }
    }

    static void run() throws Exception {
        Looper.prepareMainLooper();

        // System context via ActivityThread (hidden API; reflect so we compile
        // against android.jar only).
        Class<?> atC = Class.forName("android.app.ActivityThread");
        Object at = atC.getMethod("systemMain").invoke(null);
        Method getSysCtx = atC.getMethod("getSystemContext");
        final Context ctx = (Context) getSysCtx.invoke(at);
        log("context=" + ctx + " uid=" + android.os.Process.myUid());

        PackageManager pm = ctx.getPackageManager();

        // --- discover the ScreenCaptureService host ---
        Intent target = null;
        String[] actions = {
            "com.oculus.aidl.IScreenCaptureService",
            "com.oculus.vrapi.ScreenCaptureService",
        };
        for (String a : actions) {
            try {
                List<ResolveInfo> ris = pm.queryIntentServices(new Intent(a), PackageManager.MATCH_ALL);
                log("query action " + a + " -> " + (ris == null ? 0 : ris.size()));
                if (ris != null) {
                    for (ResolveInfo ri : ris) {
                        ServiceInfo si = ri.serviceInfo;
                        log("  host pkg=" + si.packageName + " name=" + si.name
                                + " perm=" + si.permission + " exported=" + si.exported);
                        if (target == null) {
                            target = new Intent(a);
                            target.setClassName(si.packageName, si.name);
                        }
                    }
                }
            } catch (Throwable t) {
                log("query " + a + " threw " + t);
            }
        }

        // Fallback: enumerate every package's services for *ScreenCaptureService.
        if (target == null) {
            log("no resolve via action; enumerating package services...");
            for (PackageInfo pi : pm.getInstalledPackages(PackageManager.GET_SERVICES)) {
                if (pi.services == null) continue;
                for (ServiceInfo si : pi.services) {
                    if (si.name != null && si.name.contains("ScreenCaptureService")) {
                        log("  found service pkg=" + si.packageName + " name=" + si.name
                                + " perm=" + si.permission + " exported=" + si.exported);
                        if (target == null) {
                            target = new Intent();
                            target.setClassName(si.packageName, si.name);
                        }
                    }
                }
            }
        }

        if (target == null) {
            log("RESULT: could not locate ScreenCaptureService host (not visible to shell).");
            System.exit(1);
        }
        log("binding " + target.getComponent());

        ServiceConnection conn = new ServiceConnection() {
            public void onServiceConnected(ComponentName name, IBinder svc) {
                String desc = "?";
                boolean alive = false;
                try { desc = svc.getInterfaceDescriptor(); } catch (Throwable ignored) {}
                try { alive = svc.isBinderAlive(); } catch (Throwable ignored) {}
                log("RESULT: CONNECTED — shell-uid bind ACCEPTED. name=" + name
                        + " descriptor=" + desc + " alive=" + alive);
                System.exit(0);
            }
            public void onServiceDisconnected(ComponentName name) { log("disconnected " + name); }
            public void onBindingDied(ComponentName name) { log("RESULT: BINDING DIED " + name); System.exit(3); }
            public void onNullBinding(ComponentName name) { log("RESULT: NULL BINDING " + name); System.exit(4); }
        };

        boolean ok = false;
        try {
            ok = ctx.bindService(target, conn, Context.BIND_AUTO_CREATE);
            log("bindService returned " + ok + " (callback may follow)");
        } catch (SecurityException se) {
            log("bindService SecurityException (app_process app-record wall, not perm): " + se.getMessage());
        } catch (Throwable t) {
            log("bindService threw " + t);
        }

        // --- Phase 2: ServiceManager paths (getService works from app_process,
        // no app record needed). This is the promising agent route. ---
        dumpServiceManagerPath("media_projection",
                "android.media.projection.IMediaProjectionManager");
        dumpServiceManagerPath("spatialmedia",
                "vros.spatial.media.ISpatialMediaManagerService");
        dumpServiceManagerPath("SurfaceForger", null);
        dumpServiceManagerPath("theaterview", "dev.vros.internal.app.theater.ITheaterViewService");
        dumpServiceManagerPath("volumetric_window", "internal.horizonos.vwr.IVolumetricWindowManager");
        dumpServiceManagerPath("interoperable_volumetric_window", "internal.horizonos.vwr.IInteroperableVolumetricWindowManager");
        dumpServiceManagerPath("OculusWindowManager", "oculus.internal.IOculusWindowManager");

        // The Quest panel/flat capture extension MetaCam uses — does it exist
        // and what does it expose? (the likely unrooted flat door)
        for (String cn : new String[] {
                "horizonos.media.projection.MediaProjectionManagerExt",
                "horizonos.view.VolumetricWindowToken",
                "dev.vros.internal.app.theater.ITheaterViewHostSubscribeCallbackVolumetricWindow",
                "dev.vros.internal.app.theater.ITheaterViewService" }) {
            dumpClass(cn);
        }

        // Can a non-app_process caller actually MINT a projection? (shell holds
        // MANAGE_MEDIA_PROJECTION). createProjection(uid, pkg, type, permanent).
        try {
            Class<?> smC = Class.forName("android.os.ServiceManager");
            IBinder b = (IBinder) smC.getMethod("getService", String.class).invoke(null, "media_projection");
            Object imp = Class.forName("android.media.projection.IMediaProjectionManager$Stub")
                    .getMethod("asInterface", IBinder.class).invoke(null, b);
            Method create = Class.forName("android.media.projection.IMediaProjectionManager")
                    .getMethod("createProjection", int.class, String.class, int.class, boolean.class);
            Object proj = create.invoke(imp, android.os.Process.myUid(), "com.android.shell", 0, false);
            log("RESULT: createProjection OK -> " + proj);
        } catch (Throwable t) {
            Throwable c = (t.getCause() != null) ? t.getCause() : t;
            log("RESULT: createProjection FAILED -> " + c);
        }

        final boolean bound = ok;
        new Handler(Looper.getMainLooper()).postDelayed(new Runnable() {
            public void run() {
                log("done (bind=" + bound + ")");
                System.exit(0);
            }
        }, 3000);
        Looper.loop();
    }

    /** Reflect a class's constructors + methods (or report it's not loadable). */
    static void dumpClass(String cn) {
        String s = cn.substring(cn.lastIndexOf('.') + 1);
        try {
            Class<?> c = Class.forName(cn);
            log("CLASS " + cn + " FOUND (iface=" + c.isInterface() + ")");
            for (java.lang.reflect.Constructor<?> k : c.getDeclaredConstructors())
                log("   " + s + ".<init> " + java.util.Arrays.toString(k.getParameterTypes()));
            for (Method m : c.getDeclaredMethods())
                log("   " + s + "." + m.getName() + " " + java.util.Arrays.toString(m.getParameterTypes())
                        + " -> " + m.getReturnType().getSimpleName());
        } catch (Throwable t) {
            log("CLASS " + cn + " not loadable (" + t.getClass().getSimpleName() + ")");
        }
    }

    /** getService a ServiceManager-registered binder and reflect its interface. */
    static void dumpServiceManagerPath(String svcName, String ifaceClass) {
        try {
            Class<?> smC = Class.forName("android.os.ServiceManager");
            IBinder b = (IBinder) smC.getMethod("getService", String.class).invoke(null, svcName);
            if (b == null) { log("SM " + svcName + " = null (not registered/visible)"); return; }
            log("SM " + svcName + " binder=" + b + " descriptor=" + b.getInterfaceDescriptor()
                    + " alive=" + b.isBinderAlive());
            if (ifaceClass != null) {
                Class<?> ifc = Class.forName(ifaceClass);
                for (Method m : ifc.getDeclaredMethods()) {
                    log("   " + svcName + " m: " + m.getName() + " " + java.util.Arrays.toString(m.getParameterTypes()));
                }
            }
        } catch (Throwable t) {
            log("SM " + svcName + " err " + t);
        }
    }
}

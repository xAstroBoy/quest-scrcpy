package com.questflat;

import android.content.AttributionSource;
import android.content.Context;
import android.content.ContextWrapper;
import android.os.Process;

/**
 * A context that claims to be the shell package, so system services that check
 * "packageName must match the calling uid" accept calls from our unrooted
 * `app_process` (uid 2000 = shell = com.android.shell). Same trick scrcpy uses.
 */
public class FakeContext extends ContextWrapper {
    public static final String PACKAGE_NAME = "com.android.shell";

    public FakeContext(Context base) {
        super(base);
    }

    @Override
    public String getPackageName() {
        return PACKAGE_NAME;
    }

    @Override
    public String getOpPackageName() {
        return PACKAGE_NAME;
    }

    @Override
    public Context getApplicationContext() {
        return this;
    }

    /** API 31+: identity used by many system services. Match shell uid + package. */
    @Override
    public AttributionSource getAttributionSource() {
        AttributionSource.Builder b = new AttributionSource.Builder(Process.myUid());
        b.setPackageName(PACKAGE_NAME);
        return b.build();
    }
}

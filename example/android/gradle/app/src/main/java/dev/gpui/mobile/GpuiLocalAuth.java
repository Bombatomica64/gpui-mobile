package dev.gpui.mobile;

import android.app.Activity;
import android.content.Intent;
import android.os.Build;

import java.util.ArrayList;
import java.util.concurrent.CountDownLatch;

/**
 * Biometric authentication helper for GPUI.
 *
 * <p>Uses BiometricManager (API 29+) for availability checks and launches
 * {@link GpuiAuthActivity} (a transparent FragmentActivity) to show the
 * BiometricPrompt dialog.</p>
 *
 * <p>The {@code authenticate} method blocks the calling thread, which must not be
 * the UI thread, until the prompt completes or {@link GpuiAuthActivity} goes away.</p>
 */
public final class GpuiLocalAuth {

    /** One authentication request, and the result code the calling thread waits for. */
    static final class Request {
        final String reason;
        final CountDownLatch done = new CountDownLatch(1);
        int result = 7;

        Request(String reason) { this.reason = reason; }

        synchronized void complete(int result) {
            if (done.getCount() == 0) return;
            this.result = result;
            done.countDown();
        }
    }

    /** The request being shown; one at a time. */
    private static Request sPending;

    static synchronized Request pending() { return sPending; }

    // Result codes (must match Rust int_to_auth_result)
    // 0 = success, 1 = failed, 2 = not_available, 3 = not_enrolled,
    // 4 = cancelled, 5 = passcode_not_set, 6 = lockout, 7 = other

    /**
     * Check if the device has biometric hardware.
     *
     * @return {@code true} if biometric hardware is present (even if not enrolled).
     */
    public static boolean isDeviceSupported(Activity activity) {
        if (Build.VERSION.SDK_INT >= 29) {
            android.hardware.biometrics.BiometricManager bm =
                activity.getSystemService(android.hardware.biometrics.BiometricManager.class);
            if (bm != null) {
                int result = bm.canAuthenticate();
                return result != android.hardware.biometrics.BiometricManager.BIOMETRIC_ERROR_HW_UNAVAILABLE
                    && result != android.hardware.biometrics.BiometricManager.BIOMETRIC_ERROR_NO_HARDWARE;
            }
        }
        // Fallback for API < 29: check for fingerprint hardware
        return activity.getPackageManager().hasSystemFeature("android.hardware.fingerprint");
    }

    /**
     * Check if biometrics are enrolled and ready to use.
     *
     * @return {@code true} if the user can authenticate right now.
     */
    public static boolean canAuthenticate(Activity activity) {
        if (Build.VERSION.SDK_INT >= 29) {
            android.hardware.biometrics.BiometricManager bm =
                activity.getSystemService(android.hardware.biometrics.BiometricManager.class);
            if (bm != null) {
                return bm.canAuthenticate() == android.hardware.biometrics.BiometricManager.BIOMETRIC_SUCCESS;
            }
        }
        return false;
    }

    /**
     * Get pipe-delimited list of available biometric types.
     *
     * @return e.g. "fingerprint|face" or "" if none.
     */
    public static String getAvailableBiometrics(Activity activity) {
        ArrayList<String> types = new ArrayList<>();
        if (activity.getPackageManager().hasSystemFeature("android.hardware.fingerprint")) {
            types.add("fingerprint");
        }
        if (activity.getPackageManager().hasSystemFeature("android.hardware.biometrics.face")) {
            types.add("face");
        }
        if (activity.getPackageManager().hasSystemFeature("android.hardware.biometrics.iris")) {
            types.add("iris");
        }
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < types.size(); i++) {
            if (i > 0) sb.append("|");
            sb.append(types.get(i));
        }
        return sb.toString();
    }

    /**
     * Authenticate the user with biometrics. Blocks until complete.
     *
     * <p>Launches {@link GpuiAuthActivity} which shows a BiometricPrompt.
     * The calling thread blocks until the prompt callback fires or the
     * Activity is destroyed.</p>
     *
     * @param activity the current Activity context
     * @param reason   the reason string shown to the user
     * @return result code (0=success, 1=failed, 2=not_available, etc.)
     */
    public static int authenticate(Activity activity, String reason) throws InterruptedException {
        if (!canAuthenticate(activity)) {
            if (!isDeviceSupported(activity)) {
                return 2; // not available
            }
            return 3; // not enrolled
        }

        Request request = new Request(reason);
        synchronized (GpuiLocalAuth.class) {
            if (sPending != null) throw new IllegalStateException("An authentication prompt is already open");
            sPending = request;
        }
        try {
            // Launch GpuiAuthActivity which will show BiometricPrompt
            activity.startActivity(new Intent(activity, GpuiAuthActivity.class));
            request.done.await();
            return request.result;
        } finally {
            synchronized (GpuiLocalAuth.class) {
                if (sPending == request) sPending = null;
            }
        }
    }

    private GpuiLocalAuth() {}
}

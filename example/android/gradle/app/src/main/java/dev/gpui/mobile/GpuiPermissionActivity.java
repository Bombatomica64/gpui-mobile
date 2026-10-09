package dev.gpui.mobile;

import android.app.Activity;
import android.os.Bundle;

import java.util.concurrent.CountDownLatch;

/**
 * Transparent helper Activity for handling runtime permission requests.
 *
 * <p>NativeActivity does not receive onRequestPermissionsResult callbacks,
 * so this lightweight Activity is used as a proxy.</p>
 *
 * <p>Handles process death: if the process is killed while the permission
 * dialog is showing, the recreated Activity gracefully finishes without
 * crashing.</p>
 */
public class GpuiPermissionActivity extends Activity {

    private static final int PERMISSION_REQUEST_CODE = 9002;
    private static final String KEY_WAITING = "gpui_waiting_for_permission";

    /** One permission request, and the grant results the calling thread waits for. */
    static final class Request {
        final String[] permissions;
        final CountDownLatch done = new CountDownLatch(1);
        /** Null, or shorter than {@code permissions}, if the request was interrupted. */
        int[] grantResults;

        Request(String[] permissions) { this.permissions = permissions; }

        synchronized void complete(int[] grantResults) {
            if (done.getCount() == 0) return;
            this.grantResults = grantResults;
            done.countDown();
        }
    }

    /** The request being shown; one at a time. */
    private static Request sPending;

    private Request mRequest;

    /** Whether we are waiting for a permission result. */
    private boolean mWaitingForResult = false;

    /**
     * Request {@code permissions} and wait for the user's answer.
     *
     * <p>Blocks the calling thread, which must not be the UI thread, until the
     * dialog is answered or this Activity goes away.</p>
     *
     * @return the grant results; null or short if the request was interrupted.
     */
    static int[] request(Activity activity, String[] permissions) throws InterruptedException {
        Request request = new Request(permissions);
        synchronized (GpuiPermissionActivity.class) {
            if (sPending != null) throw new IllegalStateException("A permission request is already open");
            sPending = request;
        }
        try {
            activity.startActivity(new android.content.Intent(activity, GpuiPermissionActivity.class));
            request.done.await();
            return request.grantResults;
        } finally {
            synchronized (GpuiPermissionActivity.class) {
                if (sPending == request) sPending = null;
            }
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        synchronized (GpuiPermissionActivity.class) { mRequest = sPending; }

        if (savedInstanceState != null && savedInstanceState.getBoolean(KEY_WAITING, false)) {
            // Recreated while the permission dialog was showing.
            // The system will re-deliver the result via onRequestPermissionsResult.
            android.util.Log.i("GpuiPermission", "Recreated, waiting for result");
            mWaitingForResult = true;
            return;
        }

        if (mRequest != null && mRequest.permissions.length > 0) {
            mWaitingForResult = true;
            requestPermissions(mRequest.permissions, PERMISSION_REQUEST_CODE);
        } else {
            finish();
        }
    }

    @Override
    protected void onSaveInstanceState(Bundle outState) {
        super.onSaveInstanceState(outState);
        outState.putBoolean(KEY_WAITING, mWaitingForResult);
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        mWaitingForResult = false;
        if (requestCode == PERMISSION_REQUEST_CODE && mRequest != null) {
            mRequest.complete(grantResults);
        }
        finish();
    }

    @Override
    protected void onDestroy() {
        // Gone without an answer, e.g. the task brought to front from the launcher.
        if (isFinishing() && mRequest != null) mRequest.complete(null);
        super.onDestroy();
    }
}

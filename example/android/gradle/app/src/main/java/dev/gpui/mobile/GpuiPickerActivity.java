package dev.gpui.mobile;

import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.net.Uri;
import android.os.Bundle;

import java.util.ArrayList;
import java.util.concurrent.CountDownLatch;

/**
 * Transparent helper Activity that handles startActivityForResult calls.
 *
 * <p>NativeActivity cannot easily receive activity results, so this lightweight
 * transparent Activity is used as a proxy. It launches the requested intent,
 * hands the result to the thread waiting in {@link #launch}, and finishes itself.</p>
 *
 * <p>Handles process death: when Android kills the process while the system
 * picker is in the foreground, this Activity is recreated from savedInstanceState.
 * The picker result is saved to SharedPreferences so it can be retrieved after
 * the app fully restarts.</p>
 *
 * <p>Called from Rust via JNI through GpuiFilePicker / GpuiImagePicker.</p>
 */
public class GpuiPickerActivity extends Activity {

    private static final int REQUEST_CODE = 9001;
    private static final String KEY_WAITING = "gpui_waiting_for_result";
    static final String PREFS_NAME = "gpui_picker_prefs";
    static final String PREF_PENDING_RESULT = "pending_result";
    static final String PREF_HAS_PENDING = "has_pending_result";

    /** One picker request, and the result the calling thread waits for. */
    private static final class Request {
        final Intent intent;
        final CountDownLatch done = new CountDownLatch(1);
        ArrayList<String> uris;
        Exception error;

        Request(Intent intent) { this.intent = intent; }

        synchronized void complete(ArrayList<String> uris, Exception error) {
            if (done.getCount() == 0) return;
            this.uris = uris;
            this.error = error;
            done.countDown();
        }
    }

    /** The request being shown; one at a time. */
    private static Request sPending;

    /** This Activity's request; null after process death, when nobody is waiting. */
    private Request mRequest;

    /** Whether we are waiting for an onActivityResult callback. */
    private boolean mWaitingForResult = false;

    /**
     * Launch {@code intent} through this Activity and wait for its result.
     *
     * <p>Blocks the calling thread, which must not be the UI thread, until the picker
     * returns or this Activity goes away (back, the task brought to front from the
     * launcher, ...).</p>
     *
     * @return the picked URIs, or null if cancelled.
     */
    static ArrayList<String> launch(Activity activity, Intent intent) throws Exception {
        Request request = new Request(intent);
        synchronized (GpuiPickerActivity.class) {
            if (sPending != null) throw new IllegalStateException("A picker is already open");
            sPending = request;
        }
        try {
            activity.startActivity(new Intent(activity, GpuiPickerActivity.class));
            request.done.await();
            if (request.error != null) throw request.error;
            return request.uris;
        } finally {
            synchronized (GpuiPickerActivity.class) {
                if (sPending == request) sPending = null;
            }
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        synchronized (GpuiPickerActivity.class) { mRequest = sPending; }

        if (savedInstanceState != null && savedInstanceState.getBoolean(KEY_WAITING, false)) {
            // Recreated while the system picker was showing (after process death,
            // or with "don't keep activities"). The system delivers the picker
            // result via onActivityResult. Just wait for it — don't launch again.
            android.util.Log.i("GpuiPicker", "Recreated, waiting for result");
            mWaitingForResult = true;
            return;
        }

        if (mRequest == null) {
            finish();
            return;
        }
        try {
            startActivityForResult(mRequest.intent, REQUEST_CODE);
            mWaitingForResult = true;
        } catch (Exception e) {
            android.util.Log.e("GpuiPicker", "Failed to start picker intent", e);
            mRequest.complete(null, e);
            finish();
        }
    }

    @Override
    protected void onSaveInstanceState(Bundle outState) {
        super.onSaveInstanceState(outState);
        outState.putBoolean(KEY_WAITING, mWaitingForResult);
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        mWaitingForResult = false;

        ArrayList<String> uris = null;
        if (requestCode == REQUEST_CODE && resultCode == RESULT_OK && data != null) {
            uris = extractUris(data);
        }

        deliverResult(uris);
        finish();
    }

    @Override
    protected void onDestroy() {
        // Gone without a result: back, or the task brought to front from the launcher.
        if (isFinishing() && mRequest != null) mRequest.complete(null, null);
        super.onDestroy();
    }

    /**
     * Deliver the result to the waiting thread, or save it to SharedPreferences
     * if the process was recreated and nobody is waiting.
     */
    private void deliverResult(ArrayList<String> uris) {
        if (mRequest != null) {
            mRequest.complete(uris, null);
        } else {
            // Process death recovery: save to SharedPreferences for later retrieval.
            android.util.Log.i("GpuiPicker", "No waiting request (process death recovery), saving to prefs");
            savePendingResult(uris);
        }
    }

    /**
     * Extract URIs from the picker result intent.
     */
    private static ArrayList<String> extractUris(Intent data) {
        ArrayList<String> uris = new ArrayList<>();
        if (data.getClipData() != null) {
            int count = data.getClipData().getItemCount();
            for (int i = 0; i < count; i++) {
                Uri uri = data.getClipData().getItemAt(i).getUri();
                if (uri != null) {
                    uris.add(uri.toString());
                }
            }
        } else if (data.getData() != null) {
            uris.add(data.getData().toString());
        }
        return uris;
    }

    /**
     * Save picker result to SharedPreferences so it survives process death.
     */
    private void savePendingResult(ArrayList<String> uris) {
        SharedPreferences prefs = getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE);
        SharedPreferences.Editor editor = prefs.edit();
        if (uris != null && !uris.isEmpty()) {
            // Join with \n separator — URIs don't contain newlines.
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i < uris.size(); i++) {
                if (i > 0) sb.append('\n');
                sb.append(uris.get(i));
            }
            editor.putBoolean(PREF_HAS_PENDING, true);
            editor.putString(PREF_PENDING_RESULT, sb.toString());
        } else {
            editor.putBoolean(PREF_HAS_PENDING, false);
            editor.remove(PREF_PENDING_RESULT);
        }
        editor.apply();
    }

    // ── Static helpers for Rust JNI access ──────────────────────────────

    /**
     * Check if there is a pending picker result from a previous process death.
     * Called from Rust via JNI.
     *
     * @param activity The current Activity context.
     * @return Array of URI strings, or null if no pending result.
     */
    public static String[] getPendingResult(Activity activity) {
        SharedPreferences prefs = activity.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE);
        if (!prefs.getBoolean(PREF_HAS_PENDING, false)) {
            return null;
        }
        String result = prefs.getString(PREF_PENDING_RESULT, null);
        // Clear the pending result.
        prefs.edit()
            .putBoolean(PREF_HAS_PENDING, false)
            .remove(PREF_PENDING_RESULT)
            .apply();

        if (result == null || result.isEmpty()) {
            return null;
        }
        return result.split("\n");
    }

    /**
     * Clear any pending picker result.
     * Called from Rust via JNI.
     */
    public static void clearPendingResult(Activity activity) {
        activity.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(PREF_HAS_PENDING, false)
            .remove(PREF_PENDING_RESULT)
            .apply();
    }
}

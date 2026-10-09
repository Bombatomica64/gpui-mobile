package dev.gpui.mobile;

import android.app.Activity;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.database.Cursor;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.Matrix;
import android.media.ExifInterface;
import android.net.Uri;
import android.os.Bundle;
import android.provider.OpenableColumns;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.UUID;
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
    private static final String KEY_IMPORTING = "gpui_importing";
    static final String PREFS_NAME = "gpui_picker_prefs";
    static final String PREF_PENDING_RESULT = "pending_result";
    static final String PREF_HAS_PENDING = "has_pending_result";

    /**
     * What to do with the picked documents. Without one, the caller gets the URIs.
     *
     * <p>With one, each document is copied into the cache directory (under its
     * display name) while this Activity still holds the URI grants, and the caller
     * gets file paths. {@code cameraOutput} is a file the camera app was asked to
     * write instead. Images are scaled down to {@code maxWidth} x {@code maxHeight}
     * and re-encoded at {@code quality} (0-100) when any of them is positive.</p>
     */
    static final class Import {
        int maxWidth;
        int maxHeight;
        int quality;
        File cameraOutput;
    }

    /** One picker request, and the result the calling thread waits for. */
    private static final class Request {
        final Intent intent;
        final Import importing;
        final CountDownLatch done = new CountDownLatch(1);
        ArrayList<String> uris;
        Exception error;

        Request(Intent intent, Import importing) {
            this.intent = intent;
            this.importing = importing;
        }

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

    /** Whether picked documents are being copied for {@link #mRequest}. */
    private boolean mImporting = false;

    /**
     * Launch {@code intent} through this Activity and wait for its result.
     *
     * <p>Blocks the calling thread, which must not be the UI thread, until the picker
     * returns or this Activity goes away (back, the task brought to front from the
     * launcher, ...).</p>
     *
     * @return the picked URIs, or file paths when {@code importing} is set; null if cancelled.
     */
    static ArrayList<String> launch(Activity activity, Intent intent, Import importing) throws Exception {
        Request request = new Request(intent, importing);
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

        if (savedInstanceState != null && savedInstanceState.getBoolean(KEY_IMPORTING, false)) {
            // Recreated (e.g. rotated) while copying: the copy carries on and
            // completes the request; don't open the picker again.
            if (mRequest == null) {
                finish();
                return;
            }
            mImporting = true;
            Request request = mRequest;
            new Thread(() -> {
                try {
                    request.done.await();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
                runOnUiThread(this::finish);
            }, "gpui-picker-wait").start();
            return;
        }

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
        outState.putBoolean(KEY_IMPORTING, mImporting);
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        mWaitingForResult = false;
        if (requestCode != REQUEST_CODE) return;

        ArrayList<Uri> uris = new ArrayList<>();
        if (resultCode == RESULT_OK && data != null) uris = extractUris(data);
        Import importing = mRequest != null ? mRequest.importing : null;
        if (importing == null || resultCode != RESULT_OK) {
            deliverResult(resultCode == RESULT_OK ? toStrings(uris) : null);
            finish();
            return;
        }
        final ArrayList<Uri> picked = uris;
        final Request request = mRequest;
        mImporting = true;
        // Keep this Activity (and its temporary URI grants) alive while copying;
        // documents from remote providers can be large, so not on the UI thread.
        new Thread(() -> {
            ArrayList<String> paths = new ArrayList<>();
            try {
                if (importing.cameraOutput != null) {
                    if (importing.cameraOutput.length() > 0) {
                        paths.add(scaleImage(importing.cameraOutput, importing).getAbsolutePath());
                    }
                } else {
                    for (Uri uri : picked) {
                        File file = importFile(uri);
                        if (importing.maxWidth > 0 || importing.maxHeight > 0 || importing.quality > 0) {
                            file = scaleImage(file, importing);
                        }
                        paths.add(file.getAbsolutePath());
                    }
                }
                request.complete(paths.isEmpty() ? null : paths, null);
            } catch (Exception e) {
                request.complete(null, e);
            } finally {
                runOnUiThread(this::finish);
            }
        }, "gpui-picker-import").start();
    }

    @Override
    protected void onDestroy() {
        // Gone without a result: back, or the task brought to front from the launcher.
        // (While importing, the copy completes it instead.)
        if (isFinishing() && mRequest != null && !mImporting) mRequest.complete(null, null);
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
    private static ArrayList<Uri> extractUris(Intent data) {
        ArrayList<Uri> uris = new ArrayList<>();
        if (data.getClipData() != null) {
            int count = data.getClipData().getItemCount();
            for (int i = 0; i < count; i++) {
                Uri uri = data.getClipData().getItemAt(i).getUri();
                if (uri != null) {
                    uris.add(uri);
                }
            }
        } else if (data.getData() != null) {
            uris.add(data.getData());
        }
        return uris;
    }

    private static ArrayList<String> toStrings(ArrayList<Uri> uris) {
        ArrayList<String> strings = new ArrayList<>();
        for (Uri uri : uris) strings.add(uri.toString());
        return strings;
    }

    /** A new, empty directory in the cache for one picked file. */
    static File newCacheDirectory(Context context) throws IOException {
        File directory = new File(new File(context.getCacheDir(), "gpui-picked"), UUID.randomUUID().toString());
        if (!directory.mkdirs()) throw new IOException("Could not create " + directory);
        return directory;
    }

    /** Copy a document into the cache, keeping its display name. */
    private File importFile(Uri uri) throws IOException {
        String name = null;
        try (Cursor cursor = getContentResolver().query(uri,
                new String[] { OpenableColumns.DISPLAY_NAME }, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) name = cursor.getString(0);
        } catch (Exception e) {
            // Not every provider answers queries; fall back to the URI.
        }
        if (name == null || name.isEmpty()) name = uri.getLastPathSegment();
        // A provider's display name must not escape the import directory.
        if (name == null) name = "file";
        name = name.replace('/', '_').replace('\\', '_').replace('\0', '_');
        if (name.isEmpty() || name.equals(".") || name.equals("..")) name = "file";
        File file = new File(newCacheDirectory(this), name);
        try (InputStream input = getContentResolver().openInputStream(uri);
             FileOutputStream output = new FileOutputStream(file)) {
            if (input == null) throw new IOException("Could not read " + uri);
            byte[] buffer = new byte[64 * 1024];
            int count;
            while ((count = input.read(buffer)) != -1) output.write(buffer, 0, count);
        }
        return file;
    }

    /**
     * Scale an image down to fit {@code importing}'s bounds, upright, re-encoded at
     * its quality. Returns {@code file} itself when it is not an image or needs nothing.
     */
    private static File scaleImage(File file, Import importing) throws IOException {
        BitmapFactory.Options bounds = new BitmapFactory.Options();
        bounds.inJustDecodeBounds = true;
        BitmapFactory.decodeFile(file.getPath(), bounds);
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return file; // not an image

        int rotation = 0;
        try {
            switch (new ExifInterface(file.getPath()).getAttributeInt(
                    ExifInterface.TAG_ORIENTATION, ExifInterface.ORIENTATION_NORMAL)) {
                case ExifInterface.ORIENTATION_ROTATE_90: rotation = 90; break;
                case ExifInterface.ORIENTATION_ROTATE_180: rotation = 180; break;
                case ExifInterface.ORIENTATION_ROTATE_270: rotation = 270; break;
            }
        } catch (IOException e) {
            // No readable EXIF: keep the stored orientation.
        }
        boolean turned = rotation == 90 || rotation == 270;
        int width = turned ? bounds.outHeight : bounds.outWidth;
        int height = turned ? bounds.outWidth : bounds.outHeight;
        double scale = 1.0;
        if (importing.maxWidth > 0) scale = Math.min(scale, (double) importing.maxWidth / width);
        if (importing.maxHeight > 0) scale = Math.min(scale, (double) importing.maxHeight / height);
        if (scale >= 1.0 && importing.quality <= 0 && rotation == 0) return file;

        BitmapFactory.Options decode = new BitmapFactory.Options();
        decode.inSampleSize = 1;
        while (scale * decode.inSampleSize * 2 <= 1.0) decode.inSampleSize *= 2;
        Bitmap bitmap = BitmapFactory.decodeFile(file.getPath(), decode);
        if (bitmap == null) return file;
        Matrix matrix = new Matrix();
        double sampled = scale * decode.inSampleSize;
        if (sampled < 1.0) matrix.postScale((float) sampled, (float) sampled);
        matrix.postRotate(rotation);
        Bitmap output = Bitmap.createBitmap(bitmap, 0, 0, bitmap.getWidth(), bitmap.getHeight(), matrix, true);

        String name = file.getName();
        int dot = name.lastIndexOf('.');
        boolean png = name.toLowerCase(java.util.Locale.ROOT).endsWith(".png");
        File scaled = new File(file.getParentFile(),
                (dot > 0 ? name.substring(0, dot) : name) + "-scaled" + (png ? ".png" : ".jpg"));
        try (FileOutputStream stream = new FileOutputStream(scaled)) {
            output.compress(png ? Bitmap.CompressFormat.PNG : Bitmap.CompressFormat.JPEG,
                    importing.quality > 0 ? Math.min(importing.quality, 100) : 100, stream);
        } finally {
            if (output != bitmap) output.recycle();
            bitmap.recycle();
        }
        file.delete();
        return scaled;
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

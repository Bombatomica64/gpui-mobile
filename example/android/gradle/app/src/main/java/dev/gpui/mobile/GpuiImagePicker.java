package dev.gpui.mobile;

import android.app.Activity;
import android.content.Intent;
import android.net.Uri;
import android.os.Build;
import android.provider.MediaStore;

import androidx.core.content.FileProvider;

import java.io.File;
import java.util.ArrayList;

/**
 * Image/video picker helper for gallery selection and camera capture.
 *
 * <p>All public methods are static and called from Rust via JNI.
 * They block the calling thread, which must not be the UI thread, until the user
 * completes or cancels the picker. They return paths of copies in the cache
 * directory.</p>
 *
 * <p>Camera capture needs a {@code FileProvider} with the authority
 * {@code <applicationId>.gpui.fileprovider} that shares the {@code gpui-picked/}
 * cache path; see the example's AndroidManifest.xml.</p>
 */
public final class GpuiImagePicker {

    /** Source constants matching Rust's ImageSource enum. */
    private static final int SOURCE_GALLERY = 0;
    private static final int SOURCE_CAMERA = 1;

    /** Camera facing constants matching Rust's CameraDevice enum. */
    private static final int CAMERA_REAR = 0;
    private static final int CAMERA_FRONT = 1;

    /**
     * Pick a single image from gallery or camera.
     *
     * @param activity     The current Activity.
     * @param source       0 = gallery, 1 = camera.
     * @param cameraFacing 0 = rear, 1 = front.
     * @param maxWidth     Scale down to this width; 0 = no limit.
     * @param maxHeight    Scale down to this height; 0 = no limit.
     * @param quality      Re-encode at this JPEG quality (1-100); 0 = keep.
     * @return The image path, or null if cancelled.
     */
    public static String pickImage(final Activity activity, int source, int cameraFacing,
                                   int maxWidth, int maxHeight, int quality) throws Exception {
        GpuiPickerActivity.Import importing = scaling(maxWidth, maxHeight, quality);
        Intent intent;
        if (source == SOURCE_CAMERA) {
            intent = new Intent(MediaStore.ACTION_IMAGE_CAPTURE);
            // Without EXTRA_OUTPUT the camera only returns a thumbnail.
            importing.cameraOutput = new File(GpuiPickerActivity.newCacheDirectory(activity), "photo.jpg");
            Uri output = FileProvider.getUriForFile(activity,
                    activity.getPackageName() + ".gpui.fileprovider", importing.cameraOutput);
            intent.putExtra(MediaStore.EXTRA_OUTPUT, output);
            intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_WRITE_URI_PERMISSION);
            applyFacing(intent, cameraFacing);
        } else {
            intent = galleryIntent("image/*", false);
        }
        return first(GpuiPickerActivity.launch(activity, intent, importing));
    }

    /**
     * Pick multiple images from the gallery.
     *
     * @return Image paths, or null if cancelled.
     */
    public static String[] pickMultiImage(final Activity activity,
                                          int maxWidth, int maxHeight, int quality) throws Exception {
        ArrayList<String> result = GpuiPickerActivity.launch(
                activity, galleryIntent("image/*", true), scaling(maxWidth, maxHeight, quality));
        return result != null ? result.toArray(new String[0]) : null;
    }

    /**
     * Pick a video from gallery or camera.
     *
     * @param activity    The current Activity.
     * @param source      0 = gallery, 1 = camera.
     * @param cameraFacing 0 = rear, 1 = front.
     * @return The video path, or null if cancelled.
     */
    public static String pickVideo(final Activity activity, int source, int cameraFacing) throws Exception {
        Intent intent;
        if (source == SOURCE_CAMERA) {
            intent = new Intent(MediaStore.ACTION_VIDEO_CAPTURE);
            applyFacing(intent, cameraFacing);
        } else {
            intent = galleryIntent("video/*", false);
        }
        return first(GpuiPickerActivity.launch(activity, intent, new GpuiPickerActivity.Import()));
    }

    // ── Internal ─────────────────────────────────────────────────────────

    /**
     * The system photo picker on API 33+ (no permission needed, supports several
     * items), a document picker before. ACTION_PICK ignores EXTRA_ALLOW_MULTIPLE.
     */
    private static Intent galleryIntent(String type, boolean multiple) {
        Intent intent;
        if (Build.VERSION.SDK_INT >= 33) {
            intent = new Intent(MediaStore.ACTION_PICK_IMAGES);
            if (multiple) intent.putExtra(MediaStore.EXTRA_PICK_IMAGES_MAX, MediaStore.getPickImagesMaxLimit());
        } else {
            intent = new Intent(Intent.ACTION_GET_CONTENT);
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, multiple);
        }
        intent.setType(type);
        return intent;
    }

    private static GpuiPickerActivity.Import scaling(int maxWidth, int maxHeight, int quality) {
        GpuiPickerActivity.Import importing = new GpuiPickerActivity.Import();
        importing.maxWidth = maxWidth;
        importing.maxHeight = maxHeight;
        importing.quality = quality;
        return importing;
    }

    private static void applyFacing(Intent intent, int cameraFacing) {
        if (cameraFacing == CAMERA_FRONT) {
            intent.putExtra("android.intent.extras.CAMERA_FACING", 1);
            intent.putExtra("android.intent.extras.LENS_FACING_FRONT", 1);
            intent.putExtra("android.intent.extra.USE_FRONT_CAMERA", true);
        }
    }

    private static String first(ArrayList<String> result) {
        return result != null && !result.isEmpty() ? result.get(0) : null;
    }

    private GpuiImagePicker() {}
}

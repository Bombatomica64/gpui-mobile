package dev.gpui.mobile;

import android.app.Activity;
import android.media.MediaPlayer;
import android.media.PlaybackParams;
import android.net.Uri;
import android.os.Build;
import android.util.SparseArray;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.ViewGroup;
import android.widget.FrameLayout;

import java.io.IOException;
import java.util.concurrent.CountDownLatch;

/**
 * Java helper for video playback using {@link MediaPlayer}.
 *
 * <p>All public methods are {@code static} and called from Rust via JNI.
 * Player instances are stored in a {@link SparseArray} keyed by integer ID.</p>
 *
 * <p>Uses {@link SurfaceView} instead of TextureView because NativeActivity
 * does not provide hardware acceleration for views added via addContentView().
 * TextureView requires HW accel and silently renders nothing without it.
 * SurfaceView has its own compositor surface and works without HW accel.</p>
 *
 * <p>Streams are prepared asynchronously, so no call blocks on the network: until
 * the stream is ready, play, pause, seek and speed are remembered and applied then.
 * MediaPlayer delivers its callbacks on the main thread, since the threads that
 * create players have no Looper.</p>
 */
public final class GpuiVideoPlayer {

    private static final String TAG = "GpuiVideoPlayer";

    private static final SparseArray<MediaPlayer> sPlayers = new SparseArray<>();
    private static final SparseArray<SurfaceView> sSurfaces = new SparseArray<>();
    private static int sNextId = 1;
    private static final Object sLock = new Object();

    /**
     * A player whose source is still being prepared, and what to do once it is.
     * Read and written only while holding {@code sLock}, so a call made while the
     * player is preparing is either recorded here or sees the prepared player.
     */
    private static final class Pending {
        boolean playWhenReady;
        long seekMs = -1;
        float speed = -1;
    }

    private static final SparseArray<Pending> sPending = new SparseArray<>();

    /** Players whose source failed to load, with the reason. */
    private static final SparseArray<String> sErrors = new SparseArray<>();

    // State codes for getState (must match Rust's video_player::android).
    private static final int STATE_LOADING = 1;
    private static final int STATE_READY = 2;
    private static final int STATE_PLAYING = 3;
    private static final int STATE_PAUSED = 4;
    private static final int STATE_COMPLETED = 5;

    /**
     * Create a new MediaPlayer instance.
     *
     * @param activity The current Activity.
     * @return A positive player ID, or -1 on failure.
     */
    public static int create(Activity activity) {
        try {
            MediaPlayer mp = new MediaPlayer();
            synchronized (sLock) {
                int id = sNextId++;
                sPlayers.put(id, mp);
                return id;
            }
        } catch (Exception e) {
            android.util.Log.e(TAG, "create failed", e);
            return -1;
        }
    }

    /**
     * Set a URL as the data source.
     *
     * <p>A local file or content URI is prepared right away. A stream is prepared
     * in the background; {@link #getState} reports loading until it is ready, or
     * throws its error.</p>
     *
     * @param activity The current Activity.
     * @param id       Player ID.
     * @param url      URL to play.
     * @return "duration|width|height" if the source is ready, or null while it loads.
     * @throws IOException if the source cannot be set or a local file cannot be prepared.
     */
    public static String setUrl(Activity activity, int id, String url) throws IOException {
        MediaPlayer mp = resetPlayer(id);
        // Use the String overload for http/https URLs to avoid
        // ContentResolver trying to resolve them as content:// URIs.
        Uri uri = Uri.parse(url);
        String scheme = uri.getScheme();
        boolean stream = "http".equals(scheme) || "https".equals(scheme)
                || "rtsp".equals(scheme);
        if (stream) {
            mp.setDataSource(url);
            prepareAsync(id, mp);
            return null;
        }
        mp.setDataSource(activity, uri);
        mp.prepare();
        return videoInfo(mp);
    }

    /**
     * Set a file path as the data source and prepare synchronously.
     *
     * @param activity The current Activity.
     * @param id       Player ID.
     * @param path     File path to play.
     * @return "duration|width|height" on success, or null on failure.
     */
    public static String setFilePath(Activity activity, int id, String path) throws IOException {
        MediaPlayer mp = resetPlayer(id);
        mp.setDataSource(path);
        mp.prepare();
        return videoInfo(mp);
    }

    private static String videoInfo(MediaPlayer mp) {
        return mp.getDuration() + "|" + mp.getVideoWidth() + "|" + mp.getVideoHeight();
    }

    /**
     * The player, reset, with any load in progress or failed load forgotten.
     * Under the lock, so a prepared callback of the previous source either ran
     * before or sees its load was abandoned, and never touches the new one.
     */
    private static MediaPlayer resetPlayer(int id) {
        synchronized (sLock) {
            MediaPlayer mp = sPlayers.get(id);
            if (mp == null) throw new IllegalArgumentException("No video player " + id);
            sPending.remove(id);
            sErrors.remove(id);
            mp.reset();
            return mp;
        }
    }

    private static void prepareAsync(int id, MediaPlayer mp) {
        Pending pending = new Pending();
        synchronized (sLock) {
            sPending.put(id, pending);
        }
        mp.setOnPreparedListener(player -> {
            synchronized (sLock) {
                // Gone if the player was disposed or given a new source meanwhile.
                if (sPending.get(id) != pending) return;
                sPending.remove(id);
                try {
                    if (pending.speed > 0) applySpeed(player, pending.speed);
                    // Seeking also draws the first frame on a surface attached while loading.
                    if (pending.seekMs >= 0 || !pending.playWhenReady) {
                        seekTo(player, Math.max(pending.seekMs, 0));
                    }
                    if (pending.playWhenReady) player.start();
                } catch (IllegalStateException e) {
                    android.util.Log.e(TAG, "applying calls made while loading failed", e);
                }
            }
        });
        mp.setOnErrorListener((player, what, extra) -> {
            synchronized (sLock) {
                if (sPending.get(id) == pending) sPending.remove(id);
                sErrors.put(id, "MediaPlayer error " + what + " (" + extra + ")");
            }
            return true;
        });
        mp.prepareAsync();
    }

    /** Whether the player is still preparing. */
    private static boolean preparing(int id) {
        synchronized (sLock) {
            return sPending.get(id) != null;
        }
    }

    /**
     * If the player is still preparing, record {@code change} for when it is ready
     * and return true; otherwise return false and leave the call to the player.
     */
    private static boolean whenPrepared(int id, java.util.function.Consumer<Pending> change) {
        synchronized (sLock) {
            Pending pending = sPending.get(id);
            if (pending == null) return false;
            change.accept(pending);
            return true;
        }
    }

    private static void seekTo(MediaPlayer mp, long ms) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            mp.seekTo(ms, MediaPlayer.SEEK_CLOSEST);
        } else {
            mp.seekTo((int) ms);
        }
    }

    /**
     * Start or resume playback.
     */
    public static void play(int id) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null) {
            if (whenPrepared(id, pending -> pending.playWhenReady = true)) return;
            try {
                mp.start();
            } catch (Exception e) {
                android.util.Log.e(TAG, "play failed", e);
            }
        }
    }

    /**
     * Pause playback.
     */
    public static void pause(int id) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null) {
            if (whenPrepared(id, pending -> pending.playWhenReady = false)) return;
            try {
                mp.pause();
            } catch (Exception e) {
                android.util.Log.e(TAG, "pause failed", e);
            }
        }
    }

    /**
     * Seek to a position in milliseconds.
     */
    public static void seek(int id, long ms) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null) {
            if (whenPrepared(id, pending -> pending.seekMs = ms)) return;
            try {
                seekTo(mp, ms);
            } catch (Exception e) {
                android.util.Log.e(TAG, "seek failed", e);
            }
        }
    }

    /**
     * Set volume (0.0 to 1.0).
     */
    public static void setVolume(int id, float volume) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null) {
            try {
                mp.setVolume(volume, volume);
            } catch (Exception e) {
                android.util.Log.e(TAG, "setVolume failed", e);
            }
        }
    }

    /**
     * Set playback speed.
     */
    public static void setSpeed(int id, float speed) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null) {
            if (whenPrepared(id, pending -> pending.speed = speed)) return;
            applySpeed(mp, speed);
        }
    }

    private static void applySpeed(MediaPlayer mp, float speed) {
        try {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                PlaybackParams params = mp.getPlaybackParams();
                params.setSpeed(speed);
                mp.setPlaybackParams(params);
            }
        } catch (Exception e) {
            android.util.Log.e(TAG, "setSpeed failed", e);
        }
    }

    /**
     * Enable or disable looping.
     */
    public static void setLooping(int id, boolean looping) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null) {
            try {
                mp.setLooping(looping);
            } catch (Exception e) {
                android.util.Log.e(TAG, "setLooping failed", e);
            }
        }
    }

    /**
     * Get current playback position in milliseconds.
     */
    public static long getPosition(int id) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null && !preparing(id)) {
            try {
                return mp.getCurrentPosition();
            } catch (Exception e) {
                android.util.Log.e(TAG, "getPosition failed", e);
            }
        }
        return 0;
    }

    /**
     * Get total duration in milliseconds.
     */
    public static long getDuration(int id) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null && !preparing(id)) {
            try {
                return mp.getDuration();
            } catch (Exception e) {
                android.util.Log.e(TAG, "getDuration failed", e);
            }
        }
        return 0;
    }

    /**
     * Get video width in pixels.
     */
    public static int getWidth(int id) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null && !preparing(id)) {
            try {
                return mp.getVideoWidth();
            } catch (Exception e) {
                android.util.Log.e(TAG, "getWidth failed", e);
            }
        }
        return 0;
    }

    /**
     * Get video height in pixels.
     */
    public static int getHeight(int id) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null && !preparing(id)) {
            try {
                return mp.getVideoHeight();
            } catch (Exception e) {
                android.util.Log.e(TAG, "getHeight failed", e);
            }
        }
        return 0;
    }

    /**
     * Check if the player is currently playing.
     */
    public static boolean isPlaying(int id) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp != null && !preparing(id)) {
            try {
                return mp.isPlaying();
            } catch (Exception e) {
                android.util.Log.e(TAG, "isPlaying failed", e);
            }
        }
        return false;
    }

    /**
     * The player's state: 1 loading, 2 ready, 3 playing, 4 paused, 5 completed.
     *
     * @throws IllegalStateException with the reason, if the source failed to load.
     */
    public static int getState(int id) {
        MediaPlayer mp;
        String error;
        synchronized (sLock) {
            mp = sPlayers.get(id);
            error = sErrors.get(id);
        }
        if (mp == null) throw new IllegalArgumentException("No video player " + id);
        if (error != null) throw new IllegalStateException(error);
        if (preparing(id)) return STATE_LOADING;
        try {
            if (mp.isPlaying()) return STATE_PLAYING;
            int position = mp.getCurrentPosition();
            int duration = mp.getDuration();
            if (duration > 0 && position >= duration) return STATE_COMPLETED;
            return position > 0 ? STATE_PAUSED : STATE_READY;
        } catch (IllegalStateException e) {
            return STATE_READY;
        }
    }

    /**
     * Show a native SurfaceView at the given position and size (in px).
     * The MediaPlayer renders video frames to this surface.
     *
     * @param activity The current Activity.
     * @param id       Player ID.
     * @param x        Left position in pixels.
     * @param y        Top position in pixels.
     * @param width    Width in pixels.
     * @param height   Height in pixels.
     */
    public static void showSurface(Activity activity, int id, int x, int y, int width, int height) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp == null) return;

        final MediaPlayer fmp = mp;
        final CountDownLatch latch = new CountDownLatch(1);

        activity.runOnUiThread(() -> {
            try {
                // Remove existing surface if any
                hideSurfaceInternal(id);

                SurfaceView sv = new SurfaceView(activity);
                // Render above NativeActivity's own SurfaceView (wgpu/Vulkan).
                sv.setZOrderOnTop(true);

                FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(width, height);
                params.leftMargin = x;
                params.topMargin = y;
                activity.addContentView(sv, params);

                synchronized (sLock) {
                    sSurfaces.put(id, sv);
                }

                sv.getHolder().addCallback(new SurfaceHolder.Callback() {
                    @Override
                    public void surfaceCreated(SurfaceHolder holder) {
                        attachSurfaceToPlayer(id, fmp, holder.getSurface());
                        latch.countDown();
                    }

                    @Override
                    public void surfaceChanged(SurfaceHolder holder, int format, int w, int h) {}

                    @Override
                    public void surfaceDestroyed(SurfaceHolder holder) {
                        try {
                            fmp.setSurface(null);
                        } catch (Exception ignored) {}
                    }
                });

                // If surface is already valid
                if (sv.getHolder().getSurface().isValid()) {
                    attachSurfaceToPlayer(id, fmp, sv.getHolder().getSurface());
                    latch.countDown();
                }
            } catch (Exception e) {
                android.util.Log.e(TAG, "showSurface failed", e);
                latch.countDown();
            }
        });

        try {
            latch.await(java.util.concurrent.TimeUnit.SECONDS.toMillis(3),
                        java.util.concurrent.TimeUnit.MILLISECONDS);
        } catch (InterruptedException ignored) {}
    }

    /**
     * Hide (remove) the native video surface overlay.
     *
     * @param activity The current Activity.
     * @param id       Player ID.
     */
    public static void hideSurface(Activity activity, int id) {
        final CountDownLatch latch = new CountDownLatch(1);
        activity.runOnUiThread(() -> {
            hideSurfaceInternal(id);
            latch.countDown();
        });
        try {
            latch.await(java.util.concurrent.TimeUnit.SECONDS.toMillis(2),
                        java.util.concurrent.TimeUnit.MILLISECONDS);
        } catch (InterruptedException ignored) {}
    }

    private static void hideSurfaceInternal(int id) {
        SurfaceView sv;
        synchronized (sLock) {
            sv = sSurfaces.get(id);
            sSurfaces.remove(id);
        }
        if (sv != null) {
            ViewGroup parent = (ViewGroup) sv.getParent();
            if (parent != null) {
                parent.removeView(sv);
            }
        }
    }

    /**
     * Release and remove the player.
     */
    public static void dispose(int id) {
        // Remove surface first
        hideSurfaceInternal(id);

        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
            sPlayers.remove(id);
            sPending.remove(id);
            sErrors.remove(id);
        }
        if (mp != null) {
            try {
                mp.setSurface(null);
            } catch (Exception ignored) {
            }
            try {
                mp.stop();
            } catch (Exception ignored) {
            }
            try {
                mp.release();
            } catch (Exception ignored) {
            }
        }
    }

    /**
     * Create a SurfaceView configured for video playback.
     *
     * <p>Uses SurfaceView with setZOrderOnTop(true) so the video surface
     * renders above NativeActivity's own rendering surface (wgpu/Vulkan).
     * TextureView cannot be used here because NativeActivity does not
     * provide hardware acceleration for overlaid views.</p>
     *
     * <p>The view is NOT added to the hierarchy — the caller
     * (GpuiPlatformView) handles that.</p>
     *
     * @param activity The current Activity.
     * @param id       Player ID.
     * @return A configured SurfaceView, or an empty FrameLayout if the player is not found.
     */
    public static android.view.View createVideoSurface(Activity activity, int id) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp == null) {
            android.util.Log.w(TAG, "createVideoSurface: player " + id + " not found");
            return new FrameLayout(activity);
        }

        final MediaPlayer fmp = mp;
        SurfaceView sv = new SurfaceView(activity);
        // Render above NativeActivity's own SurfaceView.
        sv.setZOrderOnTop(true);

        sv.getHolder().addCallback(new SurfaceHolder.Callback() {
            @Override
            public void surfaceCreated(SurfaceHolder holder) {
                android.util.Log.i(TAG, "createVideoSurface: surfaceCreated for player " + id);
                attachSurfaceToPlayer(id, fmp, holder.getSurface());
            }

            @Override
            public void surfaceChanged(SurfaceHolder holder, int format, int w, int h) {
                android.util.Log.i(TAG, "createVideoSurface: surfaceChanged " + w + "x" + h);
            }

            @Override
            public void surfaceDestroyed(SurfaceHolder holder) {
                android.util.Log.i(TAG, "createVideoSurface: surfaceDestroyed for player " + id);
                try {
                    fmp.setSurface(null);
                } catch (Exception ignored) {}
            }
        });

        // If surface is already valid (shouldn't happen before layout, but be safe)
        if (sv.getHolder().getSurface().isValid()) {
            attachSurfaceToPlayer(id, fmp, sv.getHolder().getSurface());
        }

        // Track this surface
        synchronized (sLock) {
            sSurfaces.put(id, sv);
        }

        android.util.Log.i(TAG, "createVideoSurface: SurfaceView created for player " + id);
        return sv;
    }

    /**
     * Attach a surface to the MediaPlayer, then re-seek to force a video frame
     * to render. Without the re-seek, frames decoded before the surface was
     * attached are discarded and the view stays black.
     *
     * <p>While the player is preparing it only takes the surface: querying or
     * seeking it then would move it to the Error state. The prepared callback
     * seeks, which draws the first frame.</p>
     */
    private static void attachSurfaceToPlayer(int id, MediaPlayer mp, Surface surface) {
        try {
            mp.setSurface(surface);
            if (preparing(id)) return;
            android.util.Log.i(TAG, "attachSurfaceToPlayer: surface attached, isPlaying=" + mp.isPlaying()
                    + " pos=" + mp.getCurrentPosition() + " dur=" + mp.getDuration()
                    + " w=" + mp.getVideoWidth() + " h=" + mp.getVideoHeight());
            // Re-seek to current position to force the decoder to output a
            // frame to the newly attached surface.
            seekTo(mp, mp.getCurrentPosition());
        } catch (Exception e) {
            android.util.Log.e(TAG, "attachSurfaceToPlayer failed", e);
        }
    }

    // Prevent instantiation.
    private GpuiVideoPlayer() {}
}

package dev.gpui.mobile;

import android.app.Activity;
import android.media.MediaPlayer;
import android.media.PlaybackParams;
import android.net.Uri;
import android.os.Build;
import android.util.SparseArray;
import android.util.SparseBooleanArray;
import android.util.SparseIntArray;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.ViewGroup;
import android.widget.FrameLayout;

import java.io.IOException;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

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
 * <p>Streams are loaded on a worker thread and prepared asynchronously, so no call
 * blocks on the network or on the decoder: until the stream is ready, play, pause,
 * seek and speed are remembered and applied then.
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
     * Changes a player's source and releases players, one job at a time and in
     * order, off the caller's thread: a later setUrl or dispose runs after an
     * earlier one has finished with the player.
     */
    private static final ExecutorService sWorker =
            Executors.newSingleThreadExecutor(r -> new Thread(r, "GpuiVideoPlayer"));

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

    /**
     * Players that played their source to the end, until play, seek or a new
     * source. MediaPlayer's own state can't tell this from a pause. Guarded by
     * {@code sLock}.
     */
    private static final SparseBooleanArray sCompleted = new SparseBooleanArray();

    /**
     * Which source each player is on, so a completion or error queued for the
     * previous source doesn't land on the new one. Guarded by {@code sLock}.
     */
    private static final SparseIntArray sSource = new SparseIntArray();

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
     * <p>A stream (http, https, rtsp) is loaded in the background: this returns at
     * once, {@link #getState} reports loading until it is ready, or throws its
     * error. A local file or content URI is prepared before returning.</p>
     *
     * @param activity The current Activity.
     * @param id       Player ID.
     * @param url      URL to play.
     * @return "duration|width|height" if the source is ready, or null while it loads.
     * @throws Exception if a local source cannot be set or prepared.
     */
    public static String setUrl(Activity activity, int id, String url) throws Exception {
        Uri uri = Uri.parse(url);
        String scheme = uri.getScheme();
        boolean stream = "http".equals(scheme) || "https".equals(scheme)
                || "rtsp".equals(scheme);
        if (!stream) {
            return loadNow(id, mp -> mp.setDataSource(activity, uri));
        }

        Pending pending = new Pending();
        MediaPlayer mp;
        int source;
        synchronized (sLock) {
            mp = player(id);
            sErrors.remove(id);
            sPending.put(id, pending);
            source = nextSource(id);
        }
        // Even reset() and setDataSource() take tens of ms (tearing down a
        // prepared decoder, opening the connection), so they run on the worker too.
        sWorker.execute(() -> {
            synchronized (sLock) {
                // Superseded by a newer source, or disposed: nothing to do.
                if (sPending.get(id) != pending) return;
            }
            try {
                mp.reset();
                listen(id, mp, source);
                mp.setOnPreparedListener(player -> onPrepared(id, pending, player));
                mp.setOnErrorListener((player, what, extra) -> {
                    fail(id, pending, "MediaPlayer error " + what + " (" + extra + ")");
                    return true;
                });
                // The String overload, so ContentResolver does not try the URL.
                mp.setDataSource(url);
                mp.prepareAsync();
            } catch (Exception e) {
                android.util.Log.e(TAG, "setUrl failed: " + url, e);
                fail(id, pending, e.toString());
            }
        });
        return null;
    }

    /**
     * Set a file path as the data source and prepare synchronously.
     *
     * @param activity The current Activity.
     * @param id       Player ID.
     * @param path     File path to play.
     * @return "duration|width|height".
     * @throws Exception if the source cannot be set or prepared.
     */
    public static String setFilePath(Activity activity, int id, String path) throws Exception {
        return loadNow(id, mp -> mp.setDataSource(path));
    }

    private interface Source {
        void set(MediaPlayer mp) throws IOException;
    }

    /** Load and prepare a local source on the worker, and wait for it. */
    private static String loadNow(int id, Source source) throws Exception {
        MediaPlayer mp;
        int current;
        synchronized (sLock) {
            mp = player(id);
            sPending.remove(id);
            sErrors.remove(id);
            current = nextSource(id);
        }
        try {
            return sWorker.submit(() -> {
                mp.reset();
                listen(id, mp, current);
                // Without it, a playback error ends in onCompletion.
                mp.setOnErrorListener((player, what, extra) -> {
                    synchronized (sLock) {
                        if (sSource.get(id) == current) {
                            sErrors.put(id, "MediaPlayer error " + what + " (" + extra + ")");
                        }
                    }
                    return true;
                });
                source.set(mp);
                mp.prepare();
                return mp.getDuration() + "|" + mp.getVideoWidth() + "|" + mp.getVideoHeight();
            }).get();
        } catch (ExecutionException e) {
            Throwable cause = e.getCause();
            throw cause instanceof Exception ? (Exception) cause : e;
        }
    }

    /**
     * Start a new source for this player: forget that the old one completed, and
     * return the new source's number. Call with {@code sLock} held.
     */
    private static int nextSource(int id) {
        sCompleted.delete(id);
        int source = sSource.get(id) + 1;
        sSource.put(id, source);
        return source;
    }

    /** Record when {@code source} plays to the end. */
    private static void listen(int id, MediaPlayer mp, int source) {
        mp.setOnCompletionListener(player -> {
            synchronized (sLock) {
                if (sSource.get(id) == source) sCompleted.put(id, true);
            }
        });
    }

    private static void clearCompleted(int id) {
        synchronized (sLock) {
            sCompleted.delete(id);
        }
    }

    /** The player with this ID; call with {@code sLock} held. */
    private static MediaPlayer player(int id) {
        MediaPlayer mp = sPlayers.get(id);
        if (mp == null) throw new IllegalArgumentException("No video player " + id);
        return mp;
    }

    /** Runs on the main thread when {@code pending}'s source is prepared. */
    private static void onPrepared(int id, Pending pending, MediaPlayer player) {
        synchronized (sLock) {
            // Gone if the player was disposed or given a new source meanwhile. A
            // newer setUrl replaces it under this lock, before its worker job resets
            // the player, so these calls never reach a reset player.
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
    }

    /** Record a load or playback error, unless a newer source replaced {@code pending}. */
    private static void fail(int id, Pending pending, String reason) {
        synchronized (sLock) {
            Pending current = sPending.get(id);
            if (current != null && current != pending) return;
            sPending.remove(id);
            sErrors.put(id, reason);
        }
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
            clearCompleted(id);
            try {
                // From the end, this starts over.
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
            clearCompleted(id);
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
        boolean loading;
        boolean completed;
        synchronized (sLock) {
            mp = sPlayers.get(id);
            error = sErrors.get(id);
            loading = sPending.get(id) != null;
            completed = sCompleted.get(id);
        }
        if (mp == null) throw new IllegalArgumentException("No video player " + id);
        if (error != null) throw new IllegalStateException(error);
        if (loading) return STATE_LOADING;
        if (completed) return STATE_COMPLETED;
        try {
            if (mp.isPlaying()) return STATE_PLAYING;
            return mp.getCurrentPosition() > 0 ? STATE_PAUSED : STATE_READY;
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
            sCompleted.delete(id);
            sSource.delete(id);
        }
        if (mp != null) {
            // On the worker: after any source change still in flight, and off the
            // caller's thread (release waits for the decoder).
            sWorker.execute(() -> {
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
            });
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
     * @param context  The view context (the platform view host's, which follows Activity recreation).
     * @param id       Player ID.
     * @return A configured SurfaceView, or an empty FrameLayout if the player is not found.
     */
    public static android.view.View createVideoSurface(android.content.Context context, int id) {
        MediaPlayer mp;
        synchronized (sLock) {
            mp = sPlayers.get(id);
        }
        if (mp == null) {
            android.util.Log.w(TAG, "createVideoSurface: player " + id + " not found");
            return new FrameLayout(context);
        }

        final MediaPlayer fmp = mp;
        SurfaceView sv = new SurfaceView(context);
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

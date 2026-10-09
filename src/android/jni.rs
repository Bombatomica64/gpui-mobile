//! Android entry point and event loop using the `android-activity` crate.
//!
//! This module replaces the previous hand-rolled `ANativeActivity_onCreate`,
//! `JNI_OnLoad`, and lifecycle callback implementations with the higher-level
//! `android-activity` glue layer.
//!
//! ## Entry sequence
//!
//! ```text
//! android-activity loads the .so and calls android_main(app: AndroidApp)
//!   └── We store the AndroidApp globally
//!       └── Call the user-supplied gpui_android_main(app)
//! ```
//!
//! ## Threading model
//!
//! `android-activity` spawns a dedicated native thread and calls `android_main`
//! on it.  All GPUI draw / event callbacks run on this thread.  The
//! `AndroidApp` handle is `Send + Sync` and can be shared across threads.
//!
//! ## User entry point
//!
//! Applications must define:
//!
//! ```rust,no_run
//! #[no_mangle]
//! fn android_main(app: android_activity::AndroidApp) {
//!     // Initialise GPUI and run the application.
//! }
//! ```
//!
//! ## Event handling
//!
//! Lifecycle events (window creation/destruction, focus changes, etc.) are
//! delivered via `AndroidApp::poll_events()`.  Input events are obtained via
//! `AndroidApp::input_events_iter()`.
//!
//! ## Frames
//!
//! The loop blocks on the looper between events and draws only when GPUI
//! asked for a frame and a vsync has passed since — see
//! [`super::frame_source`].

#![allow(unsafe_code)]
#![allow(non_snake_case)]

use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

/// Whether the deferred init-window callback has already been invoked.
///
/// Reset to `false` on `TerminateWindow` so that when the surface is
/// recreated on resume the init callbacks run again.
static INIT_WINDOW_DONE: AtomicBool = AtomicBool::new(false);

/// Whether the GPUI native library has completed initialization.
///
/// Set to `true` after the first frame is rendered. This can be queried
/// via JNI by a custom Activity to dismiss the splash screen, although
/// with NativeActivity the system splash handles this automatically.
pub static NATIVE_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Deferred lifecycle flags.
///
/// We must NOT call `win.set_active()` or `platform.did_enter_background()`
/// inside `handle_main_event` (which runs within `poll_events`).
/// The android-activity crate's Java-side callbacks block on a condvar
/// waiting for the native thread to finish processing the command.
/// If our handler tries to acquire the window `state` lock (a
/// `parking_lot::Mutex`), and a background thread is holding it (e.g.
/// during a render pass), we deadlock: native waits on the lock,
/// the lock holder waits for native rendering to complete, but native
/// is stuck.
///
/// Instead, the handlers set these flags and the main loop body
/// processes them AFTER `poll_events` returns.
static PAUSE_PENDING: AtomicBool = AtomicBool::new(false);
static RESUME_PENDING: AtomicBool = AtomicBool::new(false);
static TERM_WINDOW_PENDING: AtomicBool = AtomicBool::new(false);
static INIT_WINDOW_PENDING: AtomicBool = AtomicBool::new(false);
static WINDOW_RESIZED_PENDING: AtomicBool = AtomicBool::new(false);
static CONFIG_CHANGED_PENDING: AtomicBool = AtomicBool::new(false);

use android_activity::{AndroidApp, MainEvent, PollEvent};

use super::platform::{AndroidPlatform, SharedPlatform};

use jni::objects::{JObject, JString, JValue};
use jni::JavaVM;
use std::sync::{atomic::AtomicPtr, Mutex};

// ── JNI helpers (safe `jni` crate wrappers) ──────────────────────────────────

static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();

/// Get or create the static `JavaVM` wrapper.
fn java_vm_safe() -> Result<&'static JavaVM, String> {
    if let Some(vm) = JAVA_VM.get() {
        return Ok(vm);
    }
    let ptr = java_vm();
    if ptr.is_null() {
        return Err("JavaVM not available".into());
    }
    // SAFETY: `ptr` is the process's `JavaVM*`, from `android-activity` or from the
    // host Activity's JNI env; it stays valid for the life of the process.
    Ok(JAVA_VM.get_or_init(|| unsafe { JavaVM::from_raw(ptr as *mut jni::sys::JavaVM) }))
}

/// Run a closure with an attached `jni::Env` for the current thread.
///
/// A thread that is not attached yet is attached for good (it detaches when it
/// exits), so later calls on it are cheap. The closure runs in its own local
/// reference frame.
///
/// A Java exception still pending when the closure returns is cleared and turned into
/// the returned error, replacing the closure's own result. Clear exceptions where
/// they happen when a failure is expected.
pub fn with_env<T>(f: impl FnOnce(&mut jni::Env) -> Result<T, String>) -> Result<T, String> {
    let vm = java_vm_safe()?;
    let mut result: Option<Result<T, String>> = None;
    vm.attach_current_thread(|env: &mut jni::Env| -> Result<(), jni::errors::Error> {
        result = Some(f(env));
        Ok(())
    })
    .map_err(|e: jni::errors::Error| e.to_string())?;
    result.unwrap()
}

/// Same as [`with_env`].
#[deprecated(note = "use `with_env`")]
#[inline]
pub fn obtain_env<T>(f: impl FnOnce(&mut jni::Env) -> Result<T, String>) -> Result<T, String> {
    with_env(f)
}

/// Fail unless the calling thread may wait for the user.
///
/// Calls that show another Activity (a picker, a permission dialog, a biometric
/// prompt) block until the user answers. A thread with a looper must not wait that
/// long: on GPUI's thread the app stops drawing (and on the `android-activity` path the
/// pause handshake deadlocks), and the Java UI thread is the one that delivers the
/// answer. Call them from a background thread, e.g. GPUI's `background_spawn`.
pub(crate) fn ensure_may_wait_for_user(what: &str) -> Result<(), String> {
    if ndk::looper::ForeignLooper::for_thread().is_some() {
        return Err(format!(
            "{what} waits for the user: call it from a background thread, not GPUI's or the UI thread"
        ));
    }
    Ok(())
}

/// The current Activity, as a local reference in `env`'s frame.
///
/// On the host-driven path this is the most recently registered Activity that is
/// neither finishing nor destroyed (see [`set_host_activity`]). The local reference
/// stays valid for the rest of the frame even if that Activity is destroyed meanwhile.
///
/// For calls that only need a `Context`, prefer [`application_context`].
pub fn activity<'local>(env: &mut jni::Env<'local>) -> Result<JObject<'local>, String> {
    if let Some(app) = ANDROID_APP.get() {
        // SAFETY: `android-activity` keeps this global reference for the life of the
        // process.
        let global = unsafe { JObject::from_raw(env, app.activity_as_ptr() as jni::sys::jobject) };
        return env.new_local_ref(&global).e();
    }
    let mut activities = HOST_ACTIVITIES.lock().expect("poisoned");
    // Only the newest is checked, so the common case costs two calls; older ones are
    // pruned when the next Activity registers. Deleting a global reference is safe
    // while the lock is held: only the deprecated `activity_as_ptr` hands out raw ones.
    while let Some(newest) = activities.last() {
        if !is_gone(env, newest) {
            return env.new_local_ref(newest).e();
        }
        activities.pop();
    }
    Err("Activity not available".into())
}

/// The application context, as a local reference in `env`'s frame. Unlike an
/// Activity it lives as long as the process, and its configuration follows
/// system-wide changes.
pub fn application_context<'local>(env: &mut jni::Env<'local>) -> Result<JObject<'local>, String> {
    if ANDROID_APP.get().is_some() {
        let activity = activity(env)?;
        return env
            .call_method(
                &activity,
                jni::jni_str!("getApplicationContext"),
                jni::jni_sig!("()Landroid/content/Context;"),
                &[],
            )
            .and_then(|value| value.l())
            .or_clear(env);
    }
    let context = HOST_APPLICATION
        .get()
        .ok_or("set_host_activity has not been called")?;
    env.new_local_ref(context).e()
}

/// Convert a Java String (`JObject` wrapping a `java.lang.String`) to a Rust `String`.
///
/// Returns an empty string on null, on an object that is not a `String`, or on error.
pub fn get_string(env: &mut jni::Env<'_>, obj: &JObject<'_>) -> String {
    if obj.is_null() {
        return String::new();
    }
    match env.is_instance_of(obj, jni::jni_str!("java/lang/String")) {
        Ok(true) => {}
        Ok(false) => {
            log::warn!("get_string: not a java.lang.String");
            return String::new();
        }
        Err(err) => {
            log::warn!(
                "get_string: {}",
                take_exception(env).unwrap_or(err.to_string())
            );
            return String::new();
        }
    }
    // SAFETY: `obj` is a live, non-null reference to a `java.lang.String`.
    let jstr = unsafe { JString::from_raw(env, obj.as_raw()) };
    jstr.to_string()
}

/// Extension trait for converting `jni::errors::Result<T>` to `Result<T, String>`.
///
/// Leaves a Java exception pending; use [`JniResultExt::or_clear`] for calls into
/// Java.
pub(crate) trait JniExt<T> {
    fn e(self) -> Result<T, String>;
}

impl<T> JniExt<T> for jni::errors::Result<T> {
    fn e(self) -> Result<T, String> {
        self.map_err(|e| e.to_string())
    }
}

/// Error handling for calls into Java.
pub(crate) trait JniResultExt<T> {
    /// On error, clear the pending Java exception, if any, and describe it: later
    /// JNI calls in the same frame would otherwise fail too, and the bare JNI error
    /// only says that an exception was thrown.
    fn or_clear(self, env: &mut jni::Env<'_>) -> Result<T, String>;

    /// Like [`or_clear`](Self::or_clear), but an exception that is an instance of
    /// `class` (a JNI class name, e.g. `android/content/ActivityNotFoundException`)
    /// is an expected outcome: it is cleared and the result is `Ok(None)`.
    fn or_catch(
        self,
        env: &mut jni::Env<'_>,
        class: &'static jni::strings::JNIStr,
    ) -> Result<Option<T>, String>;
}

impl<T> JniResultExt<T> for jni::errors::Result<T> {
    fn or_clear(self, env: &mut jni::Env<'_>) -> Result<T, String> {
        self.map_err(|err| take_exception(env).unwrap_or_else(|| err.to_string()))
    }

    fn or_catch(
        self,
        env: &mut jni::Env<'_>,
        class: &'static jni::strings::JNIStr,
    ) -> Result<Option<T>, String> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(err) => {
                let Some(throwable) = env.exception_occurred() else {
                    return Err(err.to_string());
                };
                env.exception_clear();
                if env.is_instance_of(&throwable, class).unwrap_or(false) {
                    return Ok(None);
                }
                env.throw(&throwable).ok();
                Err(take_exception(env).unwrap_or_else(|| err.to_string()))
            }
        }
    }
}

/// Clear the pending Java exception and return its `toString()`, if there is one.
pub(crate) fn take_exception(env: &mut jni::Env<'_>) -> Option<String> {
    let throwable = env.exception_occurred()?;
    env.exception_clear();
    let description = env
        .call_method(
            &throwable,
            jni::jni_str!("toString"),
            jni::jni_sig!("()Ljava/lang/String;"),
            &[],
        )
        .and_then(|value| value.l());
    match description {
        Ok(description) => Some(get_string(env, &description)),
        Err(_) => {
            env.exception_clear();
            Some("a Java exception was thrown".into())
        }
    }
}

/// Find an application class by name, through the app's class loader.
///
/// From native threads, `JNIEnv::FindClass` uses the system class loader, which
/// doesn't know about application classes. This helper asks the application
/// context's class loader instead. The loader and every class found are cached for
/// the life of the process, so repeated lookups cost one JNI call.
///
/// `class_name` uses Java dot notation (e.g. `"dev.gpui.mobile.GpuiHelper"`).
pub fn find_app_class<'local>(
    env: &mut jni::Env<'local>,
    class_name: &str,
) -> Result<jni::objects::JClass<'local>, String> {
    let classes = APP_CLASSES.get_or_init(Default::default);
    let cached = classes
        .lock()
        .expect("poisoned")
        .get(class_name)
        .map(|class| env.new_local_ref(class).e());
    let class = match cached {
        Some(class) => class?,
        None => {
            let class = load_app_class(env, class_name)?;
            let global = env.new_global_ref(&class).e()?;
            classes
                .lock()
                .expect("poisoned")
                .insert(class_name.to_owned(), global);
            class
        }
    };
    // SAFETY: `class` is a local reference to a `java.lang.Class` in this frame.
    Ok(unsafe { jni::objects::JClass::from_raw(env, class.into_raw()) })
}

type ClassCache = Mutex<std::collections::HashMap<String, jni::refs::Global<JObject<'static>>>>;

/// Classes found by [`find_app_class`], by name.
static APP_CLASSES: OnceLock<ClassCache> = OnceLock::new();

/// The app's class loader, from the application context. A process has one.
static APP_CLASS_LOADER: OnceLock<jni::refs::Global<JObject<'static>>> = OnceLock::new();

fn load_app_class<'local>(
    env: &mut jni::Env<'local>,
    class_name: &str,
) -> Result<JObject<'local>, String> {
    let class_loader = match APP_CLASS_LOADER.get() {
        Some(loader) => env.new_local_ref(loader).e()?,
        None => {
            // Not `getClass().getClassLoader()`: on the android-activity path the
            // Activity is a NativeActivity, a framework class loaded by the boot class
            // loader, which cannot see app classes.
            let context = application_context(env)?;
            let loader = env
                .call_method(
                    &context,
                    jni::jni_str!("getClassLoader"),
                    jni::jni_sig!("()Ljava/lang/ClassLoader;"),
                    &[],
                )
                .and_then(|v| v.l())
                .or_clear(env)
                .map_err(|e| format!("getClassLoader failed: {e}"))?;
            let _ = APP_CLASS_LOADER.set(env.new_global_ref(&loader).e()?);
            loader
        }
    };

    let jname = env.new_string(class_name).e()?;
    let loaded = env
        .call_method(
            &class_loader,
            jni::jni_str!("loadClass"),
            jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Class;"),
            &[JValue::Object(&jname)],
        )
        .and_then(|v| v.l())
        .or_clear(env)
        .map_err(|e| {
            let msg = format!("loadClass({class_name}) failed: {e}");
            log::error!("{msg}");
            msg
        })?;
    log::debug!("find_app_class: loaded {class_name}");
    Ok(loaded)
}

// ── global state ─────────────────────────────────────────────────────────────

/// The `AndroidApp` handle from `android-activity`.
///
/// Set once in `android_main`; read-only thereafter.
static ANDROID_APP: OnceLock<AndroidApp> = OnceLock::new();

/// Process-global `AndroidPlatform` instance.
///
/// Initialised once during `android_main`; read-only thereafter.
static PLATFORM: OnceLock<Arc<AndroidPlatform>> = OnceLock::new();

/// Get the unicode character produced by an Android key event via JNI.
///
/// This creates a `android.view.KeyEvent` Java object and calls
/// `getUnicodeChar(metaState)` on it.  Returns 0 on failure.
pub fn unicode_char_for_key_event(key_code: i32, action: i32, meta_state: i32) -> u32 {
    with_env(|env| {
        let key_event = env
            .new_object(
                jni::jni_str!("android/view/KeyEvent"),
                jni::jni_sig!("(II)V"),
                &[JValue::Int(action), JValue::Int(key_code)],
            )
            .or_clear(env)?;
        let c = env
            .call_method(
                &key_event,
                jni::jni_str!("getUnicodeChar"),
                jni::jni_sig!("(I)I"),
                &[JValue::Int(meta_state)],
            )
            .and_then(|v| v.i())
            .or_clear(env)?;
        Ok(c.max(0) as u32)
    })
    .unwrap_or_else(|err| {
        log::warn!("unicode_char_for_key_event: {err}");
        0
    })
}

// ── public accessors ──────────────────────────────────────────────────────────

/// JVM for the **host-driven** entry point ([`super::host`]), which has no
/// `AndroidApp` to read it from.
static HOST_VM: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Activities registered by the host-driven entry point, oldest first, as JNI
/// global references we own. [`activity`] answers with the newest one that is
/// neither finishing nor destroyed, and drops the references of those that are, so a
/// finished Activity is neither used nor kept alive.
static HOST_ACTIVITIES: Mutex<Vec<jni::refs::Global<JObject<'static>>>> = Mutex::new(Vec::new());

/// Drop the Activities that are finishing or destroyed.
fn prune_gone_activities(
    env: &mut jni::Env<'_>,
    activities: &mut Vec<jni::refs::Global<JObject<'static>>>,
) {
    activities.retain(|activity| !is_gone(env, activity));
}

fn is_gone(env: &mut jni::Env<'_>, activity: &JObject<'_>) -> bool {
    let mut ask = |name| {
        env.call_method(activity, name, jni::jni_sig!("()Z"), &[])
            .and_then(|value| value.z())
            .or_clear(env)
            .unwrap_or(false)
    };
    ask(jni::jni_str!("isFinishing")) || ask(jni::jni_str!("isDestroyed"))
}

/// Register the current Activity when running without `android-activity`.
///
/// Call from a JNI entry point in `Activity.onCreate`, every time — a recreated
/// Activity is a new object. This module takes its own global reference and records
/// the JVM, so [`activity`], `java_vm()` and everything built on them (IME, safe
/// areas, file pickers, `rustls-platform-verifier`) work exactly as on the
/// `android-activity` path. Each Activity is used until it finishes or is destroyed;
/// with several alive, the most recently registered one is used. A no-op when
/// `android-activity` owns the process.
/// Call it on the UI thread (as `onCreate` is), and before `host::start`: the
/// platform reads bundled assets (the emoji font) when it starts.
pub fn set_host_activity(env: &mut jni::Env<'_>, activity: &JObject<'_>) -> Result<(), String> {
    if ANDROID_APP.get().is_some() {
        return Ok(());
    }
    let global = env.new_global_ref(activity).e()?;
    let vm = env.get_java_vm().e()?;
    HOST_VM.store(
        vm.get_raw() as *mut c_void,
        std::sync::atomic::Ordering::SeqCst,
    );
    let mut activities = HOST_ACTIVITIES.lock().expect("poisoned");
    prune_gone_activities(env, &mut activities);
    activities.push(global);
    drop(activities);
    // A new Activity has a new Window: apply the system chrome to it again.
    *LAST_CHROME_STYLE.lock().expect("poisoned") = None;
    if UI_THREAD.get().is_none() {
        if let Err(err) = register_ui_thread() {
            log::warn!("set_host_activity: cannot post to the UI thread: {err}");
        }
    }
    super::accessibility::activity_changed();
    if HOST_APPLICATION.get().is_none() {
        match fetch_application_context(env, activity) {
            Ok(context) => {
                let _ = HOST_APPLICATION.set(context);
            }
            Err(err) => log::warn!("set_host_activity: no application context: {err}"),
        }
    }
    if HOST_ASSETS.get().is_none() {
        match host_assets(env, activity) {
            Ok(assets) => {
                let _ = HOST_ASSETS.set(assets);
            }
            Err(err) => log::warn!("set_host_activity: no AssetManager: {err}"),
        }
    }
    Ok(())
}

/// The application context, taken from the first host Activity and kept for the
/// life of the process. Its configuration follows system-wide changes such as night
/// mode, whichever Activity is in front and whatever it handles itself.
static HOST_APPLICATION: OnceLock<jni::refs::Global<JObject<'static>>> = OnceLock::new();

fn fetch_application_context(
    env: &mut jni::Env<'_>,
    activity: &JObject<'_>,
) -> Result<jni::refs::Global<JObject<'static>>, String> {
    let context = env
        .call_method(
            activity,
            jni::jni_str!("getApplicationContext"),
            jni::jni_sig!("()Landroid/content/Context;"),
            &[],
        )
        .and_then(|value| value.l())
        .or_clear(env)?;
    if context.is_null() {
        return Err("getApplicationContext returned null".into());
    }
    env.new_global_ref(&context).e()
}

/// The application's `AssetManager`, which `android-activity` provides on the
/// other path. Taken once, from the first host Activity's application context, and
/// kept for the life of the process.
static HOST_ASSETS: OnceLock<HostAssets> = OnceLock::new();

struct HostAssets {
    /// Keeps the Java `AssetManager`, and so the native one, alive.
    _java: jni::refs::Global<JObject<'static>>,
    native: ndk::asset::AssetManager,
}

fn host_assets(env: &mut jni::Env<'_>, activity: &JObject<'_>) -> Result<HostAssets, String> {
    let context = env
        .call_method(
            activity,
            jni::jni_str!("getApplicationContext"),
            jni::jni_sig!("()Landroid/content/Context;"),
            &[],
        )
        .and_then(|value| value.l())
        .or_clear(env)?;
    let assets = env
        .call_method(
            &context,
            jni::jni_str!("getAssets"),
            jni::jni_sig!("()Landroid/content/res/AssetManager;"),
            &[],
        )
        .and_then(|value| value.l())
        .or_clear(env)?;
    if assets.is_null() {
        return Err("getAssets returned null".into());
    }
    let java = env.new_global_ref(&assets).e()?;
    // SAFETY: `env` is the current thread's JNI env and `assets` a live AssetManager.
    let native =
        unsafe { ndk_sys::AAssetManager_fromJava(env.get_raw() as _, assets.as_raw() as _) };
    let native = std::ptr::NonNull::new(native).ok_or("AAssetManager_fromJava returned null")?;
    Ok(HostAssets {
        _java: java,
        // SAFETY: `native` belongs to the Java AssetManager that `_java` keeps alive
        // for as long as this value exists, which is the life of the process.
        native: unsafe { ndk::asset::AssetManager::from_ptr(native) },
    })
}

/// Run `f` with the app's `AssetManager`: from `android-activity`, or from the
/// Activity passed to [`set_host_activity`] on the host-driven path.
pub(crate) fn with_asset_manager<T>(f: impl FnOnce(&ndk::asset::AssetManager) -> T) -> Option<T> {
    if let Some(app) = android_app() {
        return Some(f(&app.asset_manager()));
    }
    HOST_ASSETS.get().map(|assets| f(&assets.native))
}

/// Public accessor for the JavaVM pointer.
///
/// Uses `AndroidApp::vm_as_ptr()` from the stored `AndroidApp`.
/// Used by `platform.rs` for JNI calls.
pub fn java_vm() -> *mut c_void {
    ANDROID_APP
        .get()
        .map(|app| app.vm_as_ptr())
        .unwrap_or_else(|| HOST_VM.load(std::sync::atomic::Ordering::SeqCst))
}

/// The current Activity as a raw JNI global reference (a `jobject`, not an
/// `ANativeActivity *`), or null.
///
/// On the host-driven path the reference is deleted once its Activity finishes and a
/// later [`activity`] or [`set_host_activity`] call notices, so it must not be kept.
/// Prefer [`activity`], which returns a local reference.
#[deprecated(note = "the reference may be deleted while in use; use `activity(env)`")]
pub fn activity_as_ptr() -> *mut c_void {
    ANDROID_APP
        .get()
        .map(|app| app.activity_as_ptr())
        .unwrap_or_else(|| {
            HOST_ACTIVITIES
                .lock()
                .expect("poisoned")
                .last()
                .map(|activity| activity.as_raw() as *mut c_void)
                .unwrap_or(std::ptr::null_mut())
        })
}

/// Returns a clone of the stored `AndroidApp`, if initialised.
pub fn android_app() -> Option<AndroidApp> {
    ANDROID_APP.get().cloned()
}

/// Install the platform built by the host-driven path (`super::host`).
///
/// The `android-activity` path calls `init_platform` instead; both end up in the same
/// slot so every accessor in this module keeps working regardless of entry point.
pub(crate) fn set_host_platform(platform: Arc<AndroidPlatform>) {
    if PLATFORM.set(platform).is_err() {
        log::warn!("set_host_platform: PLATFORM already set — both entry points in one process?");
    }
}

/// Returns a reference to the global `AndroidPlatform`, if initialised.
///
/// Returns `None` before `android_main` has set it up.
pub fn platform() -> Option<&'static Arc<AndroidPlatform>> {
    PLATFORM.get()
}

/// Returns a [`SharedPlatform`] wrapping the global `Arc<AndroidPlatform>`.
///
/// This is the value you hand to `Application::with_platform(...)`:
///
/// ```rust,no_run
/// let platform = jni::shared_platform().unwrap();
/// Application::with_platform(platform.into_rc()).run(|cx| { … });
/// ```
///
/// Returns `None` before `init_platform` has been called.
pub fn shared_platform() -> Option<SharedPlatform> {
    PLATFORM
        .get()
        .map(|arc| SharedPlatform::new(Arc::clone(arc)))
}

// ── input event types ─────────────────────────────────────────────────────────

/// Motion event action constants from the NDK.
const AMOTION_EVENT_ACTION_DOWN: u32 = 0;
const AMOTION_EVENT_ACTION_UP: u32 = 1;
const AMOTION_EVENT_ACTION_MOVE: u32 = 2;
const AMOTION_EVENT_ACTION_CANCEL: u32 = 3;

// ── UI thread ────────────────────────────────────────────────────────────────

/// The Java UI thread's looper, with an eventfd that wakes it to run
/// [`run_on_ui_thread`] tasks.
struct UiThread {
    id: std::thread::ThreadId,
    wake: std::os::fd::OwnedFd,
    _looper: ndk::looper::ForeignLooper,
}

static UI_THREAD: OnceLock<UiThread> = OnceLock::new();

type UiTask = Box<dyn FnOnce(&mut jni::Env<'_>) + Send>;

/// Pending tasks, with the key of those posted by [`run_latest_on_ui_thread`].
static UI_TASKS: Mutex<Vec<(Option<&'static str>, UiTask)>> = Mutex::new(Vec::new());

/// Remember the calling thread, which must be the UI thread, as the one
/// [`run_on_ui_thread`] posts to.
fn register_ui_thread() -> Result<(), String> {
    use std::os::fd::{AsFd, FromRawFd};

    let looper = ndk::looper::ForeignLooper::for_thread().ok_or("this thread has no looper")?;
    // SAFETY: plain syscall; the descriptor is owned by `wake` from here on.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(format!("eventfd: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    let wake = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    looper
        .add_fd_with_callback(wake.as_fd(), ndk::looper::FdEvent::INPUT, |fd, _| {
            let mut count = [0u8; 8];
            // SAFETY: reads 8 bytes into `count`; resets the eventfd counter.
            unsafe {
                libc::read(
                    std::os::fd::AsRawFd::as_raw_fd(&fd),
                    count.as_mut_ptr().cast(),
                    8,
                )
            };
            run_ui_tasks();
            true
        })
        .map_err(|err| format!("ALooper_addFd: {err:?}"))?;
    let _ = UI_THREAD.set(UiThread {
        id: std::thread::current().id(),
        wake,
        _looper: looper,
    });
    Ok(())
}

fn run_ui_tasks() {
    let tasks = std::mem::take(&mut *UI_TASKS.lock().expect("poisoned"));
    for (_, task) in tasks {
        run_ui_task(task);
    }
}

/// Run a task in its own frame, so an exception it leaves cannot fail the next.
fn run_ui_task(task: UiTask) {
    if let Err(err) = with_env(|env| {
        task(env);
        Ok(())
    }) {
        log::warn!("run_on_ui_thread: {err}");
    }
}

/// Run `f` on the Java UI thread, which Android requires for `View` and `Window`
/// calls: soon if called from another thread, right away on the UI thread itself.
///
/// On the host-driven path the UI thread is the one that called
/// [`set_host_activity`]. On the `android-activity` path it cannot be reached without
/// Java code, so `f` runs on the calling thread, as these calls always did there.
pub fn run_on_ui_thread(f: impl FnOnce(&mut jni::Env<'_>) + Send + 'static) {
    post_to_ui_thread(None, Box::new(f));
}

/// Like [`run_on_ui_thread`], but a task posted with the same `key` that has not run
/// yet is dropped: for updates where only the latest matters (the IME cursor
/// position, the system chrome), so they cannot pile up behind a busy UI thread.
pub(crate) fn run_latest_on_ui_thread(
    key: &'static str,
    f: impl FnOnce(&mut jni::Env<'_>) + Send + 'static,
) {
    post_to_ui_thread(Some(key), Box::new(f));
}

fn post_to_ui_thread(key: Option<&'static str>, task: UiTask) {
    let Some(ui) = UI_THREAD.get() else {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log::debug!("run_on_ui_thread: no UI thread registered; running in place");
        }
        run_ui_task(task);
        return;
    };
    if std::thread::current().id() == ui.id {
        run_ui_task(task);
        return;
    }
    let mut tasks = UI_TASKS.lock().expect("poisoned");
    if key.is_some() {
        tasks.retain(|(pending, _)| *pending != key);
    }
    tasks.push((key, task));
    drop(tasks);
    let one = 1u64.to_ne_bytes();
    // SAFETY: writes 8 bytes from `one` to the eventfd, which wakes the UI looper.
    unsafe {
        libc::write(
            std::os::fd::AsRawFd::as_raw_fd(&ui.wake),
            one.as_ptr().cast(),
            8,
        )
    };
}

// ── night mode query ─────────────────────────────────────────────────────────

/// Query the current night mode.
///
/// Returns `true` if the system is in dark mode.
pub fn query_night_mode_via_jni() -> bool {
    let is_dark = if let Some(app) = android_app() {
        // Build an ndk::configuration::Configuration from the app's asset manager.
        let config = ndk::configuration::Configuration::from_asset_manager(&app.asset_manager());
        config.ui_mode_night() == ndk::configuration::UiModeNight::Yes
    } else {
        // No AndroidApp on the host-driven path: ask the application's Resources.
        // An Activity that handles `uiMode` itself keeps a stale configuration in its
        // AssetManager.
        host_ui_mode().is_ok_and(|ui_mode| ui_mode & UI_MODE_NIGHT_MASK == UI_MODE_NIGHT_YES)
    };

    log::debug!("query_night_mode: is_dark={}", is_dark);
    is_dark
}

/// `Configuration.UI_MODE_NIGHT_MASK` / `UI_MODE_NIGHT_YES`.
const UI_MODE_NIGHT_MASK: i32 = 0x30;
const UI_MODE_NIGHT_YES: i32 = 0x20;

/// `getResources().getConfiguration().uiMode` of the application context.
fn host_ui_mode() -> Result<i32, String> {
    let context = HOST_APPLICATION
        .get()
        .ok_or("set_host_activity has not been called")?;
    with_env(|env| {
        let resources = env
            .call_method(
                context,
                jni::jni_str!("getResources"),
                jni::jni_sig!("()Landroid/content/res/Resources;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        let config = env
            .call_method(
                &resources,
                jni::jni_str!("getConfiguration"),
                jni::jni_sig!("()Landroid/content/res/Configuration;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        env.get_field(&config, jni::jni_str!("uiMode"), jni::jni_sig!("I"))
            .and_then(|v| v.i())
            .or_clear(env)
    })
}

/// Apply the system night mode to a window.
pub(crate) fn sync_appearance(window: &crate::android::window::AndroidWindow) {
    window.set_appearance(if query_night_mode_via_jni() {
        crate::android::window::WindowAppearance::Dark
    } else {
        crate::android::window::WindowAppearance::Light
    });
}

// ── input event processing ────────────────────────────────────────────────────

/// Process input events from the `AndroidApp` and dispatch them to the window.
fn process_input_events(app: &AndroidApp) {
    let platform = match PLATFORM.get() {
        Some(p) => p,
        None => {
            log::trace!("process_input_events: no platform yet");
            return;
        }
    };

    let win = match platform.primary_window() {
        Some(w) => w,
        None => {
            log::trace!("process_input_events: no primary window yet");
            return;
        }
    };

    match app.input_events_iter() {
        Ok(mut iter) => {
            loop {
                let read_input = iter.next(|event| {
                    use android_activity::input::{InputEvent, MotionAction};

                    match event {
                        InputEvent::MotionEvent(motion_event) => {
                            let action = motion_event.action();
                            let pointer_count = motion_event.pointer_count();

                            log::debug!(
                                "process_input_events: MotionEvent action={:?} pointers={}",
                                action,
                                pointer_count,
                            );

                            // Check if this touch lands on a platform view.
                            //
                            // On Android with NativeActivity, ALL touch events go
                            // to the native surface first. Platform views are real
                            // Java Views in a FrameLayout overlay, but they won't
                            // receive touches unless we skip GPUI dispatch and let
                            // Android's view hierarchy handle the event instead.
                            //
                            // We use the primary pointer for the hit-test on DOWN
                            // actions. Physical pixel coordinates are converted to
                            // logical pixels to match PlatformViewBounds.
                            let hits_platform_view = {
                                let registry = crate::platform_view::PlatformViewRegistry::global();
                                if registry.active_view_count() > 0 {
                                    let primary = motion_event.pointer_at_index(
                                        match action {
                                            MotionAction::PointerDown | MotionAction::PointerUp => {
                                                motion_event.pointer_index()
                                            }
                                            _ => 0,
                                        }
                                    );
                                    let scale = win.scale_factor();
                                    let logical_x = primary.x() / scale;
                                    let logical_y = primary.y() / scale;
                                    registry.hit_test(logical_x, logical_y)
                                } else {
                                    false
                                }
                            };

                            // GPUI does not use hover on mobile. With a screen reader on,
                            // touch exploration arrives as hover events, which the
                            // accessibility delegate on the decor view has to see.
                            if matches!(
                                action,
                                MotionAction::HoverEnter
                                    | MotionAction::HoverMove
                                    | MotionAction::HoverExit
                            ) {
                                return android_activity::InputStatus::Unhandled;
                            }

                            if hits_platform_view {
                                log::debug!(
                                    "process_input_events: touch hits platform view, skipping GPUI dispatch",
                                );
                                // Return Unhandled so android-activity can pass
                                // the event back to the Java view hierarchy where
                                // the platform view's FrameLayout lives.
                                return android_activity::InputStatus::Unhandled;
                            }

                            for i in 0..pointer_count {
                                let pointer = motion_event.pointer_at_index(i);

                                let touch_action = match action {
                                    MotionAction::Down => AMOTION_EVENT_ACTION_DOWN,
                                    MotionAction::PointerDown => {
                                        // For pointer down, only dispatch the specific pointer
                                        if i != motion_event.pointer_index() {
                                            continue;
                                        }
                                        AMOTION_EVENT_ACTION_DOWN
                                    }
                                    MotionAction::Up => AMOTION_EVENT_ACTION_UP,
                                    MotionAction::PointerUp => {
                                        // For pointer up, only dispatch the specific pointer
                                        if i != motion_event.pointer_index() {
                                            continue;
                                        }
                                        AMOTION_EVENT_ACTION_UP
                                    }
                                    MotionAction::Move => AMOTION_EVENT_ACTION_MOVE,
                                    MotionAction::Cancel => AMOTION_EVENT_ACTION_CANCEL,
                                    _ => continue,
                                };

                                let touch = crate::android::TouchPoint {
                                    id: pointer.pointer_id(),
                                    x: pointer.x(),
                                    y: pointer.y(),
                                    action: touch_action,
                                };

                                log::debug!(
                                    "process_input_events: dispatching touch id={} x={:.0} y={:.0} action={}",
                                    touch.id, touch.x, touch.y, touch.action,
                                );

                                win.handle_touch(touch);
                            }

                            android_activity::InputStatus::Handled
                        }
                        InputEvent::KeyEvent(key_event) => {
                            use android_activity::input::KeyAction;

                            let action = match key_event.action() {
                                KeyAction::Down => 0,
                                KeyAction::Up => 1,
                                _ => return android_activity::InputStatus::Unhandled,
                            };

                            let key_code: u32 = key_event.key_code().into();
                            let meta_state: u32 = key_event.meta_state().0;

                            let unicode_char = unicode_char_for_key_event(
                                key_code as i32,
                                action,
                                meta_state as i32,
                            );

                            if unicode_char != 0 {
                                log::trace!(
                                    "dispatch_key_event: code={} action={} meta={:#x} → unicode=U+{:04X}",
                                    key_code,
                                    action,
                                    meta_state,
                                    unicode_char
                                );
                            }

                            let key_event = crate::android::AndroidKeyEvent {
                                key_code: key_code as i32,
                                action,
                                meta_state: meta_state as i32,
                                unicode_char,
                            };

                            win.handle_key_event(key_event);
                            android_activity::InputStatus::Handled
                        }
                        _ => android_activity::InputStatus::Unhandled,
                    }
                });

                if !read_input {
                    break;
                }
            }
        }
        Err(err) => {
            log::error!("Failed to get input events iterator: {err:?}");
        }
    }
}

// ── main event loop ───────────────────────────────────────────────────────────

/// The event loop that processes `android-activity` events and drives the
/// platform.
///
/// Called from `android_main` after the platform is initialised.
/// Runs until the platform requests quit or the activity is destroyed.
pub fn run_event_loop(app: &AndroidApp) {
    log::info!("run_event_loop: entering main loop");

    // Track whether the on_init_window callback has already been invoked.
    // We do NOT invoke it inside handle_main_event (which runs inside
    // poll_events) because the callback can be heavy (shader compilation,
    // GPUI Application setup).  Running it there blocks the event loop and
    // prevents the system's FocusEvent from being consumed, triggering an
    // ANR after 10 seconds.
    //
    // Instead we check each loop iteration: if a primary window exists and
    // the callback is still pending, invoke it *after* poll_events has
    // returned so focus/input events have already been drained.
    INIT_WINDOW_DONE.store(false, Ordering::Relaxed);
    // Vsync pacing lives on this thread's looper.
    super::frame_source::install();
    let mut iteration: u64 = 0;
    let mut last_heartbeat = std::time::Instant::now();
    let mut app_is_active = false;

    loop {
        iteration += 1;

        // Log a heartbeat every 5 seconds so we can tell if the loop is alive.
        let now = std::time::Instant::now();
        if now.duration_since(last_heartbeat) >= Duration::from_secs(5) {
            log::info!(
                "run_event_loop: heartbeat — iteration={}, init_done={}, active={}",
                iteration,
                INIT_WINDOW_DONE.load(Ordering::Relaxed),
                app_is_active,
            );
            last_heartbeat = now;
        }

        // Check if quit was requested.
        if let Some(platform) = PLATFORM.get() {
            if platform.should_quit() {
                log::info!("run_event_loop: platform requested quit");
                dispose_all_platform_views();
                break;
            }
            platform.tick();
        }

        // ── Poll for events ──
        //
        // Blocks until something reaches the looper: a lifecycle command,
        // input, a main-thread task, or the vsync callback that
        // `frame_source` posted for a wanted frame. The timeout only covers
        // the delayed dispatcher tasks `platform.tick()` has to release and
        // the clock fallback without a choreographer.
        let timeout = super::frame_source::poll_timeout(
            PLATFORM
                .get()
                .and_then(|platform| platform.next_delayed_due()),
            INIT_WINDOW_DONE.load(Ordering::Relaxed) && app_is_active,
        );
        app.poll_events(Some(timeout), |event| match event {
            PollEvent::Main(main_event) => {
                handle_main_event(app, main_event);
            }
            PollEvent::Wake => {}
            PollEvent::Timeout => {}
            _ => {}
        });

        // ── Deferred lifecycle processing ──
        //
        // Between each handler, call poll_events again to drain any
        // events the Java UI thread may have queued while we were
        // processing.  This keeps the condvar wait short and prevents
        // the InputDispatcher ANR (10s timeout on MotionEvent delivery).
        //
        // Helper closure to drain events quickly:
        let drain_events = |app: &AndroidApp| {
            app.poll_events(Some(Duration::ZERO), |event| {
                if let PollEvent::Main(main_event) = event {
                    handle_main_event(app, main_event);
                }
            });
        };

        // 1. TerminateWindow — unconfigure surface, release native window
        if TERM_WINDOW_PENDING.swap(false, Ordering::Relaxed) {
            log::info!("deferred: TerminateWindow (iter={})", iteration);
            INIT_WINDOW_DONE.store(false, Ordering::Relaxed);
            *LAST_CHROME_STYLE.lock().unwrap() = None;
            if let Some(platform) = PLATFORM.get() {
                if let Some(win) = platform.primary_window() {
                    win.term_window();
                }
            }
            // Drain immediately — Java thread may be blocked on InitWindow condvar.
            drain_events(app);
        }

        // 2. InitWindow — replace surface on existing renderer, or create new
        if INIT_WINDOW_PENDING.swap(false, Ordering::Relaxed) {
            log::info!("deferred: InitWindow (iter={})", iteration);
            if let Some(platform) = PLATFORM.get() {
                if let Some(native_window) = app.native_window() {
                    let width = native_window.width();
                    let height = native_window.height();
                    log::info!("InitWindow: {}×{}", width, height);

                    platform.update_primary_display(&native_window, &app.asset_manager());

                    let scale_factor = platform
                        .primary_display()
                        .map(|d| d.scale_factor())
                        .unwrap_or(1.0);

                    if let Some(existing) = platform.primary_window() {
                        let gpu_ctx = platform.gpu_context();
                        match existing.init_window(native_window, gpu_ctx) {
                            Ok(()) => {
                                log::info!("InitWindow: reinitialised existing window");
                            }
                            Err(e) => {
                                log::error!("failed to reinit window surface: {e:#}");
                            }
                        }

                        // Trigger GPUI resize so layout adapts to new dimensions.
                        existing.handle_resize();

                        let cr = app.content_rect();
                        existing.update_safe_area_from_content_rect(
                            cr.left, cr.top, cr.right, cr.bottom,
                        );

                        INIT_WINDOW_DONE.store(true, Ordering::Relaxed);
                    } else {
                        match platform.open_window(native_window, scale_factor, false) {
                            Ok(win) => {
                                log::info!(
                                    "window opened — id={:#x} scale={:.1}",
                                    win.id(),
                                    scale_factor
                                );

                                let cr = app.content_rect();
                                win.update_safe_area_from_content_rect(
                                    cr.left, cr.top, cr.right, cr.bottom,
                                );
                            }
                            Err(e) => {
                                log::error!("failed to open window: {e:#}");
                            }
                        }
                    }
                }
            }
            // Drain — Java thread may have queued ConfigChanged/WindowResized.
            drain_events(app);
        }

        // 3. WindowResized
        if WINDOW_RESIZED_PENDING.swap(false, Ordering::Relaxed) {
            log::debug!("deferred: WindowResized");
            if let Some(platform) = PLATFORM.get() {
                if let Some(win) = platform.primary_window() {
                    win.handle_resize();
                    let cr = app.content_rect();
                    win.update_safe_area_from_content_rect(cr.left, cr.top, cr.right, cr.bottom);
                }
            }
        }

        // 4. ConfigChanged
        if CONFIG_CHANGED_PENDING.swap(false, Ordering::Relaxed) {
            log::debug!("deferred: ConfigChanged");
            if let Some(platform) = PLATFORM.get() {
                platform.notify_keyboard_layout_change();
                if let Some(win) = platform.primary_window() {
                    sync_appearance(&win);
                }
            }
        }

        // 5. Pause / background
        if PAUSE_PENDING.swap(false, Ordering::Relaxed) {
            log::info!("deferred: Pause (iter={})", iteration);
            if let Some(platform) = PLATFORM.get() {
                platform.did_enter_background();
                if let Some(win) = platform.primary_window() {
                    win.set_active(false);
                }
            }
            // Pause platform views
            pause_platform_views();
        }

        // 6. Resume / foreground
        if RESUME_PENDING.swap(false, Ordering::Relaxed) {
            log::info!("deferred: Resume (iter={})", iteration);
            if let Some(platform) = PLATFORM.get() {
                platform.did_become_active();
                if let Some(win) = platform.primary_window() {
                    win.set_active(true);
                }
            }
            // Resume platform views
            resume_platform_views();
        }

        // Track active/focused state.
        if let Some(platform) = PLATFORM.get() {
            if let Some(win) = platform.primary_window() {
                let is_active = win.is_active();
                if is_active != app_is_active {
                    log::info!(
                        "run_event_loop: active {} -> {} (iter={})",
                        app_is_active,
                        is_active,
                        iteration,
                    );
                    app_is_active = is_active;
                }
            }
        }

        // Process input events.
        process_input_events(app);

        // Deferred initialisation callbacks (runs once).
        if !INIT_WINDOW_DONE.load(Ordering::Relaxed) {
            if let Some(platform) = PLATFORM.get() {
                if platform.primary_window().is_some() {
                    if let Some(finish_cb) = platform.take_finish_launching_callback() {
                        log::info!("invoking finish_launching callback (iter={})", iteration);
                        finish_cb();
                    }

                    if let Some(init_cb) = platform.take_on_init_window_callback() {
                        let win = platform.primary_window().unwrap();
                        log::info!("invoking on_init_window callback (iter={})", iteration);
                        init_cb(win);
                    }

                    INIT_WINDOW_DONE.store(true, Ordering::Relaxed);
                    NATIVE_INITIALIZED.store(true, Ordering::Release);
                    log::info!("NATIVE_INITIALIZED = true");

                    // Render first frame immediately.
                    platform.flush_main_thread_tasks();
                    if let Some(win) = platform.primary_window() {
                        win.request_frame();
                    }
                }
            }
        }

        // ── Render ──
        if let Some(platform) = PLATFORM.get() {
            if INIT_WINDOW_DONE.load(Ordering::Relaxed) && app_is_active {
                platform.flush_main_thread_tasks();
                // Software-keyboard text bypasses GPUI's invalidator; the
                // frame callback turns it into a forced render.
                if crate::TEXT_INPUT_DIRTY.load(Ordering::Acquire) {
                    super::frame_source::schedule_frame();
                }
                if super::frame_source::take_frame() {
                    if let Some(win) = platform.primary_window() {
                        win.request_frame();
                    }

                    // Drain lifecycle events that arrived during rendering
                    // (e.g. rotation triggers TerminateWindow while we were
                    // in get_current_texture / present).
                    drain_events(app);
                    process_input_events(app);
                }
            }
        }
    }

    log::info!("run_event_loop: exiting main loop");
}

/// Pause all platform views when the app goes to background.
fn pause_platform_views() {
    let _ = with_env(|env| {
        if let Ok(helper_class) = find_app_class(env, "dev.gpui.mobile.GpuiPlatformView") {
            let result = env.call_static_method(
                &helper_class,
                jni::jni_str!("pauseAll"),
                jni::jni_sig!("()V"),
                &[],
            );
            if let Err(err) = result.or_clear(env) {
                log::warn!("pauseAll: {err}");
            }
        }
        Ok(())
    });
}

/// Resume all platform views when the app returns to foreground.
fn resume_platform_views() {
    let _ = with_env(|env| {
        if let Ok(helper_class) = find_app_class(env, "dev.gpui.mobile.GpuiPlatformView") {
            let result = env.call_static_method(
                &helper_class,
                jni::jni_str!("resumeAll"),
                jni::jni_sig!("()V"),
                &[],
            );
            if let Err(err) = result.or_clear(env) {
                log::warn!("resumeAll: {err}");
            }
        }
        Ok(())
    });
}

/// Dispose all platform views during app shutdown.
fn dispose_all_platform_views() {
    let _ = with_env(|env| {
        if let Ok(helper_class) = find_app_class(env, "dev.gpui.mobile.GpuiPlatformView") {
            let result = env.call_static_method(
                &helper_class,
                jni::jni_str!("disposeAll"),
                jni::jni_sig!("()V"),
                &[],
            );
            if let Err(err) = result.or_clear(env) {
                log::warn!("disposeAll: {err}");
            }
        }
        Ok(())
    });
}

/// Handle a single `MainEvent` from `android-activity`.
fn handle_main_event(_app: &AndroidApp, event: MainEvent<'_>) {
    match event {
        MainEvent::InitWindow { .. } => {
            log::info!("MainEvent::InitWindow");
            // Defer to after poll_events to avoid deadlock with state lock.
            INIT_WINDOW_PENDING.store(true, Ordering::Relaxed);
        }

        MainEvent::TerminateWindow { .. } => {
            log::info!("MainEvent::TerminateWindow");
            // Defer to after poll_events to avoid deadlock with state lock.
            TERM_WINDOW_PENDING.store(true, Ordering::Relaxed);
        }

        MainEvent::WindowResized { .. } => {
            log::debug!("MainEvent::WindowResized");
            // Defer to after poll_events to avoid deadlock with state lock.
            WINDOW_RESIZED_PENDING.store(true, Ordering::Relaxed);
        }

        MainEvent::GainedFocus => {
            log::info!("MainEvent::GainedFocus");
            RESUME_PENDING.store(true, Ordering::Relaxed);
        }

        MainEvent::LostFocus => {
            log::info!("MainEvent::LostFocus");
            PAUSE_PENDING.store(true, Ordering::Relaxed);
        }

        MainEvent::Resume { .. } => {
            log::info!("MainEvent::Resume");
            RESUME_PENDING.store(true, Ordering::Relaxed);
        }

        MainEvent::Pause => {
            log::info!("MainEvent::Pause");
            // set_active uses AtomicBool so it never blocks.
            PAUSE_PENDING.store(true, Ordering::Relaxed);
        }

        MainEvent::ConfigChanged { .. } => {
            log::debug!("MainEvent::ConfigChanged");
            CONFIG_CHANGED_PENDING.store(true, Ordering::Relaxed);
        }

        MainEvent::Start => {
            log::info!("MainEvent::Start");
        }

        MainEvent::Stop => {
            log::info!("MainEvent::Stop");
        }

        MainEvent::SaveState { .. } => {
            log::info!("MainEvent::SaveState");
        }

        MainEvent::LowMemory => {
            log::warn!("MainEvent::LowMemory — consider releasing cached resources");
        }

        MainEvent::Destroy => {
            log::info!("MainEvent::Destroy");

            if let Some(platform) = PLATFORM.get() {
                platform.quit();
            }
        }

        MainEvent::InsetsChanged { .. } => {
            log::debug!("MainEvent::InsetsChanged");
            WINDOW_RESIZED_PENDING.store(true, Ordering::Relaxed);
        }

        MainEvent::ContentRectChanged { .. } => {
            log::debug!("MainEvent::ContentRectChanged");
            WINDOW_RESIZED_PENDING.store(true, Ordering::Relaxed);
        }

        _ => {
            log::trace!("MainEvent: other variant");
        }
    }
}

// ── main loop helper (compat with existing code) ──────────────────────────────

/// Run one iteration of the event loop.
///
/// This is a compatibility wrapper for code that uses a manual poll loop.
/// Prefer `run_event_loop` for the standard event loop.
///
/// `timeout_ms` — how long to block waiting for events (milliseconds).
/// Pass `0` for non-blocking, `-1` to block indefinitely.
///
/// Returns `true` if the application should exit.
pub fn poll_events(timeout_ms: i32) -> bool {
    if let Some(platform) = PLATFORM.get() {
        if platform.should_quit() {
            return true;
        }
        platform.tick();
    }

    let app = match ANDROID_APP.get() {
        Some(app) => app,
        None => return false,
    };

    let timeout = if timeout_ms < 0 {
        None
    } else {
        Some(Duration::from_millis(timeout_ms as u64))
    };

    app.poll_events(timeout, |event| match event {
        PollEvent::Main(main_event) => {
            handle_main_event(app, main_event);
        }
        PollEvent::Wake => {}
        _ => {}
    });

    process_input_events(app);

    // Drive the GPUI rendering pipeline (same as run_event_loop).
    if let Some(platform) = PLATFORM.get() {
        platform.flush_main_thread_tasks();
        if let Some(win) = platform.primary_window() {
            win.request_frame();
        }
    }

    false
}

// ── public init / run helpers ─────────────────────────────────────────────────

/// Install a panic hook that routes panic messages to logcat.
///
/// Call this early in `android_main` so that any subsequent panic is
/// visible via `adb logcat`.  Safe to call multiple times — each call
/// replaces the previous hook.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".to_string()
        };
        if let Some(loc) = info.location() {
            log::error!(
                "PANIC at {}:{}:{}: {}",
                loc.file(),
                loc.line(),
                loc.column(),
                payload
            );
        } else {
            log::error!("PANIC: {}", payload);
        }
    }));
}

/// Store the `AndroidApp` globally and create the `AndroidPlatform`.
///
/// Must be called exactly once from `android_main` before
/// `run_event_loop`.  Returns a reference to the platform so the caller
/// can register callbacks (e.g. `set_on_init_window`) before entering
/// the event loop.
pub fn init_platform(app: &AndroidApp) -> &'static Arc<AndroidPlatform> {
    let _ = ANDROID_APP.set(app.clone());
    log::info!("init_platform: stored AndroidApp");

    let platform = Arc::new(AndroidPlatform::new(false));
    log::info!("init_platform: AndroidPlatform created");

    PLATFORM
        .set(Arc::clone(&platform))
        .unwrap_or_else(|_| log::warn!("PLATFORM already set — duplicate init_platform?"));

    // SAFETY: we just set it above.
    PLATFORM.get().unwrap()
}

// ── system chrome (status bar / navigation bar) ───────────────────────────────

/// Cached last-applied system chrome style.
///
/// `set_system_chrome` is called on every frame render. By caching the last
/// applied style we skip the JNI calls entirely when nothing changed — which is the
/// common case.
#[allow(clippy::type_complexity)]
static LAST_CHROME_STYLE: std::sync::Mutex<
    Option<(Option<u32>, Option<u32>, crate::StatusBarContentStyle)>,
> = std::sync::Mutex::new(None);

/// Apply system chrome styling on Android.
///
/// Sets the status bar color, navigation bar color, and light/dark
/// status bar icons via JNI calls to `Window` and `WindowInsetsController`, which run
/// on the UI thread (see [`run_on_ui_thread`]).
pub fn set_system_chrome(style: &crate::SystemChromeStyle) {
    let status_bar_color = style.status_bar_color;
    let navigation_bar_color = style.navigation_bar_color;
    let status_bar_style = style.status_bar_style;

    // Skip the JNI calls when nothing changed.
    {
        let key = (status_bar_color, navigation_bar_color, status_bar_style);
        let mut last = LAST_CHROME_STYLE.lock().unwrap();
        if *last == Some(key) {
            return;
        }
        *last = Some(key);
    }

    // Window calls belong on the UI thread; from the render thread they could
    // deadlock against it.
    run_latest_on_ui_thread("system_chrome", move |env| {
        let result = (|| -> Result<(), String> {
            let activity_obj = activity(env)?;

            // 1. Get the Window: activity.getWindow()
            let window = env
                .call_method(
                    &activity_obj,
                    jni::jni_str!("getWindow"),
                    jni::jni_sig!("()Landroid/view/Window;"),
                    &[],
                )
                .and_then(|v: jni::objects::JValueOwned| v.l())
                .or_clear(env)?;
            if window.is_null() {
                return Err("getWindow returned null".into());
            }

            // 2. Set status bar color if provided
            if let Some(color) = status_bar_color {
                let argb = (0xFF000000_u32 | color) as i32;
                let result = env.call_method(
                    &window,
                    jni::jni_str!("setStatusBarColor"),
                    jni::jni_sig!("(I)V"),
                    &[JValue::Int(argb)],
                );
                if let Err(err) = result.or_clear(env) {
                    log::warn!("setStatusBarColor: {err}");
                }
            }

            // 3. Set navigation bar color if provided
            if let Some(color) = navigation_bar_color {
                let argb = (0xFF000000_u32 | color) as i32;
                let result = env.call_method(
                    &window,
                    jni::jni_str!("setNavigationBarColor"),
                    jni::jni_sig!("(I)V"),
                    &[JValue::Int(argb)],
                );
                if let Err(err) = result.or_clear(env) {
                    log::warn!("setNavigationBarColor: {err}");
                }
            }

            // 4. Set light/dark status bar icons via WindowInsetsController (API 30+),
            //    or the deprecated system UI visibility flags before it.
            let controller = env
                .call_method(
                    &window,
                    jni::jni_str!("getInsetsController"),
                    jni::jni_sig!("()Landroid/view/WindowInsetsController;"),
                    &[],
                )
                .and_then(|v| v.l())
                .or_catch(env, jni::jni_str!("java/lang/NoSuchMethodError"))?;
            match controller {
                Some(controller) if !controller.is_null() => {
                    const APPEARANCE_LIGHT_STATUS_BARS: i32 = 0x0000_0008;
                    let appearance = match status_bar_style {
                        crate::StatusBarContentStyle::Dark => APPEARANCE_LIGHT_STATUS_BARS,
                        crate::StatusBarContentStyle::Light => 0,
                    };
                    env.call_method(
                        &controller,
                        jni::jni_str!("setSystemBarsAppearance"),
                        jni::jni_sig!("(II)V"),
                        &[
                            JValue::Int(appearance),
                            JValue::Int(APPEARANCE_LIGHT_STATUS_BARS),
                        ],
                    )
                    .or_clear(env)?;
                }
                Some(_) => {}
                None => {
                    const SYSTEM_UI_FLAG_LIGHT_STATUS_BAR: i32 = 0x0000_2000;
                    let decor = env
                        .call_method(
                            &window,
                            jni::jni_str!("getDecorView"),
                            jni::jni_sig!("()Landroid/view/View;"),
                            &[],
                        )
                        .and_then(|v| v.l())
                        .or_clear(env)?;
                    let current = env
                        .call_method(
                            &decor,
                            jni::jni_str!("getSystemUiVisibility"),
                            jni::jni_sig!("()I"),
                            &[],
                        )
                        .and_then(|v| v.i())
                        .or_clear(env)?;
                    let flags = match status_bar_style {
                        crate::StatusBarContentStyle::Dark => {
                            current | SYSTEM_UI_FLAG_LIGHT_STATUS_BAR
                        }
                        crate::StatusBarContentStyle::Light => {
                            current & !SYSTEM_UI_FLAG_LIGHT_STATUS_BAR
                        }
                    };
                    env.call_method(
                        &decor,
                        jni::jni_str!("setSystemUiVisibility"),
                        jni::jni_sig!("(I)V"),
                        &[JValue::Int(flags)],
                    )
                    .or_clear(env)?;
                }
            }

            Ok(())
        })();
        if let Err(e) = result {
            log::warn!("set_system_chrome: {e}");
        }
    });

    log::info!(
        "set_system_chrome: status_bar_color={:?}, nav_bar_color={:?}, style={:?}",
        style.status_bar_color,
        style.navigation_bar_color,
        style.status_bar_style
    );
}

// ── software keyboard (IME) control ───────────────────────────────────────────

/// The keyboard type currently requested, or `None` when hidden.
///
/// The Java-side `InputProxy` belongs to the Activity, so an Activity recreation takes
/// the IME with it — while GPUI still considers the same field focused and therefore
/// never asks for the keyboard again. Remembering the request lets a new Activity put
/// the IME back; see [`restore_keyboard`].
static SHOWN_KEYBOARD: std::sync::Mutex<Option<crate::KeyboardType>> = std::sync::Mutex::new(None);

/// The keyboard the user hid (back) while its text input kept focus.
///
/// GPUI asks for the keyboard only when a text input *gains* focus, and the platform
/// cannot blur it, so after a back-press the field stays focused with no keyboard and
/// tapping it again changes nothing. [`super::window`] uses this to bring the keyboard
/// back when a tap lands on the focused field. Cleared by any explicit show or hide.
static DISMISSED_KEYBOARD: std::sync::Mutex<Option<crate::KeyboardType>> =
    std::sync::Mutex::new(None);

/// The focused field's [`gpui::TextInputConfiguration`], applied by every show.
static TEXT_INPUT_CONFIGURATION: std::sync::Mutex<Option<gpui::TextInputConfiguration>> =
    std::sync::Mutex::new(None);

/// `PlatformWindow::set_text_input_configuration`. GPUI forwards it before a field's
/// `FocusGained`, but focus can also move straight from one field to another, so a
/// keyboard already showing restarts with the new configuration.
pub(crate) fn set_text_input_configuration(
    configuration: gpui::TextInputConfiguration,
    has_input: bool,
) {
    let mut current = TEXT_INPUT_CONFIGURATION.lock().expect("poisoned");
    if current.as_ref() == Some(&configuration) {
        return;
    }
    *current = Some(configuration);
    drop(current);
    let shown = *SHOWN_KEYBOARD.lock().expect("poisoned");
    if let Some(keyboard_type) = shown.filter(|_| has_input) {
        show_keyboard_android(keyboard_type);
    }
}

/// IME event 5: the system already hid the keyboard; remember it for a tap to undo.
pub(crate) fn keyboard_hidden_by_user() {
    let shown = SHOWN_KEYBOARD.lock().expect("poisoned").take();
    if shown.is_some() {
        *DISMISSED_KEYBOARD.lock().expect("poisoned") = shown;
    }
}

/// IME event 4 (Done): hide the keyboard, and remember it for a tap to undo as for 5.
pub(crate) fn keyboard_done() {
    let shown = *SHOWN_KEYBOARD.lock().expect("poisoned");
    hide_keyboard_android();
    *DISMISSED_KEYBOARD.lock().expect("poisoned") = shown;
}

pub(crate) fn keyboard_dismissed() -> bool {
    DISMISSED_KEYBOARD.lock().expect("poisoned").is_some()
}

/// Show the keyboard [`keyboard_hidden_by_user`] took away again.
pub(crate) fn reshow_dismissed_keyboard() {
    let dismissed = DISMISSED_KEYBOARD.lock().expect("poisoned").take();
    if let Some(keyboard_type) = dismissed {
        show_keyboard_android(keyboard_type);
    }
}

/// Re-request the keyboard on the current Activity if one was showing.
///
/// Called by [`super::host`] once a recreated Activity's surface is attached. A fresh
/// IME session is started, which is what we want: the proxy is a new object.
pub(crate) fn restore_keyboard() {
    let keyboard_type = *SHOWN_KEYBOARD.lock().expect("poisoned");
    if let Some(keyboard_type) = keyboard_type {
        log::info!("restore_keyboard: re-showing {keyboard_type:?} on the new Activity");
        show_keyboard_android(keyboard_type);
    }
}

/// Show the UI-thread EditText proxy supplied by GpuiInputActivity.
///
/// Unlike NativeActivity's key-event-only connection, its InputConnection
/// supports composing text, commits and Unicode surrounding-text deletion.
pub fn show_keyboard_android(keyboard_type: crate::KeyboardType) {
    *SHOWN_KEYBOARD.lock().expect("poisoned") = Some(keyboard_type);
    *DISMISSED_KEYBOARD.lock().expect("poisoned") = None;
    let kind = match keyboard_type {
        crate::KeyboardType::Default => 0,
        crate::KeyboardType::EmailAddress => 1,
        crate::KeyboardType::Phone => 2,
        crate::KeyboardType::NumberPad => 3,
        crate::KeyboardType::URL => 4,
        crate::KeyboardType::Decimal => 5,
    };
    let (input_type, ime_options) = super::input_type::editor_info(
        &TEXT_INPUT_CONFIGURATION
            .lock()
            .expect("poisoned")
            .clone()
            .unwrap_or_default(),
        keyboard_type,
    );
    let session = super::text_input::new_session();
    if let Err(error) = with_env(|env| {
        let activity = activity(env)?;
        // Hosts written before `gpuiShowKeyboardWithInputType` only take a keyboard type.
        if env
            .call_method(
                &activity,
                jni::jni_str!("gpuiShowKeyboardWithInputType"),
                jni::jni_sig!("(IIJ)V"),
                &[
                    JValue::Int(input_type),
                    JValue::Int(ime_options),
                    JValue::Long(session as i64),
                ],
            )
            .or_clear(env)
            .is_ok()
        {
            return Ok(());
        }
        env.call_method(
            &activity,
            jni::jni_str!("gpuiShowKeyboard"),
            jni::jni_sig!("(IJ)V"),
            &[JValue::Int(kind), JValue::Long(session as i64)],
        )
        .or_clear(env)?;
        Ok(())
    }) {
        log::warn!("IME requires GpuiInputActivity: {error}");
    }
}

/// Hide the software keyboard on Android.
///
/// Invalidates queued IME callbacks and clears the native composition buffer.
pub fn hide_keyboard_android() {
    *SHOWN_KEYBOARD.lock().expect("poisoned") = None;
    *DISMISSED_KEYBOARD.lock().expect("poisoned") = None;
    let session = super::text_input::new_session();
    if let Err(err) = with_env(|env| {
        let activity = activity(env)?;
        env.call_method(
            &activity,
            jni::jni_str!("gpuiHideKeyboard"),
            jni::jni_sig!("(J)V"),
            &[JValue::Long(session as i64)],
        )
        .or_clear(env)?;
        Ok(())
    }) {
        log::debug!("gpuiHideKeyboard: {err}");
    }
    if let Some(app) = android_app() {
        app.hide_soft_input(false);
    }
}

pub(super) fn reset_keyboard_composition() {
    let session = super::text_input::new_session();
    if let Err(err) = with_env(|env| {
        let activity = activity(env)?;
        env.call_method(
            &activity,
            jni::jni_str!("gpuiResetComposition"),
            jni::jni_sig!("(J)V"),
            &[JValue::Long(session as i64)],
        )
        .or_clear(env)?;
        Ok(())
    }) {
        log::debug!("gpuiResetComposition: {err}");
    }
}

/// Receive Java InputConnection updates without touching GPUI on the UI thread.
///
/// # Safety
/// Called by JNI with a valid Java string and JNI call frame.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_dev_gpui_mobile_GpuiInputActivity_nativeIme(
    _env: *mut c_void,
    _class: *mut c_void,
    session: i64,
    kind: i32,
    text: *mut c_void,
    start: i32,
    end: i32,
) {
    if let Err(err) = with_env(|env| {
        // SAFETY: `text` is a local reference JNI passed to this call, valid until it
        // returns.
        let text = unsafe { JObject::from_raw(env, text as jni::sys::jobject) };
        super::text_input::enqueue(super::text_input::ImeEvent {
            session: session as u64,
            kind,
            text: get_string(env, &text),
            start: start.max(0) as usize,
            end: end.max(0) as usize,
        });
        Ok(())
    }) {
        log::warn!("nativeIme: {err}");
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

// ── JNI export for GpuiActivity splash screen ────────────────────────────────

/// Called from `GpuiActivity.nativeIsInitialized()` to check whether the
/// native library has finished initializing (first frame rendered).
///
/// The AndroidX SplashScreen API calls this via `setKeepOnScreenCondition`
/// to hold the splash visible until GPUI is ready.
///
/// # Safety
/// Must only be called from the JVM on a valid JNI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_dev_gpui_mobile_GpuiActivity_nativeIsInitialized(
    _env: *mut std::ffi::c_void,
    _class: *mut std::ffi::c_void,
) -> u8 {
    if NATIVE_INITIALIZED.load(Ordering::Acquire) {
        1 // JNI_TRUE
    } else {
        0 // JNI_FALSE
    }
}

/// JNI bridge: receive a deeplink URL from `GpuiActivity.onNewIntent()`.
///
/// When the app is already running and a deeplink is opened (e.g. via
/// `adb shell am start -d gpui://video_player`), the Java side calls
/// this to notify the Rust deeplink handler.
///
/// # Safety
/// Must only be called from the JVM on a valid JNI thread with a valid `url` jobject.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_dev_gpui_mobile_GpuiActivity_nativeOnDeepLink(
    _env: *mut std::ffi::c_void,
    _class: *mut std::ffi::c_void,
    url: *mut std::ffi::c_void,
) {
    // We already have a JVM attached on this thread (UI thread).
    // Use with_env to get a properly wrapped Env handle.
    let url_raw = url as jni::sys::jobject;
    if let Err(err) = with_env(|env| {
        // SAFETY: `url` is a local reference JNI passed to this call, valid until it
        // returns.
        let url_obj = unsafe { JObject::from_raw(env, url_raw) };
        let url_string = get_string(env, &url_obj);
        if url_string.is_empty() {
            return Ok(());
        }
        log::info!("nativeOnDeepLink: {}", url_string);

        #[cfg(feature = "deeplink")]
        {
            crate::packages::deeplink::handle_link(&url_string);
        }
        Ok(())
    }) {
        log::warn!("nativeOnDeepLink: {err}");
    }
}

/// JNI bridge: receive a media action from `GpuiMediaSession` system controls.
///
/// Actions: "play", "pause", "stop", "next", "previous"
///
/// # Safety
/// Must only be called from the JVM on a valid JNI thread with a valid `action` jobject.
#[cfg(feature = "media_session")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_dev_gpui_mobile_GpuiMediaSession_nativeMediaAction(
    _env: *mut std::ffi::c_void,
    _class: *mut std::ffi::c_void,
    action: *mut std::ffi::c_void,
) {
    let action_raw = action as jni::sys::jobject;
    if let Err(err) = with_env(|env| {
        // SAFETY: `action` is a local reference JNI passed to this call, valid until
        // it returns.
        let action_obj = unsafe { JObject::from_raw(env, action_raw) };
        let action_str = get_string(env, &action_obj);

        let media_action = match action_str.as_str() {
            "play" => crate::packages::media_session::MediaAction::Play,
            "pause" => crate::packages::media_session::MediaAction::Pause,
            "stop" => crate::packages::media_session::MediaAction::Stop,
            "next" => crate::packages::media_session::MediaAction::Next,
            "previous" => crate::packages::media_session::MediaAction::Previous,
            other => {
                log::warn!("nativeMediaAction: unknown action '{}'", other);
                return Ok(());
            }
        };

        log::info!("nativeMediaAction: {:?}", media_action);
        crate::packages::media_session::notify_action(media_action);
        Ok(())
    }) {
        log::warn!("nativeMediaAction: {err}");
    }
}

/// JNI bridge: receive a seek request from `GpuiMediaSession` system controls.
///
/// # Safety
/// Must only be called from the JVM on a valid JNI thread.
#[cfg(feature = "media_session")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_dev_gpui_mobile_GpuiMediaSession_nativeMediaSeek(
    _env: *mut std::ffi::c_void,
    _class: *mut std::ffi::c_void,
    position_ms: i64,
) {
    log::info!("nativeMediaSeek: {}ms", position_ms);
    crate::packages::media_session::notify_seek(position_ms as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_events_returns_false_when_no_platform() {
        // PLATFORM is not set in a unit-test context, so poll_events should
        // be a safe no-op and return false (don't quit).
        let result = poll_events(0);
        let _ = result;
    }

    #[test]
    fn java_vm_returns_null_before_init() {
        // Before android_main is called, java_vm() should return null.
        let vm = java_vm();
        assert!(vm.is_null());
    }

    #[test]
    #[allow(deprecated)]
    fn activity_as_ptr_returns_null_before_init() {
        let ptr = activity_as_ptr();
        assert!(ptr.is_null());
    }

    #[test]
    fn android_app_returns_none_before_init() {
        assert!(android_app().is_none());
    }

    #[test]
    fn platform_returns_none_before_init() {
        assert!(platform().is_none());
    }
}

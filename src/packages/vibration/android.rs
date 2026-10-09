use super::HapticFeedback;
use crate::android::jni::{self as jni_helpers, JniExt, JniResultExt as _};
use jni::objects::{JObject, JValue};

pub fn vibrate(duration_ms: u32) -> Result<(), String> {
    jni_helpers::with_env(|env| {
        let context = jni_helpers::application_context(env)?;

        let vibrator = get_vibrator_service(env, &context)?;

        // VibrationEffect.createOneShot (API 26+), or vibrate(long) before it.
        let effect_class = env
            .find_class(jni::jni_str!("android/os/VibrationEffect"))
            .or_catch(env, jni::jni_str!("java/lang/NoClassDefFoundError"))?;
        let Some(effect_class) = effect_class else {
            env.call_method(
                &vibrator,
                jni::jni_str!("vibrate"),
                jni::jni_sig!("(J)V"),
                &[JValue::Long(duration_ms as i64)],
            )
            .or_clear(env)?;
            return Ok(());
        };
        const DEFAULT_AMPLITUDE: i32 = -1;
        let effect = env
            .call_static_method(
                &effect_class,
                jni::jni_str!("createOneShot"),
                jni::jni_sig!("(JI)Landroid/os/VibrationEffect;"),
                &[
                    JValue::Long(duration_ms as i64),
                    JValue::Int(DEFAULT_AMPLITUDE),
                ],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        env.call_method(
            &vibrator,
            jni::jni_str!("vibrate"),
            jni::jni_sig!("(Landroid/os/VibrationEffect;)V"),
            &[JValue::Object(&effect)],
        )
        .or_clear(env)?;
        Ok(())
    })
}

pub fn haptic_feedback(feedback: HapticFeedback) -> Result<(), String> {
    // Map to Android HapticFeedbackConstants
    let constant: i32 = match feedback {
        HapticFeedback::Light => 1,     // VIRTUAL_KEY
        HapticFeedback::Medium => 1,    // VIRTUAL_KEY
        HapticFeedback::Heavy => 0,     // LONG_PRESS
        HapticFeedback::Selection => 3, // KEYBOARD_TAP
        HapticFeedback::Success => 1,   // VIRTUAL_KEY
        HapticFeedback::Warning => 0,   // LONG_PRESS
        HapticFeedback::Error => 0,     // LONG_PRESS
    };

    jni_helpers::with_env(|env| {
        let activity = jni_helpers::activity(env)?;

        // activity.getWindow().getDecorView().performHapticFeedback(constant)
        let window = env
            .call_method(
                &activity,
                jni::jni_str!("getWindow"),
                jni::jni_sig!("()Landroid/view/Window;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        if window.is_null() {
            return Err("getWindow returned null".into());
        }

        let decor = env
            .call_method(
                &window,
                jni::jni_str!("getDecorView"),
                jni::jni_sig!("()Landroid/view/View;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        if decor.is_null() {
            return Err("getDecorView returned null".into());
        }

        env.call_method(
            &decor,
            jni::jni_str!("performHapticFeedback"),
            jni::jni_sig!("(I)Z"),
            &[JValue::Int(constant)],
        )
        .or_clear(env)?;
        Ok(())
    })
}

pub fn can_vibrate() -> bool {
    jni_helpers::with_env(|env| {
        let context = jni_helpers::application_context(env)?;

        let vibrator = get_vibrator_service(env, &context)?;

        let result = env
            .call_method(
                &vibrator,
                jni::jni_str!("hasVibrator"),
                jni::jni_sig!("()Z"),
                &[],
            )
            .and_then(|v| v.z())
            .or_clear(env)?;
        Ok(result)
    })
    .unwrap_or_else(|err| {
        log::warn!("can_vibrate: {err}");
        false
    })
}

fn get_vibrator_service<'local>(
    env: &mut jni::Env<'local>,
    context: &JObject<'_>,
) -> Result<JObject<'local>, String> {
    let service_name = env.new_string("vibrator").e()?;
    let vibrator = env
        .call_method(
            context,
            jni::jni_str!("getSystemService"),
            jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
            &[JValue::Object(&service_name)],
        )
        .and_then(|v| v.l())
        .or_clear(env)?;
    if vibrator.is_null() {
        return Err("Vibrator service not available".into());
    }
    Ok(vibrator)
}

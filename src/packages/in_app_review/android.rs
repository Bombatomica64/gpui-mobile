use crate::android::jni::{self as jni_helpers, get_string, JniExt, JniResultExt as _};
use jni::objects::JValue;

/// Whether the Play Store is installed.
pub fn is_available() -> Result<bool, String> {
    jni_helpers::with_env(|env| {
        let context = jni_helpers::application_context(env)?;
        let pm = env
            .call_method(
                &context,
                jni::jni_str!("getPackageManager"),
                jni::jni_sig!("()Landroid/content/pm/PackageManager;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        let package = env.new_string("com.android.vending").e()?;
        let info = env.call_method(
            &pm,
            jni::jni_str!("getPackageInfo"),
            jni::jni_sig!("(Ljava/lang/String;I)Landroid/content/pm/PackageInfo;"),
            &[JValue::Object(&package), JValue::Int(0)],
        );
        // NameNotFoundException: not installed.
        Ok(info.or_clear(env).is_ok())
    })
}

/// Open this app's Play Store page.
pub fn request_review() -> Result<(), String> {
    let package = jni_helpers::with_env(|env| {
        let context = jni_helpers::application_context(env)?;
        let package = env
            .call_method(
                &context,
                jni::jni_str!("getPackageName"),
                jni::jni_sig!("()Ljava/lang/String;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        Ok(get_string(env, &package))
    })?;
    if open_store_page(&package)? {
        Ok(())
    } else {
        Err("Failed to launch review flow".into())
    }
}

pub fn open_store_listing(app_id: &str) -> Result<(), String> {
    if open_store_page(app_id)? {
        Ok(())
    } else {
        Err("Failed to open store listing".into())
    }
}

/// Open `app_id`'s page in the Play Store app, or on the web without it.
fn open_store_page(app_id: &str) -> Result<bool, String> {
    Ok(view(&format!("market://details?id={app_id}"))?
        || view(&format!(
            "https://play.google.com/store/apps/details?id={app_id}"
        ))?)
}

/// Start an `ACTION_VIEW` intent for `uri` in a new task. `false` if no app takes it.
fn view(uri: &str) -> Result<bool, String> {
    jni_helpers::with_env(|env| {
        let activity = jni_helpers::activity(env)?;
        let action = env.new_string("android.intent.action.VIEW").e()?;
        let uri = env.new_string(uri).e()?;
        let uri = env
            .call_static_method(
                jni::jni_str!("android/net/Uri"),
                jni::jni_str!("parse"),
                jni::jni_sig!("(Ljava/lang/String;)Landroid/net/Uri;"),
                &[JValue::Object(&uri)],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        let intent = env
            .new_object(
                jni::jni_str!("android/content/Intent"),
                jni::jni_sig!("(Ljava/lang/String;Landroid/net/Uri;)V"),
                &[JValue::Object(&action), JValue::Object(&uri)],
            )
            .or_clear(env)?;
        const FLAG_ACTIVITY_NEW_TASK: i32 = 0x1000_0000;
        env.call_method(
            &intent,
            jni::jni_str!("addFlags"),
            jni::jni_sig!("(I)Landroid/content/Intent;"),
            &[JValue::Int(FLAG_ACTIVITY_NEW_TASK)],
        )
        .or_clear(env)?;
        let started = env.call_method(
            &activity,
            jni::jni_str!("startActivity"),
            jni::jni_sig!("(Landroid/content/Intent;)V"),
            &[JValue::Object(&intent)],
        );
        // ActivityNotFoundException: no app for this URI.
        Ok(started.or_clear(env).is_ok())
    })
}

use crate::android::jni::{self as jni_helpers, get_string, JniResultExt as _};
use jni::objects::JObject;
use std::sync::Mutex;

static LATEST_LINK: Mutex<Option<String>> = Mutex::new(None);

/// The link the Activity was launched with: its intent's data, for an
/// `ACTION_VIEW` or `ACTION_MAIN` intent.
pub fn get_initial_link() -> Result<Option<String>, String> {
    jni_helpers::with_env(|env| {
        let activity = jni_helpers::activity(env)?;
        let intent = env
            .call_method(
                &activity,
                jni::jni_str!("getIntent"),
                jni::jni_sig!("()Landroid/content/Intent;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        if intent.is_null() {
            return Ok(None);
        }
        let action = env
            .call_method(
                &intent,
                jni::jni_str!("getAction"),
                jni::jni_sig!("()Ljava/lang/String;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        let action = get_string(env, &action);
        if action != "android.intent.action.VIEW" && action != "android.intent.action.MAIN" {
            return Ok(None);
        }
        let data = env
            .call_method(
                &intent,
                jni::jni_str!("getData"),
                jni::jni_sig!("()Landroid/net/Uri;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        let url = to_string(env, &data)?;
        if url.is_empty() {
            Ok(None)
        } else {
            *LATEST_LINK.lock().unwrap() = Some(url.clone());
            Ok(Some(url))
        }
    })
}

/// `object.toString()`, or an empty string for null.
fn to_string(env: &mut jni::Env<'_>, object: &JObject<'_>) -> Result<String, String> {
    if object.is_null() {
        return Ok(String::new());
    }
    let string = env
        .call_method(
            object,
            jni::jni_str!("toString"),
            jni::jni_sig!("()Ljava/lang/String;"),
            &[],
        )
        .and_then(|v| v.l())
        .or_clear(env)?;
    Ok(get_string(env, &string))
}

pub fn get_latest_link() -> Option<String> {
    LATEST_LINK.lock().unwrap().clone()
}

pub(super) fn set_latest_link(url: &str) {
    *LATEST_LINK.lock().unwrap() = Some(url.to_owned());
}

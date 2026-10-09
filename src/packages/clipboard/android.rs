use crate::android::jni::{self as jni_helpers, get_string, JniExt, JniResultExt as _};
use jni::objects::{JObject, JValue};

/// The `ClipboardManager`, or `None` if the system has none.
fn clipboard_manager<'local>(
    env: &mut jni::Env<'local>,
) -> Result<Option<JObject<'local>>, String> {
    let context = jni_helpers::application_context(env)?;
    let name = env.new_string("clipboard").e()?;
    let manager = env
        .call_method(
            &context,
            jni::jni_str!("getSystemService"),
            jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
            &[JValue::Object(&name)],
        )
        .and_then(|v| v.l())
        .or_clear(env)?;
    Ok((!manager.is_null()).then_some(manager))
}

/// The first item of the primary clip, or `None` if the clipboard is empty.
fn first_clip_item<'local>(
    env: &mut jni::Env<'local>,
    manager: &JObject<'_>,
) -> Result<Option<JObject<'local>>, String> {
    let has_clip = env
        .call_method(
            manager,
            jni::jni_str!("hasPrimaryClip"),
            jni::jni_sig!("()Z"),
            &[],
        )
        .and_then(|v| v.z())
        .or_clear(env)?;
    if !has_clip {
        return Ok(None);
    }
    let clip = env
        .call_method(
            manager,
            jni::jni_str!("getPrimaryClip"),
            jni::jni_sig!("()Landroid/content/ClipData;"),
            &[],
        )
        .and_then(|v| v.l())
        .or_clear(env)?;
    if clip.is_null() {
        return Ok(None);
    }
    let count = env
        .call_method(
            &clip,
            jni::jni_str!("getItemCount"),
            jni::jni_sig!("()I"),
            &[],
        )
        .and_then(|v| v.i())
        .or_clear(env)?;
    if count == 0 {
        return Ok(None);
    }
    let item = env
        .call_method(
            &clip,
            jni::jni_str!("getItemAt"),
            jni::jni_sig!("(I)Landroid/content/ClipData$Item;"),
            &[JValue::Int(0)],
        )
        .and_then(|v| v.l())
        .or_clear(env)?;
    Ok((!item.is_null()).then_some(item))
}

/// The item's text as a `CharSequence`, or null.
fn item_text<'local>(
    env: &mut jni::Env<'local>,
    item: &JObject<'_>,
) -> Result<JObject<'local>, String> {
    env.call_method(
        item,
        jni::jni_str!("getText"),
        jni::jni_sig!("()Ljava/lang/CharSequence;"),
        &[],
    )
    .and_then(|v| v.l())
    .or_clear(env)
}

pub fn set_text(text: &str) -> Result<(), String> {
    let text = text.to_owned();
    jni_helpers::with_env(|env| {
        let Some(manager) = clipboard_manager(env)? else {
            return Ok(());
        };
        let label = env.new_string("text").e()?;
        let j_text = env.new_string(&text).e()?;
        let clip = env
            .call_static_method(
                jni::jni_str!("android/content/ClipData"),
                jni::jni_str!("newPlainText"),
                jni::jni_sig!(
                    "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;"
                ),
                &[JValue::Object(&label), JValue::Object(&j_text)],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        env.call_method(
            &manager,
            jni::jni_str!("setPrimaryClip"),
            jni::jni_sig!("(Landroid/content/ClipData;)V"),
            &[JValue::Object(&clip)],
        )
        .or_clear(env)?;
        Ok(())
    })
}

pub fn get_text() -> Result<Option<String>, String> {
    jni_helpers::with_env(|env| {
        let Some(manager) = clipboard_manager(env)? else {
            return Ok(None);
        };
        let Some(item) = first_clip_item(env, &manager)? else {
            return Ok(None);
        };
        let text = item_text(env, &item)?;
        if text.is_null() {
            return Ok(None);
        }
        let text = env
            .call_method(
                &text,
                jni::jni_str!("toString"),
                jni::jni_sig!("()Ljava/lang/String;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        let text = get_string(env, &text);
        Ok((!text.is_empty()).then_some(text))
    })
}

pub fn has_text() -> Result<bool, String> {
    jni_helpers::with_env(|env| {
        let Some(manager) = clipboard_manager(env)? else {
            return Ok(false);
        };
        let Some(item) = first_clip_item(env, &manager)? else {
            return Ok(false);
        };
        Ok(!item_text(env, &item)?.is_null())
    })
}

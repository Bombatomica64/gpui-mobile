use crate::android::jni::{self as jni_helpers, get_string, JniExt, JniResultExt as _};
use jni::objects::{JObject, JValue};

pub struct AndroidSharedPreferences;

impl AndroidSharedPreferences {
    pub fn new() -> Self {
        Self
    }

    pub fn get_string(&self, key: &str) -> Option<String> {
        let key = key.to_owned();
        jni_helpers::with_env(|env| {
            let prefs = get_default_prefs(env)?;

            let jkey = env.new_string(&key).e()?;
            let result = env
                .call_method(
                    &prefs,
                    jni::jni_str!("getString"),
                    jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                    &[JValue::Object(&jkey), JValue::Object(&JObject::null())],
                )
                .and_then(|v| v.l())
                .or_clear(env)?;

            if result.is_null() {
                Ok(None)
            } else {
                Ok(Some(get_string(env, &result)))
            }
        })
        .unwrap_or_else(|err| {
            log::warn!("shared_preferences: get_string: {err}");
            None
        })
    }

    pub fn set_string(&self, key: &str, value: &str) -> Result<(), String> {
        with_editor(|env, editor| {
            let jkey = env.new_string(key).e()?;
            let jval = env.new_string(value).e()?;
            let put = env.call_method(
                editor,
                jni::jni_str!("putString"),
                jni::jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/SharedPreferences$Editor;"),
                &[JValue::Object(&jkey), JValue::Object(&jval)],
            );
            put.or_clear(env)?;
            Ok(())
        })
    }

    pub fn get_int(&self, key: &str) -> Option<i64> {
        let key = key.to_owned();
        jni_helpers::with_env(|env| {
            let prefs = get_default_prefs(env)?;

            if !contains_key_jni(env, &prefs, &key)? {
                return Ok(None);
            }
            let jkey = env.new_string(&key).e()?;
            let val = env
                .call_method(
                    &prefs,
                    jni::jni_str!("getLong"),
                    jni::jni_sig!("(Ljava/lang/String;J)J"),
                    &[JValue::Object(&jkey), JValue::Long(0)],
                )
                .and_then(|v| v.j())
                .or_clear(env)?;
            Ok(Some(val))
        })
        .unwrap_or_else(|err| {
            log::warn!("shared_preferences: get_int: {err}");
            None
        })
    }

    pub fn set_int(&self, key: &str, value: i64) -> Result<(), String> {
        with_editor(|env, editor| {
            let jkey = env.new_string(key).e()?;
            env.call_method(
                editor,
                jni::jni_str!("putLong"),
                jni::jni_sig!("(Ljava/lang/String;J)Landroid/content/SharedPreferences$Editor;"),
                &[JValue::Object(&jkey), JValue::Long(value)],
            )
            .or_clear(env)?;
            Ok(())
        })
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        let key = key.to_owned();
        jni_helpers::with_env(|env| {
            let prefs = get_default_prefs(env)?;

            if !contains_key_jni(env, &prefs, &key)? {
                return Ok(None);
            }
            let jkey = env.new_string(&key).e()?;
            let val = env
                .call_method(
                    &prefs,
                    jni::jni_str!("getBoolean"),
                    jni::jni_sig!("(Ljava/lang/String;Z)Z"),
                    &[JValue::Object(&jkey), JValue::Bool(false)],
                )
                .and_then(|v| v.z())
                .or_clear(env)?;
            Ok(Some(val))
        })
        .unwrap_or_else(|err| {
            log::warn!("shared_preferences: get_bool: {err}");
            None
        })
    }

    pub fn set_bool(&self, key: &str, value: bool) -> Result<(), String> {
        with_editor(|env, editor| {
            let jkey = env.new_string(key).e()?;
            env.call_method(
                editor,
                jni::jni_str!("putBoolean"),
                jni::jni_sig!("(Ljava/lang/String;Z)Landroid/content/SharedPreferences$Editor;"),
                &[JValue::Object(&jkey), JValue::Bool(value)],
            )
            .or_clear(env)?;
            Ok(())
        })
    }

    pub fn remove(&self, key: &str) -> Result<(), String> {
        with_editor(|env, editor| {
            let jkey = env.new_string(key).e()?;
            env.call_method(
                editor,
                jni::jni_str!("remove"),
                jni::jni_sig!("(Ljava/lang/String;)Landroid/content/SharedPreferences$Editor;"),
                &[JValue::Object(&jkey)],
            )
            .or_clear(env)?;
            Ok(())
        })
    }

    pub fn clear(&self) -> Result<(), String> {
        with_editor(|env, editor| {
            env.call_method(
                editor,
                jni::jni_str!("clear"),
                jni::jni_sig!("()Landroid/content/SharedPreferences$Editor;"),
                &[],
            )
            .or_clear(env)?;
            Ok(())
        })
    }

    pub fn contains_key(&self, key: &str) -> bool {
        let key = key.to_owned();
        jni_helpers::with_env(|env| {
            let prefs = get_default_prefs(env)?;
            contains_key_jni(env, &prefs, &key)
        })
        .unwrap_or_else(|err| {
            log::warn!("shared_preferences: contains_key: {err}");
            false
        })
    }
}

fn contains_key_jni(
    env: &mut jni::Env<'_>,
    prefs: &JObject<'_>,
    key: &str,
) -> Result<bool, String> {
    let jkey = env.new_string(key).e()?;
    env.call_method(
        prefs,
        jni::jni_str!("contains"),
        jni::jni_sig!("(Ljava/lang/String;)Z"),
        &[JValue::Object(&jkey)],
    )
    .and_then(|v| v.z())
    .or_clear(env)
}

/// Get default SharedPreferences via PreferenceManager.
fn get_default_prefs<'local>(env: &mut jni::Env<'local>) -> Result<JObject<'local>, String> {
    let context = jni_helpers::application_context(env)?;
    let prefs = env
        .call_static_method(
            jni::jni_str!("android/preference/PreferenceManager"),
            jni::jni_str!("getDefaultSharedPreferences"),
            jni::jni_sig!("(Landroid/content/Context;)Landroid/content/SharedPreferences;"),
            &[JValue::Object(&context)],
        )
        .and_then(|v| v.l())
        .or_clear(env)?;
    if prefs.is_null() {
        return Err("getDefaultSharedPreferences returned null".into());
    }
    Ok(prefs)
}

/// Get an editor, run the callback, then commit.
fn with_editor(
    f: impl FnOnce(&mut jni::Env<'_>, &JObject<'_>) -> Result<(), String>,
) -> Result<(), String> {
    jni_helpers::with_env(|env| {
        let prefs = get_default_prefs(env)?;

        let editor = env
            .call_method(
                &prefs,
                jni::jni_str!("edit"),
                jni::jni_sig!("()Landroid/content/SharedPreferences$Editor;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        if editor.is_null() {
            return Err("edit() returned null".into());
        }

        f(env, &editor)?;

        // Commit
        let committed = env
            .call_method(&editor, jni::jni_str!("commit"), jni::jni_sig!("()Z"), &[])
            .and_then(|v| v.z())
            .or_clear(env)?;
        if !committed {
            return Err("SharedPreferences.Editor.commit() failed".into());
        }
        Ok(())
    })
}

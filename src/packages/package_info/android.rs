use super::PackageInfo;
use crate::android::jni::{self as jni_helpers, get_string, JniExt, JniResultExt as _};
use jni::objects::JValue;

pub fn get_package_info() -> Result<PackageInfo, String> {
    jni_helpers::with_env(|env| {
        let context = jni_helpers::application_context(env)?;

        // context.getPackageName() → String
        let pkg_name_obj = env
            .call_method(
                &context,
                jni::jni_str!("getPackageName"),
                jni::jni_sig!("()Ljava/lang/String;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        let package_name = get_string(env, &pkg_name_obj);

        // context.getPackageManager() → PackageManager
        let pm = env
            .call_method(
                &context,
                jni::jni_str!("getPackageManager"),
                jni::jni_sig!("()Landroid/content/pm/PackageManager;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        if pm.is_null() {
            return Err("getPackageManager returned null".into());
        }

        // pm.getPackageInfo(packageName, 0) → android.content.pm.PackageInfo
        let jpkg = env.new_string(&package_name).e()?;
        let pkg_info = env
            .call_method(
                &pm,
                jni::jni_str!("getPackageInfo"),
                jni::jni_sig!("(Ljava/lang/String;I)Landroid/content/pm/PackageInfo;"),
                &[JValue::Object(&jpkg), JValue::Int(0)],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        if pkg_info.is_null() {
            return Err("getPackageInfo returned null".into());
        }

        // versionName: String
        let version = env
            .get_field(
                &pkg_info,
                jni::jni_str!("versionName"),
                jni::jni_sig!("Ljava/lang/String;"),
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        let version = get_string(env, &version);

        // versionCode: int
        let build_number = env
            .get_field(&pkg_info, jni::jni_str!("versionCode"), jni::jni_sig!("I"))
            .and_then(|v| v.i())
            .or_clear(env)?
            .to_string();

        // applicationInfo → getApplicationLabel
        let app_name = (|| -> Result<String, String> {
            let app_info = env
                .get_field(
                    &pkg_info,
                    jni::jni_str!("applicationInfo"),
                    jni::jni_sig!("Landroid/content/pm/ApplicationInfo;"),
                )
                .and_then(|v| v.l())
                .or_clear(env)?;
            if app_info.is_null() {
                return Ok(String::new());
            }
            let cs = env
                .call_method(
                    &pm,
                    jni::jni_str!("getApplicationLabel"),
                    jni::jni_sig!("(Landroid/content/pm/ApplicationInfo;)Ljava/lang/CharSequence;"),
                    &[JValue::Object(&app_info)],
                )
                .and_then(|v| v.l())
                .or_clear(env)?;
            if cs.is_null() {
                return Ok(String::new());
            }
            let label = env
                .call_method(
                    &cs,
                    jni::jni_str!("toString"),
                    jni::jni_sig!("()Ljava/lang/String;"),
                    &[],
                )
                .and_then(|v| v.l())
                .or_clear(env)?;
            Ok(get_string(env, &label))
        })()?;

        Ok(PackageInfo {
            app_name,
            package_name,
            version,
            build_number,
        })
    })
}

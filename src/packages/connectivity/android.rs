use super::ConnectivityStatus;
use crate::android::jni::{self as jni_helpers, JniExt, JniResultExt as _};
use jni::objects::JValue;

pub fn check_connectivity() -> ConnectivityStatus {
    jni_helpers::with_env(|env| {
        let context = jni_helpers::application_context(env)?;

        // context.getSystemService("connectivity") → ConnectivityManager
        let service_name = env.new_string("connectivity").e()?;
        let cm = env
            .call_method(
                &context,
                jni::jni_str!("getSystemService"),
                jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                &[JValue::Object(&service_name)],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        if cm.is_null() {
            return Err("no ConnectivityManager".into());
        }

        // cm.getActiveNetworkInfo() → NetworkInfo, null when offline
        let net_info = env
            .call_method(
                &cm,
                jni::jni_str!("getActiveNetworkInfo"),
                jni::jni_sig!("()Landroid/net/NetworkInfo;"),
                &[],
            )
            .and_then(|v| v.l())
            .or_clear(env)?;
        if net_info.is_null() {
            return Ok(ConnectivityStatus::None);
        }

        // networkInfo.isConnected()
        let connected = env
            .call_method(
                &net_info,
                jni::jni_str!("isConnected"),
                jni::jni_sig!("()Z"),
                &[],
            )
            .and_then(|v| v.z())
            .or_clear(env)?;
        if !connected {
            return Ok(ConnectivityStatus::None);
        }

        // networkInfo.getType()
        let network_type = env
            .call_method(
                &net_info,
                jni::jni_str!("getType"),
                jni::jni_sig!("()I"),
                &[],
            )
            .and_then(|v| v.i())
            .or_clear(env)?;
        Ok(match network_type {
            1 => ConnectivityStatus::Wifi,     // TYPE_WIFI
            0 => ConnectivityStatus::Cellular, // TYPE_MOBILE
            _ => ConnectivityStatus::Wifi,     // Ethernet etc. treated as Wifi
        })
    })
    .unwrap_or_else(|err| {
        log::warn!("check_connectivity: {err}");
        ConnectivityStatus::None
    })
}

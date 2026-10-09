use super::{BarometerData, SensorAvailability, SensorData};
use crate::android::jni::{self as jni_helpers, JniExt, JniResultExt as _};
use jni::objects::JValue;

// Android Sensor.TYPE_* constants
const TYPE_ACCELEROMETER: i32 = 1;
const TYPE_GYROSCOPE: i32 = 4;
const TYPE_MAGNETIC_FIELD: i32 = 2;
const TYPE_PRESSURE: i32 = 6;

pub fn available_sensors() -> SensorAvailability {
    jni_helpers::with_env(|env| {
        let context = jni_helpers::application_context(env)?;
        let Some(sm) = get_sensor_manager(env, &context)? else {
            return Ok(SensorAvailability::default());
        };
        Ok(SensorAvailability {
            accelerometer: has_sensor(env, &sm, TYPE_ACCELEROMETER)?,
            gyroscope: has_sensor(env, &sm, TYPE_GYROSCOPE)?,
            magnetometer: has_sensor(env, &sm, TYPE_MAGNETIC_FIELD)?,
            barometer: has_sensor(env, &sm, TYPE_PRESSURE)?,
        })
    })
    .unwrap_or_else(|err| {
        log::warn!("available_sensors: {err}");
        SensorAvailability::default()
    })
}

pub fn accelerometer() -> Option<SensorData> {
    // Single-shot sensor reads require registering a SensorEventListener via JNI,
    // which involves creating a Java callback object. This is complex with pure JNI.
    // For now, sensor data is available through the availability check.
    // A full implementation would use android.hardware.SensorManager with a listener
    // or the NDK ASensorManager API with an ALooper.
    None
}

pub fn gyroscope() -> Option<SensorData> {
    None
}

pub fn magnetometer() -> Option<SensorData> {
    None
}

pub fn barometer() -> Option<BarometerData> {
    None
}

/// The `SensorManager`, or `None` if the device has none.
fn get_sensor_manager<'local>(
    env: &mut jni::Env<'local>,
    context: &jni::objects::JObject<'_>,
) -> Result<Option<jni::objects::JObject<'local>>, String> {
    let service_name = env.new_string("sensor").e()?;
    let sm = env
        .call_method(
            context,
            jni::jni_str!("getSystemService"),
            jni::jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
            &[JValue::Object(&service_name)],
        )
        .and_then(|v| v.l())
        .or_clear(env)?;
    Ok((!sm.is_null()).then_some(sm))
}

fn has_sensor(
    env: &mut jni::Env<'_>,
    sm: &jni::objects::JObject<'_>,
    sensor_type: i32,
) -> Result<bool, String> {
    let sensor = env
        .call_method(
            sm,
            jni::jni_str!("getDefaultSensor"),
            jni::jni_sig!("(I)Landroid/hardware/Sensor;"),
            &[JValue::Int(sensor_type)],
        )
        .and_then(|v| v.l())
        .or_clear(env)?;
    Ok(!sensor.is_null())
}

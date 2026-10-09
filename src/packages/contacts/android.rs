use super::{Contact, EmailAddress, PhoneNumber};
use crate::android::jni::{self as jni_helpers, JniExt, JniResultExt as _};
use jni::objects::JValue;

const HELPER_CLASS: &str = "dev.gpui.mobile.GpuiContacts";

pub fn get_contacts() -> Result<Vec<Contact>, String> {
    query(None)
}

pub fn search_contacts(query_text: &str) -> Result<Vec<Contact>, String> {
    query(Some(("searchContacts", query_text)))
}

pub fn get_contact(id: &str) -> Result<Option<Contact>, String> {
    Ok(query(Some(("getContact", id)))?.into_iter().next())
}

/// `getContacts()`, or the given method with its one string argument.
fn query(method: Option<(&str, &str)>) -> Result<Vec<Contact>, String> {
    jni_helpers::with_env(|env| {
        let activity = jni_helpers::activity(env)?;
        let cls = jni_helpers::find_app_class(env, HELPER_CLASS)?;

        let result = match method {
            None => env.call_static_method(
                &cls,
                jni::jni_str!("getContacts"),
                jni::jni_sig!("(Landroid/app/Activity;)[Ljava/lang/String;"),
                &[JValue::Object(&activity)],
            ),
            Some((name, argument)) => {
                let argument = env.new_string(argument).e()?;
                let name = jni::strings::JNIString::from(name);
                env.call_static_method(
                    &cls,
                    &name,
                    jni::jni_sig!("(Landroid/app/Activity;Ljava/lang/String;)[Ljava/lang/String;"),
                    &[JValue::Object(&activity), JValue::Object(&argument)],
                )
            }
        }
        .and_then(|v| v.l())
        .or_clear(env)?;

        let fields = jni_helpers::get_string_array(env, &result)?;
        parse_contacts(fields)
    })
}

/// Parse `GpuiContacts`' flat list: per contact, id, display name, given name, family
/// name, then a count of phones followed by number and label of each, then the same
/// for emails.
fn parse_contacts(fields: Vec<String>) -> Result<Vec<Contact>, String> {
    let mut fields = fields.into_iter();
    let mut contacts = Vec::new();
    while let Some(id) = fields.next() {
        let display_name = take(&mut fields)?;
        let given_name = take(&mut fields)?;
        let family_name = take(&mut fields)?;
        let phones = take_pairs(&mut fields)?
            .into_iter()
            .map(|(number, label)| PhoneNumber { number, label })
            .collect();
        let emails = take_pairs(&mut fields)?
            .into_iter()
            .map(|(address, label)| EmailAddress { address, label })
            .collect();
        contacts.push(Contact {
            id,
            display_name,
            given_name,
            family_name,
            phones,
            emails,
        });
    }
    Ok(contacts)
}

fn take(fields: &mut impl Iterator<Item = String>) -> Result<String, String> {
    fields
        .next()
        .ok_or_else(|| "GpuiContacts: truncated contact list".to_string())
}

/// A count, then that many (value, label) pairs.
fn take_pairs(fields: &mut impl Iterator<Item = String>) -> Result<Vec<(String, String)>, String> {
    let count: usize = take(fields)?
        .parse()
        .map_err(|_| "GpuiContacts: bad count".to_string())?;
    (0..count)
        .map(|_| Ok((take(fields)?, take(fields)?)))
        .collect()
}

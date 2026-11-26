//! Android Autofill for GPUI text fields.
//!
//! GPUI draws every field itself, so password managers see one opaque `SurfaceView`. A
//! host that uses `GpuiAutofill` exposes the fields as virtual children of its IME proxy
//! (the view the keyboard is connected to, which Android requires for a fill request to
//! reach the service), and this module feeds it.
//!
//! The field list comes from GPUI's AccessKit tree, which already has each field's
//! role, label, bounds and focus. GPUI builds that tree only when something asks for
//! it, so while a soft keyboard is up the tree is requested the way a screen reader
//! would ([`begin`]/[`end`]). Fields are filled with the same `SetValue` action a
//! screen reader uses, so any input that handles it (Kit's do) works.
//!
//! Nothing is sent unless a field on screen looks like a login or contact field, so
//! the text of an ordinary form never reaches the autofill service. The tree carries no
//! value for a masked field, so while one has focus its text is read from the window's
//! input handler ([`needs_focused_text`]/[`focused_text`]): without it a password
//! manager has nothing to save. Values are never logged.
//!
//! When the keyboard goes away after a password was typed, the host commits the
//! autofill session, which is when Android offers to save the login.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

use gpui::accesskit::{Node, NodeId, Role, TreeUpdate};
use jni::objects::{JObject, JValue};

use super::jni::{JniExt, JniResultExt, activity, with_env};

const HINT_PASSWORD: &str = "password";
const HINT_USERNAME: &str = "username";
const HINT_EMAIL: &str = "emailAddress";
const HINT_PHONE: &str = "phone";
/// `androidx.autofill.HintConstants.AUTOFILL_HINT_SMS_OTP`; the platform has no constant.
const HINT_SMS_OTP: &str = "smsOTPCode";

#[derive(Debug, Clone, PartialEq)]
struct Field {
    node: NodeId,
    /// Left, top, right, bottom in surface pixels.
    bounds: [i32; 4],
    hint: &'static str,
    label: String,
    value: Option<String>,
    focused: bool,
    /// The tree hides this field's value (a password); see [`focused_text`].
    masked: bool,
}

#[derive(Default)]
struct State {
    wanted: bool,
    /// Virtual view ids handed to Android, which keeps them for a whole fill session.
    ids: HashMap<NodeId, i32>,
    nodes: HashMap<i32, NodeId>,
    /// What the host last received, to send only changes.
    sent: Vec<(i32, Field)>,
    /// Text typed into masked fields during this keyboard session.
    typed: HashMap<NodeId, String>,
}

impl State {
    fn virtual_id(&mut self, node: NodeId) -> i32 {
        let next = self.ids.len() as i32 + 1;
        let id = *self.ids.entry(node).or_insert(next);
        self.nodes.insert(id, node);
        id
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

/// A keyboard came up: keep the tree coming until [`end`].
pub(super) fn begin() {
    if !host_supports_autofill() {
        return;
    }
    let mut state = STATE.lock().expect("poisoned");
    if state.wanted {
        return;
    }
    state.wanted = true;
    state.sent.clear();
    drop(state);
    super::accessibility::request_tree();
}

/// The keyboard went away: tell the host and stop building the tree.
pub(super) fn end() {
    let mut state = STATE.lock().expect("poisoned");
    if !state.wanted {
        return;
    }
    state.wanted = false;
    state.sent.clear();
    state.typed.clear();
    drop(state);
    if let Err(err) = send(&[]) {
        log::debug!("autofill: {err}");
    }
    super::accessibility::release_tree();
}

pub(super) fn wanted() -> bool {
    STATE.lock().expect("poisoned").wanted
}

/// A new frame's tree, on the GPUI thread.
pub(super) fn tree_updated(tree: &TreeUpdate) {
    let mut state = STATE.lock().expect("poisoned");
    if !state.wanted {
        return;
    }
    let mut fields = fields_in(tree);
    with_typed_text(&mut fields, &state.typed);
    let fields: Vec<(i32, Field)> = fields
        .into_iter()
        .map(|field| (state.virtual_id(field.node), field))
        .collect();
    if fields == state.sent {
        return;
    }
    state.sent = fields.clone();
    drop(state);
    if let Err(err) = send(&fields) {
        log::debug!("autofill: {err}");
    }
}

/// Whether the focused field is masked, so its text must come from [`focused_text`].
pub(super) fn needs_focused_text() -> bool {
    let state = STATE.lock().expect("poisoned");
    state.wanted
        && state
            .sent
            .iter()
            .any(|(_, field)| field.focused && field.masked)
}

/// The text of the focused input, from the window's input handler, on the GPUI thread
/// after a frame. Kept for the focused masked field only.
pub(super) fn focused_text(text: Option<String>) {
    let mut state = STATE.lock().expect("poisoned");
    if !state.wanted {
        return;
    }
    let text = text.unwrap_or_default();
    let Some(index) = state
        .sent
        .iter()
        .position(|(_, field)| field.focused && field.masked)
    else {
        return;
    };
    let node = state.sent[index].1.node;
    if state.typed.get(&node) == Some(&text) {
        return;
    }
    state.typed.insert(node, text.clone());
    state.sent[index].1.value = Some(text);
    let fields = state.sent.clone();
    drop(state);
    if let Err(err) = send(&fields) {
        log::debug!("autofill: {err}");
    }
}

/// Give masked fields the text typed into them; the tree has none.
fn with_typed_text(fields: &mut [Field], typed: &HashMap<NodeId, String>) {
    for field in fields.iter_mut().filter(|field| field.masked) {
        field.value = typed.get(&field.node).cloned();
    }
}

/// The fields worth offering, in reading order: none if nothing looks fillable.
fn fields_in(tree: &TreeUpdate) -> Vec<Field> {
    let mut fields: Vec<(Field, Role)> = tree
        .nodes
        .iter()
        .filter(|(_, node)| is_fillable(node))
        .filter_map(|(id, node)| {
            let rect = node.bounds()?;
            let bounds = [
                rect.x0.round() as i32,
                rect.y0.round() as i32,
                rect.x1.round() as i32,
                rect.y1.round() as i32,
            ];
            if bounds[2] <= bounds[0] || bounds[3] <= bounds[1] {
                return None;
            }
            let label = node
                .label()
                .or(node.placeholder())
                .unwrap_or_default()
                .to_string();
            let hint = declared_hint(node.role(), node.author_id().unwrap_or_default(), &label);
            let masked = node.role() == Role::PasswordInput;
            let value = node.value().map(str::to_string).filter(|_| !masked);
            let field = Field {
                node: *id,
                bounds,
                hint,
                label,
                value,
                focused: *id == tree.focus,
                masked,
            };
            Some((field, node.role()))
        })
        .collect();
    fields.sort_by_key(|(field, _)| (field.bounds[1], field.bounds[0]));

    // The plain field right before a password is its username, as browsers assume.
    for index in 1..fields.len() {
        let (before, after) = fields.split_at_mut(index);
        let (previous, role) = &mut before[index - 1];
        if after[0].0.hint == HINT_PASSWORD && previous.hint.is_empty() && *role == Role::TextInput
        {
            previous.hint = HINT_USERNAME;
        }
    }

    let fields: Vec<Field> = fields.into_iter().map(|(field, _)| field).collect();
    if fields.iter().any(|field| !field.hint.is_empty()) {
        fields
    } else {
        Vec::new()
    }
}

fn is_fillable(node: &Node) -> bool {
    matches!(
        node.role(),
        Role::TextInput
            | Role::PasswordInput
            | Role::EmailInput
            | Role::PhoneNumberInput
            | Role::UrlInput
            | Role::NumberInput
    ) && !node.is_disabled()
        && !node.is_read_only()
}

/// The Android autofill hint of a field, or `""`.
///
/// The role is what the field declares. A role cannot say "username" or "one-time
/// code", so those come from `Input::accessibility_id`, then from the visible label.
fn declared_hint(role: Role, author_id: &str, label: &str) -> &'static str {
    match role {
        Role::PasswordInput => return HINT_PASSWORD,
        Role::EmailInput => return HINT_EMAIL,
        Role::PhoneNumberInput => return HINT_PHONE,
        _ => {}
    }
    let hint = hint_from_words(author_id);
    if hint.is_empty() {
        hint_from_words(label)
    } else {
        hint
    }
}

fn hint_from_words(text: &str) -> &'static str {
    let text = text.to_lowercase().replace(['-', '_'], " ");
    let has = |words: &[&str]| words.iter().any(|word| text.contains(word));
    if has(&["one time", "otp", "verification code", "sms code", "2fa"]) {
        HINT_SMS_OTP
    } else if has(&["password", "passcode"]) {
        HINT_PASSWORD
    } else if has(&["email", "e mail"]) {
        HINT_EMAIL
    } else if has(&["username", "user name", "login"]) {
        HINT_USERNAME
    } else if has(&["phone"]) {
        HINT_PHONE
    } else {
        ""
    }
}

fn host_supports_autofill() -> bool {
    with_env(|env| {
        let activity = activity(env)?;
        let class = env.get_object_class(&activity).e()?;
        Ok(env
            .get_method_id(
                &class,
                jni::jni_str!("gpuiAutofillFields"),
                jni::jni_sig!("([I[Ljava/lang/String;I)V"),
            )
            .or_clear(env)
            .is_ok())
    })
    .unwrap_or(false)
}

/// Hand the fields to the host: five ints and three strings per field, flattened, and
/// the focused field's virtual id (0 for none).
fn send(fields: &[(i32, Field)]) -> Result<(), String> {
    with_env(|env| {
        let mut ints = Vec::with_capacity(fields.len() * 5);
        for (id, field) in fields {
            ints.push(*id);
            ints.extend(field.bounds);
        }
        let int_array = env.new_int_array(ints.len()).e()?;
        int_array.set_region(env, 0, &ints).e()?;

        let string_array = env
            .new_object_array(
                (fields.len() * 3) as i32,
                jni::jni_str!("java/lang/String"),
                JObject::null(),
            )
            .e()?;
        for (index, (_, field)) in fields.iter().enumerate() {
            let hint = env.new_string(field.hint).e()?;
            string_array.set_element(env, index * 3, &hint).e()?;
            let label = env.new_string(&field.label).e()?;
            string_array.set_element(env, index * 3 + 1, &label).e()?;
            if let Some(value) = &field.value {
                let value = env.new_string(value).e()?;
                string_array.set_element(env, index * 3 + 2, &value).e()?;
            }
        }

        let focused = fields
            .iter()
            .find(|(_, field)| field.focused)
            .map_or(0, |(id, _)| *id);
        let activity = activity(env)?;
        env.call_method(
            &activity,
            jni::jni_str!("gpuiAutofillFields"),
            jni::jni_sig!("([I[Ljava/lang/String;I)V"),
            &[
                JValue::Object(int_array.as_ref()),
                JValue::Object(string_array.as_ref()),
                JValue::Int(focused),
            ],
        )
        .or_clear(env)?;
        Ok(())
    })
}

/// `autofill(SparseArray)` on the host view, called on the Android UI thread: `ids` and
/// `values` are the chosen dataset, in matching order.
///
/// # Safety
/// Called by JNI with valid array references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn Java_dev_gpui_mobile_GpuiAutofill_nativeFill(
    _env: *mut std::ffi::c_void,
    _class: *mut std::ffi::c_void,
    ids: *mut std::ffi::c_void,
    values: *mut std::ffi::c_void,
) {
    let result = with_env(|env| {
        // SAFETY: local references JNI passed to this call, valid until it returns.
        let ids = unsafe { jni::objects::JIntArray::from_raw(env, ids as jni::sys::jintArray) };
        let values = unsafe { JObject::from_raw(env, values as jni::sys::jobject) };
        let values = super::jni::get_string_array(env, &values)?;
        let mut buffer = vec![0; values.len()];
        ids.get_region(env, 0, &mut buffer).e()?;
        Ok(buffer.into_iter().zip(values).collect::<Vec<_>>())
    });
    match result {
        Ok(fills) => {
            for (id, value) in fills {
                let node = STATE.lock().expect("poisoned").nodes.get(&id).copied();
                if let Some(node) = node {
                    super::accessibility::set_value(node, value);
                }
            }
        }
        Err(err) => log::warn!("autofill: cannot read the dataset: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::accesskit::{Rect, Tree, TreeId};

    fn input(role: Role, y: f64, placeholder: &str) -> Node {
        let mut node = Node::new(role);
        node.set_bounds(Rect::new(0.0, y, 100.0, y + 40.0));
        node.set_placeholder(placeholder);
        node
    }

    fn tree(nodes: Vec<(u64, Node)>, focus: u64) -> TreeUpdate {
        TreeUpdate {
            nodes: nodes
                .into_iter()
                .map(|(id, node)| (NodeId(id), node))
                .collect(),
            tree: None::<Tree>,
            tree_id: TreeId::ROOT,
            focus: NodeId(focus),
        }
    }

    #[test]
    fn a_login_form_gets_hints_in_reading_order() {
        let fields = fields_in(&tree(
            vec![
                (2, input(Role::PasswordInput, 100.0, "Secret")),
                (1, input(Role::TextInput, 50.0, "Who are you")),
            ],
            2,
        ));
        assert_eq!(fields[0].node, NodeId(1));
        assert_eq!(fields[0].hint, HINT_USERNAME);
        assert_eq!(fields[1].hint, HINT_PASSWORD);
        assert!(fields[1].focused);
    }

    #[test]
    fn labels_name_one_time_codes_and_emails() {
        assert_eq!(hint_from_words("One-time code"), HINT_SMS_OTP);
        assert_eq!(hint_from_words("login.otp"), HINT_SMS_OTP);
        assert_eq!(hint_from_words("Your e-mail"), HINT_EMAIL);
        assert_eq!(hint_from_words("Postal code"), "");
    }

    #[test]
    fn an_author_id_overrides_the_label() {
        assert_eq!(
            declared_hint(Role::TextInput, "login.username", "Pick something"),
            HINT_USERNAME
        );
    }

    #[test]
    fn a_form_with_no_recognisable_field_stays_private() {
        let fields = fields_in(&tree(vec![(1, input(Role::TextInput, 0.0, "Message"))], 1));
        assert!(fields.is_empty());
    }

    #[test]
    fn disabled_read_only_and_collapsed_fields_are_skipped() {
        let mut disabled = input(Role::PasswordInput, 0.0, "");
        disabled.set_disabled();
        let mut read_only = input(Role::PasswordInput, 0.0, "");
        read_only.set_read_only();
        let mut collapsed = input(Role::PasswordInput, 0.0, "");
        collapsed.set_bounds(Rect::new(0.0, 0.0, 0.0, 0.0));
        let fields = fields_in(&tree(
            vec![(1, disabled), (2, read_only), (3, collapsed)],
            1,
        ));
        assert!(fields.is_empty());
    }

    #[test]
    fn masked_fields_take_the_typed_text() {
        let mut fields = fields_in(&tree(
            vec![
                (1, input(Role::TextInput, 0.0, "Username")),
                (2, input(Role::PasswordInput, 50.0, "Password")),
            ],
            2,
        ));
        assert!(fields[1].masked && fields[1].value.is_none());
        let typed = HashMap::from([(NodeId(2), "hunter2".to_string()), (NodeId(1), "x".into())]);
        with_typed_text(&mut fields, &typed);
        assert_eq!(fields[1].value.as_deref(), Some("hunter2"));
        // Only masked fields: a plain field keeps what the tree says.
        assert_eq!(fields[0].value, None);
    }

    #[test]
    fn virtual_ids_stay_stable() {
        let mut state = State::default();
        let first = state.virtual_id(NodeId(9));
        state.virtual_id(NodeId(10));
        assert_eq!(state.virtual_id(NodeId(9)), first);
        assert_eq!(state.nodes[&first], NodeId(9));
    }
}

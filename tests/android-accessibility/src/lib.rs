extern crate self as gpui;

pub mod accesskit {
    pub use ::accesskit::*;
}
#[path = "../../../src/android/accesskit_adapter.rs"]
#[allow(dead_code)]
mod accesskit_adapter;
use accesskit_adapter::{get_accessibility_delegate, raise_events};

use accesskit::{ActivationHandler, Node, NodeId, Role, Tree, TreeId, TreeUpdate};
use accesskit_android::{
    jni::{
        objects::{JClass, JObject, JValue},
        JNIEnv,
    },
    Adapter,
};

struct Activate;
impl ActivationHandler for Activate {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        None
    }
}

// Runs on the Android UI thread with accessibility disabled. Creating an active
// tree queues a real window-content-changed event, reproducing delivery of an
// update queued before TalkBack was turned off, independently of GPUI's precheck.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_gpui_a11ytest_MainActivity_test(
    mut env: JNIEnv,
    _class: JClass,
    view: JObject,
    expected_delegate: JObject,
) {
    std::panic::set_hook(Box::new(|info| {
        let message = std::ffi::CString::new(info.to_string()).unwrap();
        unsafe {
            android_log_write(6, c"GpuiA11yRegression".as_ptr(), message.as_ptr());
        }
    }));
    let delegate = get_accessibility_delegate(&mut env, &view).unwrap();
    assert!(env.is_same_object(&delegate, &expected_delegate).unwrap());
    // Exercise pre-29 dispatch on the installed runtime too. Restore SDK_INT
    // before any Android framework code runs; actual API 26/28 runs additionally
    // verify the OS's method availability and hidden-API enforcement.
    let version = env.find_class("android/os/Build$VERSION").unwrap();
    let sdk_field = env.get_static_field_id(&version, "SDK_INT", "I").unwrap();
    let sdk = env
        .get_static_field(&version, "SDK_INT", "I")
        .unwrap()
        .i()
        .unwrap();
    for legacy_sdk in [26, 28] {
        env.set_static_field(&version, sdk_field, JValue::Int(legacy_sdk))
            .unwrap();
        let result = get_accessibility_delegate(&mut env, &view);
        env.set_static_field(&version, sdk_field, JValue::Int(sdk))
            .unwrap();
        let delegate = result.unwrap();
        assert!(env.is_same_object(&delegate, &expected_delegate).unwrap());
    }
    let mut adapter = Adapter::default();
    let info = adapter.create_accessibility_node_info(&mut Activate, &mut env, &view, -1);
    assert!(!info.is_null());
    let mut root = Node::new(Role::Window);
    root.set_label("Regression test");
    let root_id = NodeId(1);
    let events = adapter
        .update_if_active(|| TreeUpdate {
            nodes: vec![(root_id, root)],
            tree: Some(Tree::new(root_id)),
            tree_id: TreeId::ROOT,
            focus: root_id,
        })
        .expect("placeholder must activate and queue events");
    raise_events(events, &mut env, &view);
    assert!(!env.exception_check().unwrap());
}

use accesskit::{ActionHandler, ActionRequest};
use accesskit_adapter::InjectingAdapter;
use std::sync::Mutex;
static ADAPTER: Mutex<Option<InjectingAdapter>> = Mutex::new(None);
struct Actions;
impl ActionHandler for Actions {
    fn do_action(&mut self, _: ActionRequest) {}
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_gpui_a11ytest_MainActivity_install(
    mut env: JNIEnv,
    _class: JClass,
    view: JObject,
) {
    *ADAPTER.lock().unwrap() = Some(InjectingAdapter::new(&mut env, &view, Activate, Actions));
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_gpui_a11ytest_MainActivity_dropAdapter(
    _env: JNIEnv,
    _class: JClass,
) {
    *ADAPTER.lock().unwrap() = None;
}

#[link(name = "log")]
unsafe extern "C" {
    #[link_name = "__android_log_write"]
    fn android_log_write(
        priority: i32,
        tag: *const std::ffi::c_char,
        text: *const std::ffi::c_char,
    ) -> i32;
}

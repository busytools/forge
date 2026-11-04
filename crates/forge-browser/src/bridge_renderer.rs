//! The renderer half of the host opener bridge.
//!
//! Installed in every frame's context before any page script: a native object
//! (`window.__forgeBridgeNative`) whose methods are process messages to the
//! browser process, and the page shim (`bridge.js`) around `window.open`. A
//! bridged popup's browser carries its bridge id in `extra_info`, which
//! `on_browser_created` reads before the first context exists, so the popup's
//! `window.opener` is installed before its first document's scripts run.
//!
//! **Nothing here decides trust.** The browser process derives both sides and
//! their origins from the tab a message arrives on; this side only relays the
//! page and runs what the host sends back.

#![allow(
    clippy::transmute_ptr_to_ptr,
    reason = "CEF's wrap_* macros transmute the wrapper reference to the base object"
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use cef::*;

/// The native object and the two globals the host sets before the shim runs.
const NATIVE: &str = "__forgeBridgeNative";
const POPUP: &str = "__forgeBridgePopup";
const DELIVER: &str = "__forgeBridgeDeliver";
const ENABLE: &str = "__forgeBridgeEnablePopup";

/// The browser process's two messages to this side.
const ASSIGN: &str = "forge.bridge.assign";
const ASSIGNED: &str = "forge.bridge.assigned";
const EVENT: &str = "forge.bridge.event";

/// The page shim, evaluated into every main frame.
const SHIM: &str = include_str!("bridge.js");

/// The subframe shim: a refusal only, with no native object and no relay.
const FRAME_SHIM: &str = include_str!("bridge_frame.js");

/// The extra_info key naming a bridged popup's channel.
const EXTRA: &str = "forge_bridge";

// One renderer's bridge state: browser identifier to bridge id, from the
// browser's `extra_info` or a later `assign` for a renderer that did not
// receive it.
cef::wrap_render_process_handler! {
    struct RendererBridge {
        bridges: Arc<Mutex<HashMap<i32, String>>>,
    }

    impl RenderProcessHandler {
        fn on_browser_created(
            &self,
            browser: Option<&mut Browser>,
            extra_info: Option<&mut DictionaryValue>,
        ) {
            let (Some(browser), Some(info)) = (browser, extra_info) else { return };
            let key = CefString::from(EXTRA);
            if info.has_key(Some(&key)) != 1 {
                return;
            }
            let id = CefString::from(&info.string(Some(&key))).to_string();
            self.bridges
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(browser.identifier(), id);
        }

        fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            self.bridges
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&browser.identifier());
        }

        fn on_context_created(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            context: Option<&mut V8Context>,
        ) {
            let (Some(browser), Some(frame), Some(context)) = (browser, frame, context) else {
                return;
            };
            let bridged = self
                .bridges
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&browser.identifier())
                .cloned();
            // **The role is the main frame's.** A third-party iframe inside
            // the sign-in page is not a popup: the platform gives it a null
            // opener, and its own `window.close()` must not close the tab.
            let role = bridged.is_some() && frame.is_main() == 1;
            install(frame, context, role);
            if role {
                // **The role is in this document before its own scripts.**
                // Tell the host so it can start the popup's address; an ack
                // that lands before the host registered the start is
                // remembered there.
                send(frame, ASSIGNED, &[]);
            }
        }

        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> i32 {
            let (Some(browser), Some(frame), Some(message)) = (browser, frame, message) else {
                return 0;
            };
            let name = CefString::from(&message.name()).to_string();
            if name == ASSIGN {
                let Some(args) = message.argument_list() else { return 1 };
                let id = CefString::from(&args.string(0)).to_string();
                if id.is_empty() {
                    return 1;
                }
                self.bridges
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(browser.identifier(), id);
                // **The start waits for a document that has the role.** An
                // assignment that arrives before the context only records
                // the id; `on_context_created` installs and acknowledges.
                if frame.is_main() == 1 && enable_popup(frame) {
                    send(frame, ASSIGNED, &[]);
                }
                return 1;
            }
            if name == EVENT {
                let Some(args) = message.argument_list() else { return 1 };
                let kind = CefString::from(&args.string(0)).to_string();
                let handle = CefString::from(&args.string(1)).to_string();
                let origin = CefString::from(&args.string(2)).to_string();
                let data = CefString::from(&args.string(3)).to_string();
                let target = CefString::from(&args.string(4)).to_string();
                deliver(frame, &kind, &handle, &origin, &data, &target);
                return 1;
            }
            0
        }
    }
}

/// A render process handler for [`crate::engine::HostApp`].
pub(crate) fn handler() -> RenderProcessHandler {
    RendererBridge::new(Arc::new(Mutex::new(HashMap::new())))
}

/// **A subframe gets no native object and no relay.** It only receives the
/// frame shim, which refuses a cross-site `window.open` before CEF can create
/// the popup; its `window.open` is the only thing this side touches there.
fn install(frame: &mut Frame, context: &mut V8Context, bridged: bool) {
    if context.enter() != 1 {
        return;
    }
    if frame.is_main() != 1 {
        eval(context, FRAME_SHIM);
        let _ = context.exit();
        return;
    }
    if let Some(global) = context.global() {
        let mut handler = NativeHandler::new(frame.clone());
        let mut native = v8_value_create_object(None, None);
        if native.is_some() {
            for method in ["open", "post", "close", "closeSelf", "navigate"] {
                let mut function =
                    v8_value_create_function(Some(&CefString::from(method)), Some(&mut handler));
                if let Some(object) = native.as_mut() {
                    object.set_value_bykey(
                        Some(&CefString::from(method)),
                        function.as_mut(),
                        attributes(),
                    );
                }
            }
            global.set_value_bykey(Some(&CefString::from(NATIVE)), native.as_mut(), attributes());
        }
        if bridged {
            let mut yes = v8_value_create_bool(1);
            global.set_value_bykey(Some(&CefString::from(POPUP)), yes.as_mut(), attributes());
        }
        eval(context, SHIM);
    }
    let _ = context.exit();
}

/// Evaluate one asset into a context already entered. **A failed install is
/// named**: the page keeps the native `window.open` after it, so a silent
/// failure would look like a bridge that never worked.
fn eval(context: &mut V8Context, source: &str) {
    let mut retval = None;
    let mut exception = None;
    let installed = context.eval(
        Some(&CefString::from(source)),
        Some(&CefString::from("forge://opener-bridge")),
        0,
        Some(&mut retval),
        Some(&mut exception),
    );
    if installed != 1 {
        eprintln!(
            "cef bridge: the opener shim did not install (event_name \
             cef_opener_bridge_shim_failed)"
        );
    }
}

/// The popup role for a document that only learned of it after its context
/// was created: set the flag and run the shim's own enable path. Answers
/// whether the role is in a document now, which is what the host's start
/// waits for.
fn enable_popup(frame: &mut Frame) -> bool {
    let Some(context) = frame.v8_context() else { return false };
    if context.enter() != 1 {
        return false;
    }
    let mut enabled = false;
    if let Some(mut global) = context.global() {
        let mut yes = v8_value_create_bool(1);
        global.set_value_bykey(Some(&CefString::from(POPUP)), yes.as_mut(), attributes());
        if let Some(enable) = global.value_bykey(Some(&CefString::from(ENABLE)))
            && enable.is_function() == 1
        {
            let _ = enable.execute_function(Some(&mut global), None);
            enabled = true;
        }
    }
    let _ = context.exit();
    enabled
}

/// One host event, dispatched on the page's window by the shim.
fn deliver(frame: &mut Frame, kind: &str, handle: &str, origin: &str, data: &str, target: &str) {
    let Some(context) = frame.v8_context() else { return };
    if context.enter() != 1 {
        return;
    }
    if let Some(mut global) = context.global()
        && let Some(function) = global.value_bykey(Some(&CefString::from(DELIVER)))
        && function.is_function() == 1
    {
        let arguments = [
            v8_value_create_string(Some(&CefString::from(kind))),
            v8_value_create_string(Some(&CefString::from(handle))),
            v8_value_create_string(Some(&CefString::from(origin))),
            v8_value_create_string(Some(&CefString::from(data))),
            v8_value_create_string(Some(&CefString::from(target))),
        ];
        let _ = function.execute_function(Some(&mut global), Some(&arguments));
    }
    let _ = context.exit();
}

// The page's only IPC surface: each function becomes one process message.
cef::wrap_v8_handler! {
    struct NativeHandler {
        frame: Frame,
    }

    impl V8Handler {
        fn execute(
            &self,
            name: Option<&CefString>,
            _object: Option<&mut V8Value>,
            arguments: Option<&[Option<V8Value>]>,
            _retval: Option<&mut Option<V8Value>>,
            _exception: Option<&mut CefString>,
        ) -> i32 {
            let Some(name) = name else { return 0 };
            let args = arguments.unwrap_or(&[]);
            let string = |at: usize| -> String {
                args.get(at)
                    .and_then(|value| value.as_ref())
                    .filter(|value| value.is_string() == 1)
                    .map(|value| CefString::from(&value.string_value()).to_string())
                    .unwrap_or_default()
            };
            let integer = |at: usize| -> i32 {
                args.get(at)
                    .and_then(|value| value.as_ref())
                    .filter(|value| value.is_int() == 1)
                    .map(V8Value::int_value)
                    .unwrap_or_default()
            };
            let (message, message_args): (&str, Vec<String>) = match name.to_string().as_str() {
                "open" => (
                    OPEN,
                    vec![string(0), string(1), string(2), integer(3).to_string()],
                ),
                "post" => (POST, vec![string(0), string(1), string(2)]),
                "close" => (CLOSE, vec![string(0)]),
                "closeSelf" => (CLOSE_SELF, Vec::new()),
                "navigate" => (NAVIGATE, vec![string(0), string(1)]),
                _ => return 0,
            };
            send(&self.frame, message, &message_args);
            1
        }
    }
}

/// The browser process's names for the shim's operations.
const OPEN: &str = "forge.bridge.open";
const POST: &str = "forge.bridge.post";
const CLOSE: &str = "forge.bridge.close";
const CLOSE_SELF: &str = "forge.bridge.close_self";
const NAVIGATE: &str = "forge.bridge.navigate";

fn send(frame: &Frame, name: &str, arguments: &[String]) {
    let Some(mut message) = process_message_create(Some(&CefString::from(name))) else { return };
    let Some(list) = message.argument_list() else { return };
    for (at, value) in arguments.iter().enumerate() {
        list.set_string(at, Some(&CefString::from(value.as_str())));
    }
    frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
}

/// The native object and the popup flag.
fn attributes() -> V8Propertyattribute {
    let flags: cef::sys::cef_v8_propertyattribute_t =
        cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTENUM
            | cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_DONTDELETE;
    flags.into()
}

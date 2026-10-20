//! The application class's ivars, checked where AppKit allows it.
//!
//! **NSApplication is main-thread-only**, so the check cannot be a `#[test]`
//! body: libtest runs those on a thread that is not the main one.
//! `tests/app_ivars.rs` is a harness-free target whose `main` calls this on
//! the process main thread, and the `testing` feature is what carries the
//! entry point there.

use objc2::{ClassType as _, MainThreadMarker, runtime::NSObjectProtocol};

use super::ClientApplication;

/// Build the application the way the host does, store and clear its flag.
///
/// **`+sharedApplication` is the class's one construction point**, and the
/// first flag read is the one an initializer that never called `set_ivars`
/// fails on: in a `testing` build the ivars carry drop glue, so objc2's
/// debug check panics on uninitialized memory instead of reading zeroes.
pub fn the_application_ivars_hold() -> Result<(), String> {
    let _mtm = MainThreadMarker::new()
        .ok_or_else(|| "the application check must run on the process main thread".to_owned())?;
    let app = ClientApplication::shared_application();
    if !app.isKindOfClass(ClientApplication::class()) {
        return Err("+sharedApplication did not answer a ClientApplication".to_owned());
    }
    if app.is_handling_send_event() {
        return Err("the send-event flag did not start false".to_owned());
    }
    app.set_handling_send_event(true);
    if !app.is_handling_send_event() {
        return Err("the send-event flag did not store true".to_owned());
    }
    app.set_handling_send_event(false);
    if app.is_handling_send_event() {
        return Err("the send-event flag did not clear".to_owned());
    }
    Ok(())
}

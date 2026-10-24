//! The NSApplication subclass CEF's macOS protocol requires.
//!
//! CEF hooks `sendEvent:` to know when Chromium itself is dispatching an
//! event, so its own event path and the application's do not re-enter each
//! other. The class must be the application object from the start, which is
//! why [`setup`] runs before CEF initializes.

#![allow(unsafe_code, reason = "objc2's NSApplication subclassing is unsafe by construction")]

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::{
    ClassType as _, DefinedClass as _, MainThreadMarker, define_class, extern_methods, msg_send,
    rc::{Allocated, Retained},
    runtime::{Bool, NSObjectProtocol},
};
use objc2_app_kit::{NSApp, NSApplication, NSApplicationActivationPolicy, NSEvent};
use std::cell::Cell;

/// The flag Chromium's two protocols read and write around `sendEvent:`.
#[derive(Default)]
pub struct ClientApplicationIvars {
    handling_send_event: Cell<Bool>,
    /// Test builds only: a droppable field makes objc2 register its drop
    /// flag, so an ivar read before `set_ivars` panics in a debug build. A
    /// bare `Cell` gives that check nothing to read.
    #[cfg(feature = "testing")]
    _probe: Option<Box<()>>,
}

define_class!(
    #[unsafe(super(NSApplication))]
    #[ivars = ClientApplicationIvars]
    pub struct ClientApplication;

    impl ClientApplication {
        /// **objc2 requires an ivars class's initializer to call
        /// `set_ivars`.** `+sharedApplication` allocates the receiver class
        /// and sends it `-init`, so this override is the class's one
        /// construction path.
        #[unsafe(method_id(init))]
        unsafe fn init(this: Allocated<Self>) -> Retained<Self> {
            let this = this.set_ivars(ClientApplicationIvars::default());
            unsafe { msg_send![super(this), init] }
        }

        #[unsafe(method(sendEvent:))]
        unsafe fn send_event(&self, event: &NSEvent) {
            let was_sending = self.is_handling_send_event();
            if !was_sending {
                self.set_handling_send_event(true);
            }
            let _: () = msg_send![super(self), sendEvent: event];
            if !was_sending {
                self.set_handling_send_event(false);
            }
        }
    }

    unsafe impl NSObjectProtocol for ClientApplication {}

    unsafe impl CrAppControlProtocol for ClientApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        unsafe fn _set_handling_send_event(&self, handling_send_event: Bool) {
            self.ivars().handling_send_event.set(handling_send_event);
        }
    }

    unsafe impl CrAppProtocol for ClientApplication {
        #[unsafe(method(isHandlingSendEvent))]
        unsafe fn _is_handling_send_event(&self) -> Bool {
            self.ivars().handling_send_event.get()
        }
    }

    unsafe impl CefAppProtocol for ClientApplication {}
);

impl ClientApplication {
    extern_methods!(
        #[unsafe(method(sharedApplication))]
        fn shared_application() -> Retained<Self>;

        #[unsafe(method(setHandlingSendEvent:))]
        fn set_handling_send_event(&self, handling_send_event: bool);

        #[unsafe(method(isHandlingSendEvent))]
        fn is_handling_send_event(&self) -> bool;
    );
}

/// Make NSApplication THIS class and a background app, before anything else
/// asks for it.
///
/// **Called before CEF initializes.** If a prior caller already created
/// NSApp, the class is fixed and CEF's protocol has nothing to hook - the
/// caller gets told rather than left with a silently unpumped browser.
///
/// **A host is an engine, not an app.** Under the regular policy macOS
/// registers the bundle as an app of its own and gives it a Dock tile; an
/// accessory one is a background process with no tile, which is all a host
/// is.
pub fn setup() {
    let _ = ClientApplication::shared_application();
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("forge host: CEF's application must be made on the main thread");
        return;
    };
    let app = NSApp(mtm);
    if !app.isKindOfClass(ClientApplication::class()) {
        eprintln!("forge host: NSApplication was created before CEF claimed it");
    }
    if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
        eprintln!("forge host: the accessory activation policy was refused");
    }
}

/// The application class's ivars, checked where AppKit allows it.
///
/// **NSApplication is main-thread-only**, so the check cannot be a `#[test]`
/// body: libtest runs those on a thread that is not the main one.
/// `tests/app_ivars.rs` is a harness-free target whose `main` calls this on
/// the process main thread, and the `testing` feature carries the entry
/// point there.
#[cfg(feature = "testing")]
pub mod testing {
    use objc2::{ClassType as _, MainThreadMarker, runtime::NSObjectProtocol};
    use objc2_app_kit::NSApplicationActivationPolicy;

    use super::ClientApplication;

    /// **The check's teeth are the probe's drop glue**: without it objc2
    /// keeps no drop flag, an uninitialized read is zeroed memory, and the
    /// old shape would pass. Deleting the probe must fail the build, not
    /// weaken the test.
    const _: () = assert!(std::mem::needs_drop::<super::ClientApplicationIvars>());

    /// Build the application the way the host does, store and clear its flag.
    ///
    /// **`setup` is the host's own entry call**, so this checks the function
    /// `run` invokes rather than a copy of it.
    pub fn the_host_application_holds_its_ivars_and_policy() -> Result<(), String> {
        let _mtm = MainThreadMarker::new().ok_or_else(|| {
            "the application check must run on the process main thread".to_owned()
        })?;
        super::setup();
        let app = ClientApplication::shared_application();
        if !app.isKindOfClass(ClientApplication::class()) {
            return Err("+sharedApplication did not answer a ClientApplication".to_owned());
        }
        if app.activationPolicy() != NSApplicationActivationPolicy::Accessory {
            return Err(format!(
                "the host's activation policy is {:?}, not accessory",
                app.activationPolicy()
            ));
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
}

//! The NSApplication subclass CEF's macOS protocol requires.
//!
//! CEF hooks `sendEvent:` to know when Chromium itself is dispatching an
//! event, so its own event path and the application's do not re-enter each
//! other. The class must be the application object from the start, which is
//! why [`setup`] runs before CEF initializes.

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::{
    ClassType as _, DefinedClass as _, MainThreadMarker, define_class, extern_methods, msg_send,
    rc::{Allocated, Retained},
    runtime::{Bool, NSObjectProtocol},
};
use objc2_app_kit::{NSApp, NSApplication, NSEvent};
use std::cell::Cell;

#[cfg(feature = "testing")]
pub mod testing;

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

/// Make NSApplication THIS class, before anything else asks for it.
///
/// **Called before CEF initializes.** If a prior caller already created
/// NSApp, the class is fixed and CEF's protocol has nothing to hook - the
/// caller gets told rather than left with a silently unpumped browser.
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
}

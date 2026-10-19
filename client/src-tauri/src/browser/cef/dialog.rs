//! The file picker: a page's file dialog is answered with a real NSOpenPanel
//! on the main thread, in whichever process is asking.
//!
//! CEF's own callback answers a dialog the host raises, and the surface's
//! click path asks in the client's process - the app the person is looking
//! at, which is what gives the panel focus. The selected paths go back the
//! way the asker expects, so the page's `change` handler runs with files a
//! browser would give it.

use cef::rc::Rc as _;
use cef::{
    CefString, CefStringList, DialogHandler, FileDialogCallback, FileDialogMode, ImplDialogHandler,
    ImplFileDialogCallback as _, WrapDialogHandler,
};

cef::wrap_dialog_handler! {
    pub struct FileDialogHandler {}

    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut cef::Browser>,
            _mode: FileDialogMode,
            _title: Option<&CefString>,
            _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            match show_panel() {
                Some(paths) if !paths.is_empty() => {
                    let mut list = CefStringList::new();
                    for path in &paths {
                        list.append(path);
                    }
                    callback.cont(Some(&mut list));
                }
                _ => callback.cancel(),
            }
            1
        }
    }
}

/// The handler the host's client hands CEF.
pub fn handler() -> DialogHandler {
    FileDialogHandler::new()
}

/// The person's file picker, on the main thread. Shared with the surface's
/// click path, which asks in the client's own process.
pub fn show_panel() -> Option<Vec<String>> {
    use objc2::MainThreadMarker;
    use objc2::runtime::NSObjectProtocol as _;
    use objc2_app_kit::{NSApp, NSModalResponseOK, NSOpenPanel};

    let mtm = MainThreadMarker::new()?;
    // **The host has no window of its own.** A panel opened without
    // activating the app lands behind whatever is frontmost, and the person
    // sees only a bouncing dock icon; the host is brought forward for the
    // picker, and they click back to the client afterwards.
    // **`activate` is macOS 14+; the client still ships to 11.0.** The
    // selector is chosen at run time rather than built against, so neither
    // the deprecated binding nor a crash on an older system.
    let app = NSApp(mtm);
    unsafe {
        if app.respondsToSelector(objc2::sel!(activate)) {
            let _: () = objc2::msg_send![&*app, activate];
        } else {
            let _: () = objc2::msg_send![&*app, activateIgnoringOtherApps: true];
        }
    }
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    let response = panel.runModal();
    if response != NSModalResponseOK {
        return None;
    }
    let mut paths = Vec::new();
    for url in panel.URLs().iter() {
        if let Some(path) = url.path() {
            paths.push(path.to_string());
        }
    }
    Some(paths)
}

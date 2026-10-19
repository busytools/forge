//! The file picker: CEF has no native window, so the host answers a page's
//! file dialog with a real NSOpenPanel on the main thread.
//!
//! The page's job sites upload files; without this the input would open
//! nothing. The selected paths go back through CEF's own callback, so the
//! page's `change` handler runs with files a browser would give it.

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

#[cfg(target_os = "macos")]
fn show_panel() -> Option<Vec<String>> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};

    let mtm = MainThreadMarker::new()?;
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    if panel.runModal() != NSModalResponseOK {
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

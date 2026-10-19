//! The page's file dialogs, refused by the host.
//!
//! **The picker is the CLIENT's** (see `surface::pick_file`): the app the
//! person is looking at, whose panel takes focus. This handler answers every
//! dialog CEF raises by cancelling it, so a click can never open a second
//! panel - and never block the host's message pump behind a modal the person
//! cannot attribute to a window they never saw.

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
            // Cancelled, not shown: the client's click path asks its own
            // panel and sets the files on the input over CDP.
            if let Some(callback) = callback {
                callback.cancel();
            }
            1
        }
    }
}

/// The handler the host's client hands CEF.
pub fn handler() -> DialogHandler {
    FileDialogHandler::new()
}

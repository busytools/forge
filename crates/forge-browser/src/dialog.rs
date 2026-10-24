//! The page's file dialogs, answered by the person's own picker.
//!
//! **The dialog is raised where the page is and answered where the person
//! is.** CEF offers the host no chooser of its own - the host is a windowless
//! process on the server - so the request crosses to the controlling viewer,
//! whose shell opens the platform's picker, and the chosen files come back as
//! server-local paths for [`FileDialogCallback::cont`]. A viewer that is not
//! there, or a person who dismisses the picker, is a cancel.

#![allow(
    clippy::transmute_ptr_to_ptr,
    reason = "CEF's wrap_* macros transmute the wrapper reference to the base object"
)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use cef::rc::Rc as _;
use cef::{
    CefString, CefStringList, DialogHandler, FileDialogCallback, FileDialogMode, ImplDialogHandler,
    ImplFileDialogCallback as _, WrapDialogHandler,
};

use crate::control::{self, HostEvent};

// The dialogs CEF has raised and the client has not answered yet.
//
// **The callbacks live on the UI thread that raised them.** CEF's callback
// is thread-affine, and the host's pump - the thread that answers it - is
// the same thread.
thread_local! {
    static PENDING: RefCell<HashMap<u64, FileDialogCallback>> = RefCell::new(HashMap::new());
}

static NEXT: AtomicU64 = AtomicU64::new(1);

cef::wrap_dialog_handler! {
    pub struct FileDialogHandler {}

    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut cef::Browser>,
            mode: FileDialogMode,
            _title: Option<&CefString>,
            _default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            let multiple = mode == FileDialogMode::OPEN_MULTIPLE;
            if mode != FileDialogMode::OPEN && !multiple {
                // A page cannot raise a save dialog here; only a chooser has
                // a client-side answer.
                callback.cancel();
                return 1;
            }
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            PENDING.with(|pending| {
                let mut pending = pending.borrow_mut();
                // **One chooser is the person's to answer; the rest are
                // stale.** A click while a chooser is open can raise another
                // (four piled up when a page's input was clicked four times),
                // and the client is shown only the newest: every older
                // callback would wait forever and the browser stops raising
                // new ones while one is pending. They are cancelled here, so
                // the newest is the only one CEF is waiting on.
                for callback in take_stale(&mut pending, id) {
                    callback.cancel();
                }
                pending.insert(id, callback.clone());
            });
            control::emit(&HostEvent::FileDialog { id, multiple });
            1
        }
    }
}

/// Remove every pending dialog but `fresh`, in the order they were raised.
fn take_stale<T>(pending: &mut HashMap<u64, T>, fresh: u64) -> Vec<T> {
    let stale: Vec<u64> = pending.keys().copied().filter(|id| *id != fresh).collect();
    stale.into_iter().filter_map(|id| pending.remove(&id)).collect()
}

/// Answer one dialog: the paths the client uploaded, or none for a cancel.
/// `false` names an id no dialog is waiting on.
pub fn answer(id: u64, paths: &[String]) -> bool {
    let callback = PENDING.with(|pending| pending.borrow_mut().remove(&id));
    let Some(callback) = callback else { return false };
    if paths.is_empty() {
        callback.cancel();
        return true;
    }
    let mut list = CefStringList::new();
    for path in paths {
        list.append(path);
    }
    callback.cont(Some(&mut list));
    true
}

/// The handler the host's client hands CEF.
pub fn handler() -> DialogHandler {
    FileDialogHandler::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A modal a person cannot see is never left open.** A page's chooser
    /// with no viewer to answer it, or one dismissed by the person, is a
    /// cancel rather than a pump held by CEF behind a dialog nobody owns.
    #[test]
    fn an_unanswered_or_dismissed_dialog_is_cancelled() {
        assert!(!answer(9_999, &[]), "an id no dialog raised is not waited on");
    }

    /// **A newer ask cancels every older one.** A click while a chooser is
    /// open raises another, and only the newest reaches the client; the older
    /// callbacks are taken out of the wait rather than held forever.
    #[test]
    fn a_newer_ask_cancels_every_older_one() {
        let mut pending = HashMap::new();
        pending.insert(1_u64, "one");
        pending.insert(2_u64, "two");
        let stale = take_stale(&mut pending, 2);
        assert_eq!(stale, vec!["one"], "the older ask is the stale one");
        assert_eq!(pending.len(), 1, "the fresh ask keeps waiting");

        pending.insert(3_u64, "three");
        let stale = take_stale(&mut pending, 3);
        assert_eq!(stale, vec!["two"], "each newer ask cancels what preceded it");
    }
}

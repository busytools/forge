//! The page's JavaScript dialogs, answered on a deadline.
//!
//! **The host implements no JS dialog handler upstream**, so a page's
//! `confirm()` opens a modal nothing on this side answers: the page's own
//! thread stops, and every page-domain CDP call on it waits for ever, which is
//! how one paused tab held every session's browser calls (#2732). The handler
//! takes every dialog CEF raises, and dismisses one nobody answered after
//! [`DEADLINE_MS`]. The agent's own `browser_handle_dialog` still lands first
//! when it comes: it closes the dialog through the page's CDP session, which
//! cancels this deadline. Every raise and every dismissal is a host event.

#![allow(
    clippy::transmute_ptr_to_ptr,
    reason = "CEF's wrap_* macros transmute the wrapper reference to the base object"
)]

use std::cell::RefCell;
use std::collections::HashMap;

use cef::rc::Rc as _;
use cef::{
    CefString, ImplJsdialogCallback as _, ImplJsdialogHandler, JsdialogCallback, JsdialogHandler,
    JsdialogType, WrapJsdialogHandler,
};

use crate::control::{self, HostEvent, JsDialogState};

/// How long a dialog nobody answers stays open. **The agent's answer is the
/// one that matters**: its `browser_handle_dialog` closes the dialog through
/// the page's CDP session, which reaches [`JsDialogHandler::on_dialog_closed`]
/// and cancels this deadline.
pub const DEADLINE_MS: u64 = 5_000;

// The dialogs CEF has raised and nobody answered yet, keyed by the tab that
// raised them: **a page has one dialog at a time**, and the handler's own tab
// is what `on_dialog_closed` can name.
//
// **The callbacks live on the UI thread that raised them.** CEF's callback is
// thread-affine, and the host's pump - the thread that expires it - is the
// same thread.
thread_local! {
    static OPEN: RefCell<HashMap<u16, Open>> = RefCell::new(HashMap::new());
}

struct Open {
    at: u64,
    dialog: &'static str,
    message: String,
    callback: JsdialogCallback,
}

cef::wrap_jsdialog_handler! {
    pub struct PageDialogHandler {
        // The tab whose page raised this dialog.
        tab: u16,
    }

    impl JsdialogHandler {
        /// **Every dialog is answered here**, not left for CEF's own modal:
        /// the host is a windowless process on a server, so a dialog CEF
        /// draws has nobody to draw it to. The page gets a dismissal unless
        /// its agent answers first, and the ask is a host event either way.
        fn on_jsdialog(
            &self,
            _browser: Option<&mut cef::Browser>,
            _origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            _default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            _suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            let dialog = if dialog_type == JsdialogType::CONFIRM {
                "confirm"
            } else if dialog_type == JsdialogType::PROMPT {
                "prompt"
            } else {
                "alert"
            };
            raise(self.tab, dialog, message_text.map(ToString::to_string).unwrap_or_default(), callback);
            1
        }

        /// **A beforeunload prompt blocks the renderer the same way**, so it
        /// gets the same deadline rather than a host that cannot close a tab
        /// whose page is asking.
        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut cef::Browser>,
            message_text: Option<&CefString>,
            _is_reload: ::std::os::raw::c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> i32 {
            let Some(callback) = callback else { return 0 };
            raise(self.tab, "beforeunload", message_text.map(ToString::to_string).unwrap_or_default(), callback);
            1
        }

        /// **An answer the agent made** - through the page's own CDP session -
        /// closes the dialog and cancels its deadline. The dismissal this
        /// module makes has already taken the entry, so it is not announced
        /// twice.
        fn on_dialog_closed(&self, _browser: Option<&mut cef::Browser>) {
            forget(self.tab);
        }

        /// The page navigated out of its own dialog; there is nothing left to
        /// answer, and nothing to announce.
        fn on_reset_dialog_state(&self, _browser: Option<&mut cef::Browser>) {
            forget(self.tab);
        }
    }
}

/// Register one dialog and announce it.
fn raise(tab: u16, dialog: &'static str, message: String, callback: &JsdialogCallback) {
    let at = crate::engine::elapsed_ms();
    // **A page has one dialog at a time**, so a second raise replaces the
    // first: its callback is answered rather than leaked.
    if let Some(previous) = OPEN.with(|open| {
        open.borrow_mut()
            .insert(tab, Open { at, dialog, message: message.clone(), callback: callback.clone() })
    }) {
        previous.callback.cont(0, None);
    }
    control::emit(&HostEvent::JsDialog {
        tab,
        dialog: dialog.to_owned(),
        message,
        state: JsDialogState::Shown,
    });
}

/// Forget a tab's dialog without answering it. **The browser is going**, so
/// its callback has nothing left to answer, and the entry must not outlive
/// the page that raised it.
pub fn forget(tab: u16) {
    OPEN.with(|open| open.borrow_mut().remove(&tab));
}

/// Answer one dialog: `accept` is the button the agent chose, `prompt` the
/// text a prompt asks for. `false` names a tab no dialog is open on, **which
/// is not an error**: the deadline may have answered it first.
///
/// **Called on the UI thread** by the host's pump, the thread that raised the
/// callback.
pub fn answer(tab: u16, accept: bool, prompt: Option<&str>) -> bool {
    let dialog = OPEN.with(|open| open.borrow_mut().remove(&tab));
    let Some(dialog) = dialog else { return false };
    let input = prompt.map(CefString::from);
    dialog.callback.cont(i32::from(accept), input.as_ref());
    control::emit(&HostEvent::JsDialog {
        tab,
        dialog: dialog.dialog.to_owned(),
        message: dialog.message,
        state: JsDialogState::Answered,
    });
    true
}

/// Dismiss every dialog past its deadline. **Called on the UI thread** by the
/// host's pump, the thread that raised them.
pub fn expire(now: u64) {
    let due: Vec<(u16, Open)> = OPEN.with(|open| {
        let mut open = open.borrow_mut();
        let due: Vec<u16> = open
            .iter()
            .filter(|(_, dialog)| now.saturating_sub(dialog.at) >= DEADLINE_MS)
            .map(|(tab, _)| *tab)
            .collect();
        due.into_iter().filter_map(|tab| open.remove(&tab).map(|dialog| (tab, dialog))).collect()
    });
    for (tab, dialog) in due {
        dialog.callback.cont(0, None);
        control::emit(&HostEvent::JsDialog {
            tab,
            dialog: dialog.dialog.to_owned(),
            message: dialog.message,
            state: JsDialogState::Expired,
        });
        tracing::debug!(
            event_name = "cef_js_dialog_dismissed",
            tab,
            dialog = dialog.dialog,
            "a JavaScript dialog nobody answered was dismissed at its deadline"
        );
    }
}

/// The handler the host's tab hands CEF.
pub fn handler(tab: u16) -> JsdialogHandler {
    PageDialogHandler::new(tab)
}

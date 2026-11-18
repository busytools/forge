//! The screen's lock state, as macOS reports it.
//!
//! **The page cannot see a locked screen**: the webview is suspended with the
//! rest of the session, and the microphone stream the shell holds stops
//! delivering without anything saying so. That is the state the hold has to
//! know about (see [`super`]): a capture kept through a locked screen is
//! reopened by every later staleness check, and each open on macOS rebuilds
//! the menu bar.
//!
//! The reading is `CGSessionCopyCurrentDictionary`, the session's own record:
//! `CGSSessionScreenIsLocked` while the login window holds the screen, and
//! `kCGSSessionOnConsoleKey` false while another user is on the console (a
//! fast user switch leaves this session running but away from the display).
//! Both are the platform's own words for away. `CGDisplayIsAsleep` adds the
//! display's own sleep, which is the pre-lock half: a screen that is going
//! away stops feeding audio before the login window has finished locking it.
//!
//! **A poll, not a notification.** The distributed lock notifications are
//! posted by loginwindow and are not documented as stable; the dictionary is
//! the state they describe, and reading it every couple of seconds costs one
//! copy. Only the transition matters, and a couple of seconds is far inside
//! the multi-second reopen cadence this exists to stop.

#![allow(unsafe_code, reason = "CGSessionCopyCurrentDictionary is a CoreGraphics C API")]

use std::sync::Arc;
use std::time::Duration;

use core_foundation::base::{CFType, TCFType as _};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::CFString;

use super::Mic;

/// The key present (and true) while the login window holds the screen.
const SCREEN_LOCKED: &str = "CGSSessionScreenIsLocked";

/// The key false while another user's session is on the console.
const ON_CONSOLE: &str = "kCGSSessionOnConsoleKey";

/// How often the dictionary is read. See the module note.
const POLL: Duration = Duration::from_secs(2);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// The current session's own record, or null when there is none.
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    /// The main display, which is the one a laptop locks.
    fn CGMainDisplayID() -> u32;
    /// Whether a display is asleep, as a C `boolean_t`.
    fn CGDisplayIsAsleep(display: u32) -> u32;
}

/// Watch the screen and report every change of its lock state to `mic`,
/// calling `awake` as it comes back.
///
/// The thread lives for the process's life: nothing else has an exit to hook
/// it to, and the reading is one dictionary copy every [`POLL`].
pub(crate) fn watch(mic: Arc<Mic>, awake: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let mut away = is_away();
        mic.set_screen_away(away);
        loop {
            std::thread::sleep(POLL);
            let reading = is_away();
            if reading == away {
                continue;
            }
            away = reading;
            mic.set_screen_away(reading);
            if !reading {
                awake();
            }
        }
    });
}

/// Whether the session is away from its screen: locked, asleep, or another
/// user is on the console.
fn is_away() -> bool {
    let asleep = unsafe { CGDisplayIsAsleep(CGMainDisplayID()) } != 0;
    if asleep {
        return true;
    }
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        // No session record: read as an unlocked screen, because the other
        // reading is a hold that never comes back on a machine that cannot
        // tell us why.
        return false;
    }
    let session: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_create_rule(raw) };
    let locked = flag(&session, SCREEN_LOCKED).unwrap_or(false);
    let ours = flag(&session, ON_CONSOLE).unwrap_or(true);
    locked || !ours
}

/// One boolean in the session record, when present.
fn flag(session: &CFDictionary<CFString, CFType>, name: &str) -> Option<bool> {
    session.find(CFString::new(name))?.downcast::<CFBoolean>().map(bool::from)
}

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

/// One reading of the screen.
#[derive(Clone, Copy)]
struct Screen {
    /// Whether the session is away from its screen.
    away: bool,
    /// Whether the session record could not be read at all. A blind reading
    /// is answered as an unlocked screen, and the transition says so: a
    /// permanently blind machine must not look like a real unlock.
    blind: bool,
}

/// The screen's own reading, as one function so a test can stand one in.
type Reading = fn() -> Screen;

/// Watch the screen and report every change of its lock state to `mic`,
/// calling `awake` as it comes back.
///
/// The thread lives for the process's life: nothing else has an exit to hook
/// it to, and the reading is one dictionary copy every [`POLL`].
pub(crate) fn watch(mic: Arc<Mic>, awake: impl Fn() + Send + 'static) {
    watch_with(mic, awake, is_away);
}

/// [`watch`], over the reading a test hands in.
///
/// **The first reading is applied before the thread starts**: a process
/// launched while the screen is already locked is away from its first
/// instruction rather than only once the first poll comes back, and a lock
/// that lands on a running process is seen as a transition from that same
/// reading. The debug lines are what make a misread visible: without them, a
/// wrong reading restores the old reopen behaviour in silence, and a blind
/// one reads as a real unlock.
fn watch_with(mic: Arc<Mic>, awake: impl Fn() + Send + 'static, read: Reading) {
    let first = read();
    tauri_plugin_log::log::debug!(
        target: super::LOG_TARGET,
        "the screen's lock reading at start (event_name client_mic_screen, away {}, blind {})",
        first.away,
        first.blind,
    );
    mic.set_screen_away(first.away);
    std::thread::spawn(move || {
        let mut seen = first;
        loop {
            std::thread::sleep(POLL);
            let reading = read();
            if reading.away == seen.away && reading.blind == seen.blind {
                continue;
            }
            let was_away = seen.away;
            seen = reading;
            tauri_plugin_log::log::debug!(
                target: super::LOG_TARGET,
                "the screen's lock state changed (event_name client_mic_screen, away {}, blind \
                 {})",
                reading.away,
                reading.blind,
            );
            mic.set_screen_away(reading.away);
            if was_away && !reading.away {
                awake();
            }
        }
    });
}

/// Whether the session is away from its screen: locked, asleep, or another
/// user is on the console - and whether the record could be read at all.
fn is_away() -> Screen {
    let asleep = unsafe { CGDisplayIsAsleep(CGMainDisplayID()) } != 0;
    if asleep {
        return Screen { away: true, blind: false };
    }
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    if raw.is_null() {
        // No session record: read as an unlocked screen, because the other
        // reading is a hold that never comes back on a machine that cannot
        // tell us why - and said blind, so the transition is visible.
        return Screen { away: false, blind: true };
    }
    let session: CFDictionary<CFString, CFType> =
        unsafe { CFDictionary::wrap_under_create_rule(raw) };
    let locked = flag(&session, SCREEN_LOCKED).unwrap_or(false);
    let ours = flag(&session, ON_CONSOLE).unwrap_or(true);
    Screen { away: locked || !ours, blind: false }
}

/// One boolean in the session record, when present.
fn flag(session: &CFDictionary<CFString, CFType>, name: &str) -> Option<bool> {
    session.find(CFString::new(name))?.downcast::<CFBoolean>().map(bool::from)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use forge_capture::Device;

    use super::*;
    use crate::bridge::Bridge;
    use crate::mic::{AWAY, Carry, Held, Restraint};

    /// A stand-in capture: it counts its opens and holds no device.
    struct Fake {
        device: Device,
        carry: Arc<Carry>,
    }

    impl Held for Fake {
        fn device(&self) -> &Device {
            &self.device
        }

        fn carry(&self) -> &Arc<Carry> {
            &self.carry
        }

        fn release(self: Box<Self>) {}
    }

    /// A microphone over the stand-in, and the opens it made.
    fn rig() -> (Arc<Mic>, Arc<Bridge>, Arc<AtomicUsize>) {
        let opens = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&opens);
        let mic = Arc::new(Mic {
            warm: Mutex::new(None),
            screen_away: AtomicBool::new(false),
            hidden_away: AtomicBool::new(false),
            restraint: Mutex::new(Restraint::default()),
            opener: Box::new(move |device, carry| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Fake {
                    device: Device {
                        id: device.unwrap_or("default").to_owned(),
                        name: "stand-in".to_owned(),
                        is_default: device.is_none(),
                    },
                    carry,
                }))
            }),
        });
        let (events, _frames) = tokio::sync::mpsc::unbounded_channel();
        (mic, Bridge::new(None, events), opens)
    }

    /// **A process launched while the screen is already locked opens
    /// nothing.** The first reading is applied before the poll loop starts,
    /// so both open paths - a hold and a press racing the lock - are shut,
    /// and the test sees it without waiting for a transition.
    #[test]
    fn a_locked_boot_reading_keeps_every_open_path_shut() {
        let (mic, bridge, opens) = rig();
        watch_with(Arc::clone(&mic), || {}, || Screen { away: true, blind: false });

        assert!(
            mic.hold(&bridge, None, "page").expect("the hold answers").is_none(),
            "a hold behind a locked screen opens nothing",
        );
        assert_eq!(
            mic.start(&bridge, None, "page", "take").err().as_deref(),
            Some(AWAY),
            "a press racing the lock is refused by name",
        );
        assert_eq!(opens.load(Ordering::SeqCst), 0, "no device was opened");

        // And the reading that says the screen is back is what opens it.
        mic.set_screen_away(false);
        assert!(
            mic.hold(&bridge, None, "page").expect("the hold opens").is_some(),
            "the hold is taken once the screen is back",
        );
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }
}

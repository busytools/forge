//! The client logger's own lines, for the tests that pin a line's level.
//!
//! **`log` installs one logger per process**: the first test to capture
//! installs this one, and every later test shares its buffer. The capture
//! keeps what the shipping filter keeps - `Info` for the crate, `debug` for
//! the targets [`crate::DEBUG_TARGETS`] names - which is what tells a line
//! `forge.log` actually holds from one it drops.

use std::sync::{Mutex, Once, PoisonError};

use tauri_plugin_log::log::{Level, LevelFilter, Metadata, Record};

/// One line the filter kept, as a test reads it back.
#[derive(Clone)]
pub(crate) struct Line {
    pub(crate) level: Level,
    pub(crate) target: String,
    pub(crate) message: String,
}

static CAPTURED: Mutex<Vec<Line>> = Mutex::new(Vec::new());

/// One capture at a time: two tests sharing the buffer would clear each
/// other's lines between a log and the read that asserts on it.
static GATE: Mutex<()> = Mutex::new(());

struct KeepLines;

impl tauri_plugin_log::log::Log for KeepLines {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        // The shipping filter's own shape: `Info` for the crate, and `debug`
        // only where the default directives name the target.
        let kept = record.level() <= Level::Info || crate::DEBUG_TARGETS.contains(&record.target());
        if !kept {
            return;
        }
        CAPTURED.lock().unwrap_or_else(PoisonError::into_inner).push(Line {
            level: record.level(),
            target: record.target().to_owned(),
            message: record.args().to_string(),
        });
    }

    fn flush(&self) {}
}

/// Keep the client logger's lines from here on, and clear the old ones.
///
/// **Hold the guard for the whole test**: it serialises captures, so a
/// parallel test cannot clear the lines between a log and its assertion.
pub(crate) fn capture() -> std::sync::MutexGuard<'static, ()> {
    static KEEP: KeepLines = KeepLines;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tauri_plugin_log::log::set_logger(&KEEP);
    });
    let held = GATE.lock().unwrap_or_else(PoisonError::into_inner);
    CAPTURED.lock().unwrap_or_else(PoisonError::into_inner).clear();
    // The floor the shipping levels derive: a per-target `debug` raises the
    // logger's own maximum, and [`KeepLines`] drops the targets it does not
    // name.
    tauri_plugin_log::log::set_max_level(LevelFilter::Debug);
    held
}

/// The lines kept since the last [`capture`], whole.
pub(crate) fn records() -> Vec<Line> {
    CAPTURED.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The messages kept since the last [`capture`].
pub(crate) fn lines() -> Vec<String> {
    records().into_iter().map(|line| line.message).collect()
}

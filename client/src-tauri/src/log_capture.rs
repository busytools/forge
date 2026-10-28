//! The client logger's own lines, for the tests that pin a line's level.
//!
//! **`log` installs one logger per process**: the first test to capture
//! installs this one, and every later test shares its buffer. The capture
//! keeps by [`crate::keeps`] - the shipped logger's own policy - so a test
//! tells a line `forge.log` holds from one it drops, and cannot model a filter
//! the app does not ship.

use std::sync::{Mutex, Once, PoisonError};

use tauri_plugin_log::log::{Level, Metadata, Record};

/// One line the filter kept, as a test reads it back.
#[derive(Clone)]
pub(crate) struct Line {
    pub(crate) level: Level,
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
        if !crate::keeps(record.level(), record.target()) {
            return;
        }
        CAPTURED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Line { level: record.level(), message: record.args().to_string() });
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
    // The bound the shipped logger derives: high enough that the facade never
    // drops a record [`crate::keeps`] would keep.
    tauri_plugin_log::log::set_max_level(crate::LOG_MAX);
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

#[cfg(test)]
mod tests {
    /// **The capture drops what the shipped filter drops.** Without this, a
    /// double that kept everything would leave every capture-based test
    /// green, and a `debug` line outside the named targets would look
    /// readable when `forge.log` never writes it.
    #[test]
    fn a_debug_line_outside_the_named_targets_is_not_kept() {
        let _held = super::capture();
        tauri_plugin_log::log::debug!(target: "forge_client::socket", "a debug line elsewhere");
        tauri_plugin_log::log::debug!(target: crate::mic::LOG_TARGET, "the mic's own debug line");
        let lines = super::lines();
        assert!(
            !lines.iter().any(|line| line.contains("a debug line elsewhere")),
            "a target outside the directives is dropped at `debug`: {lines:?}",
        );
        assert!(
            lines.iter().any(|line| line.contains("the mic's own debug line")),
            "a named target is kept at `debug`, which is what the press line needs: {lines:?}",
        );
    }
}

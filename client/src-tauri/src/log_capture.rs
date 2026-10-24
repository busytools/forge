//! The client logger's own lines, for the tests that pin a refusal's level.
//!
//! **`log` installs one logger per process**: the first test to capture
//! installs this one, and every later test shares its buffer. A test that
//! expects the shipping filter - `LevelFilter::Info` - is what tells a line
//! at `debug!` from one `forge.log` actually keeps.

use std::sync::{Mutex, Once, PoisonError};

static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// One capture at a time: two tests sharing the buffer would clear each
/// other's lines between a log and the read that asserts on it.
static GATE: Mutex<()> = Mutex::new(());

struct KeepLines;

impl tauri_plugin_log::log::Log for KeepLines {
    fn enabled(&self, _: &tauri_plugin_log::log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &tauri_plugin_log::log::Record<'_>) {
        CAPTURED.lock().unwrap_or_else(PoisonError::into_inner).push(record.args().to_string());
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
        tauri_plugin_log::log::set_max_level(tauri_plugin_log::log::LevelFilter::Info);
    });
    let held = GATE.lock().unwrap_or_else(PoisonError::into_inner);
    CAPTURED.lock().unwrap_or_else(PoisonError::into_inner).clear();
    held
}

/// The lines kept since the last [`capture`].
pub(crate) fn lines() -> Vec<String> {
    CAPTURED.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

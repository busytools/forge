//! Terminal process tracking for spawned shell commands. Used by
//! `app::terminal` to snapshot accumulated stdout/stderr into the
//! associated tool call's render state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Shared handle to all spawned terminal processes.
pub type TerminalMap = Rc<RefCell<HashMap<String, TerminalProcess>>>;

/// Minimal terminal process state used by UI snapshot rendering.
/// Single-threaded by construction - the whole `TerminalMap` is
/// `Rc<RefCell<…>>`, so the inner buffer doesn't need cross-thread
/// synchronisation either.
pub struct TerminalProcess {
    /// Accumulated stdout+stderr - append-only, never cleared.
    pub output_buffer: Rc<RefCell<Vec<u8>>>,
    /// The shell command that was executed.
    pub command: String,
}

/// Clear the terminal map. Call on app exit.
pub fn kill_all_terminals(terminals: &TerminalMap) {
    terminals.borrow_mut().clear();
}

//! Terminal process tracking for spawned shell commands.
//!
//! Phase 4 deleted the `ClientEvent` enum (replaced by direct
//! `forge_workspace::SessionUpdate` consumption). The remaining
//! types live here for the existing `app::terminal` integration.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Shared handle to all spawned terminal processes.
pub type TerminalMap = Rc<RefCell<HashMap<String, TerminalProcess>>>;

/// Minimal terminal process state used by UI snapshot rendering.
pub struct TerminalProcess {
    /// Accumulated stdout+stderr - append-only, never cleared.
    pub output_buffer: Arc<Mutex<Vec<u8>>>,
    /// The shell command that was executed.
    pub command: String,
}

/// Clear the terminal map. Call on app exit.
pub fn kill_all_terminals(terminals: &TerminalMap) {
    terminals.borrow_mut().clear();
}

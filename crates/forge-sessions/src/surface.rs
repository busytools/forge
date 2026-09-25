//! The read surface a view uses: named verbs by subject, returning
//! values that carry no terminal type.
//!
//! Writes stay on `Workspace::dispatch` and changes on
//! `Workspace::subscribe`; this is the read half, so a second view
//! attaches to the core without reading it.

pub mod accounts;
pub mod connectors;
pub mod dictate;
pub mod plugins;
pub mod reviews;
pub mod roster;
pub mod session;
pub mod workers;

use std::path::Path;
use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_workspace::Workspace;

pub use roster::Roster;
pub use session::SessionState;
pub use workers::{WorkerRef, Workers};

/// A view's read handle on the core.
pub struct ViewSurface {
    workspace: Arc<Workspace>,
}

impl ViewSurface {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }

    /// The projects, their sessions, and the per-project lists the
    /// Projects pane renders.
    pub fn roster(&self) -> Roster {
        Roster::collect(Arc::clone(&self.workspace))
    }

    /// One session's operational state. `cwd_raw` is the caller's own
    /// cwd for the session, which a git worker's worktree overrides.
    pub fn session(&self, slot: &SessionSlot, cwd_raw: &Path) -> SessionState {
        SessionState::collect(&self.workspace, slot, cwd_raw)
    }

    /// The live workers, per project.
    pub fn workers(&self) -> Workers {
        Workers::collect(Arc::clone(&self.workspace))
    }
}

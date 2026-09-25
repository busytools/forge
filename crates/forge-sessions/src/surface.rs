//! The view surface: what a view may read from the core.
//!
//! One verb per subject rather than the workspace's own methods, so a
//! second view attaches to a contract instead of rediscovering the first
//! view's call sites. A verb returns a snapshot built from the same
//! internals the direct calls used, and every value in it is
//! self-contained - no terminal type crosses.

pub mod accounts;
pub mod connectors;
pub mod dictate;
pub mod plugins;
pub mod reviews;

use std::sync::Arc;

use forge_workspace::Workspace;

/// A view's handle on the core.
pub struct ViewSurface {
    workspace: Arc<Workspace>,
}

impl ViewSurface {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }

    /// The workspace behind the surface. Reads go through the verbs;
    /// this is for the dispatch and subscribe plumbing a view drives
    /// itself.
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
}

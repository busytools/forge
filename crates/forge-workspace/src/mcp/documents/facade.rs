//! The narrow surface the document tool calls: one verb, the workspace's
//! own read-and-raise.
//!
//! Mirrors [`crate::mcp::browser::facade`]: the `Tool` impl holds an
//! `Arc<dyn DocumentFacade>` so its tests drive a `MockDocumentFacade`
//! instead of a live workspace.

use std::sync::Arc;

use async_trait::async_trait;
use forge_primitives::SessionSlot;
use forge_primitives::document::DocumentHandOffEnding;

use crate::workspace::Workspace;

/// The hand-off, as one tool call sees it.
#[async_trait]
pub trait DocumentFacade: Send + Sync {
    /// Read the document out of `seat`'s own tree, ask the clients drawing
    /// that seat to open it, and **park until the reader answers** -
    /// answering with the resolved path and the ending the park released
    /// on.
    ///
    /// A refused read raises nothing and parks on nothing; a forge with no
    /// attached client is the read's sibling failure, its own sentence.
    async fn hand_off(
        &self,
        seat: &SessionSlot,
        path: &str,
        reviewable: bool,
    ) -> Result<(String, DocumentHandOffEnding), String>;
}

/// Production impl: the workspace's own read and parked ask.
pub struct ProdDocumentFacade {
    workspace: std::sync::Weak<Workspace>,
}

impl ProdDocumentFacade {
    pub fn from_workspace(workspace: &Arc<Workspace>) -> Arc<dyn DocumentFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }
}

#[async_trait]
impl DocumentFacade for ProdDocumentFacade {
    async fn hand_off(
        &self,
        seat: &SessionSlot,
        path: &str,
        reviewable: bool,
    ) -> Result<(String, DocumentHandOffEnding), String> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Err("the workspace went away before the document was handed".to_owned());
        };
        workspace.hand_off_document(seat, path, reviewable).await
    }
}

/// Mock for the tool's unit tests: captures what a call carried and answers
/// with a preloaded outcome.
#[cfg(test)]
pub struct MockDocumentFacade {
    /// Captured `(path, reviewable)` calls.
    pub calls: parking_lot::Mutex<Vec<(String, bool)>>,
    /// The seat each call was made for, so a caller sending the wrong one is
    /// a failed assertion rather than a silent mismatch.
    pub seats: parking_lot::Mutex<Vec<SessionSlot>>,
    /// The outcome every call answers with.
    pub answer: parking_lot::Mutex<Result<(String, DocumentHandOffEnding), String>>,
}

#[cfg(test)]
impl MockDocumentFacade {
    pub fn new() -> Self {
        Self {
            calls: parking_lot::Mutex::new(Vec::new()),
            seats: parking_lot::Mutex::new(Vec::new()),
            answer: parking_lot::Mutex::new(Ok((
                "/proj/docs/readme.md".to_owned(),
                DocumentHandOffEnding::Done { review_id: None },
            ))),
        }
    }

    pub fn into_arc(self) -> Arc<dyn DocumentFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[async_trait]
impl DocumentFacade for MockDocumentFacade {
    async fn hand_off(
        &self,
        seat: &SessionSlot,
        path: &str,
        reviewable: bool,
    ) -> Result<(String, DocumentHandOffEnding), String> {
        self.seats.lock().push(seat.clone());
        self.calls.lock().push((path.to_owned(), reviewable));
        self.answer.lock().clone()
    }
}

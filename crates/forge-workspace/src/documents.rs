//! The document hand-off's parked ask: one document handed to the reader,
//! held until their answer.
//!
//! Mirrors [`crate::browser`]'s relay shape rather than inventing a second
//! parked one: [`Workspace::hand_off_document`] registers the call and
//! awaits its ending; a client answers **not now** to decline, and the
//! viewer's **done** - after the reader's comments were filed - releases it
//! with what the finish filed. Nothing parks without a client attached to
//! draw the ask.

use forge_primitives::document::{DocumentHandOffAsk, DocumentHandOffEnding};
use tokio::sync::oneshot;

use crate::SessionSlot;
use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;

/// What a hand-off answers when no client is attached to draw its ask.
pub const NO_VIEWER_CLIENT: &str =
    "no client is connected, so the document cannot open - no view is attached to draw it";

/// One parked call, under its id: who handed it, the ask as the read
/// surface carries it, and the sender the awaiting tool holds.
pub(crate) struct ParkedDocument {
    pub(crate) seat: SessionSlot,
    pub(crate) ask: DocumentHandOffAsk,
    sender: oneshot::Sender<DocumentHandOffEnding>,
}

impl Workspace {
    /// Hold a document hand-off for the reader, and hand back its id plus
    /// the receiver the caller awaits.
    ///
    /// **No client, no park.** A forge with no attached client returns
    /// `None` rather than a receiver: holding a session on an ask no view
    /// can draw would park it for ever, which is the failure this refuses.
    /// The raise goes out as an ANSWERING prompt, the same way a browser
    /// hand-off's does - the ask needs a client that can answer it.
    pub(crate) fn register_document_hand_off(
        &self,
        seat: &SessionSlot,
        path: &str,
        reviewable: bool,
    ) -> Option<(uuid::Uuid, oneshot::Receiver<DocumentHandOffEnding>)> {
        let id = uuid::Uuid::new_v4();
        let (sender, receiver) = oneshot::channel();
        if !self.client_attached() {
            return None;
        }
        let ask = DocumentHandOffAsk { id, path: path.to_owned(), reviewable };
        self.document_handoffs
            .lock()
            .insert(id, ParkedDocument { seat: seat.clone(), ask, sender });
        let answerable =
            self.update_sender().send_answering(SessionUpdate::DocumentHandOffPending {
                key: seat.clone(),
                id,
                path: path.to_owned(),
                reviewable,
            });
        if !answerable {
            self.document_handoffs.lock().remove(&id);
            return None;
        }
        Some((id, receiver))
    }

    /// Whether a hand-off with this id is still registered to `seat`,
    /// without removing it - the read the dispatch guard makes before an
    /// answer.
    pub(crate) fn document_handoff_waiting(&self, id: uuid::Uuid, seat: &SessionSlot) -> bool {
        self.document_handoffs.lock().get(&id).is_some_and(|held| &held.seat == seat)
    }

    /// Whether `seat` is parked on a document hand-off: the read the
    /// activity gate and the needs-you row both make, so a held ask lights
    /// the same marks a held draft and a held browser hand-off do.
    pub(crate) fn has_document_handoff(&self, seat: &SessionSlot) -> bool {
        self.document_handoffs.lock().values().any(|held| &held.seat == seat)
    }

    /// Remove a parked hand-off, answer its waiter, and tell every view it
    /// is gone. `false` when the id is not this seat's to resolve.
    pub(crate) fn resolve_document_hand_off(
        &self,
        id: uuid::Uuid,
        seat: &SessionSlot,
        ending: DocumentHandOffEnding,
    ) -> bool {
        let mut parked = self.document_handoffs.lock();
        if !parked.get(&id).is_some_and(|held| &held.seat == seat) {
            return false;
        }
        let Some(held) = parked.remove(&id) else { return false };
        drop(parked);
        let _ = held.sender.send(ending.clone());
        let _ = self.update_sender().send(SessionUpdate::DocumentHandOffResolved {
            key: held.seat,
            id,
            ending,
        });
        true
    }

    /// Hand one document to the reader: read it out of `seat`'s own tree,
    /// ask the clients drawing that seat to open it, and park until the
    /// reader answers.
    ///
    /// **The answer is the finish, not the open.** The reader accepts, reads
    /// and comments; the viewer's done releases the call with what the
    /// finish filed, `NotNow` releases it with their decline, and the
    /// waiter's own drop with [`DocumentHandOffEnding::Abandoned`]. A
    /// refused read raises nothing and parks on nothing.
    pub async fn hand_off_document(
        &self,
        seat: &SessionSlot,
        path: &str,
        reviewable: bool,
    ) -> Result<(String, DocumentHandOffEnding), String> {
        let Some(cwd) = self.cwd_for_session(seat) else {
            return Err(format!("forge holds no session for {}", seat.display()));
        };
        let read = crate::env::documents::read(
            std::path::Path::new(&cwd),
            path,
            reviewable,
            self.document_max_bytes(),
        )
        .map_err(|refusal| refusal.to_string())?;
        let resolved = read.path.to_string_lossy().into_owned();
        let Some((id, ending)) = self.register_document_hand_off(seat, &resolved, read.reviewable)
        else {
            return Err(NO_VIEWER_CLIENT.to_owned());
        };
        // A waiter that goes away releases the park. Resolving an id the
        // normal path already removed is a no-op, so the guard needs no
        // arming flag: it only ever fires on the drop.
        let _guard = AbandonOnDrop { workspace: self, id, seat };
        Ok((resolved, ending.await.unwrap_or(DocumentHandOffEnding::Abandoned)))
    }
}

/// Releases a parked hand-off as [`DocumentHandOffEnding::Abandoned`] when
/// the awaiting call goes away mid-ask - a cancelled turn must not leave the
/// registry holding a sender nothing awaits, or an ask no view can close.
struct AbandonOnDrop<'a> {
    workspace: &'a Workspace,
    id: uuid::Uuid,
    seat: &'a SessionSlot,
}

impl Drop for AbandonOnDrop<'_> {
    fn drop(&mut self) {
        self.workspace.resolve_document_hand_off(
            self.id,
            self.seat,
            DocumentHandOffEnding::Abandoned,
        );
    }
}

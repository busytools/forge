//! The document hand-off's parked ask: one document handed to the reader,
//! held until their answer.
//!
//! Mirrors [`crate::browser`]'s relay shape rather than inventing a second
//! parked one: [`Workspace::hand_off_document`] registers the call and
//! awaits its ending; a client answers **not now** to decline, and the
//! viewer's **approve** or **feedback** - after the reader's comments were
//! filed - releases it with the decision and the note the reader gave.
//! **The ask parks whether or not a client is attached**: a client that
//! attaches later reads it from the seat's `pending_asks`, and the call
//! waits for its answer.

use forge_primitives::document::{DocumentHandOffAsk, DocumentHandOffEnding};
use tokio::sync::oneshot;

use crate::SessionSlot;
use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;

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
    /// **The park is unconditional.** Whether a client is attached when the
    /// ask is raised does not decide it: the ask waits for the person, and a
    /// client that attaches later reads it from the seat's `pending_asks`.
    pub(crate) fn register_document_hand_off(
        &self,
        seat: &SessionSlot,
        path: &str,
        reviewable: bool,
    ) -> (uuid::Uuid, oneshot::Receiver<DocumentHandOffEnding>) {
        let id = uuid::Uuid::new_v4();
        let (sender, receiver) = oneshot::channel();
        let ask = DocumentHandOffAsk { id, path: path.to_owned(), reviewable };
        self.document_handoffs
            .lock()
            .insert(id, ParkedDocument { seat: seat.clone(), ask, sender });
        // Delivered to whoever is attached; the send's report on whether an
        // answering subscriber took it no longer decides the hand-off.
        let _ = self.update_sender().send_answering(SessionUpdate::DocumentHandOffPending {
            key: seat.clone(),
            id,
            path: path.to_owned(),
            reviewable,
        });
        self.note_ask(seat);
        (id, receiver)
    }

    /// Whether a hand-off with this id is still registered to `seat`,
    /// without removing it - the read the dispatch guard makes before an
    /// answer.
    pub(crate) fn document_handoff_waiting(&self, id: uuid::Uuid, seat: &SessionSlot) -> bool {
        self.document_handoffs.lock().get(&id).is_some_and(|held| &held.seat == seat)
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

    /// End every document hand-off `seat` still holds as abandoned.
    ///
    /// **A parked hand-off outlives the session that raised it.** The blocked
    /// tool handler is a detached control task, so its future is not dropped
    /// when the session goes and the caller's own drop never fires; this
    /// sweep answers it, tells every view the card is over, and keeps a later
    /// occupant of the slot from inheriting a hand-off it never raised.
    pub(crate) fn abandon_document_handoffs(&self, seat: &SessionSlot) {
        let ids: Vec<uuid::Uuid> = self
            .document_handoffs
            .lock()
            .iter()
            .filter(|(_, held)| &held.seat == seat)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.resolve_document_hand_off(id, seat, DocumentHandOffEnding::Abandoned);
        }
    }

    /// Hand one document to the reader: read it out of `seat`'s own tree,
    /// ask the clients drawing that seat to open it, and park until the
    /// reader answers.
    ///
    /// **The answer is a decision, not the open.** The reader accepts,
    /// reads and comments; the viewer's approve or feedback releases the
    /// call with the decision, note and filed review, `NotNow` releases it
    /// with their decline, and the waiter's own drop with
    /// [`DocumentHandOffEnding::Abandoned`]. A refused read raises nothing
    /// and parks on nothing.
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
            &self.document_roots_for_session(seat),
            path,
            reviewable,
            self.document_max_bytes(),
        )
        .map_err(|refusal| refusal.to_string())?;
        let resolved = read.path.to_string_lossy().into_owned();
        // The ask carries what the AGENT asked for, not the document's own
        // reviewability: a reader can comment on any document a client can
        // read, so the flag is the hand-off's own fact and nothing gates on
        // the read's answer.
        let (id, ending) = self.register_document_hand_off(seat, &resolved, reviewable);
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use forge_primitives::document::DocumentHandOffEnding;

    use crate::SessionSlot;
    use crate::workspace::Workspace;

    /// A workspace holding one project rooted at `cwd` with `roots` as
    /// its document roots, and the project's lead slot. The project is
    /// declared in the config rather than seeded into the test overlay:
    /// a lead's cwd comes from `project_for_slot`, which reads
    /// `config.projects`.
    fn ws_with_project(cwd: &str, roots: Vec<PathBuf>) -> (Arc<Workspace>, SessionSlot) {
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.projects.push(crate::config::LoadedProject {
            name: "myproj".to_owned(),
            path: PathBuf::from(cwd),
            display_path: cwd.to_owned(),
            org: "TestOrg".to_owned(),
            accounts: Vec::new(),
            fallback_accounts: Vec::new(),
            model: None,
            env: std::collections::HashMap::new(),
            env_file: None,
            max_workers: None,
            permission_mode: forge_primitives::permission::PermissionMode::Auto,
            document_roots: roots,
            handoff: None,
            browser_profile: forge_primitives::browser::ProfileKey::of("TestOrg", "default")
                .expect("a profile key"),
            slack_workspace: None,
            engine: forge_agent::client::EngineSetting::cli(),
        });
        let (ws, _rx) = Workspace::testing_stub_with_config(
            PathBuf::from("/tmp/forge-document-roots-test"),
            config,
        )
        .expect("the stub builds");
        (ws, SessionSlot::lead("TestOrg", "myproj"))
    }

    /// **A waiter that goes away releases its park.** The document guard
    /// carries the same rule the browser and Slack ones do: a cancelled
    /// turn must not leave an ask no view can close.
    #[tokio::test]
    async fn a_hand_off_waiter_that_dies_is_reported_as_abandoned() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonical");
        std::fs::create_dir_all(base.join("tree/docs")).expect("the tree");
        std::fs::write(base.join("tree/docs/readme.md"), b"# hi\n").expect("the document");
        let (ws, seat) = ws_with_project(&base.join("tree").to_string_lossy(), Vec::new());
        let mut updates = ws.subscribe_answerer();

        let task = tokio::spawn({
            let ws = Arc::clone(&ws);
            let seat = seat.clone();
            async move { ws.hand_off_document(&seat, "docs/readme.md", false).await }
        });
        // The ask lands in the registry, which is what the blocked call just
        // did.
        let mut pending = false;
        for _ in 0..400 {
            while let Ok(frame) = updates.try_recv() {
                if matches!(
                    &*frame.update,
                    crate::protocol::SessionUpdate::DocumentHandOffPending { .. }
                ) {
                    pending = true;
                }
            }
            if pending {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(pending, "the blocked call parked the ask");

        task.abort();
        let _ = task.await;

        let mut resolved = Vec::new();
        while let Ok(frame) = updates.try_recv() {
            if let crate::protocol::SessionUpdate::DocumentHandOffResolved { ending, .. } =
                &*frame.update
            {
                resolved.push(ending.clone());
            }
        }
        assert_eq!(
            resolved,
            vec![DocumentHandOffEnding::Abandoned],
            "a dropped waiter says which ending took the ask",
        );
        assert!(ws.document_handoffs.lock().is_empty(), "and leaves nothing registered");
    }

    /// The hand-off consults the seat's PROJECT's document_roots: a file
    /// under one gets past the containment check and parks with no client
    /// attached, while the same call on a project declaring none is the
    /// outside-the-tree sentence - the control that the roots are what
    /// carried it.
    #[tokio::test]
    async fn the_hand_off_reads_through_the_seats_projects_document_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonical");
        let tree = base.join("tree");
        std::fs::create_dir_all(&tree).expect("the session tree");
        std::fs::create_dir_all(base.join("vault")).expect("the vault");
        std::fs::write(base.join("vault/plan.md"), b"# plan\n").expect("the vault document");
        let plan = base.join("vault/plan.md").to_string_lossy().into_owned();

        let (ws, seat) = ws_with_project(&tree.to_string_lossy(), vec![base.join("vault")]);
        let handed = {
            let ws = Arc::clone(&ws);
            let seat = seat.clone();
            let plan = plan.clone();
            tokio::spawn(async move { ws.hand_off_document(&seat, &plan, false).await })
        };
        let mut parked = false;
        for _ in 0..400 {
            if !ws.document_handoffs.lock().is_empty() {
                parked = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(parked, "the read got past containment and the hand-off parked");
        assert!(
            ws.pending_asks(&seat)
                .iter()
                .any(|ask| matches!(ask, crate::protocol::PendingAsk::DocumentHandOff(_))),
            "and a client that attaches later reads it from the seat",
        );
        handed.abort();
        let _ = handed.await;

        let (no_roots, no_roots_seat) = ws_with_project(&tree.to_string_lossy(), Vec::new());
        let refusal = no_roots
            .hand_off_document(&no_roots_seat, &plan, false)
            .await
            .expect_err("outside the tree and every root");
        assert!(
            refusal.contains("resolves outside the session's tree"),
            "the control is the refusal sentence: {refusal}"
        );
    }
}

//! The document hand-off's parked ask: one document handed to the reader,
//! held until their answer.
//!
//! Mirrors [`crate::browser`]'s relay shape rather than inventing a second
//! parked one: [`Workspace::hand_off_document`] registers the ask and answers
//! the call at once with its id, and a spawned waiter delivers the ending as
//! a follow-up turn; a client answers **not now** to decline, and the
//! viewer's **done** - after the reader's comments were filed - releases it
//! with what the finish filed. Nothing parks without a client attached to
//! draw the ask.

use std::sync::Arc;

use forge_primitives::document::{DocumentHandOffAsk, DocumentHandOffEnding};
use forge_primitives::review::doc_scope_ref;
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
        self.note_ask(seat);
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
    /// ask the clients drawing that seat to open it, and answer at once with
    /// the resolved path and the ask's id.
    ///
    /// **The reader's finish arrives later.** The `claude` CLI aborts an MCP
    /// `tools/call` after 70 seconds and a reader may take longer, so the
    /// ending is dispatched as a follow-up turn through `deliver_ask_answer`.
    /// A refused read raises nothing and parks on nothing.
    pub fn hand_off_document(
        self: &Arc<Self>,
        seat: &SessionSlot,
        path: &str,
        reviewable: bool,
    ) -> Result<(String, uuid::Uuid), String> {
        let Some(cwd) = self.cwd_for_session(seat) else {
            return Err(format!("forge holds no session for {}", seat.display()));
        };
        let read = crate::env::documents::read(
            std::path::Path::new(&cwd),
            &self.vault_roots_for_session(seat),
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
        let Some((id, ending)) = self.register_document_hand_off(seat, &resolved, reviewable)
        else {
            return Err(NO_VIEWER_CLIENT.to_owned());
        };
        let workspace = Arc::downgrade(self);
        let seat = seat.clone();
        let at = resolved.clone();
        tokio::spawn(async move {
            let ending = ending.await.unwrap_or(DocumentHandOffEnding::Abandoned);
            if let Some(workspace) = workspace.upgrade() {
                workspace.deliver_ask_answer(&seat, document_answer(&at, id, ending));
            }
        });
        Ok((resolved, id))
    }
}

/// The follow-up turn a finished document hand-off delivers.
fn document_answer(resolved: &str, id: uuid::Uuid, ending: DocumentHandOffEnding) -> String {
    match ending {
        DocumentHandOffEnding::Done { review_id: Some(review_id) } => format!(
            "[documents__hand_off {id}] The reader finished at {resolved}, and their comments are \
             filed as the review {review_id} under the document's own scope, {} - read them back \
             with review__get and pass that scope.",
            doc_scope_ref(resolved),
        ),
        DocumentHandOffEnding::Done { review_id: None } => format!(
            "[documents__hand_off {id}] The reader finished at {resolved} without leaving comments."
        ),
        DocumentHandOffEnding::NotNow => format!(
            "[documents__hand_off {id}] The reader said not now, so nothing opened. The document \
             is not being shown."
        ),
        DocumentHandOffEnding::Abandoned => format!(
            "[documents__hand_off {id}] The reader's client went away before they answered, so \
             the hand-off ended without an answer."
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use forge_primitives::document::DocumentHandOffEnding;

    use crate::SessionSlot;
    use crate::mcp::documents::facade::ProdDocumentFacade;
    use crate::workspace::Workspace;

    /// A workspace holding one project rooted at `cwd` with `roots` as
    /// its vault roots, and the project's lead slot. The project is
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
            auto_start: false,
            model: None,
            env: std::collections::HashMap::new(),
            env_file: None,
            max_workers: None,
            permission_mode: forge_primitives::permission::PermissionMode::Auto,
            vault_roots: roots,
            handoff: None,
            browser_profile: forge_primitives::browser::ProfileKey::of("TestOrg", "default")
                .expect("a profile key"),
            slack_workspace: None,
        });
        let (ws, _rx) = Workspace::testing_stub_with_config(
            PathBuf::from("/tmp/forge-vault-roots-test"),
            config,
        )
        .expect("the stub builds");
        (ws, SessionSlot::lead("TestOrg", "myproj"))
    }

    /// The hand-off consults the seat's PROJECT's vault_roots: a file
    /// under one gets past the containment check - with no client
    /// attached the park is what refuses it - while the same call on a
    /// project declaring none is the outside-the-tree sentence, the
    /// control that the roots are what carried it.
    #[tokio::test]
    async fn the_hand_off_reads_through_the_seats_projects_vault_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonical");
        let tree = base.join("tree");
        std::fs::create_dir_all(&tree).expect("the session tree");
        std::fs::create_dir_all(base.join("vault")).expect("the vault");
        std::fs::write(base.join("vault/plan.md"), b"# plan\n").expect("the vault document");
        let plan = base.join("vault/plan.md").to_string_lossy().into_owned();

        let (ws, seat) = ws_with_project(&tree.to_string_lossy(), vec![base.join("vault")]);
        let refusal = ws
            .hand_off_document(&seat, &plan, false)
            .expect_err("no client is attached, so the ask is what refuses it");
        assert_eq!(refusal, super::NO_VIEWER_CLIENT, "the read got past containment");

        let (no_roots, no_roots_seat) = ws_with_project(&tree.to_string_lossy(), Vec::new());
        let refusal = no_roots
            .hand_off_document(&no_roots_seat, &plan, false)
            .expect_err("outside the tree and every root");
        assert!(
            refusal.contains("resolves outside the session's tree"),
            "the control is the refusal sentence: {refusal}"
        );
    }

    /// **The CLI's own boundary.** The `claude` binary aborts an MCP
    /// `tools/call` after 70 seconds, so a hand-off that waited on the reader
    /// would die mid-ask. This drives the wire's own `mcp_message`
    /// control_request, and the mock CLI finishes its turn only once the tool
    /// has answered - which a waiting call never does.
    #[tokio::test]
    async fn the_hand_off_tool_answers_the_cli_while_the_reader_has_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonical");
        std::fs::create_dir_all(base.join("tree/docs")).expect("the tree");
        std::fs::write(base.join("tree/docs/readme.md"), b"# hi\n").expect("the document");
        let (ws, seat) = ws_with_project(&base.join("tree").to_string_lossy(), Vec::new());
        ws.note_client_attached();
        // The ask needs an answering stream to park on, and the stub's own
        // receiver is dropped when the helper returns.
        let _answerer = ws.subscribe_answerer();

        let server =
            crate::mcp::documents::build_server(ProdDocumentFacade::from_workspace(&ws), seat);
        let binary = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mock_claude_mcp_ask.sh");
        let opts = forge_sdk::OptionsBuilder::new()
            .binary(binary)
            .permission_mode(forge_sdk::PermissionMode::BypassPermissions)
            .mcp_server("forge", server)
            .build();
        let (client, mut events) = forge_sdk::Client::spawn(opts).await.expect("spawn");
        client.send_user_message("hand the document off").await.expect("send");

        let mut finished = false;
        while let Ok(Some(item)) =
            tokio::time::timeout(std::time::Duration::from_secs(15), events.recv()).await
        {
            let msg = item.expect("read");
            if matches!(msg, forge_primitives::Message::Result { .. }) {
                finished = true;
                break;
            }
        }
        assert!(finished, "the mock's turn ended, so the tools/call was answered");
        assert_eq!(
            ws.document_handoffs.lock().len(),
            1,
            "the ask is still parked - the call answered before the reader did",
        );
        client.disconnect().await.expect("disconnect");
    }

    /// The follow-up names the ask it answers, so the agent can match it to
    /// the call that raised it; a finish that filed a review also names the
    /// scope it landed under and the review's id, while a finish with
    /// nothing commented names neither.
    #[test]
    fn a_finished_hand_offs_follow_up_names_the_review_and_its_scope() {
        let id = uuid::Uuid::new_v4();
        let with_review = super::document_answer(
            "/proj/docs/readme.md",
            id,
            DocumentHandOffEnding::Done { review_id: Some("r-7".to_owned()) },
        );
        assert!(with_review.contains(&id.to_string()), "the ask is named: {with_review}");
        assert!(with_review.contains("r-7"), "the review is named: {with_review}");
        assert!(with_review.contains("doc:/proj/docs/readme.md"), "and the scope: {with_review}");

        let without = super::document_answer(
            "/proj/docs/readme.md",
            id,
            DocumentHandOffEnding::Done { review_id: None },
        );
        assert!(!without.contains("doc:"), "nothing commented, so no scope: {without}");
    }

    /// A decline and an abandoned client each say so, in their own words:
    /// neither may read as a finish.
    #[test]
    fn a_declined_or_abandoned_follow_up_says_which_it_was() {
        let id = uuid::Uuid::new_v4();
        let declined =
            super::document_answer("/proj/docs/readme.md", id, DocumentHandOffEnding::NotNow);
        assert!(declined.contains("not now"), "{declined}");
        assert!(!declined.contains("finished"), "a decline is not a finish: {declined}");

        let abandoned =
            super::document_answer("/proj/docs/readme.md", id, DocumentHandOffEnding::Abandoned);
        assert!(abandoned.contains("went away"), "{abandoned}");
        assert!(!abandoned.contains("finished"), "an abandoned ask is not a finish: {abandoned}");
    }
}

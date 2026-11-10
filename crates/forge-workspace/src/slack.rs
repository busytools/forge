//! The workspace's half of the Slack subsystem's seams.
//!
//! The subsystem owns its state: the clients, the subscription set each pump
//! sweeps, the durable cursors and followed threads, the held drafts and the
//! pumps' own life. The workspace owns what every host owns - the redb store,
//! the update fanout, the ask sequence, the project catalogue and the
//! delivery path into a session - and the two meet here.
//!
//! [`SlackPorts`] implements [`SlackStore`], [`SlackEmit`] and [`AskSink`];
//! [`build`] assembles the handle the boot path and the facade keep. Nothing
//! in this module holds the subsystem's state: `slack` is a handle, and the
//! subsystem reads the workspace only through the seams.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use forge_primitives::slack::{SlackMessage, SlackSubscription, SlackThreadRecord};
use forge_primitives::{AskSink, PendingInteractionKind, SessionSlot};
use forge_tools::slack::{Slack, SlackEmit, SlackSeams, SlackStore, SlackUpdate, SlackWorkspaces};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::protocol::SessionUpdate;
use crate::target::ProjectKey;
use crate::update_fanout::UpdateFanout;
use crate::workspace::Workspace;

/// The subsystem's seams over this workspace: the durable store, the wire
/// frames, the ask numbering and the two host actions that are not a frame.
pub(crate) struct SlackPorts {
    db: Arc<Mutex<Option<crate::store::Db>>>,
    updates: UpdateFanout,
    pending_seq: Arc<Mutex<HashMap<SessionSlot, u64>>>,
    /// Set once the workspace exists as an `Arc`: the announcement reads the
    /// project catalogue and the sibling connector's set, and a delivery
    /// enters a session's task, so neither can be answered from a store
    /// handle alone.
    workspace: std::sync::OnceLock<std::sync::Weak<Workspace>>,
}

impl SlackPorts {
    pub(crate) fn new(
        db: Arc<Mutex<Option<crate::store::Db>>>,
        updates: UpdateFanout,
        pending_seq: Arc<Mutex<HashMap<SessionSlot, u64>>>,
    ) -> Self {
        Self { db, updates, pending_seq, workspace: std::sync::OnceLock::new() }
    }

    /// Bind the two host actions that need the workspace itself. Called from
    /// every `Arc<Workspace>` entry the Slack subsystem can be reached
    /// through; setting it twice is a no-op.
    pub(crate) fn bind(&self, workspace: &Arc<Workspace>) {
        let _ = self.workspace.set(Arc::downgrade(workspace));
    }

    fn host(&self) -> Option<Arc<Workspace>> {
        self.workspace.get()?.upgrade()
    }
}

impl SlackStore for SlackPorts {
    fn available(&self) -> bool {
        self.db.lock().is_some()
    }

    fn insert_subscription(&self, subscription: &SlackSubscription) -> Result<(), String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(()) };
        crate::store::slack::insert(db, subscription).map_err(|error| error.to_string())
    }

    fn list_subscriptions(&self) -> Result<Vec<SlackSubscription>, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(Vec::new()) };
        crate::store::slack::list(db).map_err(|error| error.to_string())
    }

    fn remove_subscription(&self, id: Uuid) -> Result<bool, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(false) };
        crate::store::slack::remove(db, id).map_err(|error| error.to_string())
    }

    fn watermark(&self, workspace: &str, conversation: &str) -> Result<Option<String>, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(None) };
        crate::store::slack::watermark(db, workspace, conversation)
            .map_err(|error| error.to_string())
    }

    fn set_watermark(&self, workspace: &str, conversation: &str, ts: &str) -> Result<(), String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(()) };
        crate::store::slack::set_watermark(db, workspace, conversation, ts)
            .map_err(|error| error.to_string())
    }

    fn remove_watermark(&self, workspace: &str, conversation: &str) -> Result<bool, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(false) };
        crate::store::slack::remove_watermark(db, workspace, conversation)
            .map_err(|error| error.to_string())
    }

    fn watermark_conversations(&self, workspace: &str) -> Result<Vec<String>, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(Vec::new()) };
        crate::store::slack::watermark_conversations(db, workspace)
            .map_err(|error| error.to_string())
    }

    fn thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Result<Option<SlackThreadRecord>, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(None) };
        crate::store::slack::thread(db, workspace, conversation, parent_ts)
            .map_err(|error| error.to_string())
    }

    fn set_thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        record: &SlackThreadRecord,
    ) -> Result<(), String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(()) };
        crate::store::slack::set_thread(db, workspace, conversation, parent_ts, record)
            .map_err(|error| error.to_string())
    }

    fn threads_for(
        &self,
        workspace: &str,
        conversation: &str,
    ) -> Result<Vec<(String, SlackThreadRecord)>, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(Vec::new()) };
        crate::store::slack::threads_for(db, workspace, conversation)
            .map_err(|error| error.to_string())
    }

    fn threads_for_workspace(
        &self,
        workspace: &str,
    ) -> Result<Vec<(String, String, SlackThreadRecord)>, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(Vec::new()) };
        crate::store::slack::threads_for_workspace(db, workspace).map_err(|error| error.to_string())
    }

    fn remove_thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Result<bool, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(false) };
        crate::store::slack::remove_thread(db, workspace, conversation, parent_ts)
            .map_err(|error| error.to_string())
    }

    fn own_post(&self, workspace: &str, conversation: &str, ts: &str) -> Result<bool, String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(false) };
        crate::store::slack::own_post(db, workspace, conversation, ts)
            .map_err(|error| error.to_string())
    }

    fn record_own_post(
        &self,
        workspace: &str,
        conversation: &str,
        ts: &str,
        now: u64,
    ) -> Result<(), String> {
        let db = self.db.lock();
        let Some(db) = db.as_ref() else { return Ok(()) };
        crate::store::slack::record_own_post(db, workspace, conversation, ts, now)
            .map_err(|error| error.to_string())
    }
}

impl SlackEmit for SlackPorts {
    fn emit(&self, update: SlackUpdate) -> bool {
        match update {
            // A composed draft is only answerable by a subscriber that
            // renders a dock, so this is the answering send: `false` is what
            // makes the blocked tool call fail closed instead of parking for
            // ever.
            SlackUpdate::PostPending { key, draft } => {
                self.updates.send_answering(SessionUpdate::SlackPostPending { key, draft })
            }
            SlackUpdate::DraftResolved { key, id, ending } => {
                self.updates.send(SessionUpdate::SlackDraftResolved { key, id, ending })
            }
        }
    }

    fn subscriptions_changed(&self, project: &str) {
        // `ConnectorSubscriptionsChanged` carries both connectors' sets -
        // the section a client patches is one - so the workspace composes it
        // rather than the subsystem knowing gotify exists.
        if let Some(workspace) = self.host() {
            workspace.announce_connector_subscriptions_changed(project);
        }
    }

    fn deliver(&self, project: &str, team_role: Option<&str>, messages: &[SlackMessage]) -> bool {
        let Some(workspace) = self.host() else { return false };
        // Direct, not via the command bus: the pump needs the delivery's
        // own outcome, and a bus round-trip reports only that the command
        // was accepted - a mid-delivery failure would read as success and
        // advance the cursor past an undelivered message.
        crate::spawn::deliver_slack_message(&workspace, project, team_role, messages.to_vec())
    }
}

impl AskSink for SlackPorts {
    fn note(&self, seat: &SessionSlot, _kind: PendingInteractionKind) {
        // The one counter the workspace's own registries bump too: the number
        // is the ask's identity on the wire.
        crate::workspace::note_ask_in(&self.pending_seq, seat);
    }

    fn resolve(&self, _seat: &SessionSlot, _id: Uuid) {
        // **The source's own read clears the mark.** A settled draft leaves
        // the registry, and `AskSource::asks` is what the needs-you read
        // walks, so there is no per-ask state here to clear.
    }
}

/// Assemble the subsystem and its ports. The HTTP factory is the workspace's
/// because the extra TLS roots are forge-agent's, which the tool crate may
/// not depend on; every other seam is a field on the ports.
pub(crate) fn build(
    clients: Arc<SlackWorkspaces>,
    configs: Vec<forge_primitives::slack::SlackConfig>,
    subscriptions: Vec<SlackSubscription>,
    load_failed: bool,
    db: Arc<Mutex<Option<crate::store::Db>>>,
    updates: UpdateFanout,
    pending_seq: Arc<Mutex<HashMap<SessionSlot, u64>>>,
) -> (Arc<Slack>, Arc<SlackPorts>) {
    let ports = Arc::new(SlackPorts::new(db, updates, pending_seq));
    let http: Arc<forge_tools::slack::PumpHttp> = Arc::new(|timeout| {
        forge_agent::http_trust::with_extra_roots(reqwest::Client::builder().timeout(timeout))
            .build()
            .map_err(|error| error.to_string())
    });
    let slack = Arc::new(Slack::new(
        clients,
        configs,
        subscriptions,
        load_failed,
        SlackSeams {
            store: Arc::clone(&ports) as Arc<dyn SlackStore>,
            emit: Arc::clone(&ports) as Arc<dyn SlackEmit>,
            sink: Arc::clone(&ports) as Arc<dyn AskSink>,
            http,
        },
    ));
    (slack, ports)
}

impl Workspace {
    /// Bind the subsystem's host callbacks to this workspace. Idempotent.
    pub(crate) fn bind_slack(self: &Arc<Self>) {
        self.slack_ports.bind(self);
    }

    /// Prove each configured `[[slack]]` token once. The host action that
    /// needs the workspace, so this is also where the binding lands before
    /// any subscribe can announce.
    pub fn start_slack_verification(self: &Arc<Self>) {
        self.bind_slack();
        self.slack.start_slack_verification();
    }

    /// Start one pump per configured workspace that has a subscription.
    pub fn start_slack_subsystem(self: &Arc<Self>) {
        self.bind_slack();
        self.slack.start_slack_subsystem();
    }

    /// Stop the pumps whose workspace no longer has a subscription.
    pub fn stop_slack_subsystem_if_idle(&self) {
        self.slack.stop_slack_subsystem_if_idle();
    }

    /// Per-workspace pump liveness, for the Inspector's SLACK section.
    pub fn slack_connected_workspaces(&self) -> BTreeMap<String, bool> {
        self.slack.slack_connected_workspaces()
    }

    /// Every Slack subscription owned in `project`, for the connectors
    /// surface's Slack section.
    pub fn slack_subscriptions_for_project(
        &self,
        project: &str,
    ) -> Vec<forge_primitives::slack::SlackSubscription> {
        self.slack.slack_subscriptions_for_project(project)
    }

    /// Whether boot could not read the durable Slack subscriptions.
    pub fn slack_subscription_load_failed(&self) -> bool {
        self.slack.slack_subscription_load_failed()
    }

    /// Remove every Slack subscription the worker `label` owns in the project
    /// `project_key` names. The project key is the workspace's catalogue; the
    /// subscription set is the subsystem's.
    pub(crate) fn remove_slack_subscriptions_for_worker(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) {
        let Some(project_name) = self
            .list_projects()
            .into_iter()
            .find(|view| view.key == *project_key)
            .map(|view| view.name)
        else {
            tracing::warn!(
                target: "forge_workspace::slack",
                project = %project_key.as_str(),
                label,
                "could not resolve a project name at worker teardown; its Slack subscriptions may be stranded",
            );
            return;
        };
        self.slack.remove_slack_subscriptions_for_worker(&project_name, label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use forge_primitives::slack::{
        SlackConfig, SlackDraft, SlackDraftAnswer, SlackDraftEnding, SlackSubscriptionTarget,
        SlackWatchMode,
    };
    use forge_tools::slack::{DraftResolution, SlackHost as _, SlackUpdate};

    fn cfg(workspace: &str, token: &str) -> SlackConfig {
        SlackConfig {
            workspace: workspace.to_owned(),
            token: token.to_owned(),
            poll_seconds: 30,
            dm_poll_seconds: 150,
            thread_idle_days: 14,
            stale_after_hours: 24,
        }
    }

    fn sub_for(project: &str, team_role: Option<&str>) -> SlackSubscription {
        SlackSubscription {
            id: Uuid::new_v4(),
            workspace: "acme".to_owned(),
            project: project.to_owned(),
            team_role: team_role.map(str::to_owned),
            target: SlackSubscriptionTarget::DirectMessages,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn sub_for_conversation(project: &str, team_role: Option<&str>, id: &str) -> SlackSubscription {
        let mut sub = sub_for(project, team_role);
        sub.target = SlackSubscriptionTarget::Conversation {
            id: id.to_owned(),
            name: None,
            mode: SlackWatchMode::All,
        };
        sub
    }

    /// A workspace whose `[[slack]]` holds exactly `label`. The tempdir
    /// is returned so it outlives the workspace it names.
    fn workspace_with_one_slack_workspace(
        label: &str,
    ) -> (Arc<Workspace>, tempfile::TempDir, tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>)
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.slack = vec![cfg(label, "xoxp-test")];
        let (ws, rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
            .expect("the stub config's [[slack]] entries are well-formed");
        (ws, dir, rx)
    }

    fn answered(approved: bool) -> DraftResolution {
        DraftResolution::Answered(if approved {
            SlackDraftAnswer::Approve
        } else {
            SlackDraftAnswer::Decline
        })
    }

    fn draft(workspace: &str, conversation: &str) -> SlackDraft {
        SlackDraft {
            id: Uuid::new_v4(),
            workspace: workspace.to_owned(),
            conversation: conversation.to_owned(),
            conversation_label: conversation.to_owned(),
            thread_ts: None,
            text: "hello".to_owned(),
            operation: forge_primitives::slack::SlackDraftOperation::Post,
            thread: None,
            tagged: Vec::new(),
        }
    }

    /// Through `Workspace::dispatch`, the route a view's dock answer takes:
    /// the registry the subsystem owns is reached through the handle.
    #[tokio::test]
    async fn the_dock_answer_reaches_the_registry_through_dispatch() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        let caller = SessionSlot::worker("TestOrg", "forge", "caller");
        let (id, decision) = ws.slack.register_slack_draft(&caller, draft("acme", "C1"));

        ws.dispatch(crate::protocol::Command::RespondSlackPost {
            key: caller.clone(),
            id,
            answer: SlackDraftAnswer::Approve,
        })
        .expect("the dock answer routes to the workspace");

        assert_eq!(
            decision.await.expect("the held draft answers"),
            SlackDraftAnswer::Approve,
            "the answer the dock gave reaches the awaiting caller",
        );
    }

    /// A click on a dock the core has already resolved is refused with the
    /// reason, rather than dropped: the reader learns why the click did
    /// nothing.
    #[tokio::test]
    async fn a_stale_dock_answer_is_refused_with_the_reason() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        let caller = SessionSlot::worker("TestOrg", "forge", "caller");
        let (id, _decision) = ws.slack.register_slack_draft(&caller, draft("acme", "C1"));
        assert!(ws.slack.resolve_slack_draft(id, &caller, answered(true)), "the draft was waiting");

        let refused = ws.dispatch(crate::protocol::Command::RespondSlackPost {
            key: caller,
            id,
            answer: SlackDraftAnswer::Approve,
        });

        assert!(
            matches!(refused, Err(crate::protocol::DispatchError::NoDraftWaiting { .. })),
            "a stale dock must report why the answer did not land: {refused:?}",
        );
    }

    /// The guard's owner half: a draft is answered by the session it was
    /// addressed to, so another session's dispatch is refused rather than
    /// reporting an answer that never landed.
    #[tokio::test]
    async fn another_session_answering_through_dispatch_is_refused() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        let asker = SessionSlot::worker("TestOrg", "forge", "worker");
        let other = SessionSlot::lead("TestOrg", "forge");
        let (id, _decision) = ws.slack.register_slack_draft(&asker, draft("acme", "C1"));

        let refused = ws.dispatch(crate::protocol::Command::RespondSlackPost {
            key: other,
            id,
            answer: SlackDraftAnswer::Approve,
        });

        assert!(
            matches!(refused, Err(crate::protocol::DispatchError::NoDraftWaiting { .. })),
            "another session's answer must be refused, not reported as landed: {refused:?}",
        );
        assert!(
            ws.slack.resolve_slack_draft(id, &asker, answered(true)),
            "and the draft is still the asking session's to answer",
        );
    }

    /// A send-back with nothing in it has no words to hand the agent, and
    /// resolving it would drop the reader's card for an answer that says
    /// nothing. The draft stays waiting so the field can be sent again.
    #[tokio::test]
    async fn an_empty_send_back_note_never_resolves_the_draft() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        let caller = SessionSlot::worker("TestOrg", "forge", "caller");
        let (id, _decision) = ws.slack.register_slack_draft(&caller, draft("acme", "C1"));

        let refused = ws.dispatch(crate::protocol::Command::RespondSlackPost {
            key: caller.clone(),
            id,
            answer: SlackDraftAnswer::SendBack { note: "  \n ".to_owned() },
        });

        assert!(
            matches!(refused, Err(crate::protocol::DispatchError::EmptySlackFeedback { .. })),
            "an empty note is refused by name: {refused:?}",
        );
        assert!(
            ws.slack.resolve_slack_draft(id, &caller, answered(false)),
            "and the draft is still waiting for a real answer",
        );
    }

    /// **Every one of the subsystem's updates crosses to its own wire
    /// frame**, and the draft arm uses the ANSWERING send. With only an
    /// observer attached that arm must report "nobody can answer" - swapped
    /// to the plain send, a draft would look answerable while no dock could
    /// ever open, and the blocked tool call would park forever.
    #[test]
    fn the_draft_updates_cross_to_their_wire_frames() {
        use crate::update_fanout::{SubscriberRole, UpdateFanout};

        let updates = UpdateFanout::default();
        let seq: Arc<Mutex<HashMap<SessionSlot, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let ports = SlackPorts::new(Arc::new(Mutex::new(None)), updates.clone(), seq);
        let key = SessionSlot::lead("TestOrg", "forge");
        let draft = draft("acme", "C1");

        let mut observer = updates.subscribe(SubscriberRole::Observing);
        assert!(
            !ports.emit(SlackUpdate::PostPending { key: key.clone(), draft: draft.clone() }),
            "an observer alone cannot answer a held draft",
        );
        // The frame still reaches the observer - it draws no dock, and the
        // answering send's report is what fails the call closed rather than
        // parking it forever.
        assert!(observer.try_recv().is_ok(), "the observer still sees the draft was raised");

        let mut answerer = updates.subscribe(SubscriberRole::Answering);
        assert!(
            ports.emit(SlackUpdate::PostPending { key: key.clone(), draft: draft.clone() }),
            "an answering subscriber makes the draft answerable",
        );
        match std::sync::Arc::unwrap_or_clone(answerer.try_recv().expect("the card").update) {
            SessionUpdate::SlackPostPending { key: got, draft: got_draft } => {
                assert_eq!(got, key, "the draft is addressed to the seat that asked");
                assert_eq!(got_draft.id, draft.id);
            }
            other => panic!("the held draft crossed as {other:?}"),
        }

        assert!(ports.emit(SlackUpdate::DraftResolved {
            key: key.clone(),
            id: draft.id,
            ending: SlackDraftEnding::Answered { approved: true },
        }));
        match std::sync::Arc::unwrap_or_clone(answerer.try_recv().expect("the ending").update) {
            SessionUpdate::SlackDraftResolved { key: got, id, ending } => {
                assert_eq!(got, key);
                assert_eq!(id, draft.id);
                assert_eq!(ending, SlackDraftEnding::Answered { approved: true });
            }
            other => panic!("the stand-down crossed as {other:?}"),
        }
    }

    /// `tokio::test`: starting a subsystem spawns a pump, which needs a
    /// runtime.
    #[tokio::test]
    async fn the_subsystem_does_not_start_without_subscriptions() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        ws.start_slack_subsystem();
        assert!(
            ws.slack.pump_labels().is_empty(),
            "a pump with nothing to watch is a task that only burns a timer",
        );
    }

    #[tokio::test]
    async fn the_subsystem_starts_once_a_subscription_exists() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        ws.slack.add_slack_subscription(sub_for("acme", None), true);

        ws.start_slack_subsystem();
        assert_eq!(
            ws.slack.pump_labels(),
            vec!["acme".to_owned()],
            "one pump for the one configured workspace",
        );

        let id = ws.slack.slack_subscriptions_for_project("acme")[0].id;
        ws.slack.remove_slack_subscription_owned_by("acme", id, None).expect("the lead's own row");
        ws.stop_slack_subsystem_if_idle();
        assert!(ws.slack.pump_labels().is_empty(), "the pump stops with its last subscription");
    }

    /// The port writes through to the real store: the sweep's backfill is
    /// what names a watched conversation, and a name that only lived in
    /// memory would come back unnamed after the restart that produced it.
    #[test]
    fn a_backfilled_conversation_name_reaches_the_store() {
        let (ws, dir, _rx) = workspace_with_one_slack_workspace("acme");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.slack.add_slack_subscription(sub_for_conversation("forge", None, "C1"), true);

        ws.slack.name_slack_conversation("acme", "C1", "ved-test");

        let db = ws.db.lock();
        let stored = crate::store::slack::list(db.as_ref().expect("db installed")).expect("list");
        let named = stored.iter().find_map(|sub| match &sub.target {
            SlackSubscriptionTarget::Conversation { id, name, .. } if id == "C1" => {
                Some(name.clone())
            }
            _ => None,
        });
        assert_eq!(
            named,
            Some(Some("ved-test".to_owned())),
            "the backfilled name is persisted, not only held in memory",
        );
    }

    /// Unsubscribing the last subscription covering a conversation clears
    /// its cursor through the port, so "has cursor" means a live pending
    /// window and a later subscriber starts fresh instead of replaying the
    /// gap.
    #[test]
    fn unsubscribing_the_last_subscription_clears_the_conversation_cursor() {
        let (ws, dir, _rx) = workspace_with_one_slack_workspace("acme");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.slack.add_slack_subscription(sub_for_conversation("forge", None, "C1"), true);
        ws.slack.set_slack_watermark("acme", "C1", "100.0");

        let id = ws.slack.slack_subscriptions_for_project("forge")[0].id;
        assert!(ws.slack.remove_slack_subscription_owned_by("forge", id, None).is_some());

        let db = ws.db.lock();
        assert_eq!(
            crate::store::slack::watermark(db.as_ref().expect("db installed"), "acme", "C1")
                .expect("read"),
            None,
            "a cursor with no watcher is a stale backlog waiting to replay",
        );
    }

    /// A row written before the edit stamp was stored has none. Its cursor
    /// is the last position the row judged, so it reads as its stamp through
    /// the port, and the seed is written back.
    #[test]
    fn a_thread_row_without_a_stored_edit_stamp_reads_its_cursor() {
        let (ws, dir, _rx) = workspace_with_one_slack_workspace("acme");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        // The shape a row written before the field existed has: no key at
        // all, not a null. Serde reads both as no stamp, but only the absent
        // key pins the migration this row is.
        let legacy = r#"{"cursor":"500.0","owners":[{"project":"forge","team_role":"tester"}],"participating":true}"#;
        {
            let db = ws.db.lock();
            let db = db.as_ref().expect("db installed");
            let txn = db.database().begin_write().expect("begin a write");
            {
                let table: redb::TableDefinition<&str, &str> =
                    redb::TableDefinition::new("slack_threads");
                let mut table = txn.open_table(table).expect("open the thread table");
                table.insert("acme\u{0}C1\u{0}100.0", legacy).expect("insert the legacy row");
            }
            txn.commit().expect("commit the legacy row");
        }

        let host = forge_tools::slack::SlackSubsystemHost::new(&ws.slack);
        assert_eq!(
            host.thread_edit_watermark("acme", "C1", "100.0").as_deref(),
            Some("500.0"),
            "an older row judges up to its cursor, not from the beginning",
        );
        let seeded = {
            let db = ws.db.lock();
            crate::store::slack::thread(db.as_ref().expect("db installed"), "acme", "C1", "100.0")
                .expect("read")
                .expect("the row survives")
        };
        assert_eq!(
            seeded.edit_watermark.as_deref(),
            Some("500.0"),
            "and the seed is persisted, so the next restart reads it back",
        );
    }

    /// The one `ConnectorSubscriptionsChanged` on a test's update stream, as
    /// its key and the two sets it announces.
    fn next_connectors_changed(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
    ) -> (SessionSlot, Vec<forge_primitives::GotifySubscription>, Vec<SlackSubscription>) {
        match rx.try_recv().map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
            Ok(crate::protocol::SessionUpdate::ConnectorSubscriptionsChanged {
                key,
                gotify,
                slack,
            }) => (key, gotify, slack),
            other => {
                panic!("expected a ConnectorSubscriptionsChanged on the stream, got {other:?}")
            }
        }
    }

    /// A slack subscribe, a watch that moves a mode, an unsubscribe and a
    /// worker teardown each announce the project's connector sets on its
    /// lead seat - both connectors, because the section a client patches is
    /// one - and a watch that moves nothing announces nothing.
    ///
    /// Mutants: drop any one emission; announce a set taken before the
    /// write; announce a same-mode watch, which is a write the section did
    /// not move for.
    #[test]
    fn every_slack_write_announces_the_projects_subscriptions() {
        let (ws, _dir, mut rx) = workspace_with_one_slack_workspace("acme");
        ws.seed_test_project("forge", "/tmp/tp-slack");
        let lead = SessionSlot::lead("TestOrg", "forge");

        ws.slack.add_slack_subscription(sub_for("forge", None), true);
        let (key, gotify, slack) = next_connectors_changed(&mut rx);
        assert_eq!(key, lead, "a subscribe routes on the project's lead seat");
        assert_eq!(slack.len(), 1, "and carries the set the core now holds");
        assert!(gotify.is_empty(), "beside the sibling connector's own set");

        // A watch of a conversation lands as a new record.
        let watched = ws.slack.watch_slack_conversation(
            "acme",
            "forge",
            None,
            "C1",
            SlackWatchMode::All,
            true,
        );
        let id = watched.id;
        let (_, _, slack) = next_connectors_changed(&mut rx);
        assert!(slack.iter().any(|sub| sub.id == id), "a new watch is announced: {slack:?}");

        // The same watch again moves nothing, so it announces nothing.
        let same = ws.slack.watch_slack_conversation(
            "acme",
            "forge",
            None,
            "C1",
            SlackWatchMode::All,
            true,
        );
        assert_eq!(same.id, id, "precondition: the watch found the record it had");
        let announced = rx.try_recv();
        assert!(
            announced.is_err(),
            "a watch that moved nothing announces nothing, and this was: {announced:?}",
        );

        // A watch whose mode differs is the one write that changes an
        // existing record, and it is announced as the moved set.
        ws.slack.watch_slack_conversation(
            "acme",
            "forge",
            None,
            "C1",
            SlackWatchMode::MentionsOnly,
            true,
        );
        let (_, _, slack) = next_connectors_changed(&mut rx);
        assert!(
            slack.iter().any(|sub| {
                sub.id == id
                    && sub.target
                        == SlackSubscriptionTarget::Conversation {
                            id: "C1".to_owned(),
                            name: None,
                            mode: SlackWatchMode::MentionsOnly,
                        }
            }),
            "the mode the watch moved to is what is announced: {slack:?}",
        );

        assert!(
            ws.slack.remove_slack_subscription_owned_by("forge", id, None).is_some(),
            "precondition: the lead removes its own subscription",
        );
        let (_, _, slack) = next_connectors_changed(&mut rx);
        assert!(
            slack.iter().all(|sub| sub.id != id),
            "an unsubscribe announces the set it left behind: {slack:?}",
        );

        assert!(
            ws.slack.remove_slack_subscription_owned_by("forge", Uuid::new_v4(), None).is_none(),
            "precondition: no subscription carries the id",
        );
        let announced = rx.try_recv();
        assert!(
            announced.is_err(),
            "a refused unsubscribe announces nothing, and this was: {announced:?}",
        );
    }

    /// A name heal is the COMMON way a set moves, and the client draws the
    /// name - so a heal that announces nothing leaves the row unnamed until
    /// some unrelated write.
    #[test]
    fn a_name_heal_announces_the_projects_subscriptions() {
        let (ws, _dir, mut rx) = workspace_with_one_slack_workspace("acme");
        ws.seed_test_project("forge", "/tmp/tp-slack-heal");
        let lead = SessionSlot::lead("TestOrg", "forge");

        let id = ws
            .slack
            .watch_slack_conversation("acme", "forge", None, "C1", SlackWatchMode::All, true)
            .id;
        let (_, _, slack) = next_connectors_changed(&mut rx);
        assert!(
            slack.iter().any(|sub| sub.id == id),
            "precondition: the watch announced the record it created, unnamed: {slack:?}",
        );

        ws.slack.name_slack_conversation("acme", "C1", "general");
        let (announced_key, _, slack) = next_connectors_changed(&mut rx);
        assert_eq!(announced_key, lead, "a heal routes on the project's lead seat");
        assert!(
            slack.iter().any(|sub| {
                sub.id == id
                    && sub.target
                        == SlackSubscriptionTarget::Conversation {
                            id: "C1".to_owned(),
                            name: Some("general".to_owned()),
                            mode: SlackWatchMode::All,
                        }
            }),
            "and carries the name the heal wrote: {slack:?}",
        );

        // A second heal with the same name touches no record - only unnamed
        // ones are healed - so it announces nothing.
        ws.slack.name_slack_conversation("acme", "C1", "general");
        let announced = rx.try_recv();
        assert!(
            announced.is_err(),
            "a heal that named nothing new announces nothing, and this was: {announced:?}",
        );
    }

    /// A worker teardown announces the subscriptions it left, which is the
    /// other door the section moves through - and one that matched nothing
    /// announces nothing.
    #[test]
    fn a_slack_teardown_announces_the_survivors() {
        let (ws, _dir, mut rx) = workspace_with_one_slack_workspace("acme");
        ws.seed_test_project("forge", "/tmp/tp-slack-teardown");
        let view_key = ws.project_key_for_name("forge").expect("seeded project");
        let lead = SessionSlot::lead("TestOrg", "forge");

        ws.slack.add_slack_subscription(sub_for("forge", None), true);
        ws.slack.add_slack_subscription(sub_for("forge", Some("reviewer")), true);
        let (_, _, slack) = next_connectors_changed(&mut rx);
        assert_eq!(slack.len(), 1, "precondition: the lead's subscribe announced it");
        let (_, _, slack) = next_connectors_changed(&mut rx);
        assert_eq!(slack.len(), 2, "precondition: and the worker's announced the whole set");

        ws.remove_slack_subscriptions_for_worker(&view_key, "reviewer");
        let (announced_key, _, slack) = next_connectors_changed(&mut rx);
        assert_eq!(announced_key, lead, "a teardown routes on the project's lead seat");
        assert!(
            slack.iter().all(|sub| sub.team_role.is_none()),
            "and announces what the teardown left: {slack:?}",
        );

        // A teardown matching no subscription of that label is not a write,
        // so it announces nothing.
        ws.remove_slack_subscriptions_for_worker(&view_key, "nobody");
        let announced = rx.try_recv();
        assert!(
            announced.is_err(),
            "a teardown that took nothing announces nothing, and this was: {announced:?}",
        );
    }

    /// The workspace label the live test configures, and the project whose
    /// running lead receives the block.
    const LIVE_WORKSPACE: &str = "live-test";
    const LIVE_PROJECT_PATH: &str = "/tmp/forge-slack-live";

    /// Layer-below integration test: a real Slack workspace, a real sweep,
    /// and the block the session is handed.
    ///
    /// Opt-in, because it needs three things a CI machine has not - a user
    /// token, a bot token, and a channel the bot can post into. Absent any
    /// of them it skips, so the suite stays green without them:
    ///
    /// ```text
    /// SLACK_TEST_USER_TOKEN=... SLACK_TEST_BOT_TOKEN=... SLACK_TEST_CHANNEL=... \
    ///     cargo nextest run -p forge-workspace slack_live_bundle
    /// ```
    ///
    /// The bot posts and the user reads, so the reader never filters the
    /// poster's own messages out of the sweep.
    #[tokio::test]
    async fn slack_live_bundle_delivers_a_conversations_news_as_one_block() {
        let (Ok(user_token), Ok(bot_token), Ok(channel_name)) = (
            std::env::var("SLACK_TEST_USER_TOKEN"),
            std::env::var("SLACK_TEST_BOT_TOKEN"),
            std::env::var("SLACK_TEST_CHANNEL"),
        ) else {
            eprintln!(
                "skipping slack_live_bundle: set SLACK_TEST_USER_TOKEN, SLACK_TEST_BOT_TOKEN \
                 and SLACK_TEST_CHANNEL to run it against a real workspace",
            );
            return;
        };
        // The sweep's own log is the only place a live failure says why, so
        // run this test with the log visible: `--no-capture`.
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(std::io::stderr)
            .try_init();

        let http = reqwest::Client::builder().build().expect("an http client");
        let bot = forge_tools::slack::SlackClient::new(http.clone(), bot_token);
        let user = forge_tools::slack::SlackClient::new(http, user_token.clone());

        // Either the id or the name, since Slack hands out the first and a
        // person reads the second.
        let channel = bot
            .list_conversations()
            .await
            .expect("the bot reads its own channels")
            .into_iter()
            .find(|conversation| {
                conversation.id == channel_name
                    || conversation.name.as_deref() == Some(channel_name.as_str())
            })
            .expect("the channel SLACK_TEST_CHANNEL names is one the bot is in");
        let conversation_id = channel.id.clone();
        // The name the header should carry, which is not the id: asserting
        // the id would pass on the header's `id` field alone.
        let channel_label = channel.name.clone().unwrap_or_else(|| conversation_id.clone());

        // Everything at or below this is old news, so the sweep's news is
        // exactly what is posted below.
        let watermark = user
            .history(&conversation_id, None, None, 1, None)
            .await
            .expect("the user reads the channel")
            .messages
            .first()
            .map_or_else(|| "0".to_owned(), |message| message.ts.clone());

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("the clock is past the epoch")
            .as_secs();
        let texts: Vec<String> = (1..=3).map(|n| format!("live bundle {n} {stamp}")).collect();
        let mut posted = Vec::new();
        for text in &texts {
            posted.push(
                bot.post_message(&conversation_id, text, None).await.expect("the bot posts").ts,
            );
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.slack = vec![SlackConfig {
            workspace: LIVE_WORKSPACE.to_owned(),
            token: user_token,
            poll_seconds: 1,
            dm_poll_seconds: 1,
            thread_idle_days: 14,
            stale_after_hours: 24,
        }];
        let (ws, mut update_rx) =
            Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
                .expect("the stub config's [[slack]] entries are well-formed");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );

        // The own id is what boot's auth probe caches; without it the sweep
        // refuses the tick rather than echo the user's own posts back.
        let own_id = user.auth_test().await.expect("the user token proves").user_id;
        ws.slack.seed_user_id(LIVE_WORKSPACE, &own_id);

        ws.seed_test_project("live", LIVE_PROJECT_PATH);
        let mut sub = sub_for_conversation("live", None, &conversation_id);
        sub.workspace = LIVE_WORKSPACE.to_owned();
        ws.slack.add_slack_subscription(sub, true);
        ws.slack.set_slack_watermark(LIVE_WORKSPACE, &conversation_id, &watermark);
        // Checked before the pump starts: a subscription filed under another
        // workspace leaves the sweep with nothing to do, which it does in
        // silence and which reads from the outside as a connector that never
        // delivers.
        assert_eq!(
            ws.slack.slack_subscriptions_for_project("live").len(),
            1,
            "the subscription has to be in the workspace the pump sweeps",
        );

        // A running lead is where the block lands. The dispatch is
        // intercepted so the test never spawns a child.
        ws.record_connected_session(LIVE_PROJECT_PATH, "live-lead", None);
        let lead_key = SessionSlot::lead("TestOrg", "live");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            crate::workspace::PooledAgent {
                handle: Arc::new(handle),
                account: forge_gateway::AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "live-lead".to_owned(),
            },
        );
        ws.mark_session_connected_for_test(&lead_key, "live-lead");
        ws.enable_test_dispatch_intercept();

        let host: Arc<dyn forge_tools::slack::SlackHost> =
            Arc::new(forge_tools::slack::SlackSubsystemHost::new(&ws.slack));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let pump = tokio::spawn(forge_tools::slack::run_workspace_pump(
            host,
            LIVE_WORKSPACE.to_owned(),
            1,
            1,
            shutdown_rx,
        ));

        let mut delivered = None;
        for _ in 0..600 {
            while let Ok(frame) = update_rx.try_recv() {
                if let crate::protocol::SessionUpdate::SlackMessageAppended { prose, .. } =
                    std::sync::Arc::unwrap_or_clone(frame.update)
                {
                    delivered = Some(prose);
                }
            }
            if delivered.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = shutdown_tx.send(());
        let _ = pump.await;

        let prose = delivered.expect("the sweep delivered the block the session is handed");
        assert!(
            prose.contains(&format!("({} messages)", texts.len())),
            "one block holding all three: {prose}",
        );
        assert!(prose.contains(&channel_label), "the header names the channel: {prose}");
        // The shape, line by line: a collapse that delivered only the newest
        // message would still carry its own text and ts, so the count is
        // what says all three arrived.
        let members = prose.lines().filter(|line| line.contains(" [ts ")).count();
        assert_eq!(members, texts.len(), "one member line per posted message: {prose}");
        for (index, text) in texts.iter().enumerate() {
            assert!(prose.contains(text.as_str()), "member {index} is in the block: {prose}");
            assert!(
                prose.contains(&posted[index]),
                "and keeps its own ts, so the agent can answer it alone: {prose}",
            );
        }
    }
}

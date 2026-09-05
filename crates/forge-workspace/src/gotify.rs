//! The Gotify seam on [`Workspace`]: the forge.toml `[gotify]` config
//! accessor, subscription CRUD (durable rows in the redb store,
//! ephemeral ones in memory), the subsystem start/stop lifecycle, and
//! the [`GotifyHost`] port impl the forge-connectors pump drives.
//!
//! Everything here stays on `Workspace` as a second `impl` block, so
//! every caller (the boot path in [`crate::workspace`], the
//! `mcp::gotify` facade, `spawn::deliver_gotify_message`, the
//! Inspector GOTIFY snapshot) keeps its path. The `gotify_*` fields
//! these methods own are `pub(crate)` for the same reason `db` is: so
//! this sibling module can reach them without a wrapper. Stream,
//! matching and the pump live in `forge_connectors::gotify`; store IO
//! in [`crate::store::gotify`]; the MCP tool surface in
//! [`crate::mcp::gotify`]; delivery in [`crate::spawn`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use forge_connectors::gotify::GotifyHost;

use crate::protocol::Command;
use crate::target::ProjectKey;
use crate::workspace::Workspace;

impl Workspace {
    /// The `[gotify]` server connection from forge.toml, or `None`
    /// when the section is absent. `None` keeps the Gotify subsystem
    /// dormant and makes `gotify__subscribe` error. Read-only - forge
    /// never writes forge.toml.
    pub fn gotify_config(&self) -> Option<forge_primitives::GotifyConfig> {
        self.config.gotify.clone()
    }

    /// Register a Gotify subscription in the active set. Durable ones
    /// (lead / team-worker) also persist to the redb store; ephemeral
    /// ad-hoc-worker ones stay in memory only and drop on restart.
    pub(crate) fn add_gotify_subscription(
        &self,
        sub: forge_primitives::GotifySubscription,
        durable: bool,
    ) {
        if durable
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::gotify::insert(db, &sub)
        {
            tracing::warn!(
                target: "forge_workspace::gotify",
                %error,
                "persisting a Gotify subscription failed",
            );
        }
        self.gotify_subs.lock().push(sub);
    }

    /// Remove the subscription `id` in `project` only when its owner
    /// matches `owner` (`None` = a lead subscription, `Some(label)` =
    /// that worker's), from both the active set and (when present) the
    /// redb store. Returns whether an entry was removed. Backs the
    /// owner-scoped `gotify__unsubscribe` so a caller removes only what
    /// it subscribed, mirroring [`Self::remove_cron_owned_by`]. Worker
    /// teardown uses [`Self::remove_gotify_subscriptions_for_worker`]
    /// instead and is deliberately not owner-gated.
    pub(crate) fn remove_gotify_subscription_owned_by(
        &self,
        project: &str,
        id: uuid::Uuid,
        owner: Option<&str>,
    ) -> bool {
        let removed = {
            let mut subs = self.gotify_subs.lock();
            let before = subs.len();
            subs.retain(|s| {
                !(s.id == id && s.project == project && s.team_role.as_deref() == owner)
            });
            subs.len() != before
        };
        if removed
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::gotify::remove(db, id)
        {
            tracing::warn!(
                target: "forge_workspace::gotify",
                %error,
                "removing a persisted Gotify subscription failed",
            );
        }
        removed
    }

    /// Remove the worker `label`'s Gotify subscriptions in `project_key`
    /// from both the active set and the redb store, scoped to
    /// `(project name, team_role == Some(label))`. Backs
    /// `spawn::teardown_worker` so a despawned durable dynamic worker can't
    /// orphan a persisted sub. Subscriptions are project-scoped by NAME, so
    /// the key is resolved to a name via the same view lookup
    /// `resolve_identity` uses when subscribing.
    pub(crate) fn remove_gotify_subscriptions_for_worker(
        &self,
        project_key: &ProjectKey,
        label: &str,
    ) {
        let Some(project_name) =
            self.list_projects().into_iter().find(|v| v.key == *project_key).map(|v| v.name)
        else {
            tracing::warn!(
                target: "forge_workspace::gotify",
                project = %project_key.as_str(),
                label = %label,
                "could not resolve a project name at worker teardown; its Gotify subs may be stranded",
            );
            return;
        };
        let removed_ids: Vec<uuid::Uuid> = {
            let mut subs = self.gotify_subs.lock();
            let mut removed = Vec::new();
            subs.retain(|s| {
                let owned = s.project == project_name && s.team_role.as_deref() == Some(label);
                if owned {
                    removed.push(s.id);
                }
                !owned
            });
            removed
        };
        if removed_ids.is_empty() {
            return;
        }
        if let Some(db) = self.db.lock().as_ref() {
            for id in removed_ids {
                if let Err(error) = crate::store::gotify::remove(db, id) {
                    // Same zombie-durable-state class as a stranded
                    // dynamic-worker row: match its error! severity.
                    tracing::error!(
                        target: "forge_workspace::gotify",
                        %error,
                        id = %id,
                        project = %project_name,
                        label = %label,
                        "deleting a persisted Gotify subscription failed; it reloads into the active set on restart and a future same-label worker inherits it",
                    );
                }
            }
        }
    }

    /// The active subscriptions for `project`. Backs `gotify__list` and
    /// the Inspector GOTIFY snapshot, which scopes by the active tab's
    /// stamped project name (mirroring [`Self::crons_for_project`]).
    pub fn gotify_subscriptions_for_project(
        &self,
        project: &str,
    ) -> Vec<forge_primitives::GotifySubscription> {
        self.gotify_subs.lock().iter().filter(|s| s.project == project).cloned().collect()
    }

    /// Whether the Gotify stream is currently connected. Backs the
    /// Inspector GOTIFY status line.
    pub fn gotify_connected(&self) -> bool {
        *self.gotify_connected.lock()
    }

    /// Start the Gotify subsystem when it's configured, has at least one
    /// active subscription, and isn't already running. Idempotent. Spawns
    /// the forge-connectors pump, which runs the reconnecting stream task
    /// and translates its events into the `gotify_*` state below and
    /// `GotifyHost::deliver` calls. Called at boot and after a subscribe
    /// grows the active set.
    pub fn start_gotify_subsystem(self: &Arc<Self>) {
        let Some(cfg) = self.gotify_config() else { return };
        if self.gotify_subs.lock().is_empty() {
            return;
        }
        let mut guard = self.gotify_subsystem.lock();
        if guard.is_some() {
            return;
        }
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        *guard = Some(shutdown_tx);
        drop(guard);
        let host: Arc<dyn GotifyHost> = Arc::new(SubsystemHost::new(self));
        tokio::spawn(forge_connectors::gotify::run_subsystem(host, cfg, shutdown_rx));
    }

    /// Stop the Gotify subsystem once no subscriptions remain: signal the
    /// stream task to exit and mark disconnected. No-op while any
    /// subscription is active or the subsystem isn't running.
    pub fn stop_gotify_subsystem_if_idle(&self) {
        if !self.gotify_subs.lock().is_empty() {
            return;
        }
        if let Some(shutdown_tx) = self.gotify_subsystem.lock().take() {
            let _ = shutdown_tx.send(());
            *self.gotify_connected.lock() = false;
        }
    }
}

/// The [`GotifyHost`] the forge-connectors pump drives: a
/// `Weak<Workspace>` behind the port. The pump holds the host strongly,
/// so the wrapper stays the weak boundary - the workspace can drop while
/// the subsystem runs, and every port call degrades to a no-op then,
/// exactly like the per-event upgrade the pump did when it lived here.
pub(crate) struct SubsystemHost(std::sync::Weak<Workspace>);

impl SubsystemHost {
    pub(crate) fn new(workspace: &Arc<Workspace>) -> Self {
        Self(Arc::downgrade(workspace))
    }
}

impl GotifyHost for SubsystemHost {
    fn http_client(&self, timeout: Duration) -> Result<reqwest::Client, String> {
        forge_agent::http_trust::with_extra_roots(reqwest::Client::builder().timeout(timeout))
            .build()
            .map_err(|error| error.to_string())
    }

    fn subscriptions(&self) -> Vec<forge_primitives::GotifySubscription> {
        self.0.upgrade().map(|ws| ws.gotify_subs.lock().clone()).unwrap_or_default()
    }

    /// The application NAME for `appid` via the reverse of the app index,
    /// or `None` when the id isn't known (index not yet fetched, or a new
    /// app the server added after the last refresh).
    fn app_name(&self, appid: u64) -> Option<String> {
        let ws = self.0.upgrade()?;
        ws.gotify_app_index
            .lock()
            .iter()
            .find(|&(_, &id)| id == appid)
            .map(|(name, _)| name.clone())
    }

    fn store_app_index(&self, index: HashMap<String, u64>) {
        if let Some(ws) = self.0.upgrade() {
            *ws.gotify_app_index.lock() = index;
        }
    }

    fn set_connected(&self, connected: bool) {
        if let Some(ws) = self.0.upgrade() {
            *ws.gotify_connected.lock() = connected;
        }
    }

    /// Wrap the matched message into the notification wire shape and
    /// dispatch it to the subscriber. This is the only place a
    /// forge-connectors match becomes a `GotifyNotification`, so the
    /// prose shape stays workspace-owned (the TUI's `detect_inbound`
    /// keys on it).
    fn deliver(
        &self,
        subscription: &forge_primitives::GotifySubscription,
        app: &str,
        message: &forge_primitives::GotifyMessage,
    ) {
        let Some(ws) = self.0.upgrade() else { return };
        let notification = crate::mcp::gotify::types::GotifyNotification {
            app: app.to_owned(),
            title: message.title.clone(),
            message: message.message.clone(),
            priority: message.priority,
        };
        if let Err(err) = ws.dispatch(Command::DeliverGotifyMessage {
            project: subscription.project.clone(),
            team_role: subscription.team_role.clone(),
            notification,
        }) {
            tracing::warn!(
                target: "forge_workspace::gotify",
                project = %subscription.project,
                error = ?err,
                "gotify DeliverGotifyMessage dispatch failed",
            );
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl Workspace {
    /// Register a Gotify subscription directly, bypassing the MCP
    /// subscribe path. Cross-crate test access so forge-tui can exercise
    /// the Inspector's `refresh_gotify` resolution, mirroring
    /// [`Self::seed_test_cron`].
    pub fn seed_test_gotify_subscription(&self, sub: forge_primitives::GotifySubscription) {
        self.add_gotify_subscription(sub, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use tempfile::tempdir;

    use crate::target::SessionKey;

    fn dynamic_worker_row(
        project: &str,
        label: &str,
    ) -> crate::store::dynamic_workers::DynamicWorker {
        crate::store::dynamic_workers::DynamicWorker {
            project_key: project.to_owned(),
            label: label.to_owned(),
            charter: format!("charter for {label}"),
            kick: Some(format!("kick for {label}")),
            resume_kick: None,
            interactive: false,
        }
    }

    fn live_worker_entry(label: &str, key: &str) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.to_owned(),
            charter: "c".to_owned(),
            session_key: SessionKey::from_session_id(key),
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".to_owned(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    fn gotify_sub(
        project: &str,
        applications: &[&str],
        min_priority: Option<u8>,
    ) -> forge_primitives::GotifySubscription {
        forge_primitives::GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: project.to_owned(),
            team_role: None,
            applications: applications.iter().map(|s| (*s).to_owned()).collect(),
            min_priority,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn gotify_msg(appid: u64, priority: u8) -> forge_primitives::GotifyMessage {
        forge_primitives::GotifyMessage {
            id: 1,
            appid,
            title: "Alert".to_owned(),
            message: "body".to_owned(),
            priority,
            date: "2026-07-03T00:00:00Z".to_owned(),
        }
    }

    fn gotify_notif(
        app: &str,
        title: &str,
        message: &str,
        priority: u8,
    ) -> crate::GotifyNotification {
        crate::GotifyNotification {
            app: app.to_owned(),
            title: title.to_owned(),
            message: message.to_owned(),
            priority,
        }
    }

    /// Drain every currently-queued `SessionUpdate` from the test rx.
    fn drain_updates(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::protocol::SessionUpdate>,
    ) -> Vec<crate::protocol::SessionUpdate> {
        let mut out = Vec::new();
        while let Ok(u) = rx.try_recv() {
            out.push(u);
        }
        out
    }

    #[test]
    fn durable_gotify_subscription_persists_ephemeral_stays_in_memory() {
        use forge_primitives::GotifySubscription;

        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);

        let sub = |team_role: Option<&str>| GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: "p".to_owned(),
            team_role: team_role.map(str::to_owned),
            applications: vec![],
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        };
        let durable = sub(None);
        let ephemeral = sub(Some("scratch"));
        ws.add_gotify_subscription(durable.clone(), true);
        ws.add_gotify_subscription(ephemeral, false);

        assert_eq!(
            ws.gotify_subscriptions_for_project("p").len(),
            2,
            "both live in the active in-memory set",
        );
        let persisted = || {
            crate::store::gotify::list(ws.db.lock().as_ref().expect("db installed")).expect("list")
        };
        assert_eq!(persisted().len(), 1, "only the durable subscription hit redb");
        assert_eq!(persisted()[0].id, durable.id);

        assert!(
            ws.remove_gotify_subscription_owned_by("p", durable.id, None),
            "the lead's own durable id removes",
        );
        assert!(persisted().is_empty(), "removal cleared the persisted record");
    }

    /// `remove_gotify_subscriptions_for_worker` drops only the target
    /// worker's subs (matched by project name + label) from both the
    /// in-memory set and redb; the lead's sub and a sibling worker's sub
    /// survive, proving the removal is scoped, not a blanket project wipe.
    #[test]
    fn remove_gotify_subscriptions_for_worker_is_scoped_to_project_and_label() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("forge", "/tmp/gotify-durability");
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");

        let mut scratch_sub = gotify_sub("forge", &[], None);
        scratch_sub.team_role = Some("scratch".to_owned());
        let lead_sub = gotify_sub("forge", &[], None);
        let mut sibling_sub = gotify_sub("forge", &[], None);
        sibling_sub.team_role = Some("other".to_owned());
        ws.add_gotify_subscription(scratch_sub.clone(), true);
        ws.add_gotify_subscription(lead_sub.clone(), true);
        ws.add_gotify_subscription(sibling_sub.clone(), true);

        ws.remove_gotify_subscriptions_for_worker(&view_key, "scratch");

        let in_mem = ws.gotify_subscriptions_for_project("forge");
        assert!(
            in_mem.iter().all(|s| s.id != scratch_sub.id),
            "the scratch worker's sub is gone from memory",
        );
        assert!(
            in_mem.iter().any(|s| s.id == lead_sub.id)
                && in_mem.iter().any(|s| s.id == sibling_sub.id),
            "the lead sub and the sibling worker's sub survive in memory",
        );

        let persisted = {
            let guard = ws.db.lock();
            crate::store::gotify::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted.iter().all(|s| s.id != scratch_sub.id),
            "the scratch worker's sub is gone from redb",
        );
        assert!(
            persisted.iter().any(|s| s.id == lead_sub.id)
                && persisted.iter().any(|s| s.id == sibling_sub.id),
            "the survivors are still persisted in redb",
        );
    }

    /// Closing a worker drops its durable Gotify subs alongside its
    /// dynamic-worker row; the lead's sub survives.
    #[tokio::test]
    async fn closing_a_worker_drops_the_workers_durable_gotify_subs() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("forge", "/tmp/gotify-durability");
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");
        ws.insert_live_worker(&view_key, live_worker_entry("scratch", "worker-1"));

        let mut scratch_sub = gotify_sub("forge", &[], None);
        scratch_sub.team_role = Some("scratch".to_owned());
        let lead_sub = gotify_sub("forge", &[], None);
        ws.add_gotify_subscription(scratch_sub.clone(), true);
        ws.add_gotify_subscription(lead_sub.clone(), true);

        crate::spawn::handle_close_worker(&ws, &view_key, "scratch");

        let in_mem = ws.gotify_subscriptions_for_project("forge");
        assert!(
            in_mem.iter().all(|s| s.id != scratch_sub.id),
            "teardown removed the worker's sub from memory",
        );
        assert!(in_mem.iter().any(|s| s.id == lead_sub.id), "the lead sub survives teardown");

        let persisted = {
            let guard = ws.db.lock();
            crate::store::gotify::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted.iter().all(|s| s.id != scratch_sub.id),
            "teardown removed the worker's sub from redb",
        );
        assert!(persisted.iter().any(|s| s.id == lead_sub.id), "the lead sub is still persisted");
    }

    /// The seam the bug lived in: `resolve_identity` gathers the caller's
    /// dynamic-worker labels from the table and marks a table-backed
    /// worker durable, so its sub persists through the subscribe path even
    /// though the worker is not the lead.
    #[test]
    fn resolve_identity_persists_a_table_backed_dynamic_workers_sub() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("forge", "/tmp/gotify-durability-seam");
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");

        // "scratch" is not the lead: durability
        // must come solely from its dynamic_workers row.
        let _ = ws.persist_dynamic_worker(&dynamic_worker_row(view_key.as_str(), "scratch"));
        let caller = SessionKey::from_session_id("scratch-session");
        ws.insert_live_worker(&view_key, live_worker_entry("scratch", "scratch-session"));

        let (name, team_role, durable) = crate::mcp::gotify::facade::resolve_identity(&ws, &caller)
            .expect("the worker caller resolves to its project");
        assert_eq!(
            (name.as_str(), team_role.as_deref(), durable),
            ("forge", Some("scratch"), true),
            "a table-backed dynamic worker resolves as a durable subscriber",
        );

        // The durable identity persists through the subscribe path's write.
        let sub = forge_primitives::GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: name,
            team_role,
            applications: vec![],
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        };
        let sub_id = sub.id;
        ws.add_gotify_subscription(sub, durable);
        let persisted = {
            let guard = ws.db.lock();
            crate::store::gotify::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted.iter().any(|s| s.id == sub_id),
            "the table-backed worker's sub is persisted to redb",
        );
    }

    /// A `ProjectKey` that matches no project resolves to no name, so the
    /// removal is a safe no-op that leaves every sub intact.
    #[test]
    fn remove_gotify_subscriptions_for_worker_unknown_project_is_a_no_op() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("forge", "/tmp/gotify-durability-noop");

        let mut scratch_sub = gotify_sub("forge", &[], None);
        scratch_sub.team_role = Some("scratch".to_owned());
        ws.add_gotify_subscription(scratch_sub.clone(), true);

        ws.remove_gotify_subscriptions_for_worker(&ProjectKey::new("ghost"), "scratch");

        assert!(
            ws.gotify_subscriptions_for_project("forge").iter().any(|s| s.id == scratch_sub.id),
            "an unknown project key leaves the in-memory subs intact",
        );
        let persisted = {
            let guard = ws.db.lock();
            crate::store::gotify::list(guard.as_ref().expect("db installed")).expect("list")
        };
        assert!(
            persisted.iter().any(|s| s.id == scratch_sub.id),
            "and leaves the persisted subs intact",
        );
    }

    /// With no store installed the removal still scrubs the in-memory set;
    /// the redb delete is simply skipped (mirrors
    /// `persist_dynamic_worker_errors_when_store_unavailable`).
    #[test]
    fn remove_gotify_subscriptions_for_worker_scrubs_memory_without_a_db() {
        let (ws, _rx) = Workspace::testing_stub();
        // No install_db_for_test: the store is closed for this session.
        ws.seed_test_project("forge", "/tmp/gotify-durability-nodb");
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");

        let mut scratch_sub = gotify_sub("forge", &[], None);
        scratch_sub.team_role = Some("scratch".to_owned());
        ws.add_gotify_subscription(scratch_sub.clone(), true);

        ws.remove_gotify_subscriptions_for_worker(&view_key, "scratch");

        assert!(
            ws.gotify_subscriptions_for_project("forge").iter().all(|s| s.id != scratch_sub.id),
            "the in-memory sub is scrubbed even with no store installed",
        );
    }

    /// The GotifyHost impl the forge-connectors pump drives: the port
    /// reads the `gotify_*` state, and `deliver` wraps the matched
    /// message into the notification wire shape addressed to the
    /// subscriber's project + team role.
    #[test]
    fn gotify_host_impl_reads_state_and_delivers_to_the_subscriber() {
        use forge_connectors::gotify::GotifyHost as _;

        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let mut worker_sub = gotify_sub("p1", &[], None);
        worker_sub.team_role = Some("reviewer".to_owned());
        ws.add_gotify_subscription(worker_sub.clone(), false);
        ws.add_gotify_subscription(gotify_sub("p2", &[], None), false);

        let host = SubsystemHost::new(&ws);
        host.store_app_index(HashMap::from([("alerts".to_owned(), 3u64)]));
        assert_eq!(ws.gotify_app_index.lock().len(), 1, "the index lands on the workspace");
        assert_eq!(host.app_name(3).as_deref(), Some("alerts"), "appid resolves via the index");
        assert_eq!(host.app_name(9), None, "an unknown appid resolves to nothing");

        host.set_connected(true);
        assert!(ws.gotify_connected(), "liveness reaches the Inspector's flag");

        assert_eq!(host.subscriptions().len(), 2, "the pump matches against the active set");

        ws.enable_test_dispatch_intercept();
        host.deliver(&worker_sub, "alerts", &gotify_msg(3, 5));
        let dispatched = ws.drain_test_dispatch_buffer();
        let Some(crate::protocol::Command::DeliverGotifyMessage {
            project,
            team_role,
            notification,
        }) = dispatched.first()
        else {
            panic!("deliver dispatches exactly one DeliverGotifyMessage: {dispatched:?}");
        };
        assert_eq!(project, "p1");
        assert_eq!(team_role.as_deref(), Some("reviewer"), "the team role rides along");
        assert_eq!(
            notification.app, "alerts",
            "the connector-resolved display name becomes the notification's app",
        );
        assert_eq!(notification.title, "Alert", "the message title rides into the wrap");
        assert_eq!(notification.message, "body", "the message body rides into the wrap");
        assert_eq!(notification.priority, 5, "the priority rides into the wrap");
    }

    #[test]
    fn deliver_gotify_message_spawns_asleep_project_and_buffers() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("forge", "/tmp/gotify-forge");

        let notif = gotify_notif("forge", "Heads up", "hello envelope", 5);
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(&ws, "forge", None, notif.clone());
        let dispatched = ws.drain_test_dispatch_buffer();

        let spawns = dispatched
            .iter()
            .filter(|c| {
                matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
            })
            .count();
        assert_eq!(spawns, 1, "the asleep project got a spawn");

        let synth = SessionKey::from_session_id("__spawn_forge__");
        let buffered = ws
            .domain_handles
            .lock()
            .get(&synth)
            .expect("synth domain present")
            .lock()
            .pending_gotify_prompts
            .clone();
        assert_eq!(buffered, vec![notif], "the notification was buffered");
    }

    #[test]
    fn deliver_gotify_message_skips_gone_project() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        // No project seeded: "ghost" is not in forge.toml.
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(
            &ws,
            "ghost",
            None,
            gotify_notif("ghost", "t", "x", 1),
        );
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().all(|c| !matches!(c, crate::protocol::Command::SpawnProject { .. })),
            "a gone target is skipped without a spawn or panic",
        );
    }

    #[test]
    fn deliver_gotify_message_delivers_to_running_team_worker() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("forge", "/tmp/gotify-team");
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");
        let worker_key = SessionKey::from_session_id("worker-reviewer");
        ws.insert_live_worker(
            &view_key,
            crate::mcp::workers::types::WorkerEntry {
                label: "reviewer".to_owned(),
                charter: "review".to_owned(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".to_owned(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );

        ws.mark_session_connected_for_test(&worker_key, "worker-reviewer");
        let notif = gotify_notif("Backups", "Nightly backup", "done", 5);
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(&ws, "forge", Some("reviewer"), notif.clone());
        let dispatched = ws.drain_test_dispatch_buffer();

        assert!(
            dispatched.iter().any(
                |c| matches!(c, crate::protocol::Command::Prompt { key, .. } if key == &worker_key)
            ),
            "a running team worker receives the notification directly",
        );
        assert!(
            dispatched.iter().all(|c| !matches!(c, crate::protocol::Command::SpawnProject { .. })),
            "no project spawn when the target worker is already running",
        );

        // The delivery ALSO echoes the notification block into the worker's
        // chat so the user sees what arrived (mirrors the peer echo).
        let echoed = drain_updates(&mut rx).into_iter().any(|u| matches!(
            u,
            crate::protocol::SessionUpdate::GotifyNotificationAppended { session_id, notification }
                if session_id == worker_key.as_str() && notification == notif
        ));
        assert!(echoed, "a running-target delivery emits a GotifyNotificationAppended echo");
    }

    #[test]
    fn deliver_gotify_message_team_worker_asleep_falls_through_to_lead() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("forge", "/tmp/gotify-team");

        // No live worker of that role: the subscription falls through to
        // lead delivery, which spawns the asleep project (the lead brings up
        // its team) and buffers the envelope for the Connected drain.
        let notif = gotify_notif("Backups", "backup", "env", 5);
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(&ws, "forge", Some("reviewer"), notif.clone());
        let dispatched = ws.drain_test_dispatch_buffer();

        let spawns = dispatched
            .iter()
            .filter(|c| {
                matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
            })
            .count();
        assert_eq!(spawns, 1, "an asleep team-worker subscription spawns the project lead");

        let synth = SessionKey::from_session_id("__spawn_forge__");
        let buffered = ws
            .domain_handles
            .lock()
            .get(&synth)
            .expect("synth domain present")
            .lock()
            .pending_gotify_prompts
            .clone();
        assert_eq!(buffered, vec![notif], "the notification was buffered");
    }

    /// A team-worker subscription whose worker is a live entry but still
    /// Spawning (session_id None) must buffer on the worker's own
    /// DomainSession for its Connected drain - not dispatch a bare
    /// Command::Prompt (dropped) and not fall through to the lead.
    #[test]
    fn deliver_gotify_message_to_spawning_team_worker_buffers_on_its_domain() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("forge", "/tmp/gotify-spawning");
        let view_key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .map(|v| v.key)
            .expect("seeded project view");
        let worker_key = SessionKey::from_session_id("worker-spawning");
        ws.insert_live_worker(&view_key, live_worker_entry("reviewer", "worker-spawning"));
        // Register the domain WITHOUT stamping session_id: still Spawning.
        ws.register_domain_session(worker_key.clone(), None);

        let notif = gotify_notif("Backups", "backup", "env", 5);
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_gotify_message(&ws, "forge", Some("reviewer"), notif.clone());
        let dispatched = ws.drain_test_dispatch_buffer();

        assert!(
            dispatched.is_empty(),
            "no bare Prompt (dropped) and no lead fallback for a spawning worker",
        );
        let buffered = ws
            .domain_session_for(&worker_key)
            .expect("worker domain")
            .lock()
            .pending_gotify_prompts
            .clone();
        assert_eq!(buffered, vec![notif], "buffered on the worker's own domain for its drain");
    }
}

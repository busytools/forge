//! `GotifyFacade` - the seam between the Gotify MCP tools and workspace
//! state. The production impl ([`ProdGotifyFacade`]) resolves the
//! caller's project + durable identity and drives the direct
//! `Workspace` subscription methods; the mock records calls for tool
//! tests.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_connectors::gotify::GotifyRecent;
use forge_primitives::GotifySubscription;
use uuid::Uuid;

use crate::SessionKey;
use crate::mcp::caller_context::caller_context;
use crate::workspace::Workspace;

/// Why `gotify__subscribe` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GotifySubscribeError {
    /// No `[gotify]` block in forge.toml - the server is unconfigured.
    NotConfigured,
    /// The caller couldn't be mapped to a project (transient race, or
    /// the session isn't attached to a forge.toml project).
    UnknownCallerProject,
}

/// Why a read-only Gotify tool (`gotify__apps` / `gotify__recent`) failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GotifyReadError {
    /// No `[gotify]` block in forge.toml - the server is unconfigured.
    NotConfigured,
    /// The REST request to the server failed (network, HTTP status, or
    /// body parse). Carries the formatted error chain for the LLM.
    Fetch(String),
}

/// The Gotify tools' view of the workspace. The subscription mutations
/// are synchronous state writes; the read-only `apps` / `recent` lookups
/// hit the server's REST API and are async.
#[async_trait::async_trait]
pub(crate) trait GotifyFacade: Send + Sync {
    /// Subscribe the caller's durable identity to the configured server,
    /// optionally filtered by application names and/or minimum priority.
    /// An empty `applications` matches any app. Persists when the identity
    /// is durable. Returns the new id.
    fn subscribe(
        &self,
        caller: &SessionKey,
        applications: Vec<String>,
        min_priority: Option<u8>,
    ) -> Result<Uuid, GotifySubscribeError>;

    /// The caller's own subscriptions in its project - a lead's, or one
    /// worker's, never another owner's.
    fn list(&self, caller: &SessionKey) -> Vec<GotifySubscription>;

    /// Remove one of the caller's OWN subscriptions by id within its
    /// project. `true` when an entry was removed; `false` both when no
    /// such id exists and when it belongs to another owner.
    fn unsubscribe(&self, caller: &SessionKey, id: Uuid) -> bool;

    /// The application NAMEs on the configured server (`GET /application`)
    /// so a session can self-discover what it may subscribe to.
    async fn apps(&self) -> Result<Vec<String>, GotifyReadError>;

    /// The most recent notifications, newest first, filtered by
    /// application NAME (empty = any) and `min_priority` (`None` = any),
    /// capped at `limit`. A catch-up read for a woken or live session.
    async fn recent(
        &self,
        applications: Vec<String>,
        min_priority: Option<u8>,
        limit: usize,
    ) -> Result<Vec<GotifyRecent>, GotifyReadError>;
}

/// Production facade over `Weak<Workspace>` (weak to avoid a cycle with
/// the MCP server the workspace owns).
pub(crate) struct ProdGotifyFacade {
    workspace: Weak<Workspace>,
}

impl ProdGotifyFacade {
    pub(crate) fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn GotifyFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }
}

#[async_trait::async_trait]
impl GotifyFacade for ProdGotifyFacade {
    fn subscribe(
        &self,
        caller: &SessionKey,
        applications: Vec<String>,
        min_priority: Option<u8>,
    ) -> Result<Uuid, GotifySubscribeError> {
        let ws = self.workspace.upgrade().ok_or(GotifySubscribeError::UnknownCallerProject)?;
        if ws.gotify_config().is_none() {
            return Err(GotifySubscribeError::NotConfigured);
        }
        let (project, team_role, durable) =
            resolve_identity(&ws, caller).ok_or(GotifySubscribeError::UnknownCallerProject)?;
        let sub = GotifySubscription {
            id: Uuid::new_v4(),
            project,
            team_role,
            applications,
            min_priority,
            created_at: SystemTime::now(),
        };
        let id = sub.id;
        ws.add_gotify_subscription(sub, durable);
        ws.start_gotify_subsystem();
        Ok(id)
    }

    fn list(&self, caller: &SessionKey) -> Vec<GotifySubscription> {
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        let Some(cx) = caller_context(&ws, caller) else { return Vec::new() };
        // Symmetric with `cron__list`: every caller sees only its own
        // subscriptions - a lead (`worker_label == None`) the lead ones,
        // a worker its own.
        ws.gotify_subscriptions_for_project(&cx.project_name)
            .into_iter()
            .filter(|s| s.team_role == cx.worker_label)
            .collect()
    }

    fn unsubscribe(&self, caller: &SessionKey, id: Uuid) -> bool {
        let Some(ws) = self.workspace.upgrade() else { return false };
        let Some(cx) = caller_context(&ws, caller) else { return false };
        let removed = ws.remove_gotify_subscription_owned_by(
            &cx.project_name,
            id,
            cx.worker_label.as_deref(),
        );
        ws.stop_gotify_subsystem_if_idle();
        removed
    }

    async fn apps(&self) -> Result<Vec<String>, GotifyReadError> {
        let ws = self.workspace.upgrade().ok_or(GotifyReadError::NotConfigured)?;
        let cfg = ws.gotify_config().ok_or(GotifyReadError::NotConfigured)?;
        forge_connectors::gotify::app_names(ws.as_ref(), &cfg)
            .await
            .map_err(|err| GotifyReadError::Fetch(format!("{err:#}")))
    }

    async fn recent(
        &self,
        applications: Vec<String>,
        min_priority: Option<u8>,
        limit: usize,
    ) -> Result<Vec<GotifyRecent>, GotifyReadError> {
        let ws = self.workspace.upgrade().ok_or(GotifyReadError::NotConfigured)?;
        let cfg = ws.gotify_config().ok_or(GotifyReadError::NotConfigured)?;
        forge_connectors::gotify::recent_messages(
            ws.as_ref(),
            &cfg,
            &applications,
            min_priority,
            limit,
        )
        .await
        .map_err(|err| GotifyReadError::Fetch(format!("{err:#}")))
    }
}

/// Resolve a caller to `(project_name, team_role, durable)`. `team_role`
/// is the worker's role label (`None` targets the lead); `durable` is
/// true for the lead or a worker with a row in the `dynamic_workers`
/// table, false for a worker without one.
pub(crate) fn resolve_identity(
    ws: &Workspace,
    caller: &SessionKey,
) -> Option<(String, Option<String>, bool)> {
    let cx = caller_context(ws, caller)?;
    let worker_label = if cx.is_lead {
        None
    } else {
        ws.list_live_workers(&cx.project_key)
            .into_iter()
            .find(|w| w.session_key == *caller)
            .map(|w| w.label)
    };
    let dynamic_labels: Vec<String> =
        ws.dynamic_workers_for_project(&cx.project_key).into_iter().map(|w| w.label).collect();
    let (team_role, durable) = durable_identity(worker_label.as_deref(), &dynamic_labels);
    Some((cx.project_name, team_role, durable))
}

/// `(team_role, durable)` for a caller's worker label (`None` = the lead
/// or a plain catalog session). A lead is always durable and targets
/// itself. A worker is durable when its label lives in a durable store:
/// the `dynamic_workers` table, since that row is what brings it back.
fn durable_identity(
    worker_label: Option<&str>,
    dynamic_labels: &[String],
) -> (Option<String>, bool) {
    match worker_label {
        None => (None, true),
        Some(label) => {
            let durable = dynamic_labels.iter().any(|t| t == label);
            (Some(label.to_owned()), durable)
        }
    }
}

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool correctly parses args, resolves the caller, and surfaces
/// facade results/errors - without a real workspace.
/// One recorded `subscribe` call: `(caller, applications, min_priority)`.
#[cfg(test)]
type SubscribeCall = (SessionKey, Vec<String>, Option<u8>);

/// One recorded `recent` call: `(applications, min_priority, limit)`.
#[cfg(test)]
type RecentCall = (Vec<String>, Option<u8>, usize);

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockGotifyFacade {
    pub subs: parking_lot::Mutex<Vec<GotifySubscription>>,
    pub subscribe_calls: parking_lot::Mutex<Vec<SubscribeCall>>,
    pub subscribe_result: parking_lot::Mutex<Option<Result<Uuid, GotifySubscribeError>>>,
    pub unsubscribe_calls: parking_lot::Mutex<Vec<(SessionKey, Uuid)>>,
    pub unsubscribe_result: parking_lot::Mutex<Option<bool>>,
    pub apps_result: parking_lot::Mutex<Option<Result<Vec<String>, GotifyReadError>>>,
    pub recent_calls: parking_lot::Mutex<Vec<RecentCall>>,
    pub recent_result: parking_lot::Mutex<Option<Result<Vec<GotifyRecent>, GotifyReadError>>>,
}

#[cfg(test)]
impl MockGotifyFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn into_arc(self) -> Arc<dyn GotifyFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl GotifyFacade for MockGotifyFacade {
    fn subscribe(
        &self,
        caller: &SessionKey,
        applications: Vec<String>,
        min_priority: Option<u8>,
    ) -> Result<Uuid, GotifySubscribeError> {
        self.subscribe_calls.lock().push((caller.clone(), applications, min_priority));
        self.subscribe_result.lock().clone().unwrap_or_else(|| Ok(Uuid::nil()))
    }

    fn list(&self, _caller: &SessionKey) -> Vec<GotifySubscription> {
        self.subs.lock().clone()
    }

    fn unsubscribe(&self, caller: &SessionKey, id: Uuid) -> bool {
        self.unsubscribe_calls.lock().push((caller.clone(), id));
        self.unsubscribe_result.lock().unwrap_or(false)
    }

    async fn apps(&self) -> Result<Vec<String>, GotifyReadError> {
        self.apps_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn recent(
        &self,
        applications: Vec<String>,
        min_priority: Option<u8>,
        limit: usize,
    ) -> Result<Vec<GotifyRecent>, GotifyReadError> {
        self.recent_calls.lock().push((applications, min_priority, limit));
        self.recent_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkerEntry;
    use crate::target::ProjectKey;
    use forge_primitives::WorkerLiveness;

    fn worker_entry(label: &str, session_id: &str) -> WorkerEntry {
        WorkerEntry {
            label: label.to_owned(),
            charter: "watch".to_owned(),
            session_key: SessionKey::from_session_id(session_id),
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".to_owned(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// A project with a live lead plus two live workers, mirroring the
    /// cron facade's fixture so the two families are tested the same way.
    fn fixture() -> (Arc<Workspace>, Arc<dyn GotifyFacade>, ProjectKey, SessionKey, SessionKey) {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("myproj", "/tmp/gotify-scope");
        let key =
            ws.list_projects().into_iter().find(|v| v.name == "myproj").expect("seeded view").key;
        ws.record_connected_session("/tmp/gotify-scope", "lead-uuid", None);
        ws.insert_live_worker(&key, worker_entry("reviewer", "worker-uuid"));
        ws.insert_live_worker(&key, worker_entry("analyst", "sibling-uuid"));
        let facade = ProdGotifyFacade::from_arc(&ws);
        (
            ws,
            facade,
            key,
            SessionKey::from_session_id("lead-uuid"),
            SessionKey::from_session_id("worker-uuid"),
        )
    }

    fn seed_sub(ws: &Workspace, team_role: Option<&str>) -> Uuid {
        let sub = GotifySubscription {
            id: Uuid::new_v4(),
            project: "myproj".to_owned(),
            team_role: team_role.map(str::to_owned),
            applications: vec!["alerts".to_owned()],
            min_priority: None,
            created_at: SystemTime::UNIX_EPOCH,
        };
        let id = sub.id;
        ws.add_gotify_subscription(sub, true);
        id
    }

    #[test]
    fn list_is_scoped_to_the_callers_own_subscriptions() {
        let (ws, facade, _key, lead, worker) = fixture();
        let lead_id = seed_sub(&ws, None);
        let worker_id = seed_sub(&ws, Some("reviewer"));
        seed_sub(&ws, Some("analyst"));

        let worker_list = facade.list(&worker);
        assert_eq!(
            worker_list.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![worker_id],
            "a worker sees neither the lead's subscription nor a sibling's",
        );
        let lead_list = facade.list(&lead);
        assert_eq!(
            lead_list.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![lead_id],
            "a lead sees only its own subscriptions",
        );
    }

    #[test]
    fn unsubscribe_only_removes_the_callers_own_subscription() {
        let (ws, facade, _key, lead, worker) = fixture();
        let lead_id = seed_sub(&ws, None);
        let worker_id = seed_sub(&ws, Some("reviewer"));
        let sibling_id = seed_sub(&ws, Some("analyst"));

        assert!(!facade.unsubscribe(&worker, lead_id), "a worker cannot unsubscribe the lead's");
        assert!(
            !facade.unsubscribe(&worker, sibling_id),
            "a worker cannot unsubscribe a sibling worker's",
        );
        assert!(!facade.unsubscribe(&lead, worker_id), "a lead cannot unsubscribe a worker's");
        assert_eq!(
            ws.gotify_subscriptions_for_project("myproj").len(),
            3,
            "a refused unsubscribe removes nothing",
        );

        assert!(facade.unsubscribe(&worker, worker_id), "a worker unsubscribes its own");
        assert!(facade.unsubscribe(&lead, lead_id), "a lead unsubscribes its own");
        assert_eq!(
            ws.gotify_subscriptions_for_project("myproj").iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![sibling_id],
            "only the untouched sibling subscription remains",
        );
    }

    /// Ownership is enforced at the MCP facade, NOT in the workspace
    /// teardown method, so a despawn still clears a departing worker's
    /// subscriptions with no MCP caller involved.
    #[test]
    fn worker_teardown_clears_subscriptions_without_going_through_the_facade() {
        let (ws, _facade, key, _lead, _worker) = fixture();
        let lead_id = seed_sub(&ws, None);
        let worker_id = seed_sub(&ws, Some("reviewer"));

        ws.remove_gotify_subscriptions_for_worker(&key, "reviewer");

        let remaining: Vec<Uuid> =
            ws.gotify_subscriptions_for_project("myproj").iter().map(|s| s.id).collect();
        assert!(!remaining.contains(&worker_id), "teardown removed the departing worker's");
        assert!(remaining.contains(&lead_id), "the lead's subscription survives the despawn");
    }

    #[test]
    fn worker_is_durable_when_persisted() {
        let dynamic = vec!["scratch".to_owned()];
        assert_eq!(durable_identity(Some("scratch"), &dynamic), (Some("scratch".to_owned()), true),);
    }

    #[test]
    fn lead_is_durable_and_targets_itself() {
        assert_eq!(durable_identity(None, &[]), (None, true));
    }

    #[test]
    fn worker_without_a_row_is_ephemeral() {
        // No row means nothing re-spawns the label, so its subscription is
        // in-memory only rather than written to redb for an absent owner.
        assert_eq!(
            durable_identity(Some("scratch"), &["reviewer".to_owned()]),
            (Some("scratch".to_owned()), false),
        );
    }
}

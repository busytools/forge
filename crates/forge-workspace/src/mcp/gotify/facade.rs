//! `GotifyFacade` - the seam between the Gotify MCP tools and workspace
//! state. The production impl ([`ProdGotifyFacade`]) resolves the
//! caller's project + durable identity and drives the direct
//! `Workspace` subscription methods; the mock records calls for tool
//! tests.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_agent::env::gotify::GotifyRecent;
use forge_primitives::{GotifyConfig, GotifySubscription};
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

    /// The subscriptions registered for the caller's project.
    fn list(&self, caller: &SessionKey) -> Vec<GotifySubscription>;

    /// Remove a subscription by id within the caller's project. `true`
    /// when an entry was removed.
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

    /// The configured server, or `NotConfigured` when the `[gotify]` block
    /// is absent (or the workspace has been dropped mid-shutdown).
    fn config(&self) -> Result<GotifyConfig, GotifyReadError> {
        self.workspace
            .upgrade()
            .and_then(|ws| ws.gotify_config())
            .ok_or(GotifyReadError::NotConfigured)
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
        ws.gotify_subscriptions_for_project(&cx.project_name)
    }

    fn unsubscribe(&self, caller: &SessionKey, id: Uuid) -> bool {
        let Some(ws) = self.workspace.upgrade() else { return false };
        let Some(cx) = caller_context(&ws, caller) else { return false };
        let removed = ws.remove_gotify_subscription(&cx.project_name, id);
        ws.stop_gotify_subsystem_if_idle();
        removed
    }

    async fn apps(&self) -> Result<Vec<String>, GotifyReadError> {
        let cfg = self.config()?;
        forge_agent::env::gotify::app_names(&cfg)
            .await
            .map_err(|err| GotifyReadError::Fetch(format!("{err:#}")))
    }

    async fn recent(
        &self,
        applications: Vec<String>,
        min_priority: Option<u8>,
        limit: usize,
    ) -> Result<Vec<GotifyRecent>, GotifyReadError> {
        let cfg = self.config()?;
        forge_agent::env::gotify::recent_messages(&cfg, &applications, min_priority, limit)
            .await
            .map_err(|err| GotifyReadError::Fetch(format!("{err:#}")))
    }
}

/// Resolve a caller to `(project_name, team_role, durable)`. `team_role`
/// is the worker's role label (`None` targets the lead); `durable` is
/// true for the lead or a worker whose label lives in a durable store
/// (forge.toml `static_workers` or the `dynamic_workers` table), false
/// for a worker in neither.
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
    let static_workers = ws
        .list_projects()
        .into_iter()
        .find(|v| v.key == cx.project_key)
        .map(|v| v.static_workers)
        .unwrap_or_default();
    let dynamic_labels: Vec<String> =
        ws.dynamic_workers_for_project(&cx.project_key).into_iter().map(|w| w.label).collect();
    let (team_role, durable) =
        durable_identity(worker_label.as_deref(), &static_workers, &dynamic_labels);
    Some((cx.project_name, team_role, durable))
}

/// `(team_role, durable)` for a caller's worker label (`None` = the lead
/// or a plain catalog session). A lead is always durable and targets
/// itself. A worker is durable when its label lives in a durable store:
/// forge.toml `static_workers` or the `dynamic_workers` table.
fn durable_identity(
    worker_label: Option<&str>,
    static_workers: &[String],
    dynamic_labels: &[String],
) -> (Option<String>, bool) {
    match worker_label {
        None => (None, true),
        Some(label) => {
            let durable = static_workers.iter().any(|t| t == label)
                || dynamic_labels.iter().any(|t| t == label);
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

    #[test]
    fn dynamic_worker_is_durable_when_persisted() {
        let statics = vec!["reviewer".to_owned()];
        let dynamic = vec!["scratch".to_owned()];
        // Durable because "scratch" has a dynamic_workers row, even though
        // it is not a configured static worker.
        assert_eq!(
            durable_identity(Some("scratch"), &statics, &dynamic),
            (Some("scratch".to_owned()), true),
        );
    }

    #[test]
    fn lead_is_durable_and_targets_itself() {
        assert_eq!(durable_identity(None, &["reviewer".to_owned()], &[]), (None, true));
    }

    #[test]
    fn team_worker_is_durable_with_its_role() {
        let team = vec!["reviewer".to_owned(), "tester".to_owned()];
        assert_eq!(
            durable_identity(Some("reviewer"), &team, &[]),
            (Some("reviewer".to_owned()), true),
        );
    }

    #[test]
    fn worker_in_neither_store_is_ephemeral() {
        let statics = vec!["reviewer".to_owned()];
        // A label absent from both static_workers and the dynamic_workers
        // table is an ephemeral, in-memory-only subscriber.
        assert_eq!(
            durable_identity(Some("scratch"), &statics, &[]),
            (Some("scratch".to_owned()), false),
        );
    }
}

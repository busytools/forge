//! `GotifyFacade` - the seam between the Gotify MCP tools and workspace
//! state. The production impl ([`ProdGotifyFacade`]) resolves the
//! caller's project + durable identity and drives the direct
//! `Workspace` subscription methods; the mock records calls for tool
//! tests.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

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

/// The Gotify tools' view of the workspace. Sync - subscription
/// mutations are direct state writes with no async handler to await.
pub(crate) trait GotifyFacade: Send + Sync {
    /// Subscribe the caller's durable identity to the configured server,
    /// optionally filtered by application name and/or minimum priority.
    /// Persists when the identity is durable. Returns the new id.
    fn subscribe(
        &self,
        caller: &SessionKey,
        application: Option<String>,
        min_priority: Option<u8>,
    ) -> Result<Uuid, GotifySubscribeError>;

    /// The subscriptions registered for the caller's project.
    fn list(&self, caller: &SessionKey) -> Vec<GotifySubscription>;

    /// Remove a subscription by id within the caller's project. `true`
    /// when an entry was removed.
    fn unsubscribe(&self, caller: &SessionKey, id: Uuid) -> bool;
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

impl GotifyFacade for ProdGotifyFacade {
    fn subscribe(
        &self,
        caller: &SessionKey,
        application: Option<String>,
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
            application,
            min_priority,
            created_at: SystemTime::now(),
        };
        let id = sub.id;
        ws.add_gotify_subscription(sub, durable);
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
        ws.remove_gotify_subscription(&cx.project_name, id)
    }
}

/// Resolve a caller to `(project_name, team_role, durable)`. `team_role`
/// is the worker's role label (`None` targets the lead); `durable` is
/// true for the lead or a forge.toml team worker, false for an ephemeral
/// ad-hoc worker.
fn resolve_identity(ws: &Workspace, caller: &SessionKey) -> Option<(String, Option<String>, bool)> {
    let cx = caller_context(ws, caller)?;
    let worker_label = if cx.is_lead {
        None
    } else {
        ws.list_live_workers(&cx.project_key)
            .into_iter()
            .find(|w| w.session_key == *caller)
            .map(|w| w.label)
    };
    let team = ws
        .list_projects()
        .into_iter()
        .find(|v| v.key == cx.project_key)
        .map(|v| v.team)
        .unwrap_or_default();
    let (team_role, durable) = subscriber_durability(worker_label.as_deref(), &team);
    Some((cx.project_name, team_role, durable))
}

/// `(team_role, durable)` given the caller's worker label (`None` = the
/// lead or a plain catalog session) and the project's forge.toml team
/// labels. A lead is durable and targets itself; a worker is durable
/// only when its label is a configured team role.
fn subscriber_durability(worker_label: Option<&str>, team: &[String]) -> (Option<String>, bool) {
    match worker_label {
        None => (None, true),
        Some(label) => (Some(label.to_owned()), team.iter().any(|t| t == label)),
    }
}

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool correctly parses args, resolves the caller, and surfaces
/// facade results/errors - without a real workspace.
/// One recorded `subscribe` call: `(caller, application, min_priority)`.
#[cfg(test)]
type SubscribeCall = (SessionKey, Option<String>, Option<u8>);

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockGotifyFacade {
    pub subs: parking_lot::Mutex<Vec<GotifySubscription>>,
    pub subscribe_calls: parking_lot::Mutex<Vec<SubscribeCall>>,
    pub subscribe_result: parking_lot::Mutex<Option<Result<Uuid, GotifySubscribeError>>>,
    pub unsubscribe_calls: parking_lot::Mutex<Vec<(SessionKey, Uuid)>>,
    pub unsubscribe_result: parking_lot::Mutex<Option<bool>>,
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
impl GotifyFacade for MockGotifyFacade {
    fn subscribe(
        &self,
        caller: &SessionKey,
        application: Option<String>,
        min_priority: Option<u8>,
    ) -> Result<Uuid, GotifySubscribeError> {
        self.subscribe_calls.lock().push((caller.clone(), application, min_priority));
        self.subscribe_result.lock().clone().unwrap_or_else(|| Ok(Uuid::nil()))
    }

    fn list(&self, _caller: &SessionKey) -> Vec<GotifySubscription> {
        self.subs.lock().clone()
    }

    fn unsubscribe(&self, caller: &SessionKey, id: Uuid) -> bool {
        self.unsubscribe_calls.lock().push((caller.clone(), id));
        self.unsubscribe_result.lock().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lead_is_durable_and_targets_itself() {
        assert_eq!(subscriber_durability(None, &["reviewer".to_owned()]), (None, true));
    }

    #[test]
    fn team_worker_is_durable_with_its_role() {
        let team = vec!["reviewer".to_owned(), "tester".to_owned()];
        assert_eq!(
            subscriber_durability(Some("reviewer"), &team),
            (Some("reviewer".to_owned()), true),
        );
    }

    #[test]
    fn ad_hoc_worker_is_ephemeral() {
        let team = vec!["reviewer".to_owned()];
        assert_eq!(
            subscriber_durability(Some("scratch"), &team),
            (Some("scratch".to_owned()), false),
        );
    }
}

//! `SlackFacade` - the seam between the Slack MCP tools and workspace
//! state. The production impl ([`ProdSlackFacade`]) resolves the
//! requested workspace label and drives that workspace's client; the
//! mock returns preloaded results so the tool tests can assert argument
//! handling and error surfacing without a real workspace.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_primitives::slack::{
    SlackConversation, SlackSubscription, SlackSubscriptionTarget, SlackWatchMode,
};
use uuid::Uuid;

use crate::SessionKey;
use crate::mcp::caller_context::caller_context;
use crate::mcp::gotify::facade::resolve_identity;
use crate::slack::SlackWorkspaces;
use crate::workspace::Workspace;

/// Why `slack__list` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackListError {
    /// No `[[slack]]` entry in forge.toml.
    NotConfigured,
    /// The caller named a workspace that is not configured. Carries the
    /// labels that are, so the error can list them.
    UnknownWorkspace { requested: String, known: Vec<String> },
    /// Several workspaces are configured and the caller named none.
    WorkspaceRequired { known: Vec<String> },
    /// The Web API call failed. Carries the formatted error for the LLM.
    Fetch(String),
}

/// Why `slack__subscribe` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackSubscribeError {
    /// The label named no configured workspace, or none was named and
    /// there is not exactly one.
    UnknownWorkspace,
    /// The caller couldn't be mapped to a project.
    UnknownCallerProject,
}

/// What a caller asked to watch, before it becomes records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackSubscribeRequest {
    /// The whole DM class in this workspace.
    DirectMessages,
    /// Named conversations, each with its own mode.
    Conversations(Vec<SlackChannelWatch>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackChannelWatch {
    pub id: String,
    pub mode: SlackWatchMode,
}

/// The workspace to act on: the caller's choice when it names a
/// configured one, else the only configured one.
fn resolve_label(workspaces: &SlackWorkspaces, requested: Option<&str>) -> Option<String> {
    if let Some(label) = requested {
        return workspaces.client(label).map(|_| label.to_owned());
    }
    let labels = workspaces.labels();
    if labels.len() == 1 {
        return labels.into_iter().next();
    }
    None
}

/// The Slack tools' view of the workspace.
#[async_trait::async_trait]
pub(crate) trait SlackFacade: Send + Sync {
    /// Every conversation the named workspace's token is in. `None` picks
    /// the only configured workspace and errors when there is more than one.
    async fn conversations(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<SlackConversation>, SlackListError>;

    /// The subscription targets the caller OWNS in `workspace` - a lead's
    /// the lead's, a worker's its own, never another owner's. What
    /// `slack__list` marks, so one session never sees another's watches.
    fn subscribed_targets(
        &self,
        caller: &SessionKey,
        workspace: Option<&str>,
    ) -> Vec<SlackSubscriptionTarget>;

    /// Record what the caller wants to watch in `workspace`, one record
    /// per target. Returns the new record ids.
    fn subscribe(
        &self,
        caller: &SessionKey,
        workspace: Option<&str>,
        request: SlackSubscribeRequest,
    ) -> Result<Vec<Uuid>, SlackSubscribeError>;

    /// Remove one of the caller's OWN subscriptions by id. `false` both
    /// when no such id exists and when it belongs to another owner.
    fn unsubscribe(&self, caller: &SessionKey, id: Uuid) -> bool;
}

/// Production facade over `Weak<Workspace>` (weak to avoid a cycle with
/// the MCP server the workspace owns).
pub(crate) struct ProdSlackFacade {
    workspace: Weak<Workspace>,
}

impl ProdSlackFacade {
    pub(crate) fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn SlackFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }
}

#[async_trait::async_trait]
impl SlackFacade for ProdSlackFacade {
    async fn conversations(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<SlackConversation>, SlackListError> {
        let ws = self.workspace.upgrade().ok_or(SlackListError::NotConfigured)?;
        if ws.slack.is_empty() {
            return Err(SlackListError::NotConfigured);
        }
        let Some(label) = resolve_label(&ws.slack, workspace) else {
            return Err(match workspace {
                Some(requested) => SlackListError::UnknownWorkspace {
                    requested: requested.to_owned(),
                    known: ws.slack.labels(),
                },
                None => SlackListError::WorkspaceRequired { known: ws.slack.labels() },
            });
        };
        let client = ws.slack.client(&label).ok_or_else(|| SlackListError::UnknownWorkspace {
            requested: label.clone(),
            known: ws.slack.labels(),
        })?;
        client.list_conversations().await.map_err(|err| SlackListError::Fetch(err.to_string()))
    }

    fn subscribed_targets(
        &self,
        caller: &SessionKey,
        workspace: Option<&str>,
    ) -> Vec<SlackSubscriptionTarget> {
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        let Some(label) = resolve_label(&ws.slack, workspace) else { return Vec::new() };
        let Some(cx) = caller_context(&ws, caller) else { return Vec::new() };
        ws.slack_subscriptions_for_project(&cx.project_name)
            .into_iter()
            .filter(|sub| sub.workspace == label && sub.team_role == cx.worker_label)
            .map(|sub| sub.target)
            .collect()
    }

    fn subscribe(
        &self,
        caller: &SessionKey,
        workspace: Option<&str>,
        request: SlackSubscribeRequest,
    ) -> Result<Vec<Uuid>, SlackSubscribeError> {
        let ws = self.workspace.upgrade().ok_or(SlackSubscribeError::UnknownCallerProject)?;
        let label =
            resolve_label(&ws.slack, workspace).ok_or(SlackSubscribeError::UnknownWorkspace)?;
        let (project, team_role, durable) =
            resolve_identity(&ws, caller).ok_or(SlackSubscribeError::UnknownCallerProject)?;
        let targets = match request {
            SlackSubscribeRequest::DirectMessages => {
                vec![SlackSubscriptionTarget::DirectMessages]
            }
            SlackSubscribeRequest::Conversations(watches) => watches
                .into_iter()
                .map(|watch| SlackSubscriptionTarget::Conversation {
                    id: watch.id,
                    mode: watch.mode,
                })
                .collect(),
        };
        let mut ids = Vec::with_capacity(targets.len());
        for target in targets {
            let sub = SlackSubscription {
                id: Uuid::new_v4(),
                workspace: label.clone(),
                project: project.clone(),
                team_role: team_role.clone(),
                target,
                created_at: SystemTime::now(),
            };
            ids.push(sub.id);
            ws.add_slack_subscription(sub, durable);
        }
        // A workspace that just gained its first subscription needs a pump.
        ws.start_slack_subsystem();
        Ok(ids)
    }

    fn unsubscribe(&self, caller: &SessionKey, id: Uuid) -> bool {
        let Some(ws) = self.workspace.upgrade() else { return false };
        let Some(cx) = caller_context(&ws, caller) else { return false };
        let removed =
            ws.remove_slack_subscription_owned_by(&cx.project_name, id, cx.worker_label.as_deref());
        ws.stop_slack_subsystem_if_idle();
        removed
    }
}

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool parses args, resolves the caller, and surfaces results/errors.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockSlackFacade {
    pub conversations_calls: parking_lot::Mutex<Vec<Option<String>>>,
    pub conversations_result:
        parking_lot::Mutex<Option<Result<Vec<SlackConversation>, SlackListError>>>,
    pub subscribed_targets_calls: parking_lot::Mutex<Vec<Option<String>>>,
    pub subscribed_targets: parking_lot::Mutex<Vec<SlackSubscriptionTarget>>,
    pub subscribe_calls: parking_lot::Mutex<Vec<(Option<String>, SlackSubscribeRequest)>>,
    pub subscribe_result: parking_lot::Mutex<Option<Result<Vec<Uuid>, SlackSubscribeError>>>,
    pub unsubscribe_calls: parking_lot::Mutex<Vec<Uuid>>,
    pub unsubscribe_result: parking_lot::Mutex<Option<bool>>,
}

#[cfg(test)]
impl MockSlackFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }
    pub(crate) fn into_arc(self) -> Arc<dyn SlackFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl SlackFacade for MockSlackFacade {
    async fn conversations(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<SlackConversation>, SlackListError> {
        self.conversations_calls.lock().push(workspace.map(str::to_owned));
        self.conversations_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    fn subscribed_targets(
        &self,
        _caller: &SessionKey,
        workspace: Option<&str>,
    ) -> Vec<SlackSubscriptionTarget> {
        self.subscribed_targets_calls.lock().push(workspace.map(str::to_owned));
        self.subscribed_targets.lock().clone()
    }

    fn subscribe(
        &self,
        _caller: &SessionKey,
        workspace: Option<&str>,
        request: SlackSubscribeRequest,
    ) -> Result<Vec<Uuid>, SlackSubscribeError> {
        self.subscribe_calls.lock().push((workspace.map(str::to_owned), request));
        self.subscribe_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    fn unsubscribe(&self, _caller: &SessionKey, id: Uuid) -> bool {
        self.unsubscribe_calls.lock().push(id);
        self.unsubscribe_result.lock().unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoadedConfig;
    use forge_primitives::slack::SlackConfig;
    use tempfile::tempdir;

    fn caller() -> SessionKey {
        SessionKey::from_session_id("caller-uuid")
    }

    fn cfg(label: &str) -> SlackConfig {
        SlackConfig { workspace: label.to_owned(), token: "xoxp-test".to_owned(), poll_seconds: 30 }
    }

    fn workspace_with_no_slack() -> Arc<Workspace> {
        let (ws, _rx) = Workspace::testing_stub();
        ws
    }

    /// A workspace whose `[[slack]]` holds exactly `label`, with a lead
    /// session recorded so the caller resolves to a project.
    fn workspace_with_one_slack_workspace(label: &str) -> (Arc<Workspace>, Arc<dyn SlackFacade>) {
        let dir = tempdir().expect("tempdir");
        let mut config = LoadedConfig::empty_for_test();
        config.slack = vec![cfg(label)];
        let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config);
        ws.seed_test_project("forge", "/tmp/slack-facade-scope");
        ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
        let facade = ProdSlackFacade::from_arc(&ws);
        (ws, facade)
    }

    /// `tokio::test`: a successful subscribe starts the workspace's pump,
    /// which needs a runtime.
    #[tokio::test]
    async fn subscribing_to_an_unknown_workspace_is_refused() {
        // Bound, not inline: the facade holds a Weak, so a temporary Arc
        // would drop before the call and every error would come back as
        // UnknownCallerProject instead of the one under test.
        let ws = workspace_with_no_slack();
        let facade = ProdSlackFacade::from_arc(&ws);
        let err = facade
            .subscribe(&caller(), Some("nope"), SlackSubscribeRequest::DirectMessages)
            .expect_err("a workspace that is not configured cannot be watched");
        assert_eq!(err, SlackSubscribeError::UnknownWorkspace);
    }

    #[tokio::test]
    async fn subscribing_to_dms_then_a_channel_yields_two_records() {
        let (ws, facade) = workspace_with_one_slack_workspace("acme");
        let first = facade
            .subscribe(&caller(), Some("acme"), SlackSubscribeRequest::DirectMessages)
            .expect("dms");
        let second = facade
            .subscribe(
                &caller(),
                Some("acme"),
                SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                    id: "C1".to_owned(),
                    mode: SlackWatchMode::All,
                }]),
            )
            .expect("channels");
        assert_eq!(first.len() + second.len(), 2);
        assert_eq!(ws.slack_subscriptions_for_project("forge").len(), 2);
    }
}

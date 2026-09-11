//! `SlackFacade` - the seam between the Slack MCP tools and workspace
//! state. The production impl ([`ProdSlackFacade`]) resolves the
//! requested workspace label and drives that workspace's client; the
//! mock returns preloaded results so the tool tests can assert argument
//! handling and error surfacing without a real workspace.

use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_primitives::slack::{
    SlackConversation, SlackDraft, SlackSubscription, SlackSubscriptionTarget, SlackWatchMode,
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

/// What a caller asked to post. Composed here, not in the connector, so
/// the draft can be held for the user's decision before anything is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackPostRequest {
    /// Omitted when only one workspace is configured.
    pub workspace: Option<String>,
    pub conversation: String,
    /// `None` posts a root message; `Some(ts)` replies into that thread.
    pub thread_ts: Option<String>,
    pub text: String,
}

/// Why a post did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackPostError {
    /// The user rejected the draft. Nothing was posted.
    Rejected,
    /// The named workspace is not configured.
    UnknownWorkspace,
    /// The Web API call failed. Carries the formatted error for the LLM.
    Fetch(String),
}

/// What a post did. A draft past the truncation point is split and
/// posted in sequence, so this is usually one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackPostOutcome {
    pub posted: usize,
}

/// One message to edit or delete. Not gated: what posting was gated for
/// does not apply to correcting your own message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackEditRequest {
    /// Omitted when only one workspace is configured.
    pub workspace: Option<String>,
    pub conversation: String,
    pub ts: String,
    /// `None` deletes the message; `Some(text)` replaces its body.
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackEditError {
    /// The message was not posted by the authenticated user, so Slack
    /// would refuse the update anyway. Refused locally so the caller gets
    /// a clear reason rather than a wire error code.
    NotOwnMessage,
    UnknownWorkspace,
    Fetch(String),
}

/// One reaction to add or remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackReactRequest {
    /// Omitted when only one workspace is configured.
    pub workspace: Option<String>,
    pub conversation: String,
    pub ts: String,
    /// Slack's shortcode, without colons, e.g. `white_check_mark`.
    pub name: String,
    pub add: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackReactError {
    UnknownWorkspace,
    Fetch(String),
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

    /// Post a message, held for the user's decision first. Returns only
    /// once that decision is in, and nothing is sent unless it was an
    /// approval - a rejected or unanswered draft is `Rejected`.
    async fn post(
        &self,
        caller: &SessionKey,
        request: SlackPostRequest,
    ) -> Result<SlackPostOutcome, SlackPostError>;

    /// Replace the body of one of the user's own messages, or delete it
    /// when `text` is `None`. Not gated.
    async fn edit(&self, request: SlackEditRequest) -> Result<(), SlackEditError>;

    /// Add or remove one reaction. Not gated.
    async fn react(&self, request: SlackReactRequest) -> Result<(), SlackReactError>;

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

    async fn post(
        &self,
        caller: &SessionKey,
        request: SlackPostRequest,
    ) -> Result<SlackPostOutcome, SlackPostError> {
        let ws = self.workspace.upgrade().ok_or(SlackPostError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, request.workspace.as_deref())
            .ok_or(SlackPostError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackPostError::UnknownWorkspace)?;
        let draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label,
            conversation: request.conversation,
            thread_ts: request.thread_ts,
            text: request.text,
        };
        let (_draft_id, decision) = ws.register_slack_draft(caller, draft.clone());
        // Fails closed: a caller that went away without answering is not
        // an approval, and neither is a rejected one.
        if !decision.await.unwrap_or(false) {
            return Err(SlackPostError::Rejected);
        }
        let parts = forge_connectors::slack::split_for_post(&draft.text);
        for part in &parts {
            api.post_message(&draft.conversation, part, draft.thread_ts.as_deref())
                .await
                .map_err(|err| SlackPostError::Fetch(err.to_string()))?;
        }
        Ok(SlackPostOutcome { posted: parts.len() })
    }

    async fn edit(&self, request: SlackEditRequest) -> Result<(), SlackEditError> {
        let ws = self.workspace.upgrade().ok_or(SlackEditError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, request.workspace.as_deref())
            .ok_or(SlackEditError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackEditError::UnknownWorkspace)?;
        let own = ws.slack_user_ids.lock().get(&label).cloned();
        let author = api
            .message_author(&request.conversation, &request.ts)
            .await
            .map_err(|err| SlackEditError::Fetch(err.to_string()))?;
        if own.is_none() || author != own {
            return Err(SlackEditError::NotOwnMessage);
        }
        let result = match &request.text {
            Some(text) => api.update_message(&request.conversation, &request.ts, text).await,
            None => api.delete_message(&request.conversation, &request.ts).await,
        };
        result.map_err(|err| SlackEditError::Fetch(err.to_string()))
    }

    async fn react(&self, request: SlackReactRequest) -> Result<(), SlackReactError> {
        let ws = self.workspace.upgrade().ok_or(SlackReactError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, request.workspace.as_deref())
            .ok_or(SlackReactError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackReactError::UnknownWorkspace)?;
        api.set_reaction(&request.conversation, &request.ts, &request.name, request.add)
            .await
            .map_err(|err| SlackReactError::Fetch(err.to_string()))
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
    pub post_calls: parking_lot::Mutex<Vec<SlackPostRequest>>,
    pub post_result: parking_lot::Mutex<Option<Result<SlackPostOutcome, SlackPostError>>>,
    pub edit_calls: parking_lot::Mutex<Vec<SlackEditRequest>>,
    pub edit_result: parking_lot::Mutex<Option<Result<(), SlackEditError>>>,
    pub react_calls: parking_lot::Mutex<Vec<SlackReactRequest>>,
    pub react_result: parking_lot::Mutex<Option<Result<(), SlackReactError>>>,
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

    async fn post(
        &self,
        _caller: &SessionKey,
        request: SlackPostRequest,
    ) -> Result<SlackPostOutcome, SlackPostError> {
        self.post_calls.lock().push(request);
        self.post_result.lock().clone().unwrap_or(Ok(SlackPostOutcome { posted: 1 }))
    }

    async fn edit(&self, request: SlackEditRequest) -> Result<(), SlackEditError> {
        self.edit_calls.lock().push(request);
        self.edit_result.lock().clone().unwrap_or(Ok(()))
    }

    async fn react(&self, request: SlackReactRequest) -> Result<(), SlackReactError> {
        self.react_calls.lock().push(request);
        self.react_result.lock().clone().unwrap_or(Ok(()))
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
    use forge_connectors::slack::{AuthTest, MessagePage, SlackApi, SlackError};
    use forge_primitives::slack::SlackConfig;
    use std::collections::{BTreeMap, HashMap};
    use tempfile::tempdir;

    fn caller() -> SessionKey {
        SessionKey::from_session_id("caller-uuid")
    }

    /// Records every outbound call, so a test can assert what did and did
    /// not reach Slack.
    #[derive(Default)]
    struct RecordingApi {
        posts: parking_lot::Mutex<Vec<(String, String, Option<String>)>>,
        updates: parking_lot::Mutex<Vec<(String, String, String)>>,
        deletes: parking_lot::Mutex<Vec<(String, String)>>,
        reactions: parking_lot::Mutex<Vec<(String, String, String, bool)>>,
        authors: parking_lot::Mutex<HashMap<String, String>>,
    }

    impl RecordingApi {
        fn seed_author(&self, ts: &str, user: &str) {
            self.authors.lock().insert(ts.to_owned(), user.to_owned());
        }
        fn posts(&self) -> Vec<(String, String, Option<String>)> {
            self.posts.lock().clone()
        }
    }

    #[async_trait::async_trait]
    impl SlackApi for RecordingApi {
        async fn auth_test(&self) -> Result<AuthTest, SlackError> {
            Ok(AuthTest {
                team: "Test".to_owned(),
                user: "tester".to_owned(),
                team_id: "T1".to_owned(),
                user_id: "U1".to_owned(),
                url: "https://test.slack.com/".to_owned(),
            })
        }

        async fn list_conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
            Ok(Vec::new())
        }

        async fn history(
            &self,
            _channel: &str,
            _oldest: Option<&str>,
            _limit: u32,
            _cursor: Option<&str>,
        ) -> Result<MessagePage, SlackError> {
            Ok(MessagePage { messages: Vec::new(), next_cursor: None })
        }

        async fn replies(
            &self,
            _channel: &str,
            _ts: &str,
            _limit: u32,
            _cursor: Option<&str>,
        ) -> Result<MessagePage, SlackError> {
            Ok(MessagePage { messages: Vec::new(), next_cursor: None })
        }

        async fn post_message(
            &self,
            channel: &str,
            text: &str,
            thread_ts: Option<&str>,
        ) -> Result<(), SlackError> {
            self.posts.lock().push((
                channel.to_owned(),
                text.to_owned(),
                thread_ts.map(str::to_owned),
            ));
            Ok(())
        }

        async fn update_message(
            &self,
            channel: &str,
            ts: &str,
            text: &str,
        ) -> Result<(), SlackError> {
            self.updates.lock().push((channel.to_owned(), ts.to_owned(), text.to_owned()));
            Ok(())
        }

        async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), SlackError> {
            self.deletes.lock().push((channel.to_owned(), ts.to_owned()));
            Ok(())
        }

        async fn set_reaction(
            &self,
            channel: &str,
            ts: &str,
            name: &str,
            add: bool,
        ) -> Result<(), SlackError> {
            self.reactions.lock().push((channel.to_owned(), ts.to_owned(), name.to_owned(), add));
            Ok(())
        }

        async fn message_author(
            &self,
            _channel: &str,
            ts: &str,
        ) -> Result<Option<String>, SlackError> {
            Ok(self.authors.lock().get(ts).cloned())
        }
    }

    /// A workspace whose one Slack workspace is the recording double, with
    /// a lead session recorded so the caller resolves to a project.
    fn facade_with_recording_slack() -> (Arc<dyn SlackFacade>, Arc<Workspace>, Arc<RecordingApi>) {
        let dir = tempdir().expect("tempdir");
        let mut config = LoadedConfig::empty_for_test();
        config.slack = vec![cfg("acme")];
        let api = Arc::new(RecordingApi::default());
        let mut apis: BTreeMap<String, Arc<dyn SlackApi>> = BTreeMap::new();
        apis.insert("acme".to_owned(), api.clone());
        let (ws, _rx) = Workspace::testing_stub_with_slack(
            dir.path().to_path_buf(),
            config,
            Arc::new(crate::slack::SlackWorkspaces::from_apis(apis)),
        );
        ws.seed_test_project("forge", "/tmp/slack-facade-scope");
        ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
        // The own-message check reads the workspace's resolved user id.
        ws.slack_user_ids.lock().insert("acme".to_owned(), "U1".to_owned());
        let facade = ProdSlackFacade::from_arc(&ws);
        (facade, ws, api)
    }

    /// Wait for the blocked post to register its draft, then hand back its
    /// id. The post is parked on a oneshot until this is resolved.
    async fn wait_for_draft(ws: &Arc<Workspace>) -> Uuid {
        for _ in 0..200 {
            if let Some(id) = ws.slack_drafts.lock().keys().next().copied() {
                return id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("no draft was registered");
    }

    fn post_request(conversation: &str, text: &str) -> SlackPostRequest {
        SlackPostRequest {
            workspace: Some("acme".to_owned()),
            conversation: conversation.to_owned(),
            thread_ts: None,
            text: text.to_owned(),
        }
    }

    fn edit_request(conversation: &str, ts: &str, text: Option<&str>) -> SlackEditRequest {
        SlackEditRequest {
            workspace: Some("acme".to_owned()),
            conversation: conversation.to_owned(),
            ts: ts.to_owned(),
            text: text.map(str::to_owned),
        }
    }

    fn react_request(conversation: &str, ts: &str, name: &str, add: bool) -> SlackReactRequest {
        SlackReactRequest {
            workspace: Some("acme".to_owned()),
            conversation: conversation.to_owned(),
            ts: ts.to_owned(),
            name: name.to_owned(),
            add,
        }
    }

    #[tokio::test]
    async fn a_rejected_draft_never_reaches_slack() {
        let (facade, ws, api) = facade_with_recording_slack();
        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), false);

        let outcome = task.await.expect("no panic");
        assert_eq!(outcome, Err(SlackPostError::Rejected));
        assert!(api.posts().is_empty(), "a rejected draft must not post");
    }

    /// A prompt nobody answers must read as "not approved". Dropping the
    /// sender is how that happens - the session went away, or the dock
    /// prompt was cleared without a decision - and it must never be taken
    /// for an approval.
    #[tokio::test]
    async fn a_draft_whose_answer_never_comes_posts_nothing() {
        let (facade, ws, api) = facade_with_recording_slack();
        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        drop(ws.slack_drafts.lock().remove(&id));

        let outcome = task.await.expect("no panic");
        assert_eq!(outcome, Err(SlackPostError::Rejected));
        assert!(api.posts().is_empty(), "an unanswered draft must not post");
    }

    #[tokio::test]
    async fn an_approved_draft_posts_exactly_once() {
        let (facade, ws, api) = facade_with_recording_slack();
        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), true);

        task.await.expect("no panic").expect("an approved draft posts");
        assert_eq!(api.posts().len(), 1, "exactly one message goes out");
    }

    #[tokio::test]
    async fn a_thread_reply_carries_the_parent_ts() {
        let (facade, ws, api) = facade_with_recording_slack();
        let task = tokio::spawn({
            let facade = facade.clone();
            async move {
                let mut request = post_request("C1", "hi");
                request.thread_ts = Some("100.0".to_owned());
                facade.post(&caller(), request).await
            }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), true);
        task.await.expect("no panic").expect("posted");

        assert_eq!(api.posts()[0].2.as_deref(), Some("100.0"));
    }

    #[tokio::test]
    async fn editing_someone_elses_message_is_refused_before_it_reaches_slack() {
        // Slack's own rule: only the authenticated user's messages can be
        // updated. Refused locally so the agent gets a clear reason rather
        // than a wire error code.
        let (facade, _ws, api) = facade_with_recording_slack();
        api.seed_author("100.0", "U-OTHER");

        let err = facade.edit(edit_request("C1", "100.0", Some("new text"))).await;
        assert_eq!(err, Err(SlackEditError::NotOwnMessage));
        assert!(api.updates.lock().is_empty(), "a foreign message is never updated");
    }

    #[tokio::test]
    async fn editing_own_message_updates_it() {
        let (facade, _ws, api) = facade_with_recording_slack();
        api.seed_author("100.0", "U1");

        facade.edit(edit_request("C1", "100.0", Some("new text"))).await.expect("updated");
        assert_eq!(api.updates.lock().len(), 1);
    }

    #[tokio::test]
    async fn deleting_own_message_removes_it() {
        let (facade, _ws, api) = facade_with_recording_slack();
        api.seed_author("100.0", "U1");

        facade.edit(edit_request("C1", "100.0", None)).await.expect("deleted");
        assert_eq!(api.deletes.lock().len(), 1);
    }

    #[tokio::test]
    async fn reacting_adds_then_removes() {
        let (facade, _ws, api) = facade_with_recording_slack();

        facade.react(react_request("C1", "100.0", "white_check_mark", true)).await.expect("added");
        facade
            .react(react_request("C1", "100.0", "white_check_mark", false))
            .await
            .expect("removed");

        let reactions = api.reactions.lock().clone();
        assert_eq!(reactions.len(), 2);
        assert!(reactions[0].3, "the first call adds");
        assert!(!reactions[1].3, "the second removes");
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

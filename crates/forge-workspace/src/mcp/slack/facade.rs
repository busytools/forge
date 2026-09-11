//! `SlackFacade` - the seam between the Slack MCP tools and workspace
//! state. The production impl ([`ProdSlackFacade`]) resolves the
//! requested workspace label and drives that workspace's client; the
//! mock returns preloaded results so the tool tests can assert argument
//! handling and error surfacing without a real workspace.

use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use forge_connectors::slack::MENTION_CURSOR;
use forge_primitives::slack::{
    SlackBookmark, SlackConversation, SlackDraft, SlackPin, SlackSearchMatch, SlackSubscription,
    SlackSubscriptionTarget, SlackUser, SlackWatchMode,
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
    /// Some parts of a split draft are live and the rest is not. Carries
    /// how many landed so a retry resumes instead of duplicating.
    Partial { posted: usize, total: usize, source: String },
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

/// One message to edit or delete. Both actions are gated like a post.
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
    /// The user rejected the replacement, or it went unanswered. The
    /// message is untouched.
    Rejected,
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
    /// The user rejected the reaction, or it went unanswered. Nothing was
    /// added or removed.
    Rejected,
    UnknownWorkspace,
    Fetch(String),
}

/// Why a read-only lookup failed. Shared by the search and the user
/// lookup because their failure sets are identical; the per-operation
/// error convention exists where the sets differ (post, edit and react
/// each fail differently), and two names for one set would be duplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackReadError {
    UnknownWorkspace,
    Fetch(String),
}

/// What a caller asked to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackFetchRequest {
    /// Omitted when only one workspace is configured.
    pub workspace: Option<String>,
    pub file_id: String,
    /// The directory the file lands in. The name inside it comes from
    /// Slack and is sanitised before it is used.
    pub dir: std::path::PathBuf,
}

/// What a caller asked to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackUploadRequest {
    /// Omitted when only one workspace is configured.
    pub workspace: Option<String>,
    pub conversation: String,
    /// `None` posts a root message; `Some(ts)` replies into that thread.
    pub thread_ts: Option<String>,
    pub path: std::path::PathBuf,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackAttachmentError {
    /// The file could not be read from disk, or written to it.
    Io(String),
    /// The user rejected the upload. Nothing was sent.
    Rejected,
    UnknownWorkspace,
    /// The Web API or the transfer failed.
    Fetch(String),
}

/// A file name that came from Slack is untrusted input: it is whatever the
/// uploader chose to call the file. Take only the final path component,
/// refuse `.` and `..`, and fall back to the file id when nothing usable
/// is left - so the result can only ever be one name inside the directory
/// the caller asked for.
fn safe_file_name(name: &str, fallback: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." {
        return fallback.to_owned();
    }
    base.to_owned()
}

/// How long a held draft waits for the user before it is rejected. Long
/// enough for a working session to reach the dock; short enough that a
/// prompt nobody can answer does not hold a session forever.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

/// What a caller asked to watch, before it becomes records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackSubscribeRequest {
    /// The whole DM class in this workspace.
    DirectMessages,
    /// Named conversations, each with its own mode.
    Conversations(Vec<SlackChannelWatch>),
    /// Being mentioned anywhere in the workspace.
    Mentions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackChannelWatch {
    pub id: String,
    pub mode: SlackWatchMode,
}

/// The current instant as a Slack `ts`, so a new subscription can start
/// from now rather than from the channel's history.
fn slack_ts_now() -> String {
    let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:06}", now.as_secs(), now.subsec_micros())
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
    /// when `text` is `None`.
    ///
    /// Both arms are gated exactly as `slack__post` is. A replacement puts
    /// new words in front of people as the user, which is the same act a
    /// post is; a deletion changes what they see on a message attributed
    /// to him and, unlike a bad edit, cannot be fixed by editing again.
    /// Either way the call waits for the decision and fails closed on a
    /// rejected or unanswered draft.
    async fn edit(
        &self,
        caller: &SessionKey,
        request: SlackEditRequest,
    ) -> Result<(), SlackEditError>;

    /// Add or remove one reaction. Not gated.
    async fn react(
        &self,
        caller: &SessionKey,
        request: SlackReactRequest,
    ) -> Result<(), SlackReactError>;

    /// Fetch one Slack file to `request.dir` and return where it landed.
    /// Not gated: reading a file is not posting one, and the name is
    /// sanitised so it cannot escape the directory.
    async fn fetch_attachment(
        &self,
        request: SlackFetchRequest,
    ) -> Result<std::path::PathBuf, SlackAttachmentError>;

    /// Upload a local file into a conversation, held for the user's
    /// decision first - an upload posts new content, so it is a first
    /// send and belongs behind the same gate as `slack__post`.
    async fn post_attachment(
        &self,
        caller: &SessionKey,
        request: SlackUploadRequest,
    ) -> Result<(), SlackAttachmentError>;

    /// `search.messages` in a workspace, always timestamp-ordered. It
    /// matches text; it cannot find every message that mentions a user.
    async fn search(
        &self,
        workspace: Option<&str>,
        query: &str,
        count: u32,
    ) -> Result<Vec<SlackSearchMatch>, SlackReadError>;

    /// `users.info` for one user id.
    async fn user(&self, workspace: Option<&str>, user: &str) -> Result<SlackUser, SlackReadError>;

    /// `pins.list` for one conversation.
    async fn pins(
        &self,
        workspace: Option<&str>,
        conversation: &str,
    ) -> Result<Vec<SlackPin>, SlackReadError>;

    /// `bookmarks.list` for one conversation.
    async fn bookmarks(
        &self,
        workspace: Option<&str>,
        conversation: &str,
    ) -> Result<Vec<SlackBookmark>, SlackReadError>;

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

    /// Register a draft for the user's decision and wait for it. The one
    /// place "did the user approve this" is decided, so the fail-closed
    /// path exists once rather than at each gated operation - posting,
    /// replacing a body and deleting one all come through here.
    ///
    /// A generous timeout resolves to a rejection rather than holding the
    /// session forever on a prompt nobody can answer.
    async fn await_approval(
        workspace: &Arc<Workspace>,
        caller: &SessionKey,
        draft: SlackDraft,
    ) -> bool {
        let (id, decision) = workspace.register_slack_draft(caller, draft);
        let answer = tokio::time::timeout(APPROVAL_TIMEOUT, decision).await;
        // Rejected, dropped caller, or timed out: none is an approval, and
        // all leave the message unsent.
        let approved = matches!(answer, Ok(Ok(true)));
        if !approved {
            workspace.resolve_slack_draft(id, caller, false);
        }
        approved
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
        if !Self::await_approval(&ws, caller, draft.clone()).await {
            return Err(SlackPostError::Rejected);
        }
        let parts = forge_connectors::slack::split_for_post(&draft.text);
        let mut posted = 0;
        for part in &parts {
            match api.post_message(&draft.conversation, part, draft.thread_ts.as_deref()).await {
                Ok(()) => posted += 1,
                Err(error) if posted == 0 => {
                    // Nothing landed, so the failure is an ordinary one.
                    return Err(SlackPostError::Fetch(error.to_string()));
                }
                Err(error) => {
                    // Say exactly what landed: a truncated multi-part post
                    // is live and readable, so a retry must resume rather
                    // than duplicate the parts already sent.
                    return Err(SlackPostError::Partial {
                        posted,
                        total: parts.len(),
                        source: error.to_string(),
                    });
                }
            }
        }
        Ok(SlackPostOutcome { posted: parts.len() })
    }

    async fn edit(
        &self,
        caller: &SessionKey,
        request: SlackEditRequest,
    ) -> Result<(), SlackEditError> {
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

        // A replacement puts new words in front of people as the user, and
        // a deletion changes what they see on a message attributed to him -
        // and unlike a bad edit it cannot be fixed by editing again. Both
        // go through the same gate a post does.
        let draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label,
            conversation: request.conversation.clone(),
            thread_ts: Some(request.ts.clone()),
            text: match &request.text {
                Some(text) => text.clone(),
                None => format!("[delete] message {}", request.ts),
            },
        };
        if !Self::await_approval(&ws, caller, draft).await {
            return Err(SlackEditError::Rejected);
        }

        let result = match &request.text {
            Some(text) => api.update_message(&request.conversation, &request.ts, text).await,
            None => api.delete_message(&request.conversation, &request.ts).await,
        };
        result.map_err(|err| SlackEditError::Fetch(err.to_string()))
    }

    async fn react(
        &self,
        caller: &SessionKey,
        request: SlackReactRequest,
    ) -> Result<(), SlackReactError> {
        let ws = self.workspace.upgrade().ok_or(SlackReactError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, request.workspace.as_deref())
            .ok_or(SlackReactError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackReactError::UnknownWorkspace)?;

        // A reaction is authored content in the user's name, so it goes
        // through the same gate a post does.
        let verb = if request.add { "react" } else { "unreact" };
        let draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label,
            conversation: request.conversation.clone(),
            thread_ts: Some(request.ts.clone()),
            text: format!("[{verb}: {}]", request.name),
        };
        if !Self::await_approval(&ws, caller, draft).await {
            return Err(SlackReactError::Rejected);
        }

        api.set_reaction(&request.conversation, &request.ts, &request.name, request.add)
            .await
            .map_err(|err| SlackReactError::Fetch(err.to_string()))
    }

    async fn search(
        &self,
        workspace: Option<&str>,
        query: &str,
        count: u32,
    ) -> Result<Vec<SlackSearchMatch>, SlackReadError> {
        let ws = self.workspace.upgrade().ok_or(SlackReadError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, workspace).ok_or(SlackReadError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackReadError::UnknownWorkspace)?;
        api.search_messages(query, count, None)
            .await
            .map(|page| page.matches)
            .map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn user(&self, workspace: Option<&str>, user: &str) -> Result<SlackUser, SlackReadError> {
        let ws = self.workspace.upgrade().ok_or(SlackReadError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, workspace).ok_or(SlackReadError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackReadError::UnknownWorkspace)?;
        api.user_info(user).await.map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn pins(
        &self,
        workspace: Option<&str>,
        conversation: &str,
    ) -> Result<Vec<SlackPin>, SlackReadError> {
        let ws = self.workspace.upgrade().ok_or(SlackReadError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, workspace).ok_or(SlackReadError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackReadError::UnknownWorkspace)?;
        api.pins(conversation).await.map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn bookmarks(
        &self,
        workspace: Option<&str>,
        conversation: &str,
    ) -> Result<Vec<SlackBookmark>, SlackReadError> {
        let ws = self.workspace.upgrade().ok_or(SlackReadError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, workspace).ok_or(SlackReadError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackReadError::UnknownWorkspace)?;
        api.bookmarks(conversation).await.map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn fetch_attachment(
        &self,
        request: SlackFetchRequest,
    ) -> Result<std::path::PathBuf, SlackAttachmentError> {
        let ws = self.workspace.upgrade().ok_or(SlackAttachmentError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, request.workspace.as_deref())
            .ok_or(SlackAttachmentError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackAttachmentError::UnknownWorkspace)?;

        let file = api
            .file_info(&request.file_id)
            .await
            .map_err(|err| SlackAttachmentError::Fetch(err.to_string()))?;
        let bytes = api
            .get_bytes(&file.url_private)
            .await
            .map_err(|err| SlackAttachmentError::Fetch(err.to_string()))?;

        let name = safe_file_name(&file.name, &file.id);
        let path = request.dir.join(name);
        // The name comes from Slack and the directory is not exclusive to
        // this call, so never clobber a file already sitting there.
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|err| SlackAttachmentError::Io(err.to_string()))?;
        std::io::Write::write_all(&mut handle, &bytes)
            .map_err(|err| SlackAttachmentError::Io(err.to_string()))?;
        Ok(path)
    }

    async fn post_attachment(
        &self,
        caller: &SessionKey,
        request: SlackUploadRequest,
    ) -> Result<(), SlackAttachmentError> {
        let ws = self.workspace.upgrade().ok_or(SlackAttachmentError::UnknownWorkspace)?;
        let label = resolve_label(&ws.slack, request.workspace.as_deref())
            .ok_or(SlackAttachmentError::UnknownWorkspace)?;
        let api = ws.slack.client(&label).ok_or(SlackAttachmentError::UnknownWorkspace)?;

        let bytes = std::fs::read(&request.path)
            .map_err(|err| SlackAttachmentError::Io(err.to_string()))?;
        let name = request
            .path
            .file_name()
            .map_or_else(|| "file".to_owned(), |name| name.to_string_lossy().into_owned());

        // The draft names where the bytes come from as well as what they
        // will be called. The name is caller-controlled and can look
        // innocuous, so approving it without the local path would be
        // approving something the user has not seen.
        let draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label,
            conversation: request.conversation.clone(),
            thread_ts: request.thread_ts.clone(),
            text: format!("[file] {name}\nfrom {}", request.path.display()),
        };
        if !Self::await_approval(&ws, caller, draft).await {
            return Err(SlackAttachmentError::Rejected);
        }

        let (url, file_id) = api
            .get_upload_url(&name, bytes.len())
            .await
            .map_err(|err| SlackAttachmentError::Fetch(err.to_string()))?;
        api.post_bytes(&url, bytes)
            .await
            .map_err(|err| SlackAttachmentError::Fetch(err.to_string()))?;
        api.complete_upload(&file_id, &request.conversation, request.thread_ts.as_deref())
            .await
            .map_err(|err| SlackAttachmentError::Fetch(err.to_string()))
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
            SlackSubscribeRequest::Mentions => vec![SlackSubscriptionTarget::Mentions],
        };
        let mut ids = Vec::with_capacity(targets.len());
        let mut mentions_requested = false;
        for target in targets {
            if matches!(target, SlackSubscriptionTarget::Mentions) {
                mentions_requested = true;
            }
            let sub = SlackSubscription {
                id: Uuid::new_v4(),
                workspace: label.clone(),
                project: project.clone(),
                team_role: team_role.clone(),
                target,
                created_at: SystemTime::now(),
            };
            ids.push(sub.id);
            // A subscription starts from now, not from the channel's
            // history: without a cursor the first sweep would deliver the
            // newest page as if it were all new.
            if let SlackSubscriptionTarget::Conversation { id, .. } = &sub.target {
                ws.set_slack_watermark(&label, id, &slack_ts_now());
            }
            ws.add_slack_subscription(sub, durable);
        }
        if mentions_requested {
            ws.set_slack_watermark(&label, MENTION_CURSOR, &slack_ts_now());
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
    pub search_calls: parking_lot::Mutex<Vec<(Option<String>, String, u32)>>,
    pub search_result: parking_lot::Mutex<Option<Result<Vec<SlackSearchMatch>, SlackReadError>>>,
    pub user_calls: parking_lot::Mutex<Vec<(Option<String>, String)>>,
    pub user_result: parking_lot::Mutex<Option<Result<SlackUser, SlackReadError>>>,
    pub pins_calls: parking_lot::Mutex<Vec<(Option<String>, String)>>,
    pub pins_result: parking_lot::Mutex<Option<Result<Vec<SlackPin>, SlackReadError>>>,
    pub bookmarks_calls: parking_lot::Mutex<Vec<(Option<String>, String)>>,
    pub bookmarks_result: parking_lot::Mutex<Option<Result<Vec<SlackBookmark>, SlackReadError>>>,
    pub fetch_calls: parking_lot::Mutex<Vec<SlackFetchRequest>>,
    pub fetch_result: parking_lot::Mutex<Option<Result<std::path::PathBuf, SlackAttachmentError>>>,
    pub upload_calls: parking_lot::Mutex<Vec<SlackUploadRequest>>,
    pub upload_result: parking_lot::Mutex<Option<Result<(), SlackAttachmentError>>>,
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

    async fn search(
        &self,
        workspace: Option<&str>,
        query: &str,
        count: u32,
    ) -> Result<Vec<SlackSearchMatch>, SlackReadError> {
        self.search_calls.lock().push((workspace.map(str::to_owned), query.to_owned(), count));
        self.search_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn user(&self, workspace: Option<&str>, user: &str) -> Result<SlackUser, SlackReadError> {
        self.user_calls.lock().push((workspace.map(str::to_owned), user.to_owned()));
        self.user_result.lock().clone().unwrap_or_else(|| {
            Ok(SlackUser {
                id: user.to_owned(),
                name: "tester".to_owned(),
                real_name: None,
                tz: None,
            })
        })
    }

    async fn pins(
        &self,
        workspace: Option<&str>,
        conversation: &str,
    ) -> Result<Vec<SlackPin>, SlackReadError> {
        self.pins_calls.lock().push((workspace.map(str::to_owned), conversation.to_owned()));
        self.pins_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn bookmarks(
        &self,
        workspace: Option<&str>,
        conversation: &str,
    ) -> Result<Vec<SlackBookmark>, SlackReadError> {
        self.bookmarks_calls.lock().push((workspace.map(str::to_owned), conversation.to_owned()));
        self.bookmarks_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn fetch_attachment(
        &self,
        request: SlackFetchRequest,
    ) -> Result<std::path::PathBuf, SlackAttachmentError> {
        self.fetch_calls.lock().push(request);
        self.fetch_result.lock().clone().unwrap_or_else(|| Err(SlackAttachmentError::Rejected))
    }

    async fn post_attachment(
        &self,
        _caller: &SessionKey,
        request: SlackUploadRequest,
    ) -> Result<(), SlackAttachmentError> {
        self.upload_calls.lock().push(request);
        self.upload_result.lock().clone().unwrap_or(Ok(()))
    }

    async fn post(
        &self,
        _caller: &SessionKey,
        request: SlackPostRequest,
    ) -> Result<SlackPostOutcome, SlackPostError> {
        self.post_calls.lock().push(request);
        self.post_result.lock().clone().unwrap_or(Ok(SlackPostOutcome { posted: 1 }))
    }

    async fn edit(
        &self,
        _caller: &SessionKey,
        request: SlackEditRequest,
    ) -> Result<(), SlackEditError> {
        self.edit_calls.lock().push(request);
        self.edit_result.lock().clone().unwrap_or(Ok(()))
    }

    async fn react(
        &self,
        _caller: &SessionKey,
        request: SlackReactRequest,
    ) -> Result<(), SlackReactError> {
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
    use forge_connectors::slack::{AuthTest, MessagePage, SearchPage, SlackApi, SlackError};
    use forge_primitives::slack::{
        SlackBookmark, SlackConfig, SlackFile, SlackPin, SlackPinMessage,
    };
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
        /// Seeded files: id -> (name as the uploader chose it, bytes).
        files: parking_lot::Mutex<HashMap<String, (String, Vec<u8>)>>,
        uploads: parking_lot::Mutex<Vec<(String, String, Option<String>)>>,
        upload_urls: parking_lot::Mutex<usize>,
        /// Seeded reads: channel -> rows.
        pins: parking_lot::Mutex<HashMap<String, Vec<SlackPin>>>,
        bookmark_rows: parking_lot::Mutex<HashMap<String, Vec<SlackBookmark>>>,
        pin_reads: parking_lot::Mutex<Vec<String>>,
        bookmark_reads: parking_lot::Mutex<Vec<String>>,
    }

    impl RecordingApi {
        fn seed_author(&self, ts: &str, user: &str) {
            self.authors.lock().insert(ts.to_owned(), user.to_owned());
        }
        fn seed_file(&self, id: &str, name: &str, bytes: &[u8]) {
            self.files.lock().insert(id.to_owned(), (name.to_owned(), bytes.to_vec()));
        }
        fn seed_pins(&self, channel: &str, rows: Vec<SlackPin>) {
            self.pins.lock().insert(channel.to_owned(), rows);
        }
        fn seed_bookmarks(&self, channel: &str, rows: Vec<SlackBookmark>) {
            self.bookmark_rows.lock().insert(channel.to_owned(), rows);
        }
        fn posts(&self) -> Vec<(String, String, Option<String>)> {
            self.posts.lock().clone()
        }
        fn upload_url_count(&self) -> usize {
            *self.upload_urls.lock()
        }
        fn uploads(&self) -> Vec<(String, String, Option<String>)> {
            self.uploads.lock().clone()
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
            _oldest: Option<&str>,
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

        async fn file_info(&self, id: &str) -> Result<SlackFile, SlackError> {
            let files = self.files.lock();
            let Some((name, _)) = files.get(id) else {
                return Err(SlackError::Api {
                    method: "files.info".to_owned(),
                    error: "file_not_found".to_owned(),
                    needed: None,
                });
            };
            Ok(SlackFile {
                id: id.to_owned(),
                name: name.clone(),
                url_private: format!("https://files.test/{id}"),
            })
        }

        async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, SlackError> {
            let id = url.rsplit('/').next().unwrap_or_default();
            self.files.lock().get(id).map(|(_, bytes)| bytes.clone()).ok_or_else(|| {
                SlackError::Transport {
                    method: "download".to_owned(),
                    detail: format!("no seeded file at {url}"),
                }
            })
        }

        async fn post_bytes(&self, _url: &str, _body: Vec<u8>) -> Result<(), SlackError> {
            Ok(())
        }

        async fn get_upload_url(
            &self,
            name: &str,
            _length: usize,
        ) -> Result<(String, String), SlackError> {
            *self.upload_urls.lock() += 1;
            Ok((format!("https://upload.test/{name}"), "F-uploaded".to_owned()))
        }

        async fn complete_upload(
            &self,
            file_id: &str,
            channel: &str,
            thread_ts: Option<&str>,
        ) -> Result<(), SlackError> {
            self.uploads.lock().push((
                file_id.to_owned(),
                channel.to_owned(),
                thread_ts.map(str::to_owned),
            ));
            Ok(())
        }

        async fn search_messages(
            &self,
            _query: &str,
            _count: u32,
            _cursor: Option<&str>,
        ) -> Result<SearchPage, SlackError> {
            Ok(SearchPage { matches: Vec::new(), next_cursor: None })
        }

        async fn user_info(&self, user: &str) -> Result<SlackUser, SlackError> {
            Ok(SlackUser {
                id: user.to_owned(),
                name: "tester".to_owned(),
                real_name: None,
                tz: None,
            })
        }

        async fn pins(&self, channel: &str) -> Result<Vec<SlackPin>, SlackError> {
            self.pin_reads.lock().push(channel.to_owned());
            Ok(self.pins.lock().get(channel).cloned().unwrap_or_default())
        }

        async fn bookmarks(&self, channel: &str) -> Result<Vec<SlackBookmark>, SlackError> {
            self.bookmark_reads.lock().push(channel.to_owned());
            Ok(self.bookmark_rows.lock().get(channel).cloned().unwrap_or_default())
        }
    }

    /// A workspace whose one Slack workspace is the recording double, with
    /// a lead session recorded so the caller resolves to a project.
    fn facade_with_recording_slack() -> (
        Arc<dyn SlackFacade>,
        Arc<Workspace>,
        Arc<RecordingApi>,
        tokio::sync::mpsc::UnboundedReceiver<crate::protocol::SessionUpdate>,
    ) {
        let dir = tempdir().expect("tempdir");
        let mut config = LoadedConfig::empty_for_test();
        config.slack = vec![cfg("acme")];
        let api = Arc::new(RecordingApi::default());
        let mut apis: BTreeMap<String, Arc<dyn SlackApi>> = BTreeMap::new();
        apis.insert("acme".to_owned(), api.clone());
        let (ws, rx) = Workspace::testing_stub_with_slack(
            dir.path().to_path_buf(),
            config,
            Arc::new(crate::slack::SlackWorkspaces::from_apis(apis)),
        );
        ws.seed_test_project("forge", "/tmp/slack-facade-scope");
        ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
        // The own-message check reads the workspace's resolved user id.
        ws.slack_user_ids.lock().insert("acme".to_owned(), "U1".to_owned());
        let facade = ProdSlackFacade::from_arc(&ws);
        (facade, ws, api, rx)
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

    fn fetch_request(file_id: &str, dir: &std::path::Path) -> SlackFetchRequest {
        SlackFetchRequest {
            workspace: Some("acme".to_owned()),
            file_id: file_id.to_owned(),
            dir: dir.to_path_buf(),
        }
    }

    fn upload_request(conversation: &str, path: &std::path::Path) -> SlackUploadRequest {
        SlackUploadRequest {
            workspace: Some("acme".to_owned()),
            conversation: conversation.to_owned(),
            thread_ts: None,
            path: path.to_path_buf(),
            title: None,
        }
    }

    /// A name from Slack cannot escape the directory the caller chose, by
    /// either separator and however many parents it carries.
    #[test]
    fn a_file_name_from_slack_cannot_escape_its_directory() {
        assert_eq!(safe_file_name("notes.txt", "F1"), "notes.txt");
        assert_eq!(safe_file_name("../../escape.txt", "F1"), "escape.txt");
        assert_eq!(safe_file_name("..\\..\\escape.txt", "F1"), "escape.txt");
        assert_eq!(safe_file_name("/etc/passwd", "F1"), "passwd");
        assert_eq!(safe_file_name("..", "F1"), "F1", "a bare parent falls back to the id");
        assert_eq!(safe_file_name(".", "F1"), "F1");
        assert_eq!(safe_file_name("", "F1"), "F1");
        assert_eq!(safe_file_name("dir/", "F1"), "F1", "a trailing separator leaves nothing");
    }

    #[tokio::test]
    async fn an_attachment_lands_on_disk_with_the_expected_bytes() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_file("F1", "notes.txt", b"attachment body");
        let dir = tempdir().expect("tempdir");

        let path = facade.fetch_attachment(fetch_request("F1", dir.path())).await.expect("fetched");
        assert_eq!(std::fs::read(&path).expect("read back"), b"attachment body");
        assert_eq!(path.file_name().and_then(|name| name.to_str()), Some("notes.txt"));
    }

    #[tokio::test]
    async fn a_download_refuses_to_escape_the_chosen_directory() {
        // The name comes from Slack, so it is attacker-controlled input as
        // far as this code is concerned.
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_file("F1", "../../escape.txt", b"nope");
        let dir = tempdir().expect("tempdir");

        let path = facade.fetch_attachment(fetch_request("F1", dir.path())).await.expect("fetched");
        // `starts_with` is component-wise, so `dir/../../escape.txt` passes
        // it lexically while resolving above `dir`. Assert the parent
        // instead: the file lands IN the directory or the test is asleep.
        assert_eq!(
            path.parent(),
            Some(dir.path()),
            "the file must land directly in the chosen directory, never above it: {path:?}",
        );
    }

    /// The name comes from Slack and the directory is not exclusive to
    /// this call, so a fetch must never clobber something already there.
    #[tokio::test]
    async fn a_download_refuses_to_clobber_an_existing_file() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_file("F1", "notes.txt", b"new bytes");
        let dir = tempdir().expect("tempdir");
        let existing = dir.path().join("notes.txt");
        std::fs::write(&existing, b"precious").expect("write the existing file");

        assert!(
            facade.fetch_attachment(fetch_request("F1", dir.path())).await.is_err(),
            "an existing file must not be overwritten",
        );
        assert_eq!(
            std::fs::read(&existing).expect("read back"),
            b"precious",
            "and the existing file is untouched",
        );
    }

    /// The draft text is what the prompt shows. A file NAME is
    /// caller-controlled and can look innocuous, so approving it without
    /// the local path it comes from approves something the user has not
    /// seen.
    #[tokio::test]
    async fn an_upload_draft_names_the_local_path_it_will_send() {
        let (facade, ws, _api, mut rx) = facade_with_recording_slack();
        let dir = tempdir().expect("tempdir");
        let source = dir.path().join("file.txt");
        std::fs::write(&source, b"payload").expect("write the source file");

        let task = tokio::spawn({
            let facade = facade.clone();
            let source = source.clone();
            async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
        });

        let mut seen = None;
        for _ in 0..200 {
            while let Ok(update) = rx.try_recv() {
                if let crate::protocol::SessionUpdate::SlackPostPending { draft, .. } = update {
                    seen = Some(draft.text);
                }
            }
            if seen.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let text = seen.expect("the draft reaches the prompt");
        assert!(text.contains("file.txt"), "the file name is shown: {text}");
        assert!(
            text.contains(&source.display().to_string()),
            "and so is the local path it comes from: {text}",
        );

        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), false);
        let _ = task.await;
    }

    #[tokio::test]
    async fn an_upload_goes_through_the_gate() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        let dir = tempdir().expect("tempdir");
        let source = dir.path().join("file.txt");
        std::fs::write(&source, b"payload").expect("write the source file");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), false);

        assert!(task.await.expect("no panic").is_err(), "a rejected upload must not send");
        assert_eq!(api.upload_url_count(), 0, "nothing is requested from Slack either");
    }

    #[tokio::test]
    async fn an_approved_upload_posts_the_bytes_then_completes() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        let dir = tempdir().expect("tempdir");
        let source = dir.path().join("file.txt");
        std::fs::write(&source, b"payload").expect("write the source file");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), true);
        task.await.expect("no panic").expect("an approved upload sends");

        assert_eq!(api.upload_url_count(), 1, "the upload URL is requested once");
        assert_eq!(api.uploads().len(), 1, "and completed once");
        assert_eq!(api.uploads()[0].1, "C1", "into the conversation that was named");
    }

    #[tokio::test]
    async fn a_rejected_draft_never_reaches_slack() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
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
        let (facade, ws, api, _rx) = facade_with_recording_slack();
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
        let (facade, ws, api, _rx) = facade_with_recording_slack();
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
        let (facade, ws, api, _rx) = facade_with_recording_slack();
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
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_author("100.0", "U-OTHER");

        let err = facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await;
        assert_eq!(err, Err(SlackEditError::NotOwnMessage));
        assert!(api.updates.lock().is_empty(), "a foreign message is never updated");
    }

    #[tokio::test]
    async fn editing_own_message_updates_it_once_approved() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        api.seed_author("100.0", "U1");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), true);
        task.await.expect("no panic").expect("an approved replacement is sent");

        assert_eq!(api.updates.lock().len(), 1);
    }

    #[tokio::test]
    async fn a_rejected_edit_leaves_the_message_untouched() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        api.seed_author("100.0", "U1");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), false);

        assert_eq!(task.await.expect("no panic"), Err(SlackEditError::Rejected));
        assert!(api.updates.lock().is_empty(), "a rejected replacement is never sent");
    }

    #[tokio::test]
    async fn deleting_own_message_removes_it_once_approved() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        api.seed_author("100.0", "U1");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", None)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), true);
        task.await.expect("no panic").expect("an approved deletion is sent");

        assert_eq!(api.deletes.lock().len(), 1);
    }

    #[tokio::test]
    async fn a_rejected_delete_leaves_the_message_alone() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        api.seed_author("100.0", "U1");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", None)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), false);

        assert_eq!(task.await.expect("no panic"), Err(SlackEditError::Rejected));
        assert!(api.deletes.lock().is_empty(), "a rejected deletion is never sent");
    }

    /// A reaction is authored content in the user's name, so both calls go
    /// through the same approval a post does.
    #[tokio::test]
    async fn reacting_adds_then_removes() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();

        let task = tokio::spawn({
            let facade = facade.clone();
            async move {
                facade
                    .react(&caller(), react_request("C1", "100.0", "white_check_mark", true))
                    .await?;
                facade
                    .react(&caller(), react_request("C1", "100.0", "white_check_mark", false))
                    .await
            }
        });
        let first = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(first, &caller(), true);
        let second = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(second, &caller(), true);

        task.await.expect("no panic").expect("both reactions applied");

        let reactions = api.reactions.lock().clone();
        assert_eq!(reactions.len(), 2);
        assert!(reactions[0].3, "the first call adds");
        assert!(!reactions[1].3, "the second removes");
    }

    #[tokio::test]
    async fn a_rejected_reaction_is_never_sent() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();

        let task = tokio::spawn({
            let facade = facade.clone();
            async move {
                facade
                    .react(&caller(), react_request("C1", "100.0", "white_check_mark", true))
                    .await
            }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), false);

        assert_eq!(task.await.expect("no panic"), Err(SlackReactError::Rejected));
        assert!(api.reactions.lock().is_empty(), "a rejected reaction is never applied");
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
    /// Deduplication is deliberately not done: two sessions may each want
    /// their own mention feed, and merging them would silently starve one.
    #[tokio::test]
    async fn subscribing_twice_to_mentions_yields_two_records() {
        let (facade, ws, _api, _rx) = facade_with_recording_slack();

        facade.subscribe(&caller(), Some("acme"), SlackSubscribeRequest::Mentions).expect("first");
        facade.subscribe(&caller(), Some("acme"), SlackSubscribeRequest::Mentions).expect("second");

        assert_eq!(ws.slack_subscriptions_for_project("forge").len(), 2);
    }

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

    #[tokio::test]
    async fn a_pins_read_reaches_the_workspace_client_with_its_conversation() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_pins(
            "C1",
            vec![SlackPin {
                created: 1_700_000_000,
                created_by: Some("U1".to_owned()),
                message: Some(SlackPinMessage {
                    ts: "1700000000.000100".to_owned(),
                    user: Some("U2".to_owned()),
                    text: "the pinned text".to_owned(),
                }),
            }],
        );

        let pins = facade.pins(Some("acme"), "C1").await.expect("read");
        assert_eq!(pins.len(), 1, "the seeded row comes back");
        assert_eq!(
            pins[0].message.as_ref().expect("the message rides along").text,
            "the pinned text",
        );
        assert_eq!(api.pin_reads.lock().as_slice(), ["C1"], "the conversation is what was read");
    }

    #[tokio::test]
    async fn a_bookmarks_read_reaches_the_workspace_client_with_its_conversation() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_bookmarks(
            "C1",
            vec![SlackBookmark {
                id: "Bk1".to_owned(),
                title: Some("Runbook".to_owned()),
                link: Some("https://example.com".to_owned()),
            }],
        );

        let bookmarks = facade.bookmarks(Some("acme"), "C1").await.expect("read");
        assert_eq!(bookmarks.len(), 1, "the seeded row comes back");
        assert_eq!(bookmarks[0].title.as_deref(), Some("Runbook"));
        assert_eq!(api.bookmark_reads.lock().as_slice(), ["C1"]);
    }

    /// The list marks only the CALLER's own subscriptions: a lead and a
    /// worker in the same project each see their own records, never each
    /// other's.
    #[tokio::test]
    async fn subscribed_targets_scopes_to_the_callers_role() {
        let (ws, facade) = workspace_with_one_slack_workspace("acme");
        let lead = caller();
        facade
            .subscribe(
                &lead,
                Some("acme"),
                SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                    id: "C1".to_owned(),
                    mode: SlackWatchMode::All,
                }]),
            )
            .expect("the lead subscribes to C1");

        let worker_key = SessionKey::from_session_id("worker-uuid");
        let project_key = ws
            .list_projects()
            .into_iter()
            .find(|view| view.name == "forge")
            .expect("the seeded project")
            .key;
        ws.insert_live_worker(
            &project_key,
            crate::mcp::workers::types::WorkerEntry {
                label: "tester".into(),
                charter: "c".into(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "caller-uuid".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
        facade
            .subscribe(&worker_key, Some("acme"), SlackSubscribeRequest::DirectMessages)
            .expect("the worker subscribes to the DM class");

        assert_eq!(
            facade.subscribed_targets(&lead, Some("acme")),
            vec![SlackSubscriptionTarget::Conversation {
                id: "C1".to_owned(),
                mode: SlackWatchMode::All,
            }],
            "the lead sees only its own record",
        );
        assert_eq!(
            facade.subscribed_targets(&worker_key, Some("acme")),
            vec![SlackSubscriptionTarget::DirectMessages],
            "the worker sees only its own record, never the lead's",
        );
    }
}

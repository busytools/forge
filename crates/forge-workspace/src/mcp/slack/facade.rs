//! `SlackFacade` - the seam between the Slack MCP tools and workspace
//! state. The production impl ([`ProdSlackFacade`]) resolves the
//! requested workspace label and drives that workspace's client; the
//! mock returns preloaded results so the tool tests can assert argument
//! handling and error surfacing without a real workspace.

use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use forge_connectors::slack::{
    AuthTest, MENTION_CURSOR, SlackApi, SlackError, SlackHistoryMessage, read_thread,
    shared_messages,
};
use forge_primitives::slack::{
    SlackBookmark, SlackConversation, SlackDraft, SlackDraftEnding, SlackPin, SlackSearchMatch,
    SlackSubscription, SlackSubscriptionTarget, SlackUser, SlackWatchMode,
};
use uuid::Uuid;

use crate::SessionSlot;
use crate::mcp::caller_context::caller_context;
use crate::mcp::gotify::facade::resolve_identity;
use crate::slack::SlackWorkspaces;
use crate::workspace::Workspace;

/// Why resolving a tool's `workspace` argument failed. One shape for every
/// Slack tool, so the wording - and the labels a caller can choose from - is
/// the same whichever tool was called. Omitting `workspace` with several
/// configured is its own case: reported as a missing workspace it sends a
/// caller hunting for a config key that is already set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackWorkspaceError {
    /// No `[[slack]]` entry in forge.toml.
    NotConfigured,
    /// The caller named a label that is not configured. Carries the labels
    /// that are, so the error can list them.
    UnknownWorkspace { requested: String, known: Vec<String> },
    /// Several workspaces are configured and the caller named none.
    WorkspaceRequired { known: Vec<String> },
}

/// The one wording for a refused workspace argument, so every Slack tool
/// answers this case the way `slack__list` does.
pub(crate) fn format_workspace_error(err: &SlackWorkspaceError) -> String {
    match err {
        SlackWorkspaceError::NotConfigured => {
            "no Slack workspace configured in forge.toml [[slack]]".to_owned()
        }
        SlackWorkspaceError::UnknownWorkspace { requested, known } => {
            format!("no Slack workspace named '{requested}'; configured: {}", known.join(", "))
        }
        SlackWorkspaceError::WorkspaceRequired { known } => {
            format!(
                "several Slack workspaces are configured; pass `workspace`: {}",
                known.join(", ")
            )
        }
    }
}

/// Why `slack__subscribe` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackSubscribeError {
    /// The `workspace` argument named no configured workspace, or named
    /// none while several are configured.
    Workspace(SlackWorkspaceError),
    /// The caller couldn't be mapped to a project.
    UnknownCallerProject,
}

/// Why `slack__unsubscribe` failed. The last two are the two cases a
/// refusal keeps apart: an id the project holds under another session
/// reads differently from one the project never had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackUnsubscribeError {
    /// The caller couldn't be mapped to a project.
    UnknownCallerProject,
    /// No subscription with that id in the caller's project.
    NoSuchSubscription,
    /// The id is in the caller's project but belongs to another owner.
    NotOwnedByCaller,
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
    /// The draft waited out its approval window unanswered. Nothing was
    /// posted.
    Expired,
    /// Some parts of a split draft are live and the rest is not. Carries
    /// how many landed so a retry resumes instead of duplicating.
    Partial { posted: usize, total: usize, source: String },
    /// The `workspace` argument named no configured workspace, or named
    /// none while several are configured.
    Workspace(SlackWorkspaceError),
    /// The Web API call failed. Carries the formatted error for the LLM.
    Fetch(String),
}

/// What a post did: the `ts` of each message it sent, in the order they
/// went out, the conversation it landed in (the name, or the id when the
/// lookup failed), and how many parts the text was split into. A draft past
/// the truncation point is split and posted in sequence, so a long one is
/// several messages a reply can attach to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlackPostOutcome {
    pub ts: Vec<String>,
    pub conversation_name: String,
    pub parts: usize,
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
    /// Slack refused the change because the message was not posted by the
    /// authenticated user. The wire's verdict, surfaced as its reason.
    NotOwnMessage,
    /// The user rejected the replacement. The message is untouched.
    Rejected,
    /// The draft waited out its approval window unanswered. The message
    /// is untouched.
    Expired,
    Workspace(SlackWorkspaceError),
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
    /// The user rejected the reaction. Nothing was added or removed.
    Rejected,
    /// The draft waited out its approval window unanswered. Nothing was
    /// added or removed.
    Expired,
    Workspace(SlackWorkspaceError),
    Fetch(String),
}

/// Why a read-only lookup failed. Shared by every read tool because their
/// failure sets are identical; the per-operation error convention exists
/// where the sets differ (post, edit and react each fail differently), and
/// two names for one set would be duplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlackReadError {
    Workspace(SlackWorkspaceError),
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
    /// The draft waited out its approval window unanswered. Nothing was
    /// sent.
    Expired,
    Workspace(SlackWorkspaceError),
    /// The Web API or the transfer failed.
    Fetch(String),
    /// The `dir` a fetch should land in exists but is not a directory.
    NotADirectory(std::path::PathBuf),
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

/// How long a held draft waits for the user before it expires. Long
/// enough for a working session to reach the dock; short enough that a
/// prompt nobody can answer does not hold a session forever. Expiry is
/// its own outcome, distinct from a rejection.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

/// How many search pages a conversation filter scans. A named conversation
/// is narrowed server-side and usually answers on the first page; an unnamed
/// DM is filtered locally, and five pages is where a sparse hit stops being
/// worth another call against the token's allowance.
const MAX_SEARCH_FILTER_PAGES: usize = 5;

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

/// Whether `user` answers to `needle`, which is already lowercase: a
/// case-insensitive substring over the handle, the real name and the display
/// name, which is the one people actually type.
fn user_matches(user: &SlackUser, needle: &str) -> bool {
    let display_name = user.profile.as_ref().and_then(|profile| profile.display_name.as_deref());
    [Some(user.name.as_str()), user.real_name.as_deref(), display_name]
        .into_iter()
        .flatten()
        .any(|field| !field.is_empty() && field.to_lowercase().contains(needle))
}

/// The workspace to act on and the client for it: the caller's choice when
/// it names a configured one, else the only configured one. A refusal says
/// which case it is, so an omitted `workspace` never reads as a missing
/// configuration.
fn resolve_workspace(
    workspaces: &SlackWorkspaces,
    requested: Option<&str>,
) -> Result<(String, Arc<dyn SlackApi>), SlackWorkspaceError> {
    if workspaces.is_empty() {
        return Err(SlackWorkspaceError::NotConfigured);
    }
    if let Some(label) = requested {
        return workspaces.client(label).map(|api| (label.to_owned(), api)).ok_or_else(|| {
            SlackWorkspaceError::UnknownWorkspace {
                requested: label.to_owned(),
                known: workspaces.labels(),
            }
        });
    }
    let labels = workspaces.labels();
    let [only] = labels.as_slice() else {
        return Err(SlackWorkspaceError::WorkspaceRequired { known: labels });
    };
    let api = workspaces.client(only).ok_or(SlackWorkspaceError::NotConfigured)?;
    Ok((only.clone(), api))
}

/// The Slack tools' view of the workspace.
#[async_trait::async_trait]
pub(crate) trait SlackFacade: Send + Sync {
    /// Every conversation the named workspace's token is in. `None` picks
    /// the only configured workspace and errors when there is more than one.
    async fn conversations(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<SlackConversation>, SlackReadError>;

    /// The caller's OWN subscriptions in `workspace` - a lead's the
    /// lead's, a worker's its own, never another owner's. What
    /// `slack__list` marks and reports ids for: after a restart the ids
    /// are the only handle a session has to unsubscribe with.
    fn subscribed_targets(
        &self,
        caller: &SessionSlot,
        workspace: Option<&str>,
    ) -> Vec<SlackSubscription>;

    /// Post a message, held for the user's decision first. Returns only
    /// once that decision is in, and nothing is sent unless it was an
    /// approval - a rejected or unanswered draft is `Rejected`.
    async fn post(
        &self,
        caller: &SessionSlot,
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
        caller: &SessionSlot,
        request: SlackEditRequest,
    ) -> Result<(), SlackEditError>;

    /// Add or remove one reaction, held for the user's decision first -
    /// a reaction is authored content in the user's name.
    async fn react(
        &self,
        caller: &SessionSlot,
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
        caller: &SessionSlot,
        request: SlackUploadRequest,
    ) -> Result<(), SlackAttachmentError>;

    /// `search.messages` in a workspace, always timestamp-ordered. It
    /// matches text; it cannot find every message that mentions a user.
    /// `conversation` keeps only hits from that conversation id.
    async fn search(
        &self,
        workspace: Option<&str>,
        query: &str,
        count: u32,
        conversation: Option<&str>,
    ) -> Result<Vec<SlackSearchMatch>, SlackReadError>;

    /// One page of a conversation's messages. `oldest` and `newest` are
    /// exclusive bounds, so a caller pages backwards by feeding the earliest
    /// `ts` it saw back as `newest`.
    async fn history(
        &self,
        workspace: Option<&str>,
        conversation: &str,
        limit: u32,
        oldest: Option<&str>,
        newest: Option<&str>,
    ) -> Result<Vec<SlackHistoryMessage>, SlackReadError>;

    /// Every reply in one thread, the parent first, up to `limit` rows.
    async fn replies(
        &self,
        workspace: Option<&str>,
        conversation: &str,
        ts: &str,
        limit: u32,
    ) -> Result<Vec<SlackHistoryMessage>, SlackReadError>;

    /// `users.info` for one user id.
    async fn user(&self, workspace: Option<&str>, user: &str) -> Result<SlackUser, SlackReadError>;

    /// Every member whose handle, real name or display name contains
    /// `query`, case-insensitively. Every match is returned: silently
    /// picking one of several is how a mention reaches the wrong person.
    /// Deactivated members are excluded.
    async fn users(
        &self,
        workspace: Option<&str>,
        query: &str,
    ) -> Result<Vec<SlackUser>, SlackReadError>;

    /// `auth.test`: the resolved `[[slack]]` label and the identity the
    /// workspace's token acts as. The label is returned alongside because it
    /// can differ from the team's name (`Subspace` is team `Autonomys`).
    async fn me(&self, workspace: Option<&str>) -> Result<(String, AuthTest), SlackReadError>;

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
    /// per target. Returns the records as they now stand, so a caller can
    /// name what it created.
    fn subscribe(
        &self,
        caller: &SessionSlot,
        workspace: Option<&str>,
        request: SlackSubscribeRequest,
    ) -> Result<Vec<SlackSubscription>, SlackSubscribeError>;

    /// Remove one of the caller's OWN subscriptions by id within its
    /// project, returning the row as it stood. A refusal says which case
    /// it is: [`SlackUnsubscribeError::NoSuchSubscription`] when the
    /// project has no such id, [`SlackUnsubscribeError::NotOwnedByCaller`]
    /// when it has one under another owner.
    fn unsubscribe(
        &self,
        caller: &SessionSlot,
        id: Uuid,
    ) -> Result<SlackSubscription, SlackUnsubscribeError>;
}

/// Production facade over `Weak<Workspace>` (weak to avoid a cycle with
/// the MCP server the workspace owns).
pub(crate) struct ProdSlackFacade {
    workspace: Weak<Workspace>,
}

/// How a held draft ended. `Expired` is its own outcome, not a flavour of
/// rejection: the session is told the draft timed out, and a view
/// retires the dock prompt it can no longer answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateDecision {
    Approved,
    Rejected,
    Expired,
}

/// Resolves the draft if the awaiting future is dropped before the
/// timeout answers - the calling session died mid-wait. Without this the
/// registry entry and its oneshot sender outlive everyone who could
/// answer them.
struct ResolveOnDrop {
    workspace: Arc<Workspace>,
    id: Uuid,
    caller: SessionSlot,
}

impl Drop for ResolveOnDrop {
    fn drop(&mut self) {
        // The waiter is gone rather than answering: the drop path is a
        // session that died mid-wait, never a decision.
        self.workspace.resolve_slack_draft(self.id, &self.caller, SlackDraftEnding::Abandoned);
    }
}

impl ProdSlackFacade {
    pub(crate) fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn SlackFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }

    /// The workspace a tool call acts on, with the refusal shape every Slack
    /// tool shares.
    fn resolve(
        &self,
        workspace: Option<&str>,
    ) -> Result<(String, Arc<dyn SlackApi>), SlackWorkspaceError> {
        let ws = self.workspace.upgrade().ok_or(SlackWorkspaceError::NotConfigured)?;
        resolve_workspace(&ws.slack, workspace)
    }

    /// The conversation a request addresses, as `(id, display label)`.
    /// `conversation` may be an id, a channel name or a DM's partner, and
    /// the id is what the ledger and the in-flight mark key on - a post or
    /// an edit addressed one way lands in the same conversation the sweep
    /// reads. The label is what the approval prompt shows: a DM is named
    /// after its partner, and Slack's own name for it is empty. A failed
    /// lookup is not a reason to refuse the action - but it IS why the
    /// prompt would show an id, so it says so.
    async fn resolve_conversation(
        api: &Arc<dyn SlackApi>,
        workspace: &str,
        conversation: &str,
    ) -> (String, String) {
        let conversations = match api.list_conversations().await {
            Ok(conversations) => conversations,
            Err(error) => {
                tracing::warn!(
                    target: "forge_workspace::slack",
                    event_name = "slack_conversation_list_failed",
                    workspace,
                    conversation,
                    %error,
                    "listing conversations failed; the approval prompt names the id instead",
                );
                return (conversation.to_owned(), conversation.to_owned());
            }
        };
        match conversations.iter().find(|known| {
            known.id == conversation
                || known.name.as_deref() == Some(conversation)
                || known.user.as_deref() == Some(conversation)
        }) {
            Some(known) => {
                let label = known.name.clone().or_else(|| known.user.clone()).unwrap_or_default();
                let label = if label.is_empty() { known.id.clone() } else { label };
                (known.id.clone(), label)
            }
            None => (conversation.to_owned(), conversation.to_owned()),
        }
    }

    /// Register a draft for the user's decision and wait for it. The one
    /// place "did the user approve this" is decided, so the fail-closed
    /// path exists once rather than at each gated operation - posting,
    /// replacing a body and deleting one all come through here.
    ///
    /// A generous timeout expires the draft rather than holding the
    /// session forever on a prompt nobody can answer; rejection and
    /// expiry are distinct outcomes the callers surface differently.
    ///
    /// The views hear `SlackDraftResolved` from the resolve below, expiry
    /// included: the draft leaving the registry is the whole of what a view
    /// does with it, so there is one signal rather than one per ending.
    async fn await_approval(
        workspace: &Arc<Workspace>,
        caller: &SessionSlot,
        draft: SlackDraft,
    ) -> GateDecision {
        let (id, decision) = workspace.register_slack_draft(caller, draft);
        let guard = ResolveOnDrop { workspace: Arc::clone(workspace), id, caller: caller.clone() };
        let answer = tokio::time::timeout(APPROVAL_TIMEOUT, decision).await;
        let resolved = match answer {
            Ok(Ok(true)) => GateDecision::Approved,
            Ok(Ok(false) | Err(_)) => GateDecision::Rejected,
            Err(_elapsed) => GateDecision::Expired,
        };
        if resolved != GateDecision::Approved {
            let ending = match resolved {
                GateDecision::Expired => SlackDraftEnding::Expired,
                _ => SlackDraftEnding::Answered { approved: false },
            };
            workspace.resolve_slack_draft(guard.id, caller, ending);
        }
        drop(guard);
        resolved
    }
}

#[async_trait::async_trait]
impl SlackFacade for ProdSlackFacade {
    async fn conversations(
        &self,
        workspace: Option<&str>,
    ) -> Result<Vec<SlackConversation>, SlackReadError> {
        let (_, api) = self.resolve(workspace).map_err(SlackReadError::Workspace)?;
        api.list_conversations().await.map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn post(
        &self,
        caller: &SessionSlot,
        request: SlackPostRequest,
    ) -> Result<SlackPostOutcome, SlackPostError> {
        let ws = self
            .workspace
            .upgrade()
            .ok_or(SlackPostError::Workspace(SlackWorkspaceError::NotConfigured))?;
        let (label, api) = resolve_workspace(&ws.slack, request.workspace.as_deref())
            .map_err(SlackPostError::Workspace)?;
        let (conversation, conversation_label) =
            Self::resolve_conversation(&api, &label, &request.conversation).await;
        let draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label,
            conversation: request.conversation,
            conversation_label,
            thread_ts: request.thread_ts,
            text: request.text,
            tool: "slack__post".to_owned(),
        };
        match Self::await_approval(&ws, caller, draft.clone()).await {
            GateDecision::Approved => {}
            GateDecision::Rejected => return Err(SlackPostError::Rejected),
            GateDecision::Expired => return Err(SlackPostError::Expired),
        }
        let conversation_name = draft.conversation_label.clone();
        let parts = forge_connectors::slack::split_for_post(&draft.text);
        let mut ts: Vec<String> = Vec::new();
        for part in &parts {
            // One guard per call, covering the call and its ledger write:
            // each part has its own request budget, so a stamp taken once
            // for the whole operation would expire between parts on a slow
            // but healthy Slack.
            let _in_flight = ws.begin_slack_post(&draft.workspace, &conversation);
            match api.post_message(&draft.conversation, part, draft.thread_ts.as_deref()).await {
                Ok(posted) => {
                    // The channel Slack answers with, not the request
                    // string: a DM addressed by user id lands in `D...`,
                    // and the sweep looks it up by that id. Recorded per
                    // part as it lands, so a post that fails halfway still
                    // refuses the parts Slack took.
                    let landed = if let Some(channel) = posted.channel.as_deref() {
                        channel
                    } else {
                        tracing::debug!(
                            target: "forge_workspace::slack",
                            event_name = "slack_post_channel_omitted",
                            workspace = %draft.workspace,
                            conversation = %draft.conversation,
                            "chat.postMessage answered no channel; the ledger keys on the resolved conversation",
                        );
                        &conversation
                    };
                    ws.record_slack_own_post(&draft.workspace, landed, &posted.ts);
                    ts.push(posted.ts);
                }
                Err(error) if ts.is_empty() => {
                    // Nothing landed, so the failure is an ordinary one.
                    return Err(SlackPostError::Fetch(error.to_string()));
                }
                Err(error) => {
                    // Say exactly what landed: a truncated multi-part post
                    // is live and readable, so a retry must resume rather
                    // than duplicate the parts already sent.
                    return Err(SlackPostError::Partial {
                        posted: ts.len(),
                        total: parts.len(),
                        source: error.to_string(),
                    });
                }
            }
        }
        Ok(SlackPostOutcome { ts, conversation_name, parts: parts.len() })
    }

    async fn edit(
        &self,
        caller: &SessionSlot,
        request: SlackEditRequest,
    ) -> Result<(), SlackEditError> {
        let ws = self
            .workspace
            .upgrade()
            .ok_or(SlackEditError::Workspace(SlackWorkspaceError::NotConfigured))?;
        let (label, api) = resolve_workspace(&ws.slack, request.workspace.as_deref())
            .map_err(SlackEditError::Workspace)?;

        // A replacement puts new words in front of people as the user, and
        // a deletion changes what they see on a message attributed to him -
        // and unlike a bad edit it cannot be fixed by editing again. Both
        // go through the same gate a post does.
        let (conversation, conversation_label) =
            Self::resolve_conversation(&api, &label, &request.conversation).await;
        let draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label.clone(),
            conversation: request.conversation.clone(),
            conversation_label,
            thread_ts: Some(request.ts.clone()),
            text: match &request.text {
                Some(text) => text.clone(),
                None => format!("[delete] message {}", request.ts),
            },
            tool: "slack__edit".to_owned(),
        };
        match Self::await_approval(&ws, caller, draft).await {
            GateDecision::Approved => {}
            GateDecision::Rejected => return Err(SlackEditError::Rejected),
            GateDecision::Expired => return Err(SlackEditError::Expired),
        }

        // Ownership is the wire's verdict, never a local lookup's:
        // conversations.history cannot see thread replies, so a pre-check
        // would wrongly refuse an edit of the user's own reply.
        let result = match &request.text {
            Some(text) => api.update_message(&request.conversation, &request.ts, text).await,
            None => api.delete_message(&request.conversation, &request.ts).await,
        };
        match result {
            Ok(()) => {
                // An edit or a delete keeps the message's ts, so this is
                // the identity the sweep sees - and the agent authored the
                // change, whoever posted the message first. Keyed by the
                // resolved id: the request may have named the DM's partner.
                ws.record_slack_own_post(&label, &conversation, &request.ts);
                Ok(())
            }
            Err(SlackError::Api { error, .. })
                if error == "cant_update_message" || error == "cant_delete_message" =>
            {
                Err(SlackEditError::NotOwnMessage)
            }
            Err(err) => Err(SlackEditError::Fetch(err.to_string())),
        }
    }

    async fn react(
        &self,
        caller: &SessionSlot,
        request: SlackReactRequest,
    ) -> Result<(), SlackReactError> {
        let ws = self
            .workspace
            .upgrade()
            .ok_or(SlackReactError::Workspace(SlackWorkspaceError::NotConfigured))?;
        let (label, api) = resolve_workspace(&ws.slack, request.workspace.as_deref())
            .map_err(SlackReactError::Workspace)?;

        // A reaction is authored content in the user's name, so it goes
        // through the same gate a post does.
        let verb = if request.add { "react" } else { "unreact" };
        let (_, conversation_label) =
            Self::resolve_conversation(&api, &label, &request.conversation).await;
        let draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label,
            conversation: request.conversation.clone(),
            conversation_label,
            thread_ts: Some(request.ts.clone()),
            text: format!("[{verb}: {}]", request.name),
            tool: "slack__react".to_owned(),
        };
        match Self::await_approval(&ws, caller, draft).await {
            GateDecision::Approved => {}
            GateDecision::Rejected => return Err(SlackReactError::Rejected),
            GateDecision::Expired => return Err(SlackReactError::Expired),
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
        conversation: Option<&str>,
    ) -> Result<Vec<SlackSearchMatch>, SlackReadError> {
        let (_, api) = self.resolve(workspace).map_err(SlackReadError::Workspace)?;
        let Some(conversation) = conversation else {
            return api
                .search_messages(query, count, None)
                .await
                .map(|page| page.matches)
                .map_err(|err| SlackReadError::Fetch(err.to_string()));
        };

        // Slack's search language narrows by channel NAME; an id in `in:`
        // finds nothing. A DM has no name to narrow on, so its hits are
        // filtered locally instead. Either way the id is the filter, so a
        // name that resolves to another conversation cannot leak a foreign
        // hit.
        let name = api
            .list_conversations()
            .await
            .map_err(|err| SlackReadError::Fetch(err.to_string()))?
            .into_iter()
            .find(|known| known.id == conversation)
            // `in:` takes a channel name. A DM has none, and a group DM's
            // generated name is not one Slack's search understands, so those
            // stay local-only rather than narrowing to nothing.
            .filter(|known| !known.is_im && !known.is_mpim)
            .and_then(|known| known.name)
            .filter(|name| !name.is_empty());
        let narrowed = name.map_or_else(|| query.to_owned(), |name| format!("in:{name} {query}"));

        let mut matches = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_SEARCH_FILTER_PAGES {
            let page = api
                .search_messages(&narrowed, count, cursor.as_deref())
                .await
                .map_err(|err| SlackReadError::Fetch(err.to_string()))?;
            matches
                .extend(page.matches.into_iter().filter(|hit| hit.conversation_id == conversation));
            if matches.len() >= count as usize {
                break;
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        matches.truncate(count as usize);
        Ok(matches)
    }

    async fn history(
        &self,
        workspace: Option<&str>,
        conversation: &str,
        limit: u32,
        oldest: Option<&str>,
        newest: Option<&str>,
    ) -> Result<Vec<SlackHistoryMessage>, SlackReadError> {
        let (_, api) = self.resolve(workspace).map_err(SlackReadError::Workspace)?;
        api.history(conversation, oldest, newest, limit, None)
            .await
            .map(|page| page.messages)
            .map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn replies(
        &self,
        workspace: Option<&str>,
        conversation: &str,
        ts: &str,
        limit: u32,
    ) -> Result<Vec<SlackHistoryMessage>, SlackReadError> {
        let (_, api) = self.resolve(workspace).map_err(SlackReadError::Workspace)?;
        read_thread(api.as_ref(), conversation, ts, limit)
            .await
            .map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn user(&self, workspace: Option<&str>, user: &str) -> Result<SlackUser, SlackReadError> {
        let (_, api) = self.resolve(workspace).map_err(SlackReadError::Workspace)?;
        api.user_info(user).await.map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn users(
        &self,
        workspace: Option<&str>,
        query: &str,
    ) -> Result<Vec<SlackUser>, SlackReadError> {
        let (_, api) = self.resolve(workspace).map_err(SlackReadError::Workspace)?;
        let needle = query.trim().to_lowercase();
        // An empty needle would match the whole directory; the tool refuses
        // it, and a direct facade call gets nothing rather than everyone.
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let mut found: Vec<SlackUser> = api
            .list_users()
            .await
            .map_err(|err| SlackReadError::Fetch(err.to_string()))?
            .into_iter()
            .filter(|user| !user.deleted && user_matches(user, &needle))
            .collect();
        found.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(found)
    }

    async fn me(&self, workspace: Option<&str>) -> Result<(String, AuthTest), SlackReadError> {
        let (label, api) = self.resolve(workspace).map_err(SlackReadError::Workspace)?;
        let auth = api.auth_test().await.map_err(|err| SlackReadError::Fetch(err.to_string()))?;
        Ok((label, auth))
    }

    async fn pins(
        &self,
        workspace: Option<&str>,
        conversation: &str,
    ) -> Result<Vec<SlackPin>, SlackReadError> {
        let (_, api) = self.resolve(workspace).map_err(SlackReadError::Workspace)?;
        api.pins(conversation).await.map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn bookmarks(
        &self,
        workspace: Option<&str>,
        conversation: &str,
    ) -> Result<Vec<SlackBookmark>, SlackReadError> {
        let (_, api) = self.resolve(workspace).map_err(SlackReadError::Workspace)?;
        api.bookmarks(conversation).await.map_err(|err| SlackReadError::Fetch(err.to_string()))
    }

    async fn fetch_attachment(
        &self,
        request: SlackFetchRequest,
    ) -> Result<std::path::PathBuf, SlackAttachmentError> {
        let (_, api) =
            self.resolve(request.workspace.as_deref()).map_err(SlackAttachmentError::Workspace)?;

        // The directory is checked and created before Slack is contacted,
        // so a bad `dir` costs no round trips.
        if let Ok(meta) = std::fs::metadata(&request.dir)
            && !meta.is_dir()
        {
            return Err(SlackAttachmentError::NotADirectory(request.dir.clone()));
        }
        std::fs::create_dir_all(&request.dir)
            .map_err(|err| SlackAttachmentError::Io(format!("{}: {err}", request.dir.display())))?;

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
        caller: &SessionSlot,
        request: SlackUploadRequest,
    ) -> Result<(), SlackAttachmentError> {
        let ws = self
            .workspace
            .upgrade()
            .ok_or(SlackAttachmentError::Workspace(SlackWorkspaceError::NotConfigured))?;
        let (label, api) = resolve_workspace(&ws.slack, request.workspace.as_deref())
            .map_err(SlackAttachmentError::Workspace)?;

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
        let (conversation, conversation_label) =
            Self::resolve_conversation(&api, &label, &request.conversation).await;
        let draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label.clone(),
            conversation: request.conversation.clone(),
            conversation_label,
            thread_ts: request.thread_ts.clone(),
            text: format!("[file] {name}\nfrom {}", request.path.display()),
            tool: "slack__attachment".to_owned(),
        };
        match Self::await_approval(&ws, caller, draft).await {
            GateDecision::Approved => {}
            GateDecision::Rejected => return Err(SlackAttachmentError::Rejected),
            GateDecision::Expired => return Err(SlackAttachmentError::Expired),
        }

        let (url, file_id) = api
            .get_upload_url(&name, bytes.len())
            .await
            .map_err(|err| SlackAttachmentError::Fetch(err.to_string()))?;
        api.post_bytes(&url, bytes)
            .await
            .map_err(|err| SlackAttachmentError::Fetch(err.to_string()))?;
        // The upload call can put the message in history; its own stamp
        // covers that call, and a second covers the shares read below. The
        // second is taken before the first drops: a stamp each covers one
        // call, and the map must never hold none between them, or a sweep
        // in the gap delivers the file message before the shares are
        // ledgered.
        let upload_call = ws.begin_slack_post(&label, &conversation);
        let uploaded = api
            .complete_upload(&file_id, &request.conversation, request.thread_ts.as_deref())
            .await
            .map_err(|err| SlackAttachmentError::Fetch(err.to_string()))?;
        let _shares = ws.begin_slack_post(&label, &conversation);
        drop(upload_call);
        // Slack names the message the upload was posted as on the file's
        // shares, keyed by the share's own conversation - which need not be
        // the string the request addressed. The upload's own answer carries
        // them when Slack fills them; `files.info` is the fallback.
        let mut shared: Vec<(String, String)> = uploaded.iter().flat_map(shared_messages).collect();
        if shared.is_empty() {
            match api.file_info(&file_id).await {
                Ok(file) => shared = shared_messages(&file),
                Err(error) => tracing::warn!(
                    target: "forge_workspace::slack",
                    event_name = "slack_attachment_shares_unreadable",
                    workspace = %label,
                    conversation = %request.conversation,
                    %error,
                    "reading a posted file's shares failed; the attachment stays unledgered until this call succeeds",
                ),
            }
        }
        if shared.is_empty() {
            // Neither the upload's answer nor `files.info` named the
            // message: say so rather than leaving the echo unexplained.
            tracing::warn!(
                target: "forge_workspace::slack",
                event_name = "slack_attachment_ledger_missing",
                workspace = %label,
                conversation = %request.conversation,
                "the upload's shares name no message; the attachment stays unledgered and can come back once",
            );
        }
        for (conversation, ts) in shared {
            ws.record_slack_own_post(&label, &conversation, &ts);
        }
        Ok(())
    }

    fn subscribed_targets(
        &self,
        caller: &SessionSlot,
        workspace: Option<&str>,
    ) -> Vec<SlackSubscription> {
        let Some(ws) = self.workspace.upgrade() else { return Vec::new() };
        let Ok((label, _)) = resolve_workspace(&ws.slack, workspace) else {
            return Vec::new();
        };
        let Some(cx) = caller_context(&ws, caller) else { return Vec::new() };
        ws.slack_subscriptions_for_project(&cx.project_name)
            .into_iter()
            .filter(|sub| sub.workspace == label && sub.team_role == cx.worker_label)
            .collect()
    }

    fn subscribe(
        &self,
        caller: &SessionSlot,
        workspace: Option<&str>,
        request: SlackSubscribeRequest,
    ) -> Result<Vec<SlackSubscription>, SlackSubscribeError> {
        let ws = self.workspace.upgrade().ok_or(SlackSubscribeError::UnknownCallerProject)?;
        let (label, _) =
            resolve_workspace(&ws.slack, workspace).map_err(SlackSubscribeError::Workspace)?;
        let (project, team_role, durable) =
            resolve_identity(&ws, caller).ok_or(SlackSubscribeError::UnknownCallerProject)?;
        let targets = match request {
            SlackSubscribeRequest::DirectMessages => {
                vec![SlackSubscriptionTarget::DirectMessages]
            }
            // Left unnamed here: the sweep's directory walk fills names in
            // on its next tick, so subscribing makes no network call.
            SlackSubscribeRequest::Conversations(watches) => watches
                .into_iter()
                .map(|watch| SlackSubscriptionTarget::Conversation {
                    id: watch.id,
                    name: None,
                    mode: watch.mode,
                })
                .collect(),
            SlackSubscribeRequest::Mentions => vec![SlackSubscriptionTarget::Mentions],
        };
        let mut created = Vec::with_capacity(targets.len());
        let mut mentions_requested = false;
        for target in targets {
            // A conversation goes through the owner's own record, which hands
            // back the record it already has rather than adding a second row
            // for one channel, and updates the mode when the caller asks for
            // a different one. Mentions and the DM class are deliberately not
            // deduped - two sessions may each want their own feed.
            if let SlackSubscriptionTarget::Conversation { id, mode, .. } = &target {
                // A subscription starts from now rather than from the
                // channel's history - but only when the conversation has no
                // cursor yet, since the cursor is shared by every owner
                // watching it and resetting it would drop their pending
                // windows.
                if !ws.slack_has_cursor(&label, id) {
                    ws.set_slack_watermark(&label, id, &slack_ts_now());
                }
                created.push(ws.watch_slack_conversation(
                    &label,
                    &project,
                    team_role.as_deref(),
                    id,
                    *mode,
                    durable,
                ));
                continue;
            }
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
            // A subscription starts from now, not from the channel's
            // history - but only when the conversation has no cursor yet.
            // The cursor belongs to the conversation and is shared by
            // every owner watching it: a second subscriber resetting it
            // would silently drop every owner's pending window.
            if let SlackSubscriptionTarget::Conversation { id, .. } = &sub.target
                && !ws.slack_has_cursor(&label, id)
            {
                ws.set_slack_watermark(&label, id, &slack_ts_now());
            }
            ws.add_slack_subscription(sub.clone(), durable);
            created.push(sub);
        }
        if mentions_requested && !ws.slack_has_cursor(&label, MENTION_CURSOR) {
            ws.set_slack_watermark(&label, MENTION_CURSOR, &slack_ts_now());
        }
        // A workspace that just gained its first subscription needs a pump.
        ws.start_slack_subsystem();
        Ok(created)
    }

    fn unsubscribe(
        &self,
        caller: &SessionSlot,
        id: Uuid,
    ) -> Result<SlackSubscription, SlackUnsubscribeError> {
        let ws = self.workspace.upgrade().ok_or(SlackUnsubscribeError::UnknownCallerProject)?;
        let cx = caller_context(&ws, caller).ok_or(SlackUnsubscribeError::UnknownCallerProject)?;
        let removed =
            ws.remove_slack_subscription_owned_by(&cx.project_name, id, cx.worker_label.as_deref());
        ws.stop_slack_subsystem_if_idle();
        if let Some(removed) = removed {
            return Ok(removed);
        }
        // The owner-scoped removal matched nothing, which is either an id
        // the project never had or one another session owns; the caller is
        // told which.
        if ws.slack_subscriptions_for_project(&cx.project_name).iter().any(|s| s.id == id) {
            Err(SlackUnsubscribeError::NotOwnedByCaller)
        } else {
            Err(SlackUnsubscribeError::NoSuchSubscription)
        }
    }
}

/// One recorded search: workspace, query, cap, conversation filter.
#[cfg(test)]
pub(crate) type SearchCall = (Option<String>, String, u32, Option<String>);

/// One recorded history read: workspace, conversation, limit, oldest, newest.
#[cfg(test)]
pub(crate) type HistoryCall = (Option<String>, String, u32, Option<String>, Option<String>);

/// One recorded thread read: workspace, conversation, parent ts, limit.
#[cfg(test)]
pub(crate) type RepliesCall = (Option<String>, String, String, u32);

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool parses args, resolves the caller, and surfaces results/errors.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockSlackFacade {
    pub conversations_calls: parking_lot::Mutex<Vec<Option<String>>>,
    pub conversations_result:
        parking_lot::Mutex<Option<Result<Vec<SlackConversation>, SlackReadError>>>,
    pub subscribed_targets_calls: parking_lot::Mutex<Vec<Option<String>>>,
    pub subscribed_targets: parking_lot::Mutex<Vec<SlackSubscription>>,
    pub search_calls: parking_lot::Mutex<Vec<SearchCall>>,
    pub search_result: parking_lot::Mutex<Option<Result<Vec<SlackSearchMatch>, SlackReadError>>>,
    pub history_calls: parking_lot::Mutex<Vec<HistoryCall>>,
    pub history_result:
        parking_lot::Mutex<Option<Result<Vec<SlackHistoryMessage>, SlackReadError>>>,
    pub replies_calls: parking_lot::Mutex<Vec<RepliesCall>>,
    pub replies_result:
        parking_lot::Mutex<Option<Result<Vec<SlackHistoryMessage>, SlackReadError>>>,
    pub user_calls: parking_lot::Mutex<Vec<(Option<String>, String)>>,
    pub user_result: parking_lot::Mutex<Option<Result<SlackUser, SlackReadError>>>,
    pub users_calls: parking_lot::Mutex<Vec<(Option<String>, String)>>,
    pub users_result: parking_lot::Mutex<Option<Result<Vec<SlackUser>, SlackReadError>>>,
    pub me_calls: parking_lot::Mutex<Vec<Option<String>>>,
    pub me_result: parking_lot::Mutex<Option<Result<(String, AuthTest), SlackReadError>>>,
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
    pub subscribe_result:
        parking_lot::Mutex<Option<Result<Vec<SlackSubscription>, SlackSubscribeError>>>,
    pub unsubscribe_calls: parking_lot::Mutex<Vec<Uuid>>,
    pub unsubscribe_result:
        parking_lot::Mutex<Option<Result<SlackSubscription, SlackUnsubscribeError>>>,
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
    ) -> Result<Vec<SlackConversation>, SlackReadError> {
        self.conversations_calls.lock().push(workspace.map(str::to_owned));
        self.conversations_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn search(
        &self,
        workspace: Option<&str>,
        query: &str,
        count: u32,
        conversation: Option<&str>,
    ) -> Result<Vec<SlackSearchMatch>, SlackReadError> {
        self.search_calls.lock().push((
            workspace.map(str::to_owned),
            query.to_owned(),
            count,
            conversation.map(str::to_owned),
        ));
        self.search_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn history(
        &self,
        workspace: Option<&str>,
        conversation: &str,
        limit: u32,
        oldest: Option<&str>,
        newest: Option<&str>,
    ) -> Result<Vec<SlackHistoryMessage>, SlackReadError> {
        self.history_calls.lock().push((
            workspace.map(str::to_owned),
            conversation.to_owned(),
            limit,
            oldest.map(str::to_owned),
            newest.map(str::to_owned),
        ));
        self.history_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn replies(
        &self,
        workspace: Option<&str>,
        conversation: &str,
        ts: &str,
        limit: u32,
    ) -> Result<Vec<SlackHistoryMessage>, SlackReadError> {
        self.replies_calls.lock().push((
            workspace.map(str::to_owned),
            conversation.to_owned(),
            ts.to_owned(),
            limit,
        ));
        self.replies_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn user(&self, workspace: Option<&str>, user: &str) -> Result<SlackUser, SlackReadError> {
        self.user_calls.lock().push((workspace.map(str::to_owned), user.to_owned()));
        self.user_result.lock().clone().unwrap_or_else(|| {
            Ok(SlackUser {
                id: user.to_owned(),
                name: "tester".to_owned(),
                real_name: None,
                tz: None,
                deleted: false,
                profile: None,
            })
        })
    }

    async fn users(
        &self,
        workspace: Option<&str>,
        query: &str,
    ) -> Result<Vec<SlackUser>, SlackReadError> {
        self.users_calls.lock().push((workspace.map(str::to_owned), query.to_owned()));
        self.users_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    async fn me(&self, workspace: Option<&str>) -> Result<(String, AuthTest), SlackReadError> {
        self.me_calls.lock().push(workspace.map(str::to_owned));
        self.me_result.lock().clone().unwrap_or_else(|| {
            Ok((
                workspace.unwrap_or_default().to_owned(),
                AuthTest {
                    team: "Test".to_owned(),
                    user: "tester".to_owned(),
                    team_id: "T1".to_owned(),
                    user_id: "U1".to_owned(),
                    url: "https://test.slack.com/".to_owned(),
                },
            ))
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
        _caller: &SessionSlot,
        request: SlackUploadRequest,
    ) -> Result<(), SlackAttachmentError> {
        self.upload_calls.lock().push(request);
        self.upload_result.lock().clone().unwrap_or(Ok(()))
    }

    async fn post(
        &self,
        _caller: &SessionSlot,
        request: SlackPostRequest,
    ) -> Result<SlackPostOutcome, SlackPostError> {
        self.post_calls.lock().push(request);
        self.post_result.lock().clone().unwrap_or(Ok(SlackPostOutcome {
            ts: vec!["1.0".to_owned()],
            conversation_name: "general".to_owned(),
            parts: 1,
        }))
    }

    async fn edit(
        &self,
        _caller: &SessionSlot,
        request: SlackEditRequest,
    ) -> Result<(), SlackEditError> {
        self.edit_calls.lock().push(request);
        self.edit_result.lock().clone().unwrap_or(Ok(()))
    }

    async fn react(
        &self,
        _caller: &SessionSlot,
        request: SlackReactRequest,
    ) -> Result<(), SlackReactError> {
        self.react_calls.lock().push(request);
        self.react_result.lock().clone().unwrap_or(Ok(()))
    }

    fn subscribed_targets(
        &self,
        _caller: &SessionSlot,
        workspace: Option<&str>,
    ) -> Vec<SlackSubscription> {
        self.subscribed_targets_calls.lock().push(workspace.map(str::to_owned));
        self.subscribed_targets.lock().clone()
    }

    fn subscribe(
        &self,
        _caller: &SessionSlot,
        workspace: Option<&str>,
        request: SlackSubscribeRequest,
    ) -> Result<Vec<SlackSubscription>, SlackSubscribeError> {
        self.subscribe_calls.lock().push((workspace.map(str::to_owned), request));
        self.subscribe_result.lock().clone().unwrap_or_else(|| Ok(Vec::new()))
    }

    fn unsubscribe(
        &self,
        _caller: &SessionSlot,
        id: Uuid,
    ) -> Result<SlackSubscription, SlackUnsubscribeError> {
        self.unsubscribe_calls.lock().push(id);
        self.unsubscribe_result
            .lock()
            .clone()
            .unwrap_or(Err(SlackUnsubscribeError::NoSuchSubscription))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LoadedConfig;
    use crate::mcp::slack::Unsubscribe;
    use crate::mcp::test_support::text_of;
    use forge_connectors::slack::{
        AuthTest, MessagePage, PostedMessage, SearchPage, SlackApi, SlackError,
    };
    use forge_primitives::slack::{
        SlackBookmark, SlackConfig, SlackFile, SlackFileShare, SlackFileShares, SlackPin,
        SlackPinMessage, SlackUserProfile,
    };
    use forge_sdk::mcp::tool::{Tool as _, ToolInput};
    use std::collections::{BTreeMap, HashMap};
    use tempfile::tempdir;

    fn caller() -> SessionSlot {
        SessionSlot::lead("TestOrg", "forge")
    }

    /// One recorded history read: conversation, oldest, newest, limit.
    type HistoryRead = (String, Option<String>, Option<String>, u32);

    /// Records every outbound call, so a test can assert what did and did
    /// not reach Slack.
    #[derive(Default)]
    struct RecordingApi {
        posts: parking_lot::Mutex<Vec<(String, String, Option<String>)>>,
        updates: parking_lot::Mutex<Vec<(String, String, String)>>,
        deletes: parking_lot::Mutex<Vec<(String, String)>>,
        reactions: parking_lot::Mutex<Vec<(String, String, String, bool)>>,
        /// Messages Slack refuses to change, by ts: not the user's own.
        foreign: parking_lot::Mutex<std::collections::HashSet<String>>,
        /// Seeded files: id -> (name as the uploader chose it, bytes).
        files: parking_lot::Mutex<HashMap<String, (String, Vec<u8>)>>,
        uploads: parking_lot::Mutex<Vec<(String, String, Option<String>)>>,
        upload_urls: parking_lot::Mutex<usize>,
        /// Seeded reads: channel -> rows.
        pins: parking_lot::Mutex<HashMap<String, Vec<SlackPin>>>,
        bookmark_rows: parking_lot::Mutex<HashMap<String, Vec<SlackBookmark>>>,
        pin_reads: parking_lot::Mutex<Vec<String>>,
        bookmark_reads: parking_lot::Mutex<Vec<String>>,
        /// Seeded conversation and thread reads.
        history_rows: parking_lot::Mutex<HashMap<String, Vec<SlackHistoryMessage>>>,
        reply_rows: parking_lot::Mutex<HashMap<String, Vec<SlackHistoryMessage>>>,
        history_reads: parking_lot::Mutex<Vec<HistoryRead>>,
        reply_reads: parking_lot::Mutex<Vec<(String, String)>>,
        /// Search answers page by page; the queries are recorded so a test
        /// can prove the conversation name was narrowed into `in:`.
        search_pages: parking_lot::Mutex<Vec<Vec<SlackSearchMatch>>>,
        search_reads: parking_lot::Mutex<Vec<String>>,
        /// The directory a user resolver reads.
        users_rows: parking_lot::Mutex<Vec<SlackUser>>,
        /// The ts `chat.postMessage` answers with.
        post_ts: parking_lot::Mutex<Option<String>>,
        /// The channel `chat.postMessage` answers with; defaults to the
        /// channel the request named.
        post_channel: parking_lot::Mutex<Option<String>>,
        /// Answer no channel at all, the shape Slack omits it in.
        post_channel_absent: parking_lot::Mutex<bool>,
        /// Parks a post, an upload or a `files.info` until the test
        /// releases it.
        post_park: parking_lot::Mutex<Option<Arc<tokio::sync::Notify>>>,
        upload_park: parking_lot::Mutex<Option<Arc<tokio::sync::Notify>>>,
        file_info_park: parking_lot::Mutex<Option<Arc<tokio::sync::Notify>>>,
        /// How many posts have been answered, so an unseeded answer is a
        /// distinct ts per call - a multi-part post needs two.
        post_calls: parking_lot::Mutex<usize>,
        /// The 0-based post call that fails, for a partial multi-part post.
        post_fail_at: parking_lot::Mutex<Option<usize>>,
        /// Where each seeded file was shared: id -> shares.
        file_shares: parking_lot::Mutex<HashMap<String, SlackFileShares>>,
        /// The shares `files.completeUploadExternal` answers with, when a
        /// test wants the extra `files.info` read to be skipped.
        upload_shares: parking_lot::Mutex<Option<SlackFileShares>>,
        /// How many `files.info` calls were made.
        file_info_calls: parking_lot::Mutex<usize>,
        /// Seeded conversations, for the display name a held draft carries.
        conversations: parking_lot::Mutex<Vec<SlackConversation>>,
    }

    impl RecordingApi {
        fn refuse_edits_of(&self, ts: &str) {
            self.foreign.lock().insert(ts.to_owned());
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
        fn seed_history(&self, channel: &str, rows: Vec<SlackHistoryMessage>) {
            self.history_rows.lock().insert(channel.to_owned(), rows);
        }
        fn seed_replies(&self, channel: &str, ts: &str, rows: Vec<SlackHistoryMessage>) {
            self.reply_rows.lock().insert(format!("{channel}/{ts}"), rows);
        }
        fn seed_search(&self, rows: Vec<SlackSearchMatch>) {
            self.search_pages.lock().push(rows);
        }
        fn seed_search_pages(&self, pages: Vec<Vec<SlackSearchMatch>>) {
            self.search_pages.lock().extend(pages);
        }
        fn seed_users(&self, rows: Vec<SlackUser>) {
            self.users_rows.lock().extend(rows);
        }
        fn history_reads(&self) -> Vec<HistoryRead> {
            self.history_reads.lock().clone()
        }
        fn reply_reads(&self) -> Vec<(String, String)> {
            self.reply_reads.lock().clone()
        }
        fn search_reads(&self) -> Vec<String> {
            self.search_reads.lock().clone()
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
        fn seed_conversation(&self, id: &str, name: &str) {
            self.conversations.lock().push(SlackConversation {
                id: id.to_owned(),
                name: Some(name.to_owned()),
                is_channel: true,
                is_private: false,
                is_im: false,
                is_mpim: false,
                is_archived: false,
                user: None,
                purpose: None,
                topic: None,
            });
        }
        fn seed_dm(&self, id: &str, user: &str) {
            self.conversations.lock().push(SlackConversation {
                id: id.to_owned(),
                name: None,
                is_channel: false,
                is_private: false,
                is_im: true,
                is_mpim: false,
                is_archived: false,
                user: Some(user.to_owned()),
                purpose: None,
                topic: None,
            });
        }
        fn seed_mpim(&self, id: &str, name: &str) {
            self.conversations.lock().push(SlackConversation {
                id: id.to_owned(),
                name: Some(name.to_owned()),
                is_channel: false,
                is_private: false,
                is_im: false,
                is_mpim: true,
                is_archived: false,
                user: None,
                purpose: None,
                topic: None,
            });
        }
        fn answer_posts_with(&self, ts: &str) {
            *self.post_ts.lock() = Some(ts.to_owned());
        }
        /// The channel `chat.postMessage` answers with.
        fn answer_posts_in(&self, channel: &str) {
            *self.post_channel.lock() = Some(channel.to_owned());
        }
        /// Answer no channel at all, the shape Slack omits it in.
        fn answer_posts_without_channel(&self) {
            *self.post_channel_absent.lock() = true;
        }
        /// Park every post until the returned gate is notified.
        fn park_posts(&self) -> Arc<tokio::sync::Notify> {
            let gate = Arc::new(tokio::sync::Notify::new());
            *self.post_park.lock() = Some(Arc::clone(&gate));
            gate
        }
        /// Park every upload until the returned gate is notified.
        fn park_uploads(&self) -> Arc<tokio::sync::Notify> {
            let gate = Arc::new(tokio::sync::Notify::new());
            *self.upload_park.lock() = Some(Arc::clone(&gate));
            gate
        }
        /// Park every `files.info` until the returned gate is notified.
        fn park_file_info(&self) -> Arc<tokio::sync::Notify> {
            let gate = Arc::new(tokio::sync::Notify::new());
            *self.file_info_park.lock() = Some(Arc::clone(&gate));
            gate
        }
        /// Make the 0-based `index` post call fail.
        fn fail_post_at(&self, index: usize) {
            *self.post_fail_at.lock() = Some(index);
        }
        /// Seed where a file is shared, as `files.info` reports it: the
        /// message ts for the conversation the share names.
        fn seed_file_shares(&self, id: &str, conversation: &str, ts: &str) {
            let mut shares = SlackFileShares::default();
            shares
                .public
                .insert(conversation.to_owned(), vec![SlackFileShare { ts: ts.to_owned() }]);
            self.file_shares.lock().insert(id.to_owned(), shares);
        }
        /// Seed the shares `files.completeUploadExternal` itself answers
        /// with, so no `files.info` follow-up is needed.
        fn seed_upload_shares(&self, conversation: &str, ts: &str) {
            let mut shares = SlackFileShares::default();
            shares
                .public
                .insert(conversation.to_owned(), vec![SlackFileShare { ts: ts.to_owned() }]);
            *self.upload_shares.lock() = Some(shares);
        }
        fn file_info_calls(&self) -> usize {
            *self.file_info_calls.lock()
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
            Ok(self.conversations.lock().clone())
        }

        async fn history(
            &self,
            channel: &str,
            oldest: Option<&str>,
            newest: Option<&str>,
            limit: u32,
            _cursor: Option<&str>,
        ) -> Result<MessagePage, SlackError> {
            self.history_reads.lock().push((
                channel.to_owned(),
                oldest.map(str::to_owned),
                newest.map(str::to_owned),
                limit,
            ));
            Ok(MessagePage {
                messages: self.history_rows.lock().get(channel).cloned().unwrap_or_default(),
                next_cursor: None,
            })
        }

        async fn replies(
            &self,
            channel: &str,
            ts: &str,
            _oldest: Option<&str>,
            _limit: u32,
            _cursor: Option<&str>,
        ) -> Result<MessagePage, SlackError> {
            self.reply_reads.lock().push((channel.to_owned(), ts.to_owned()));
            Ok(MessagePage {
                messages: self
                    .reply_rows
                    .lock()
                    .get(&format!("{channel}/{ts}"))
                    .cloned()
                    .unwrap_or_default(),
                next_cursor: None,
            })
        }

        async fn post_message(
            &self,
            channel: &str,
            text: &str,
            thread_ts: Option<&str>,
        ) -> Result<PostedMessage, SlackError> {
            self.posts.lock().push((
                channel.to_owned(),
                text.to_owned(),
                thread_ts.map(str::to_owned),
            ));
            let attempt = {
                let mut calls = self.post_calls.lock();
                let attempt = *calls;
                *calls += 1;
                attempt
            };
            let gate = self.post_park.lock().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            if *self.post_fail_at.lock() == Some(attempt) {
                return Err(SlackError::Api {
                    method: "chat.postMessage".to_owned(),
                    error: "rate_limited".to_owned(),
                    needed: None,
                });
            }
            let ts = self.post_ts.lock().clone().unwrap_or_else(|| format!("{}.0", attempt + 1));
            let answered = if *self.post_channel_absent.lock() {
                None
            } else {
                Some(self.post_channel.lock().clone().unwrap_or_else(|| channel.to_owned()))
            };
            Ok(PostedMessage { channel: answered, ts })
        }

        async fn update_message(
            &self,
            channel: &str,
            ts: &str,
            text: &str,
        ) -> Result<(), SlackError> {
            if self.foreign.lock().contains(ts) {
                return Err(SlackError::Api {
                    method: "chat.update".to_owned(),
                    error: "cant_update_message".to_owned(),
                    needed: None,
                });
            }
            self.updates.lock().push((channel.to_owned(), ts.to_owned(), text.to_owned()));
            Ok(())
        }

        async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), SlackError> {
            if self.foreign.lock().contains(ts) {
                return Err(SlackError::Api {
                    method: "chat.delete".to_owned(),
                    error: "cant_delete_message".to_owned(),
                    needed: None,
                });
            }
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

        async fn file_info(&self, id: &str) -> Result<SlackFile, SlackError> {
            *self.file_info_calls.lock() += 1;
            let gate = self.file_info_park.lock().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
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
                shares: self.file_shares.lock().get(id).cloned().unwrap_or_default(),
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
        ) -> Result<Vec<SlackFile>, SlackError> {
            self.uploads.lock().push((
                file_id.to_owned(),
                channel.to_owned(),
                thread_ts.map(str::to_owned),
            ));
            let gate = self.upload_park.lock().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            Ok(vec![SlackFile {
                id: file_id.to_owned(),
                name: String::new(),
                url_private: String::new(),
                shares: self.upload_shares.lock().clone().unwrap_or_default(),
            }])
        }

        async fn search_messages(
            &self,
            query: &str,
            _count: u32,
            cursor: Option<&str>,
        ) -> Result<SearchPage, SlackError> {
            self.search_reads.lock().push(query.to_owned());
            let pages = self.search_pages.lock();
            let index = cursor.and_then(|cursor| cursor.parse::<usize>().ok()).unwrap_or(0);
            let matches = pages.get(index).cloned().unwrap_or_default();
            Ok(SearchPage {
                matches,
                next_cursor: (index + 1 < pages.len()).then(|| (index + 1).to_string()),
            })
        }

        async fn user_info(&self, user: &str) -> Result<SlackUser, SlackError> {
            Ok(SlackUser {
                id: user.to_owned(),
                name: "tester".to_owned(),
                real_name: None,
                tz: None,
                deleted: false,
                profile: None,
            })
        }

        async fn list_users(&self) -> Result<Vec<SlackUser>, SlackError> {
            Ok(self.users_rows.lock().clone())
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
        tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
    ) {
        let (facade, ws, api, rx, _dir) = facade_with_recording_slack_inner(false);
        (facade, ws, api, rx)
    }

    /// A recording facade, its workspace, the double, the update stream,
    /// and the tempdir that must outlive them.
    type RecordingSlack = (
        Arc<dyn SlackFacade>,
        Arc<Workspace>,
        Arc<RecordingApi>,
        tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>,
        tempfile::TempDir,
    );

    /// The same workspace with a store installed, for the paths that
    /// persist - the own-post ledger.
    fn facade_with_recording_slack_and_store() -> RecordingSlack {
        facade_with_recording_slack_inner(true)
    }

    fn facade_with_recording_slack_inner(with_store: bool) -> RecordingSlack {
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
            None,
        );
        if with_store {
            ws.install_db_for_test(
                crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
            );
        }
        ws.seed_test_project("forge", "/tmp/slack-facade-scope");
        ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
        let facade = ProdSlackFacade::from_arc(&ws);
        (facade, ws, api, rx, dir)
    }

    /// An answer as the registry takes it. Every call site here is a view
    /// answering; the expiry is the facade's own window and the drop is a
    /// dead waiter, and both go through the paths under test rather than
    /// through this.
    fn answered(approved: bool) -> SlackDraftEnding {
        SlackDraftEnding::Answered { approved }
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

    /// Poll until `check` holds, failing the test rather than hanging.
    async fn until(check: impl Fn() -> bool) {
        for _ in 0..400 {
            if check() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the condition never held");
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

    /// A fetch into a directory that does not exist yet just works: the
    /// directory is created, however many levels are missing.
    #[tokio::test]
    async fn a_fetch_creates_a_missing_target_directory() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_file("F1", "notes.txt", b"attachment body");
        let root = tempdir().expect("tempdir");
        let dir = root.path().join("fresh/deeper");

        let path = facade.fetch_attachment(fetch_request("F1", &dir)).await.expect("fetched");
        assert_eq!(
            path.parent(),
            Some(dir.as_path()),
            "the file lands in the directory that was created: {path:?}",
        );
        assert_eq!(std::fs::read(&path).expect("read back"), b"attachment body");
    }

    /// A `dir` that exists but is a file is refused with the typed error
    /// before Slack is contacted: `F1` is unseeded here, so an error other
    /// than `NotADirectory` means the fetch reached Slack first.
    #[tokio::test]
    async fn a_fetch_rejects_a_dir_that_is_a_file() {
        let (facade, _ws, _api, _rx) = facade_with_recording_slack();
        let root = tempdir().expect("tempdir");
        let not_a_dir = root.path().join("plain-file");
        std::fs::write(&not_a_dir, b"data").expect("write the file");

        let err = facade
            .fetch_attachment(fetch_request("F1", &not_a_dir))
            .await
            .expect_err("a file is not a directory");
        assert_eq!(
            err,
            SlackAttachmentError::NotADirectory(not_a_dir.clone()),
            "the error carries the path the caller chose",
        );
    }

    /// When the write still fails after the checks, the error names the
    /// directory the caller chose instead of a bare errno.
    #[tokio::test]
    async fn a_fetch_failure_names_the_path() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_file("F1", "notes.txt", b"body");
        let root = tempdir().expect("tempdir");
        let block = root.path().join("plain-file");
        std::fs::write(&block, b"data").expect("write the file");
        let dir = block.join("sub");

        let err =
            facade.fetch_attachment(fetch_request("F1", &dir)).await.expect_err("cannot land");
        let SlackAttachmentError::Io(message) = err else {
            panic!("expected the io error, got {err:?}");
        };
        assert!(
            message.contains(&dir.display().to_string()),
            "the error names the directory: {message}",
        );
    }

    /// The approval prompt shows the conversation. The id is not a name
    /// anybody recognises, so it is resolved before the draft is held.
    #[tokio::test]
    async fn a_held_draft_names_the_conversation_not_its_id() {
        let (facade, ws, api, mut rx) = facade_with_recording_slack();
        api.seed_conversation("C1", "ops");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });

        let mut seen = None;
        for _ in 0..200 {
            while let Ok(frame) = rx.try_recv() {
                if let crate::protocol::SessionUpdate::SlackPostPending { draft, .. } = frame.update
                {
                    seen = Some(draft.conversation_label);
                }
            }
            if seen.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            seen.expect("the draft reaches the prompt"),
            "ops",
            "the prompt names the channel the message goes to",
        );

        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(false));
        let _ = task.await;
    }

    /// The ts Slack answers a post with is the handle an edit, a delete or
    /// a reaction takes, and Slack has already sent it.
    #[tokio::test]
    async fn a_post_carries_the_ts_slack_answered_with() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        api.answer_posts_with("1790186552.442169");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        let outcome = task.await.expect("the post task does not panic").expect("the post landed");
        assert_eq!(
            outcome.ts,
            vec!["1790186552.442169".to_owned()],
            "the ts the wire sent reaches the caller",
        );
    }

    /// Every part Slack accepted is ledgered as it lands. A long draft is
    /// several messages, and one that failed halfway would otherwise echo
    /// the parts that did post back into the session.
    #[tokio::test]
    async fn a_multi_part_post_ledgers_every_landed_part() {
        let (facade, ws, _api, _rx, _dir) = facade_with_recording_slack_and_store();
        let text = "a".repeat(forge_connectors::slack::POST_LIMIT + 1);

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", &text)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        let outcome = task.await.expect("no panic").expect("the parts land");
        assert_eq!(outcome.ts, vec!["1.0".to_owned(), "2.0".to_owned()], "two parts were sent");
        for ts in &outcome.ts {
            assert!(ws.slack_own_post("acme", "C1", ts), "part {ts} must be refused to the sweep");
        }
        assert!(
            !ws.slack_own_post("acme", "C2", "1.0"),
            "and the ledger stays scoped to the conversation it was posted in",
        );
    }

    /// A request can address a DM by user id or a channel by name, and
    /// Slack answers the conversation it actually posted into. The ledger
    /// has to key on that answer, or the sweep looks under the id and the
    /// agent's own DM comes back to it.
    #[tokio::test]
    async fn a_post_is_ledgered_under_the_channel_slack_posted_into() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        api.answer_posts_with("1700000000.000100");
        api.answer_posts_in("D456");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("U123", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));
        let outcome = task.await.expect("no panic").expect("the post lands");

        assert_eq!(outcome.ts, vec!["1700000000.000100".to_owned()]);
        assert!(
            ws.slack_own_post("acme", "D456", "1700000000.000100"),
            "the conversation the post landed in is refused to the sweep",
        );
        assert!(
            !ws.slack_own_post("acme", "U123", "1700000000.000100"),
            "the address it was sent to is not the conversation the sweep reads",
        );
    }

    /// A port that records ts but not the answer's channel would false the
    /// assertion above; the same guard for the partial case is that the
    /// parts Slack took are recorded even when a later part fails.
    #[tokio::test]
    async fn a_partial_multi_part_post_ledgers_the_parts_slack_took() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        api.fail_post_at(1);
        let text = "a".repeat(forge_connectors::slack::POST_LIMIT + 1);

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", &text)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));
        let outcome = task.await.expect("no panic");

        assert!(
            matches!(outcome, Err(SlackPostError::Partial { posted: 1, .. })),
            "the failure names how much landed: {outcome:?}",
        );
        assert!(
            ws.slack_own_post("acme", "C1", "1.0"),
            "the part Slack took is refused to the sweep",
        );
    }

    /// The mark is held across the post call: without it a sweep in the
    /// window delivers what the post is about to ledger.
    #[tokio::test]
    async fn a_post_marks_its_conversation_in_flight_until_the_ledger_write() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        let gate = api.park_posts();

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        let mut marked = false;
        for _ in 0..400 {
            if ws.slack_post_in_flight("acme", "C1").is_some() {
                marked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(marked, "the facade marks the conversation before the post returns");

        gate.notify_one();
        task.await.expect("no panic").expect("the post lands");
        assert!(
            ws.slack_post_in_flight("acme", "C1").is_none(),
            "and clears it once the ts is ledgered",
        );
    }

    /// Each part carries its own mark: a part that has burned its budget
    /// must not leave the next one unprotected, which is what a stamp
    /// taken once per operation does.
    #[tokio::test]
    async fn each_post_part_takes_a_fresh_in_flight_mark() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        let gate = api.park_posts();
        let text = "a".repeat(forge_connectors::slack::POST_LIMIT + 1);

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", &text)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        until(|| ws.slack_post_in_flight("acme", "C1").is_some()).await;
        // As if the first part had used its whole request budget.
        let stale = std::time::Instant::now()
            .checked_sub(forge_connectors::slack::REQUEST_TIMEOUT)
            .and_then(|instant| instant.checked_sub(std::time::Duration::from_secs(1)))
            .expect("the clock is past the timeout");
        ws.slack_inflight
            .lock()
            .get_mut(&("acme".to_owned(), "C1".to_owned()))
            .expect("the part's entry")
            .iter_mut()
            .for_each(|stamp| stamp.since = stale);
        gate.notify_one();

        // Part two is parked once its call has been recorded; its mark
        // must be fresh, not the expired one.
        until(|| api.posts().len() == 2).await;
        assert!(
            ws.slack_post_in_flight("acme", "C1").is_some(),
            "the second part takes a fresh mark of its own",
        );
        gate.notify_one();
        task.await.expect("no panic").expect("both parts land");
    }

    /// A failed post leaves no mark behind: the guard drops on the error
    /// return, so the conversation is swept again next tick.
    #[tokio::test]
    async fn a_failed_post_clears_its_in_flight_mark() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        api.fail_post_at(0);

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        assert!(task.await.expect("no panic").is_err(), "the post fails");
        assert!(
            ws.slack_post_in_flight("acme", "C1").is_none(),
            "a failed post leaves no mark behind",
        );
    }

    /// The upload site takes the same mark: it is held until the shares
    /// have been read and ledgered.
    #[tokio::test]
    async fn an_upload_marks_its_conversation_in_flight_until_the_ledger_write() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        let gate = api.park_uploads();
        let dir = tempdir().expect("tempdir");
        let source = dir.path().join("file.txt");
        std::fs::write(&source, b"payload").expect("write the source file");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        let mut marked = false;
        for _ in 0..400 {
            if ws.slack_post_in_flight("acme", "C1").is_some() {
                marked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(marked, "the facade marks the conversation before the upload completes");

        gate.notify_one();
        task.await.expect("no panic").expect("an approved upload sends");
        assert!(
            ws.slack_post_in_flight("acme", "C1").is_none(),
            "and clears it once the upload's shares are ledgered",
        );
    }

    /// The shares read after the upload carries a mark of its own: the
    /// file message is already in history while that call is in flight,
    /// and its ts is not ledgered until it returns.
    #[tokio::test]
    async fn the_upload_marks_its_conversation_while_the_shares_are_read() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        api.seed_file("F-uploaded", "file.txt", b"payload");
        let gate = api.park_file_info();
        let dir = tempdir().expect("tempdir");
        let source = dir.path().join("file.txt");
        std::fs::write(&source, b"payload").expect("write the source file");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        until(|| api.file_info_calls() >= 1).await;
        assert!(
            ws.slack_post_in_flight("acme", "C1").is_some(),
            "the shares read holds a fresh mark of its own",
        );

        gate.notify_one();
        task.await.expect("no panic").expect("the upload sends");
        assert!(
            ws.slack_post_in_flight("acme", "C1").is_none(),
            "and clears it once the shares are ledgered",
        );
    }

    /// A share read that fails is a ledger gap, not a failed upload: the
    /// file is already posted, and reporting it as unsent would invite a
    /// duplicate.
    #[tokio::test]
    async fn an_upload_whose_shares_cannot_be_read_still_succeeds() {
        let (facade, ws, _api, _rx, _dir) = facade_with_recording_slack_and_store();
        let dir = tempdir().expect("tempdir");
        let source = dir.path().join("file.txt");
        std::fs::write(&source, b"payload").expect("write the source file");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        assert!(
            task.await.expect("no panic").is_ok(),
            "an unreadable share is a ledger gap, not a failed upload",
        );
        assert!(
            ws.slack_post_in_flight("acme", "C1").is_none(),
            "and the mark is cleared whatever the share read did",
        );
    }

    /// Slack's `chat.postMessage` answers the channel it posted into; the
    /// fallback is for the case it omits it, and it is right where it can
    /// be right: a conversation addressed by id.
    #[tokio::test]
    async fn a_post_answered_without_a_channel_is_ledgered_under_the_resolved_conversation() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        api.answer_posts_with("1700000000.000100");
        api.answer_posts_without_channel();

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));
        task.await.expect("no panic").expect("the post lands");

        assert!(
            ws.slack_own_post("acme", "C1", "1700000000.000100"),
            "the resolved conversation is what the fallback keys on",
        );
    }

    /// An attachment posts a message of its own, and the upload call
    /// answers no message ts - the file's shares do. The share's own
    /// conversation is what the ledger keys on.
    #[tokio::test]
    async fn an_upload_ledgers_the_message_under_the_shares_own_conversation() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        api.seed_file("F-uploaded", "file.txt", b"payload");
        api.seed_file_shares("F-uploaded", "D456", "1700000000.000100");
        let dir = tempdir().expect("tempdir");
        let source = dir.path().join("file.txt");
        std::fs::write(&source, b"payload").expect("write the source file");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post_attachment(&caller(), upload_request("U123", &source)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));
        task.await.expect("no panic").expect("an approved upload sends");

        assert!(
            ws.slack_own_post("acme", "D456", "1700000000.000100"),
            "the share's own conversation holds the message, so that is what the ledger keys",
        );
        assert!(
            !ws.slack_own_post("acme", "U123", "1700000000.000100"),
            "the request string is not a conversation",
        );
    }

    /// When the upload's own answer already carries the shares, the
    /// `files.info` follow-up is not worth a call.
    #[tokio::test]
    async fn an_upload_answer_that_carries_its_shares_costs_no_files_info_call() {
        let (facade, ws, api, _rx, _dir) = facade_with_recording_slack_and_store();
        api.seed_upload_shares("C1", "1700000000.000100");
        let dir = tempdir().expect("tempdir");
        let source = dir.path().join("file.txt");
        std::fs::write(&source, b"payload").expect("write the source file");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post_attachment(&caller(), upload_request("C1", &source)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));
        task.await.expect("no panic").expect("an approved upload sends");

        assert_eq!(api.file_info_calls(), 0, "the answer was enough; no follow-up read");
        assert!(
            ws.slack_own_post("acme", "C1", "1700000000.000100"),
            "and the message it named is refused to the sweep",
        );
    }

    /// An edit keeps the message's ts, so the rewritten message is refused
    /// to the sweep whether forge posted it or the user did.
    #[tokio::test]
    async fn an_edit_ledgers_the_message_ts_it_rewrote() {
        let (facade, ws, _api, _rx, _dir) = facade_with_recording_slack_and_store();

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));
        task.await.expect("no panic").expect("an approved replacement is sent");

        assert!(
            ws.slack_own_post("acme", "C1", "100.0"),
            "the edited message's own ts is refused to the sweep",
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
            while let Ok(frame) = rx.try_recv() {
                if let crate::protocol::SessionUpdate::SlackPostPending { draft, .. } = frame.update
                {
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
        ws.resolve_slack_draft(id, &caller(), answered(false));
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
        ws.resolve_slack_draft(id, &caller(), answered(false));

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
        ws.resolve_slack_draft(id, &caller(), answered(true));
        task.await.expect("no panic").expect("an approved upload sends");

        assert_eq!(api.upload_url_count(), 1, "the upload URL is requested once");
        assert_eq!(api.uploads().len(), 1, "and completed once");
        assert_eq!(api.uploads()[0].1, "C1", "into the conversation that was named");
    }

    /// The waiter going away is its own ending: nothing was answered, and a
    /// view still holding the dock hears it rather than silence.
    #[tokio::test]
    async fn a_waiter_that_dies_is_reported_as_abandoned() {
        let (facade, ws, _api, mut rx) = facade_with_recording_slack();
        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let _id = wait_for_draft(&ws).await;

        task.abort();
        let _ = task.await;

        let mut resolved = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::SlackDraftResolved { ending, .. } = frame.update
            {
                resolved.push(ending);
            }
        }
        assert_eq!(
            resolved,
            vec![SlackDraftEnding::Abandoned],
            "a dropped waiter clears the registry and says which ending took the draft",
        );
        assert!(ws.slack_drafts.lock().is_empty(), "and leaves nothing registered");
    }

    #[tokio::test]
    async fn a_rejected_draft_never_reaches_slack() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(false));

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

    /// The approval window expiring is its own outcome, distinct from a
    /// rejection, and leaves nothing behind: no post, no registry entry,
    /// and exactly one stand-down for the views.
    #[tokio::test(start_paused = true)]
    async fn a_draft_whose_window_expires_is_expired_not_rejected() {
        let (facade, ws, api, mut rx) = facade_with_recording_slack();
        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;

        tokio::time::advance(APPROVAL_TIMEOUT + Duration::from_secs(1)).await;
        let outcome = task.await.expect("no panic");
        assert_eq!(
            outcome,
            Err(SlackPostError::Expired),
            "a timed-out draft is an expiry, never a rejection and never an approval",
        );
        assert!(api.posts().is_empty(), "an expired draft must not post");
        assert!(ws.slack_drafts.lock().is_empty(), "the expiry also clears the registry entry");

        let mut resolved = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::SlackDraftResolved { key, id, ending } =
                frame.update
            {
                resolved.push((key, id, ending));
            }
        }
        assert_eq!(
            resolved,
            vec![(caller(), id, SlackDraftEnding::Expired)],
            "the expiry stands the views down once, and says which ending it was",
        );
    }

    /// The outcome carries what the caller cannot work out for itself: the
    /// ts Slack minted, the channel's own name (the input names an id), and
    /// how many messages the text was split into.
    #[tokio::test]
    async fn an_approved_draft_posts_exactly_once() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        api.seed_conversation("C1", "ops");
        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", "hello")).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        let outcome = task.await.expect("no panic").expect("an approved draft posts");
        assert_eq!(api.posts().len(), 1, "exactly one message goes out");
        assert_eq!(outcome.ts, vec!["1.0".to_owned()], "the ts the wire minted");
        assert_eq!(outcome.conversation_name, "ops", "the channel's name, not the id it was sent");
        assert_eq!(outcome.parts, 1, "a short draft is one part");
    }

    /// A draft past the split point goes out in several messages, and the
    /// outcome's own count is the only thing that says so: a reader told
    /// "one post" would take a truncated send for the whole text.
    #[tokio::test]
    async fn a_split_draft_reports_its_part_count() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        let long = "x".repeat(forge_connectors::slack::POST_LIMIT + 100);
        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.post(&caller(), post_request("C1", &long)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        let outcome = task.await.expect("no panic").expect("an approved draft posts");
        assert_eq!(outcome.parts, 2, "the split count travels with the ts");
        assert_eq!(outcome.ts.len(), 2, "and one ts per message really went out");
        assert_eq!(api.posts().len(), 2, "both parts reached Slack");
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
        ws.resolve_slack_draft(id, &caller(), answered(true));
        task.await.expect("no panic").expect("posted");

        assert_eq!(api.posts()[0].2.as_deref(), Some("100.0"));
    }

    #[tokio::test]
    async fn a_foreign_message_is_refused_by_the_wire_and_surfaced_as_its_reason() {
        // Slack's rule: only the authenticated user's messages can be
        // changed. The refusal is the wire's verdict - a local lookup
        // would wrongly refuse an edit of the user's own thread reply,
        // which history cannot see.
        let (facade, ws, api, _rx) = facade_with_recording_slack();
        api.refuse_edits_of("100.0");

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));

        assert_eq!(
            task.await.expect("no panic"),
            Err(SlackEditError::NotOwnMessage),
            "the wire's cant_update_message reads as not-your-message",
        );
        assert!(api.updates.lock().is_empty(), "a refused update changes nothing");
    }

    #[tokio::test]
    async fn editing_own_message_updates_it_once_approved() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));
        task.await.expect("no panic").expect("an approved replacement is sent");

        assert_eq!(api.updates.lock().len(), 1);
    }

    #[tokio::test]
    async fn a_rejected_edit_leaves_the_message_untouched() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", Some("new text"))).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(false));

        assert_eq!(task.await.expect("no panic"), Err(SlackEditError::Rejected));
        assert!(api.updates.lock().is_empty(), "a rejected replacement is never sent");
    }

    #[tokio::test]
    async fn deleting_own_message_removes_it_once_approved() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", None)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(true));
        task.await.expect("no panic").expect("an approved deletion is sent");

        assert_eq!(api.deletes.lock().len(), 1);
    }

    #[tokio::test]
    async fn a_rejected_delete_leaves_the_message_alone() {
        let (facade, ws, api, _rx) = facade_with_recording_slack();

        let task = tokio::spawn({
            let facade = facade.clone();
            async move { facade.edit(&caller(), edit_request("C1", "100.0", None)).await }
        });
        let id = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(id, &caller(), answered(false));

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
        ws.resolve_slack_draft(first, &caller(), answered(true));
        let second = wait_for_draft(&ws).await;
        ws.resolve_slack_draft(second, &caller(), answered(true));

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
        ws.resolve_slack_draft(id, &caller(), answered(false));

        assert_eq!(task.await.expect("no panic"), Err(SlackReactError::Rejected));
        assert!(api.reactions.lock().is_empty(), "a rejected reaction is never applied");
    }

    fn cfg(label: &str) -> SlackConfig {
        SlackConfig {
            workspace: label.to_owned(),
            token: "xoxp-test".to_owned(),
            poll_seconds: 30,
            dm_poll_seconds: 150,
            thread_idle_days: 14,
            stale_after_hours: 24,
        }
    }

    /// A workspace whose `[[slack]]` holds exactly `label`, with a lead
    /// session recorded so the caller resolves to a project.
    fn workspace_with_one_slack_workspace(label: &str) -> (Arc<Workspace>, Arc<dyn SlackFacade>) {
        let dir = tempdir().expect("tempdir");
        let mut config = LoadedConfig::empty_for_test();
        config.slack = vec![cfg(label)];
        let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
            .expect("the stub config's [[slack]] entries are well-formed");
        ws.seed_test_project("forge", "/tmp/slack-facade-scope");
        ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
        let facade = ProdSlackFacade::from_arc(&ws);
        (ws, facade)
    }

    /// The same, with a store installed: the cursor tests need real
    /// watermarks. The tempdir is returned so it outlives the db.
    fn workspace_with_one_slack_workspace_and_store(
        label: &str,
    ) -> (tempfile::TempDir, Arc<Workspace>, Arc<dyn SlackFacade>) {
        let dir = tempdir().expect("tempdir");
        let mut config = LoadedConfig::empty_for_test();
        config.slack = vec![cfg(label)];
        let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
            .expect("the stub config's [[slack]] entries are well-formed");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("forge", "/tmp/slack-facade-scope");
        ws.record_connected_session("/tmp/slack-facade-scope", "caller-uuid", None);
        let facade = ProdSlackFacade::from_arc(&ws);
        (dir, ws, facade)
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

    /// A conversation is deduped where a mention feed is not: a repeat hands
    /// back the record that exists rather than adding a second Inspector row
    /// for one channel, and a mode the caller asks for is the mode it gets,
    /// since there is no other route to change one.
    #[tokio::test]
    async fn subscribing_twice_to_a_conversation_yields_one_record() {
        let (facade, ws, _api, _rx) = facade_with_recording_slack();
        let watching = |mode| {
            SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                id: "C1".to_owned(),
                mode,
            }])
        };

        let first = facade
            .subscribe(&caller(), Some("acme"), watching(SlackWatchMode::All))
            .expect("first");
        assert_eq!(first.len(), 1, "the first subscribe creates the record");

        let second = facade
            .subscribe(&caller(), Some("acme"), watching(SlackWatchMode::MentionsOnly))
            .expect("second");
        assert_eq!(second.len(), 1, "the repeat adds no second record");
        assert_eq!(
            second[0].id, first[0].id,
            "the repeat hands back the id it already has, so the caller can drop it without a lookup",
        );
        assert_eq!(
            second[0].target,
            SlackSubscriptionTarget::Conversation {
                id: "C1".to_owned(),
                name: None,
                mode: SlackWatchMode::MentionsOnly,
            },
            "and the record as it NOW stands, not the state before the mode moved",
        );
        let stored = ws.slack_subscriptions_for_project("forge");
        assert_eq!(stored.len(), 1, "one record, so the Inspector renders one row");
        assert_eq!(
            stored[0].target,
            SlackSubscriptionTarget::Conversation {
                id: "C1".to_owned(),
                name: None,
                mode: SlackWatchMode::MentionsOnly,
            },
            "and the mode asked for is the mode stored",
        );
    }

    /// The unsubscribe hands back the row it removed, not another one: with
    /// two owned subscriptions and the SECOND removed, a row looked up by
    /// "the project's first" names the wrong target - and the store check
    /// catches a removal that took the wrong one.
    #[tokio::test]
    async fn unsubscribe_returns_the_row_it_removed_and_leaves_the_other() {
        let (facade, ws, _api, _rx) = facade_with_recording_slack();
        let first = facade
            .subscribe(&caller(), Some("acme"), SlackSubscribeRequest::Mentions)
            .expect("first");
        let second = facade
            .subscribe(&caller(), Some("acme"), SlackSubscribeRequest::DirectMessages)
            .expect("second");
        assert_eq!(second.len(), 1, "one record per target");

        let removed = facade
            .unsubscribe(&caller(), second[0].id)
            .expect("the second subscription is the caller's own");

        assert_eq!(removed.id, second[0].id, "the echoed row is the one that went");
        assert_eq!(
            removed.target,
            SlackSubscriptionTarget::DirectMessages,
            "carrying the target it watched, not the other row's",
        );
        let left = ws.slack_subscriptions_for_project("forge");
        assert_eq!(left.len(), 1, "only the untouched subscription remains");
        assert_eq!(left[0].id, first[0].id, "and it is the one the call did not name");
    }

    /// The refusal names the case: a row another session owns reads as
    /// *not yours*, never as absent.
    #[tokio::test]
    async fn unsubscribe_refuses_another_owners_row() {
        let (facade, ws, _api, _rx) = facade_with_recording_slack();
        let worker = live_worker(&ws, "forge", "tester");
        let owned = facade
            .subscribe(&caller(), Some("acme"), SlackSubscribeRequest::Mentions)
            .expect("the lead subscribes");

        assert_eq!(
            facade.unsubscribe(&worker, owned[0].id),
            Err(SlackUnsubscribeError::NotOwnedByCaller),
            "the lead's subscription is not the worker's to remove, and the refusal says which case it is",
        );
        assert_eq!(
            ws.slack_subscriptions_for_project("forge").len(),
            1,
            "a refused unsubscribe removes nothing",
        );
    }

    /// The other half of the refusal: an id the project never had is a
    /// different answer from one that exists under another session.
    #[test]
    fn unsubscribe_refuses_an_id_the_project_never_had() {
        let (facade, _ws, _api, _rx) = facade_with_recording_slack();

        assert_eq!(
            facade.unsubscribe(&caller(), Uuid::from_u128(0xdead)),
            Err(SlackUnsubscribeError::NoSuchSubscription),
            "an id with no entry in the project is reported as absent",
        );
    }

    /// Both clauses of that message are about the caller's own project:
    /// an id living in another project is not "in this project but not
    /// subscribed by you", it is not in this project at all.
    #[test]
    fn unsubscribe_refuses_an_id_from_another_project_as_absent() {
        let (facade, ws, _api, _rx) = facade_with_recording_slack();
        let other = SlackSubscription {
            id: Uuid::new_v4(),
            workspace: "acme".to_owned(),
            project: "otherproj".to_owned(),
            team_role: None,
            target: SlackSubscriptionTarget::Mentions,
            created_at: SystemTime::UNIX_EPOCH,
        };
        let other_id = other.id;
        ws.add_slack_subscription(other, false);

        assert_eq!(
            facade.unsubscribe(&caller(), other_id),
            Err(SlackUnsubscribeError::NoSuchSubscription),
            "an id that lives in another project is absent here, not a subscription of this project's",
        );
    }

    /// A live worker row with `label`, so a caller on that slot resolves
    /// to the project and owns subscriptions under its label.
    fn live_worker(ws: &Workspace, project: &str, label: &str) -> SessionSlot {
        let slot = SessionSlot::worker("TestOrg", project, label);
        let project_key = ws
            .list_projects()
            .into_iter()
            .find(|view| view.name == project)
            .expect("the seeded project")
            .key;
        ws.insert_live_worker(
            &project_key,
            crate::mcp::workers::types::WorkerEntry {
                label: label.into(),
                charter: "c".into(),
                slot: slot.clone(),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead("TestOrg", project),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                cwd: None,
                diagnostic: None,
                kick: None,
            },
        );
        slot
    }

    /// The refusal is what a caller acts on, so it has to name the case:
    /// a row the project holds under another session reads as *not
    /// yours*, an id the project never had as *not there*. One string
    /// for both lets a caller take its own refused unsubscribe as proof
    /// the subscription is gone.
    #[tokio::test]
    async fn unsubscribe_refusals_name_the_case_through_the_tool() {
        let (ws, facade) = workspace_with_one_slack_workspace("acme");
        let worker = live_worker(&ws, "forge", "tester");
        let lead_sub = facade
            .subscribe(&caller(), Some("acme"), SlackSubscribeRequest::Mentions)
            .expect("the lead subscribes");
        let tool = Unsubscribe { facade, slot: worker };

        let not_yours = tool
            .call(ToolInput { value: serde_json::json!({ "id": lead_sub[0].id.to_string() }) })
            .await;
        assert!(not_yours.is_error, "a refused unsubscribe is an error");
        assert_eq!(
            text_of(&not_yours),
            format!(
                "subscription {} exists in this project but was not subscribed by you",
                lead_sub[0].id
            ),
            "a live subscription owned by another session must say so, not report it missing",
        );

        let ghost = Uuid::from_u128(0xdead);
        let not_there =
            tool.call(ToolInput { value: serde_json::json!({ "id": ghost.to_string() }) }).await;
        assert!(not_there.is_error, "an unknown id is an error");
        assert_eq!(
            text_of(&not_there),
            format!("no subscription with id {ghost} in this project"),
            "an id the project never had must not read as another session's subscription",
        );
    }

    #[tokio::test]
    async fn subscribing_to_an_unknown_workspace_is_refused_with_the_known_labels() {
        let (ws, facade) = workspace_with_one_slack_workspace("acme");
        let err = facade
            .subscribe(&caller(), Some("nope"), SlackSubscribeRequest::DirectMessages)
            .expect_err("a workspace that is not configured cannot be watched");
        assert_eq!(
            err,
            SlackSubscribeError::Workspace(SlackWorkspaceError::UnknownWorkspace {
                requested: "nope".to_owned(),
                known: vec!["acme".to_owned()],
            }),
            "the refusal names the label asked for and the ones that exist",
        );
        drop(ws);
    }

    /// Omitting `workspace` with several configured is its own case, not a
    /// missing configuration: every tool reports it with the labels to pick
    /// from.
    #[tokio::test]
    async fn omitting_the_workspace_with_several_configured_names_the_labels() {
        let dir = tempdir().expect("tempdir");
        let mut config = LoadedConfig::empty_for_test();
        config.slack = vec![cfg("acme"), cfg("beta")];
        let (ws, _rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config)
            .expect("the stub config's [[slack]] entries are well-formed");
        let facade = ProdSlackFacade::from_arc(&ws);

        assert_eq!(
            facade.search(None, "deploy", 5, None).await.expect_err("no label was named"),
            SlackReadError::Workspace(SlackWorkspaceError::WorkspaceRequired {
                known: vec!["acme".to_owned(), "beta".to_owned()],
            }),
            "the search names the case and the labels",
        );
        assert_eq!(
            facade.pins(None, "C1").await.expect_err("no label was named"),
            SlackReadError::Workspace(SlackWorkspaceError::WorkspaceRequired {
                known: vec!["acme".to_owned(), "beta".to_owned()],
            }),
            "and so does every other read",
        );
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

    /// A re-subscribe after the cursor cleared seeds fresh: the sweep
    /// baselines at the newest message seen instead of replaying the gap.
    #[tokio::test]
    async fn re_subscribing_after_a_gap_baselines_fresh() {
        let (_dir, ws, facade) = workspace_with_one_slack_workspace_and_store("acme");
        facade
            .subscribe(
                &caller(),
                Some("acme"),
                SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                    id: "C1".to_owned(),
                    mode: SlackWatchMode::All,
                }]),
            )
            .expect("the first subscribe");
        ws.set_slack_watermark("acme", "C1", "100.0");
        let id = ws.slack_subscriptions_for_project("forge")[0].id;
        assert!(ws.remove_slack_subscription_owned_by("forge", id, None).is_some());

        facade
            .subscribe(
                &caller(),
                Some("acme"),
                SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                    id: "C1".to_owned(),
                    mode: SlackWatchMode::All,
                }]),
            )
            .expect("re-subscribe");

        let cursor = {
            let db = ws.db.lock();
            crate::store::slack::watermark(db.as_ref().expect("db installed"), "acme", "C1")
                .expect("read")
        };
        assert_ne!(
            cursor,
            Some("100.0".to_owned()),
            "a fresh subscription starts from now, never from the stale cursor",
        );
        assert!(cursor.is_some(), "and it is seeded, so the sweep does not replay");
    }

    /// The cursor belongs to the conversation and is shared by every
    /// owner watching it: a second subscriber seeds only when none
    /// exists, or every owner's pending window is silently dropped.
    #[tokio::test]
    async fn subscribing_to_a_watched_conversation_does_not_reset_its_cursor() {
        let (_dir, ws, facade) = workspace_with_one_slack_workspace_and_store("acme");
        ws.set_slack_watermark("acme", "C1", "100.0");

        facade
            .subscribe(
                &caller(),
                Some("acme"),
                SlackSubscribeRequest::Conversations(vec![SlackChannelWatch {
                    id: "C1".to_owned(),
                    mode: SlackWatchMode::All,
                }]),
            )
            .expect("subscribe to the watched conversation");

        let db = ws.db.lock();
        let cursor =
            crate::store::slack::watermark(db.as_ref().expect("db installed"), "acme", "C1")
                .expect("read");
        assert_eq!(cursor, Some("100.0".to_owned()), "a second subscriber never moves the cursor");
    }

    /// The mention cursor is one per workspace: a second Mentions
    /// subscription seeds nothing. A fresh workspace still seeds, because
    /// the mention sweep has no baseline arm and a missing cursor would
    /// replay its whole history.
    #[tokio::test]
    async fn a_second_mention_subscription_seeds_nothing_but_the_first_seeds() {
        let (_dir, ws, facade) = workspace_with_one_slack_workspace_and_store("acme");

        facade
            .subscribe(&caller(), Some("acme"), SlackSubscribeRequest::Mentions)
            .expect("the first mention subscriber");
        let first = {
            let db = ws.db.lock();
            crate::store::slack::watermark(
                db.as_ref().expect("db installed"),
                "acme",
                MENTION_CURSOR,
            )
            .expect("read")
        };
        assert!(first.is_some(), "the first Mentions subscription seeds the stream");

        ws.set_slack_watermark("acme", MENTION_CURSOR, "100.0");
        facade
            .subscribe(&caller(), Some("acme"), SlackSubscribeRequest::Mentions)
            .expect("the second Mentions subscription");
        let second = {
            let db = ws.db.lock();
            crate::store::slack::watermark(
                db.as_ref().expect("db installed"),
                "acme",
                MENTION_CURSOR,
            )
            .expect("read")
        };
        assert_eq!(
            second,
            Some("100.0".to_owned()),
            "the second subscriber never resets the shared mention cursor",
        );
    }

    fn directory_user(
        id: &str,
        name: &str,
        real_name: Option<&str>,
        display_name: Option<&str>,
        deleted: bool,
    ) -> SlackUser {
        SlackUser {
            id: id.to_owned(),
            name: name.to_owned(),
            real_name: real_name.map(str::to_owned),
            tz: None,
            deleted,
            profile: display_name.map(|display_name| SlackUserProfile {
                display_name: Some(display_name.to_owned()),
            }),
        }
    }

    /// The three names a person is known by are all the same query: a
    /// handle, a legal name and what Slack shows as the display name.
    #[tokio::test]
    async fn a_user_resolver_matches_the_handle_the_real_name_and_the_display_name() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_users(vec![
            directory_user("U1", "ved", Some("Vedhavyas S"), Some("ved"), false),
            directory_user("U2", "gina", Some("Gina Abrams"), None, false),
            directory_user("U3", "csmith", None, Some("Chris"), false),
        ]);

        let by_handle = facade.users(Some("acme"), "VED").await.expect("handle, case-insensitive");
        assert_eq!(by_handle.iter().map(|user| user.id.as_str()).collect::<Vec<_>>(), ["U1"]);

        let by_real_name = facade.users(Some("acme"), "abrams").await.expect("real name");
        assert_eq!(by_real_name.iter().map(|user| user.id.as_str()).collect::<Vec<_>>(), ["U2"]);

        let by_display_name = facade.users(Some("acme"), "chris").await.expect("display name");
        assert_eq!(
            by_display_name.iter().map(|user| user.id.as_str()).collect::<Vec<_>>(),
            ["U3"],
            "the display name matches even when neither the handle nor the real name does",
        );
    }

    /// An empty query must not widen into the whole directory when the
    /// facade is called directly rather than through the tool.
    #[tokio::test]
    async fn a_user_resolver_returns_nothing_for_an_empty_query() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_users(vec![directory_user("U1", "ved", None, None, false)]);

        let found = facade.users(Some("acme"), "   ").await.expect("the guard is not a failure");

        assert!(found.is_empty(), "an empty query matches nobody");
    }

    /// The failure this resolver exists for is a wrong id silently
    /// notifying the wrong person, so several matches come back together
    /// and a deactivated member is never one of them.
    #[tokio::test]
    async fn a_user_resolver_returns_every_live_match_and_no_deleted_ones() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_users(vec![
            directory_user("U2", "samuel", None, None, false),
            directory_user("U1", "sam", None, None, false),
            directory_user("U3", "sam", None, None, true),
            directory_user("U4", "other", None, None, false),
        ]);

        let found = facade.users(Some("acme"), "sam").await.expect("the directory is read");

        assert_eq!(
            found.iter().map(|user| user.id.as_str()).collect::<Vec<_>>(),
            ["U1", "U2"],
            "every live match, sorted by handle, never one picked silently",
        );
    }

    /// A filtered search follows the cursor: the first page may hold no hit
    /// from the conversation, and stopping there would report none.
    #[tokio::test]
    async fn a_filtered_search_pages_until_it_finds_the_conversations_hits() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_conversation("C1", "ops");
        api.seed_search_pages(vec![
            vec![search_hit("C2", "100.1")],
            vec![search_hit("C1", "100.2")],
        ]);

        let found =
            facade.search(Some("acme"), "deploy", 20, Some("C1")).await.expect("the search runs");

        assert_eq!(found.len(), 1, "the match was on the second page: {found:?}");
        assert_eq!(found[0].conversation_id, "C1");
        assert_eq!(
            api.search_reads(),
            ["in:ops deploy", "in:ops deploy"],
            "the filter followed the cursor instead of stopping at the first page",
        );
    }

    /// The label is part of the answer: `auth.test` reports the team, and a
    /// `[[slack]]` label may well be a different string.
    #[tokio::test]
    async fn an_identity_lookup_names_the_resolved_label_and_the_token_user() {
        let (facade, _ws, _api, _rx) = facade_with_recording_slack();

        let (label, auth) = facade.me(Some("acme")).await.expect("the token is proved");

        assert_eq!(label, "acme", "the label the caller can name again");
        assert_eq!(auth.team, "Test", "the team comes from auth.test");
        assert_eq!(auth.user_id, "U1", "and so does the token's own user id");
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

    fn history_message(ts: &str, text: &str) -> SlackHistoryMessage {
        SlackHistoryMessage {
            ts: ts.to_owned(),
            user: Some("U1".to_owned()),
            text: text.to_owned(),
            thread_ts: None,
            reply_count: 0,
            bot_name: None,
            parent_user_id: None,
            latest_reply: None,
            edited: None,
            files: Vec::new(),
        }
    }

    fn search_hit(conversation: &str, ts: &str) -> SlackSearchMatch {
        SlackSearchMatch {
            ts: ts.to_owned(),
            text: "a hit".to_owned(),
            conversation_id: conversation.to_owned(),
            conversation_name: None,
            username: None,
            user: None,
            thread_ts: None,
            files: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_history_read_passes_its_bounds_and_its_page() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_history("C1", vec![history_message("100.1", "hello")]);

        let messages = facade
            .history(Some("acme"), "C1", 25, Some("100.0"), Some("200.0"))
            .await
            .expect("read");

        assert_eq!(messages.len(), 1, "the seeded page comes back");
        assert_eq!(messages[0].text, "hello");
        assert_eq!(
            api.history_reads().as_slice(),
            [("C1".to_owned(), Some("100.0".to_owned()), Some("200.0".to_owned()), 25)],
            "the conversation and both exclusive bounds reach the wire",
        );
    }

    #[tokio::test]
    async fn a_thread_read_returns_the_parent_and_its_replies() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_replies(
            "C1",
            "100.0",
            vec![history_message("100.0", "parent"), history_message("100.5", "reply")],
        );

        let messages =
            facade.replies(Some("acme"), "C1", "100.0", 200).await.expect("the thread is read");

        assert_eq!(messages.len(), 2, "the whole thread comes back");
        assert_eq!(messages[0].text, "parent", "the parent is first");
        assert_eq!(messages[1].text, "reply");
        assert_eq!(
            api.reply_reads().as_slice(),
            [("C1".to_owned(), "100.0".to_owned())],
            "the conversation and parent ts are what was read",
        );
    }

    /// Slack's search language narrows by channel NAME, and an id in `in:`
    /// finds nothing. The name is added server-side, the id still filters
    /// what comes back, and a hit from another conversation never leaks.
    #[tokio::test]
    async fn a_search_filtered_by_a_named_conversation_narrows_and_drops_foreign_hits() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_conversation("C1", "ops");
        api.seed_search(vec![search_hit("C1", "100.1"), search_hit("C2", "100.2")]);

        let found =
            facade.search(Some("acme"), "deploy", 20, Some("C1")).await.expect("the search runs");

        assert_eq!(found.len(), 1, "only the filtered conversation's hit survives: {found:?}");
        assert_eq!(found[0].conversation_id, "C1");
        assert_eq!(
            api.search_reads(),
            ["in:ops deploy"],
            "the name narrows the search server-side",
        );
    }

    /// A DM has no name, and a group DM's generated name is not one search
    /// understands, so neither narrows the query: the id alone filters.
    #[tokio::test]
    async fn a_search_filtered_by_a_dm_or_group_dm_keeps_the_query_whole() {
        let (facade, _ws, api, _rx) = facade_with_recording_slack();
        api.seed_dm("D1", "U9");
        api.seed_mpim("G1", "mpdm-ved--gina-1");
        api.seed_search(vec![
            search_hit("D1", "100.1"),
            search_hit("G1", "100.2"),
            search_hit("C2", "100.3"),
        ]);

        let direct =
            facade.search(Some("acme"), "deploy", 20, Some("D1")).await.expect("the search runs");
        assert_eq!(direct.len(), 1, "only the DM's hit survives: {direct:?}");
        assert_eq!(direct[0].conversation_id, "D1");

        let group =
            facade.search(Some("acme"), "deploy", 20, Some("G1")).await.expect("the search runs");
        assert_eq!(group.len(), 1, "only the group DM's hit survives: {group:?}");
        assert_eq!(group[0].conversation_id, "G1");

        assert_eq!(
            api.search_reads(),
            ["deploy", "deploy"],
            "neither a DM nor a group DM adds an `in:`",
        );
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

        let worker_key = SessionSlot::worker("TestOrg", "forge", "tester");
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
                slot: worker_key.clone(),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead("TestOrg", "forge"),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                cwd: None,
                diagnostic: None,
                kick: None,
            },
        );
        facade
            .subscribe(&worker_key, Some("acme"), SlackSubscribeRequest::DirectMessages)
            .expect("the worker subscribes to the DM class");

        let lead_targets: Vec<SlackSubscriptionTarget> =
            facade.subscribed_targets(&lead, Some("acme")).into_iter().map(|s| s.target).collect();
        assert_eq!(
            lead_targets,
            vec![SlackSubscriptionTarget::Conversation {
                id: "C1".to_owned(),
                name: None,
                mode: SlackWatchMode::All,
            }],
            "the lead sees only its own record",
        );
        let worker_targets: Vec<SlackSubscriptionTarget> = facade
            .subscribed_targets(&worker_key, Some("acme"))
            .into_iter()
            .map(|s| s.target)
            .collect();
        assert_eq!(
            worker_targets,
            vec![SlackSubscriptionTarget::DirectMessages],
            "the worker sees only its own record, never the lead's",
        );
    }
}

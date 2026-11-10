//! `SlackFacade` - the seam between the Slack MCP tools and the subsystem.
//!
//! The production impl ([`ProdSlackFacade`]) resolves the caller's workspace
//! through its host's config binding and drives that workspace's client; the
//! mock returns preloaded results so the tool tests can assert argument
//! handling and error surfacing without a real subsystem.
//!
//! The facade owns no state. What it needs from its host arrives through
//! [`SlackFacadeHost`] - a caller's binding, project and subscription
//! identity, the parts that live in `forge.toml` and the worker registry -
//! and everything else is a [`Slack`] method.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::SystemTime;

use forge_primitives::SessionSlot;
use forge_primitives::slack::{
    SlackBookmark, SlackConversation, SlackDraft, SlackPerson, SlackPin, SlackSearchMatch,
    SlackSubscription, SlackSubscriptionTarget, SlackThreadName, SlackUser, SlackWatchMode,
};
use uuid::Uuid;

use crate::slack::{
    AuthTest, MENTION_CURSOR, Slack, SlackApi, SlackError, SlackHistoryMessage, SlackSubsystemHost,
    flatten_mrkdwn, mention_ids, one_line_excerpt, read_thread, shared_messages, thread_parent,
    user_display_name,
};

/// The caller's project identity, as the facade scopes records by it: the
/// project its slot names and, when it is a live worker, that worker's label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackCaller {
    pub project_name: String,
    pub worker_label: Option<String>,
}

/// What the facade needs from the workspace it is registered on.
///
/// Everything here is a resolution the workspace owns: `forge.toml`'s
/// bindings, the project catalogue, the worker registry and the durable/
/// ephemeral split of a subscription's owner. The Slack subsystem holds the
/// records themselves.
pub trait SlackFacadeHost: Send + Sync {
    /// The `[[slack]]` label this caller's project acts in: the project's
    /// binding, else its org's.
    fn slack_workspace(&self, caller: &SessionSlot) -> Option<String>;

    /// The caller's project and worker identity, when its slot names a
    /// project the config still declares.
    fn caller(&self, caller: &SessionSlot) -> Option<SlackCaller>;

    /// The owner a subscription is recorded under: project, team role, and
    /// whether that owner is durable. The same identity shape the gotify
    /// family records, because a connector subscription's owner is one
    /// concept.
    fn identity(&self, caller: &SessionSlot) -> Option<(String, Option<String>, bool)>;
}

/// Why resolving a tool's workspace failed. One shape for every Slack
/// tool, so the wording is the same whichever tool was called. The scope
/// is the caller's slot, so the cases are a file with no `[[slack]]`
/// section at all, a project with nothing bound to it, and a binding to a
/// workspace whose entry was added or renamed since boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackWorkspaceError {
    /// No `[[slack]]` entry in forge.toml.
    NotConfigured,
    /// Neither the project nor its org binds a `[[slack]]` label.
    Unbound { org: String, project: String },
    /// The project binds a label whose client is not loaded - a `[[slack]]`
    /// entry added or renamed since boot, whose token is captured only at
    /// start.
    BoundButNotLive { label: String },
}

const BINDING_KEYS: &str = "[orgs.slack] workspace = \"<label>\" to the org, or \
                            [orgs.projects.slack] workspace = \"<label>\" to the project";

/// The one wording for a refused workspace, so every Slack tool answers
/// this case the way `slack__list` does.
pub fn format_workspace_error(err: &SlackWorkspaceError) -> String {
    match err {
        SlackWorkspaceError::NotConfigured => {
            "no Slack workspace configured in forge.toml [[slack]]".to_owned()
        }
        SlackWorkspaceError::Unbound { org, project } => format!(
            "no Slack workspace is bound to {org}/{project}; add {BINDING_KEYS} in forge.toml"
        ),
        SlackWorkspaceError::BoundButNotLive { label } => format!(
            "the [[slack]] workspace '{label}' is bound but not live; a [[slack]] entry added \
             or renamed since boot needs a forge restart"
        ),
    }
}

/// Why `slack__subscribe` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackSubscribeError {
    /// The caller's project resolved no live workspace.
    Workspace(SlackWorkspaceError),
    /// The caller couldn't be mapped to a project.
    UnknownCallerProject,
}

/// Why `slack__unsubscribe` failed. The last two are the two cases a
/// refusal keeps apart: an id the project holds under another session
/// reads differently from one the project never had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackUnsubscribeError {
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
pub struct SlackPostRequest {
    pub conversation: String,
    /// `None` posts a root message; `Some(ts)` replies into that thread.
    pub thread_ts: Option<String>,
    pub text: String,
}

/// Why a post did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackPostError {
    /// The user rejected the draft. Nothing was posted.
    Rejected,
    /// The user sent it back with words to revise from. Nothing was posted.
    SentBack { note: String },
    /// Some parts of a split draft are live and the rest is not. Carries
    /// how many landed so a retry resumes instead of duplicating.
    Partial { posted: usize, total: usize, source: String },
    /// The caller's project resolved no live workspace.
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
pub struct SlackPostOutcome {
    /// The `[[slack]]` label the post went into, so the result names the
    /// workspace even though the call takes no argument for it.
    pub workspace: String,
    pub ts: Vec<String>,
    pub conversation_name: String,
    pub parts: usize,
}

/// One message to edit or delete. Both actions are gated like a post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackEditRequest {
    pub conversation: String,
    pub ts: String,
    /// `None` deletes the message; `Some(text)` replaces its body.
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackEditError {
    /// Slack refused the change because the message was not posted by the
    /// authenticated user. The wire's verdict, surfaced as its reason.
    NotOwnMessage,
    /// The user rejected the replacement. The message is untouched.
    Rejected,
    /// The user sent it back with words to revise from. The message is
    /// untouched.
    SentBack {
        note: String,
    },
    Workspace(SlackWorkspaceError),
    Fetch(String),
}

/// One reaction to add or remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackReactRequest {
    pub conversation: String,
    pub ts: String,
    /// Slack's shortcode, without colons, e.g. `white_check_mark`.
    pub name: String,
    pub add: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackReactError {
    /// The user rejected the reaction. Nothing was added or removed.
    Rejected,
    /// The user sent it back with words to revise from. Nothing was added or
    /// removed.
    SentBack {
        note: String,
    },
    Workspace(SlackWorkspaceError),
    Fetch(String),
}

/// Why a read-only lookup failed. Shared by every read tool because their
/// failure sets are identical; the per-operation error convention exists
/// where the sets differ (post, edit and react each fail differently), and
/// two names for one set would be duplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackReadError {
    Workspace(SlackWorkspaceError),
    Fetch(String),
}

/// What a caller asked to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackFetchRequest {
    pub file_id: String,
    /// The directory the file lands in. The name inside it comes from
    /// Slack and is sanitised before it is used.
    pub dir: std::path::PathBuf,
}

/// What a caller asked to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackUploadRequest {
    pub conversation: String,
    /// `None` posts a root message; `Some(ts)` replies into that thread.
    pub thread_ts: Option<String>,
    pub path: std::path::PathBuf,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackAttachmentError {
    /// The file could not be read from disk, or written to it.
    Io(String),
    /// The user rejected the upload. Nothing was sent.
    Rejected,
    /// The user sent it back with words to revise from. Nothing was sent.
    SentBack {
        note: String,
    },
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

/// How many search pages a conversation filter scans. A named conversation
/// is narrowed server-side and usually answers on the first page; an unnamed
/// DM is filtered locally, and five pages is where a sparse hit stops being
/// worth another call against the token's allowance.
const MAX_SEARCH_FILTER_PAGES: usize = 5;

/// What a caller asked to watch, before it becomes records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackSubscribeRequest {
    /// The whole DM class in this workspace.
    DirectMessages,
    /// Named conversations, each with its own mode.
    Conversations(Vec<SlackChannelWatch>),
    /// Being mentioned anywhere in the workspace.
    Mentions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackChannelWatch {
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

/// The workspace `caller`'s project acts in, and the client for it: the
/// project's binding, else its org's, both resolved at load.
fn resolve_in(
    slack: &Arc<Slack>,
    host: &Arc<dyn SlackFacadeHost>,
    caller: &SessionSlot,
) -> Result<(String, Arc<dyn SlackApi>), SlackWorkspaceError> {
    if !slack.is_configured() {
        return Err(SlackWorkspaceError::NotConfigured);
    }
    let unbound = || SlackWorkspaceError::Unbound {
        org: caller.org().to_owned(),
        project: caller.project().to_owned(),
    };
    let Some(label) = host.slack_workspace(caller) else {
        return Err(unbound());
    };
    let Some(api) = slack.api_client(&label) else {
        return Err(SlackWorkspaceError::BoundButNotLive { label });
    };
    Ok((label, api))
}

/// The Slack tools' view of the workspace.
#[async_trait::async_trait]
pub trait SlackFacade: Send + Sync {
    /// Every conversation the caller's workspace token is in, with the
    /// label it came from - the one resolution the tool's result names, so
    /// a reload between calls cannot report a different workspace than the
    /// data was read from.
    async fn conversations(
        &self,
        caller: &SessionSlot,
    ) -> Result<(String, Vec<SlackConversation>), SlackReadError>;

    /// The caller's OWN subscriptions in `label` - a lead's the lead's, a
    /// worker's its own, never another owner's. `slack__list` passes the
    /// label its read resolved, so the mark and the rows cannot disagree
    /// about which workspace they are in. After a restart these ids are
    /// the only handle a session has to unsubscribe with.
    fn subscribed_targets(&self, caller: &SessionSlot, label: &str) -> Vec<SlackSubscription>;

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
        caller: &SessionSlot,
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
        caller: &SessionSlot,
        query: &str,
        count: u32,
        conversation: Option<&str>,
    ) -> Result<(String, Vec<SlackSearchMatch>), SlackReadError>;

    /// One page of a conversation's messages. `oldest` and `newest` are
    /// exclusive bounds, so a caller pages backwards by feeding the earliest
    /// `ts` it saw back as `newest`.
    async fn history(
        &self,
        caller: &SessionSlot,
        conversation: &str,
        limit: u32,
        oldest: Option<&str>,
        newest: Option<&str>,
    ) -> Result<(String, Vec<SlackHistoryMessage>), SlackReadError>;

    /// Every reply in one thread, the parent first, up to `limit` rows.
    async fn replies(
        &self,
        caller: &SessionSlot,
        conversation: &str,
        ts: &str,
        limit: u32,
    ) -> Result<(String, Vec<SlackHistoryMessage>), SlackReadError>;

    /// `users.info` for one user id.
    async fn user(
        &self,
        caller: &SessionSlot,
        user: &str,
    ) -> Result<(String, SlackUser), SlackReadError>;

    /// Every member whose handle, real name or display name contains
    /// `query`, case-insensitively. Every match is returned: silently
    /// picking one of several is how a mention reaches the wrong person.
    /// Deactivated members are excluded.
    async fn users(
        &self,
        caller: &SessionSlot,
        query: &str,
    ) -> Result<(String, Vec<SlackUser>), SlackReadError>;

    /// `auth.test`: the resolved `[[slack]]` label and the identity the
    /// workspace's token acts as. The label is returned alongside because it
    /// can differ from the team's name (`Subspace` is team `Autonomys`).
    async fn me(&self, caller: &SessionSlot) -> Result<(String, AuthTest), SlackReadError>;

    /// `pins.list` for one conversation.
    async fn pins(
        &self,
        caller: &SessionSlot,
        conversation: &str,
    ) -> Result<(String, Vec<SlackPin>), SlackReadError>;

    /// `bookmarks.list` for one conversation.
    async fn bookmarks(
        &self,
        caller: &SessionSlot,
        conversation: &str,
    ) -> Result<(String, Vec<SlackBookmark>), SlackReadError>;

    /// Record what the caller wants to watch in its project's workspace,
    /// one record per target. Returns the records as they now stand, so a
    /// caller can name what it created.
    fn subscribe(
        &self,
        caller: &SessionSlot,
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

/// Production facade over the subsystem and its host, both weak to avoid a
/// cycle with the MCP server the workspace owns.
pub struct ProdSlackFacade {
    slack: Weak<Slack>,
    host: Weak<dyn SlackFacadeHost>,
}

impl ProdSlackFacade {
    pub fn new(slack: Weak<Slack>, host: Weak<dyn SlackFacadeHost>) -> Self {
        Self { slack, host }
    }
}

/// How a held draft ended: the reader's answer, or nothing at all when the
/// asking session went away mid-wait.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GateDecision {
    Approved,
    Rejected,
    /// The reader did not send it, and wrote words for the agent to revise
    /// from.
    SentBack {
        note: String,
    },
}

/// Resolves the draft if the awaiting future is dropped before the reader
/// answers - the calling session died mid-wait. Without this the registry
/// entry and its oneshot sender outlive everyone who could answer them.
struct ResolveOnDrop {
    slack: Arc<Slack>,
    id: Uuid,
    caller: SessionSlot,
}

impl Drop for ResolveOnDrop {
    fn drop(&mut self) {
        // The waiter is gone rather than answering: the drop path is a
        // session that died mid-wait, never a decision.
        self.slack.resolve_slack_draft(
            self.id,
            &self.caller,
            crate::slack::DraftResolution::Abandoned,
        );
    }
}

impl ProdSlackFacade {
    /// The subsystem and host a tool call acts on, with the refusal shape
    /// every Slack tool shares.
    fn parts(&self) -> Result<(Arc<Slack>, Arc<dyn SlackFacadeHost>), SlackWorkspaceError> {
        let slack = self.slack.upgrade().ok_or(SlackWorkspaceError::NotConfigured)?;
        let host = self.host.upgrade().ok_or(SlackWorkspaceError::NotConfigured)?;
        Ok((slack, host))
    }

    /// The workspace a tool call acts on, with the refusal shape every Slack
    /// tool shares.
    fn resolve(
        &self,
        caller: &SessionSlot,
    ) -> Result<(String, Arc<dyn SlackApi>), SlackWorkspaceError> {
        let (slack, host) = self.parts()?;
        resolve_in(&slack, &host, caller)
    }

    /// The conversation a request addresses, as `(id, display label)`.
    /// `conversation` may be an id, a channel name or a DM's partner, and
    /// the id is what the ledger and the in-flight mark key on - a post or
    /// an edit addressed one way lands in the same conversation the sweep
    /// reads. The label is what the approval prompt shows: a channel's name
    /// carries the `#` the card draws, a DM is named after its partner - the
    /// partner's id when Slack names none. A failed lookup is not a reason to
    /// refuse the action - but it IS why the prompt would show an id, so it
    /// says so.
    pub async fn resolve_conversation(
        api: &Arc<dyn SlackApi>,
        workspace: &str,
        conversation: &str,
    ) -> (String, String) {
        // A caller may echo the card's own `#channel` label back at a tool.
        // Slack's directory keys on the bare name, so the `#` is not part of
        // the match - a channel has one spelling across every tool.
        let wanted = conversation.strip_prefix('#').unwrap_or(conversation);
        let conversations = match api.list_conversations().await {
            Ok(conversations) => conversations,
            Err(error) => {
                tracing::warn!(
                    target: "forge_tools::slack",
                    event_name = "slack_conversation_list_failed",
                    workspace,
                    conversation,
                    %error,
                    "listing conversations failed; the approval prompt names the id instead",
                );
                return (wanted.to_owned(), conversation.to_owned());
            }
        };
        match conversations.iter().find(|known| {
            known.id == wanted
                || known.name.as_deref() == Some(wanted)
                || known.user.as_deref() == Some(wanted)
        }) {
            Some(known) => {
                let label = match known.name.as_deref().filter(|name| !name.is_empty()) {
                    // A group DM's generated name is Slack's own label, not a
                    // channel's, so only a channel carries the `#`.
                    Some(name) if !known.is_im && !known.is_mpim => format!("#{name}"),
                    Some(name) => name.to_owned(),
                    None => known.user.clone().unwrap_or_else(|| known.id.clone()),
                };
                (known.id.clone(), label)
            }
            None => (wanted.to_owned(), conversation.to_owned()),
        }
    }

    /// How much of a thread's parent the card's line quotes.
    pub const THREAD_EXCERPT_CHARS: usize = 80;

    /// Name the draft's thread and the people its text tags, at the moment
    /// it is parked. Never fails the park: whatever resolves rides the draft,
    /// and what does not stays unset.
    async fn name_draft(
        slack: &Arc<Slack>,
        api: &Arc<dyn SlackApi>,
        label: &str,
        conversation: &str,
        draft: &mut SlackDraft,
    ) {
        let host = Arc::new(SlackSubsystemHost::new(slack));
        // A tag lookup is a Slack call, so they run beside the thread read
        // rather than one after another in front of the park. The parent's
        // own mentions join the same set once it lands, so one lookup names
        // a person whether the draft tags them or the quoted parent does.
        let mut lookups = Vec::new();
        let mut looked_up: Vec<String> = Vec::new();
        for id in mention_ids(&draft.text) {
            lookups.push((id.clone(), Self::look_up_user(&host, api, label, id.clone())));
            looked_up.push(id);
        }
        let mut parent = None;
        if let Some(ts) = draft.thread_ts.clone() {
            parent = match thread_parent(api.as_ref(), conversation, &ts).await {
                // A read anchored at a message inside a thread answers the
                // thread's parent instead. Naming it would label an edit or
                // a delete with the wrong message, so only an exact ts is
                // the message the card is about; anything else leaves the
                // name unset.
                Ok(Some(parent)) if parent.ts == ts => Some(parent),
                Ok(_) => None,
                Err(error) => {
                    // A missing thread name is the only other symptom, so a
                    // persistent failure must not live at debug, where no
                    // shipped directive reaches it.
                    let first = slack.first_thread_failure(label, conversation, &ts);
                    if first {
                        tracing::warn!(
                            target: "forge_tools::slack",
                            event_name = "slack_draft_thread_unreadable",
                            workspace = label,
                            conversation,
                            parent = %ts,
                            %error,
                            "reading a held draft's thread parent failed",
                        );
                    } else {
                        tracing::debug!(
                            target: "forge_tools::slack",
                            event_name = "slack_draft_thread_unreadable",
                            workspace = label,
                            conversation,
                            parent = %ts,
                            %error,
                            "reading a held draft's thread parent failed again",
                        );
                    }
                    None
                }
            };
        }
        if let Some(parent) = &parent {
            for id in mention_ids(&parent.text) {
                if !looked_up.contains(&id) {
                    lookups.push((id.clone(), Self::look_up_user(&host, api, label, id.clone())));
                    looked_up.push(id);
                }
            }
            // The author's name joins the same batch, so naming the thread
            // does not cost a Slack call of its own. A bot already names
            // itself in the payload, so only a user author is looked up.
            let bot_named = parent.bot_name.as_deref().is_some_and(|name| !name.is_empty());
            if !bot_named
                && let Some(user) = parent.user.as_deref().filter(|user| !user.is_empty())
                && !looked_up.iter().any(|seen| seen.as_str() == user)
            {
                lookups.push((
                    user.to_owned(),
                    Self::look_up_user(&host, api, label, user.to_owned()),
                ));
                looked_up.push(user.to_owned());
            }
        }
        let mut names = HashMap::new();
        for (id, lookup) in lookups {
            if let Some(name) = lookup.await.unwrap_or(None) {
                names.insert(id, name);
            }
        }
        draft.tagged = mention_ids(&draft.text)
            .into_iter()
            .map(|id| SlackPerson { name: names.get(&id).cloned(), id })
            .collect();
        draft.thread = parent.as_ref().and_then(|parent| Self::thread_name(parent, &names));
    }

    /// One user-name lookup on its own task, so a batch of them overlaps the
    /// thread read in front of the park.
    fn look_up_user(
        host: &Arc<SlackSubsystemHost>,
        api: &Arc<dyn SlackApi>,
        label: &str,
        user: String,
    ) -> tokio::task::JoinHandle<Option<String>> {
        let host = Arc::clone(host);
        let api = Arc::clone(api);
        let workspace = label.to_owned();
        tokio::spawn(async move {
            user_display_name(host.as_ref(), api.as_ref(), &workspace, &user).await
        })
    }

    /// The thread name drawn for `parent`: its author and one line of its
    /// text flattened to prose. `None` when the parent names no author at all.
    fn thread_name(
        parent: &SlackHistoryMessage,
        names: &HashMap<String, String>,
    ) -> Option<SlackThreadName> {
        let author = if let Some(name) = parent.bot_name.as_deref().filter(|name| !name.is_empty())
        {
            // A bot names itself in the payload; that is its display name.
            name.to_owned()
        } else {
            let user = parent.user.as_deref().filter(|user| !user.is_empty())?;
            names.get(user).cloned().unwrap_or_else(|| user.to_owned())
        };
        Some(SlackThreadName {
            author,
            excerpt: one_line_excerpt(
                &flatten_mrkdwn(&parent.text, names, Self::THREAD_EXCERPT_CHARS),
                Self::THREAD_EXCERPT_CHARS,
            ),
        })
    }

    /// Register a draft for the user's decision and wait for it. The one
    /// place "did the user approve this" is decided, so the fail-closed
    /// path exists once rather than at each gated operation - posting,
    /// replacing a body and deleting one all come through here.
    ///
    /// **No timeout rides the wait.** A held post is an ask like a question,
    /// a permission or a hand-off: it parks until the reader answers or the
    /// asking session goes away, and the dock's own "don't send" is the way
    /// out. A clock here would refuse a draft the user was about to approve.
    ///
    /// The views hear `SlackDraftResolved` from the answering dispatch: the
    /// draft leaving the registry is the whole of what a view does with it,
    /// and every path that removes it emits that one signal.
    async fn await_approval(
        slack: &Arc<Slack>,
        caller: &SessionSlot,
        draft: SlackDraft,
    ) -> GateDecision {
        let (id, decision) = slack.register_slack_draft(caller, draft);
        let guard = ResolveOnDrop { slack: Arc::clone(slack), id, caller: caller.clone() };
        let resolved = match decision.await {
            Ok(forge_primitives::slack::SlackDraftAnswer::Approve) => GateDecision::Approved,
            Ok(forge_primitives::slack::SlackDraftAnswer::SendBack { note }) => {
                GateDecision::SentBack { note }
            }
            // A dead waiter is the abandoned path, and it fails closed on
            // the same side a decline does.
            Ok(forge_primitives::slack::SlackDraftAnswer::Decline) | Err(_) => {
                GateDecision::Rejected
            }
        };
        drop(guard);
        resolved
    }
}

#[async_trait::async_trait]
impl SlackFacade for ProdSlackFacade {
    async fn conversations(
        &self,
        caller: &SessionSlot,
    ) -> Result<(String, Vec<SlackConversation>), SlackReadError> {
        let (label, api) = self.resolve(caller).map_err(SlackReadError::Workspace)?;
        let conversations =
            api.list_conversations().await.map_err(|err| SlackReadError::Fetch(err.to_string()))?;
        Ok((label, conversations))
    }

    async fn post(
        &self,
        caller: &SessionSlot,
        request: SlackPostRequest,
    ) -> Result<SlackPostOutcome, SlackPostError> {
        let (slack, host) = self.parts().map_err(SlackPostError::Workspace)?;
        let (label, api) = resolve_in(&slack, &host, caller).map_err(SlackPostError::Workspace)?;
        let (conversation, conversation_label) =
            Self::resolve_conversation(&api, &label, &request.conversation).await;
        let mut draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label,
            conversation: request.conversation,
            conversation_label,
            thread_ts: request.thread_ts,
            text: request.text,
            operation: forge_primitives::slack::SlackDraftOperation::Post,
            thread: None,
            tagged: Vec::new(),
        };
        let workspace = draft.workspace.clone();
        Self::name_draft(&slack, &api, &workspace, &conversation, &mut draft).await;
        match Self::await_approval(&slack, caller, draft.clone()).await {
            GateDecision::Approved => {}
            GateDecision::Rejected => return Err(SlackPostError::Rejected),
            GateDecision::SentBack { note } => return Err(SlackPostError::SentBack { note }),
        }
        // The agent-facing result names the channel the bare way every other
        // tool does; the `#` is the card's spelling, not the tool's.
        let conversation_name = draft
            .conversation_label
            .strip_prefix('#')
            .unwrap_or(&draft.conversation_label)
            .to_owned();
        let parts = crate::slack::split_for_post(&draft.text);
        let mut ts: Vec<String> = Vec::new();
        for part in &parts {
            // One guard per call, covering the call and its ledger write:
            // each part has its own request budget, so a stamp taken once
            // for the whole operation would expire between parts on a slow
            // but healthy Slack.
            let _in_flight = slack.begin_slack_post(&draft.workspace, &conversation);
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
                            target: "forge_tools::slack",
                            event_name = "slack_post_channel_omitted",
                            workspace = %draft.workspace,
                            conversation = %draft.conversation,
                            "chat.postMessage answered no channel; the ledger keys on the resolved conversation",
                        );
                        &conversation
                    };
                    slack.record_slack_own_post(&draft.workspace, landed, &posted.ts);
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
        Ok(SlackPostOutcome {
            workspace: draft.workspace.clone(),
            ts,
            conversation_name,
            parts: parts.len(),
        })
    }

    async fn edit(
        &self,
        caller: &SessionSlot,
        request: SlackEditRequest,
    ) -> Result<(), SlackEditError> {
        let (slack, host) = self.parts().map_err(SlackEditError::Workspace)?;
        let (label, api) = resolve_in(&slack, &host, caller).map_err(SlackEditError::Workspace)?;

        // A replacement puts new words in front of people as the user, and
        // a deletion changes what they see on a message attributed to him -
        // and unlike a bad edit it cannot be fixed by editing again. Both
        // go through the same gate a post does.
        let (conversation, conversation_label) =
            Self::resolve_conversation(&api, &label, &request.conversation).await;
        let mut draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label.clone(),
            conversation: request.conversation.clone(),
            conversation_label,
            thread_ts: Some(request.ts.clone()),
            text: match &request.text {
                Some(text) => text.clone(),
                None => format!("[delete] message {}", request.ts),
            },
            operation: match &request.text {
                Some(_) => forge_primitives::slack::SlackDraftOperation::Edit,
                None => forge_primitives::slack::SlackDraftOperation::Delete,
            },
            thread: None,
            tagged: Vec::new(),
        };
        let workspace = draft.workspace.clone();
        Self::name_draft(&slack, &api, &workspace, &conversation, &mut draft).await;
        match Self::await_approval(&slack, caller, draft).await {
            GateDecision::Approved => {}
            GateDecision::Rejected => return Err(SlackEditError::Rejected),
            GateDecision::SentBack { note } => return Err(SlackEditError::SentBack { note }),
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
                slack.record_slack_own_post(&label, &conversation, &request.ts);
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
        let (slack, host) = self.parts().map_err(SlackReactError::Workspace)?;
        let (label, api) = resolve_in(&slack, &host, caller).map_err(SlackReactError::Workspace)?;

        // A reaction is authored content in the user's name, so it goes
        // through the same gate a post does.
        let verb = if request.add { "react" } else { "unreact" };
        let (conversation, conversation_label) =
            Self::resolve_conversation(&api, &label, &request.conversation).await;
        let mut draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label,
            conversation: request.conversation.clone(),
            conversation_label,
            thread_ts: Some(request.ts.clone()),
            text: format!("[{verb}: {}]", request.name),
            operation: if request.add {
                forge_primitives::slack::SlackDraftOperation::React
            } else {
                forge_primitives::slack::SlackDraftOperation::Unreact
            },
            thread: None,
            tagged: Vec::new(),
        };
        let workspace = draft.workspace.clone();
        Self::name_draft(&slack, &api, &workspace, &conversation, &mut draft).await;
        match Self::await_approval(&slack, caller, draft).await {
            GateDecision::Approved => {}
            GateDecision::Rejected => return Err(SlackReactError::Rejected),
            GateDecision::SentBack { note } => return Err(SlackReactError::SentBack { note }),
        }

        api.set_reaction(&request.conversation, &request.ts, &request.name, request.add)
            .await
            .map_err(|err| SlackReactError::Fetch(err.to_string()))
    }

    async fn search(
        &self,
        caller: &SessionSlot,
        query: &str,
        count: u32,
        conversation: Option<&str>,
    ) -> Result<(String, Vec<SlackSearchMatch>), SlackReadError> {
        let (label, api) = self.resolve(caller).map_err(SlackReadError::Workspace)?;
        let Some(conversation) = conversation else {
            let matches = api
                .search_messages(query, count, None)
                .await
                .map(|page| page.matches)
                .map_err(|err| SlackReadError::Fetch(err.to_string()))?;
            return Ok((label, matches));
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
        Ok((label, matches))
    }

    async fn history(
        &self,
        caller: &SessionSlot,
        conversation: &str,
        limit: u32,
        oldest: Option<&str>,
        newest: Option<&str>,
    ) -> Result<(String, Vec<SlackHistoryMessage>), SlackReadError> {
        let (label, api) = self.resolve(caller).map_err(SlackReadError::Workspace)?;
        let messages = api
            .history(conversation, oldest, newest, limit, None)
            .await
            .map(|page| page.messages)
            .map_err(|err| SlackReadError::Fetch(err.to_string()))?;
        Ok((label, messages))
    }

    async fn replies(
        &self,
        caller: &SessionSlot,
        conversation: &str,
        ts: &str,
        limit: u32,
    ) -> Result<(String, Vec<SlackHistoryMessage>), SlackReadError> {
        let (label, api) = self.resolve(caller).map_err(SlackReadError::Workspace)?;
        let messages = read_thread(api.as_ref(), conversation, ts, limit)
            .await
            .map_err(|err| SlackReadError::Fetch(err.to_string()))?;
        Ok((label, messages))
    }

    async fn user(
        &self,
        caller: &SessionSlot,
        user: &str,
    ) -> Result<(String, SlackUser), SlackReadError> {
        let (label, api) = self.resolve(caller).map_err(SlackReadError::Workspace)?;
        let user =
            api.user_info(user).await.map_err(|err| SlackReadError::Fetch(err.to_string()))?;
        Ok((label, user))
    }

    async fn users(
        &self,
        caller: &SessionSlot,
        query: &str,
    ) -> Result<(String, Vec<SlackUser>), SlackReadError> {
        let (label, api) = self.resolve(caller).map_err(SlackReadError::Workspace)?;
        let needle = query.trim().to_lowercase();
        // An empty needle would match the whole directory; the tool refuses
        // it, and a direct facade call gets nothing rather than everyone.
        if needle.is_empty() {
            return Ok((label, Vec::new()));
        }
        let mut found: Vec<SlackUser> = api
            .list_users()
            .await
            .map_err(|err| SlackReadError::Fetch(err.to_string()))?
            .into_iter()
            .filter(|user| !user.deleted && user_matches(user, &needle))
            .collect();
        found.sort_by(|left, right| left.name.cmp(&right.name));
        Ok((label, found))
    }

    async fn me(&self, caller: &SessionSlot) -> Result<(String, AuthTest), SlackReadError> {
        let (label, api) = self.resolve(caller).map_err(SlackReadError::Workspace)?;
        let auth = api.auth_test().await.map_err(|err| SlackReadError::Fetch(err.to_string()))?;
        Ok((label, auth))
    }

    async fn pins(
        &self,
        caller: &SessionSlot,
        conversation: &str,
    ) -> Result<(String, Vec<SlackPin>), SlackReadError> {
        let (label, api) = self.resolve(caller).map_err(SlackReadError::Workspace)?;
        let pins =
            api.pins(conversation).await.map_err(|err| SlackReadError::Fetch(err.to_string()))?;
        Ok((label, pins))
    }

    async fn bookmarks(
        &self,
        caller: &SessionSlot,
        conversation: &str,
    ) -> Result<(String, Vec<SlackBookmark>), SlackReadError> {
        let (label, api) = self.resolve(caller).map_err(SlackReadError::Workspace)?;
        let bookmarks = api
            .bookmarks(conversation)
            .await
            .map_err(|err| SlackReadError::Fetch(err.to_string()))?;
        Ok((label, bookmarks))
    }

    async fn fetch_attachment(
        &self,
        caller: &SessionSlot,
        request: SlackFetchRequest,
    ) -> Result<std::path::PathBuf, SlackAttachmentError> {
        let (_, api) = self.resolve(caller).map_err(SlackAttachmentError::Workspace)?;

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
        let (slack, host) = self.parts().map_err(SlackAttachmentError::Workspace)?;
        let (label, api) =
            resolve_in(&slack, &host, caller).map_err(SlackAttachmentError::Workspace)?;

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
        let mut draft = SlackDraft {
            id: Uuid::new_v4(),
            workspace: label.clone(),
            conversation: request.conversation.clone(),
            conversation_label,
            thread_ts: request.thread_ts.clone(),
            text: format!("[file] {name}\nfrom {}", request.path.display()),
            operation: forge_primitives::slack::SlackDraftOperation::Upload,
            thread: None,
            tagged: Vec::new(),
        };
        let workspace = draft.workspace.clone();
        Self::name_draft(&slack, &api, &workspace, &conversation, &mut draft).await;
        match Self::await_approval(&slack, caller, draft).await {
            GateDecision::Approved => {}
            GateDecision::Rejected => return Err(SlackAttachmentError::Rejected),
            GateDecision::SentBack { note } => {
                return Err(SlackAttachmentError::SentBack { note });
            }
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
        let upload_call = slack.begin_slack_post(&label, &conversation);
        let uploaded = api
            .complete_upload(&file_id, &request.conversation, request.thread_ts.as_deref())
            .await
            .map_err(|err| SlackAttachmentError::Fetch(err.to_string()))?;
        let _shares = slack.begin_slack_post(&label, &conversation);
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
                    target: "forge_tools::slack",
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
                target: "forge_tools::slack",
                event_name = "slack_attachment_ledger_missing",
                workspace = %label,
                conversation = %request.conversation,
                "the upload's shares name no message; the attachment stays unledgered and can come back once",
            );
        }
        for (conversation, ts) in shared {
            slack.record_slack_own_post(&label, &conversation, &ts);
        }
        Ok(())
    }

    fn subscribed_targets(&self, caller: &SessionSlot, label: &str) -> Vec<SlackSubscription> {
        let Ok((slack, host)) = self.parts() else { return Vec::new() };
        let Some(cx) = host.caller(caller) else { return Vec::new() };
        slack
            .slack_subscriptions_for_project(&cx.project_name)
            .into_iter()
            .filter(|sub| sub.workspace == label && sub.team_role == cx.worker_label)
            .collect()
    }

    fn subscribe(
        &self,
        caller: &SessionSlot,
        request: SlackSubscribeRequest,
    ) -> Result<Vec<SlackSubscription>, SlackSubscribeError> {
        let (slack, host) = self.parts().map_err(SlackSubscribeError::Workspace)?;
        let (label, _) =
            resolve_in(&slack, &host, caller).map_err(SlackSubscribeError::Workspace)?;
        let (project, team_role, durable) =
            host.identity(caller).ok_or(SlackSubscribeError::UnknownCallerProject)?;
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
                if !slack.slack_has_cursor(&label, id) {
                    slack.set_slack_watermark(&label, id, &slack_ts_now());
                }
                created.push(slack.watch_slack_conversation(
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
                && !slack.slack_has_cursor(&label, id)
            {
                slack.set_slack_watermark(&label, id, &slack_ts_now());
            }
            slack.add_slack_subscription(sub.clone(), durable);
            created.push(sub);
        }
        if mentions_requested && !slack.slack_has_cursor(&label, MENTION_CURSOR) {
            slack.set_slack_watermark(&label, MENTION_CURSOR, &slack_ts_now());
        }
        // A workspace that just gained its first subscription needs a pump.
        slack.start_slack_subsystem();
        Ok(created)
    }

    fn unsubscribe(
        &self,
        caller: &SessionSlot,
        id: Uuid,
    ) -> Result<SlackSubscription, SlackUnsubscribeError> {
        let (slack, host) =
            self.parts().map_err(|_| SlackUnsubscribeError::UnknownCallerProject)?;
        let cx = host.caller(caller).ok_or(SlackUnsubscribeError::UnknownCallerProject)?;
        let removed = slack.remove_slack_subscription_owned_by(
            &cx.project_name,
            id,
            cx.worker_label.as_deref(),
        );
        slack.stop_slack_subsystem_if_idle();
        if let Some(removed) = removed {
            return Ok(removed);
        }
        // The owner-scoped removal matched nothing, which is either an id
        // the project never had or one another session owns; the caller is
        // told which.
        if slack.slack_subscriptions_for_project(&cx.project_name).iter().any(|s| s.id == id) {
            Err(SlackUnsubscribeError::NotOwnedByCaller)
        } else {
            Err(SlackUnsubscribeError::NoSuchSubscription)
        }
    }
}

/// One recorded search: workspace, query, cap, conversation filter.
#[cfg(any(test, feature = "testing"))]
pub type SearchCall = (SessionSlot, String, u32, Option<String>);

/// One recorded history read: workspace, conversation, limit, oldest, newest.
#[cfg(any(test, feature = "testing"))]
pub type HistoryCall = (SessionSlot, String, u32, Option<String>, Option<String>);

/// One recorded thread read: workspace, conversation, parent ts, limit.
#[cfg(any(test, feature = "testing"))]
pub type RepliesCall = (SessionSlot, String, String, u32);

/// Records calls + returns preloaded results so the tool tests can assert
/// the tool parses args, resolves the caller, and surfaces results/errors.
#[cfg(any(test, feature = "testing"))]
#[derive(Default)]
pub struct MockSlackFacade {
    pub conversations_calls: parking_lot::Mutex<Vec<SessionSlot>>,
    pub conversations_result:
        parking_lot::Mutex<Option<Result<Vec<SlackConversation>, SlackReadError>>>,
    pub subscribed_targets_calls: parking_lot::Mutex<Vec<(SessionSlot, String)>>,
    pub subscribed_targets: parking_lot::Mutex<Vec<SlackSubscription>>,
    pub search_calls: parking_lot::Mutex<Vec<SearchCall>>,
    pub search_result: parking_lot::Mutex<Option<Result<Vec<SlackSearchMatch>, SlackReadError>>>,
    pub history_calls: parking_lot::Mutex<Vec<HistoryCall>>,
    pub history_result:
        parking_lot::Mutex<Option<Result<Vec<SlackHistoryMessage>, SlackReadError>>>,
    pub replies_calls: parking_lot::Mutex<Vec<RepliesCall>>,
    pub replies_result:
        parking_lot::Mutex<Option<Result<Vec<SlackHistoryMessage>, SlackReadError>>>,
    pub user_calls: parking_lot::Mutex<Vec<(SessionSlot, String)>>,
    pub user_result: parking_lot::Mutex<Option<Result<SlackUser, SlackReadError>>>,
    pub users_calls: parking_lot::Mutex<Vec<(SessionSlot, String)>>,
    pub users_result: parking_lot::Mutex<Option<Result<Vec<SlackUser>, SlackReadError>>>,
    pub me_calls: parking_lot::Mutex<Vec<SessionSlot>>,
    pub me_result: parking_lot::Mutex<Option<Result<(String, AuthTest), SlackReadError>>>,
    pub pins_calls: parking_lot::Mutex<Vec<(SessionSlot, String)>>,
    pub pins_result: parking_lot::Mutex<Option<Result<Vec<SlackPin>, SlackReadError>>>,
    pub bookmarks_calls: parking_lot::Mutex<Vec<(SessionSlot, String)>>,
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
    pub subscribe_calls: parking_lot::Mutex<Vec<(SessionSlot, SlackSubscribeRequest)>>,
    pub subscribe_result:
        parking_lot::Mutex<Option<Result<Vec<SlackSubscription>, SlackSubscribeError>>>,
    pub unsubscribe_calls: parking_lot::Mutex<Vec<Uuid>>,
    pub unsubscribe_result:
        parking_lot::Mutex<Option<Result<SlackSubscription, SlackUnsubscribeError>>>,
}

#[cfg(any(test, feature = "testing"))]
impl MockSlackFacade {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn into_arc(self) -> Arc<dyn SlackFacade> {
        Arc::new(self)
    }
}

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl SlackFacade for MockSlackFacade {
    async fn conversations(
        &self,
        caller: &SessionSlot,
    ) -> Result<(String, Vec<SlackConversation>), SlackReadError> {
        self.conversations_calls.lock().push(caller.clone());
        self.conversations_result
            .lock()
            .clone()
            .unwrap_or_else(|| Ok(Vec::new()))
            .map(|rows| ("acme".to_owned(), rows))
    }

    async fn search(
        &self,
        caller: &SessionSlot,
        query: &str,
        count: u32,
        conversation: Option<&str>,
    ) -> Result<(String, Vec<SlackSearchMatch>), SlackReadError> {
        self.search_calls.lock().push((
            caller.clone(),
            query.to_owned(),
            count,
            conversation.map(str::to_owned),
        ));
        self.search_result
            .lock()
            .clone()
            .unwrap_or_else(|| Ok(Vec::new()))
            .map(|rows| ("acme".to_owned(), rows))
    }

    async fn history(
        &self,
        caller: &SessionSlot,
        conversation: &str,
        limit: u32,
        oldest: Option<&str>,
        newest: Option<&str>,
    ) -> Result<(String, Vec<SlackHistoryMessage>), SlackReadError> {
        self.history_calls.lock().push((
            caller.clone(),
            conversation.to_owned(),
            limit,
            oldest.map(str::to_owned),
            newest.map(str::to_owned),
        ));
        self.history_result
            .lock()
            .clone()
            .unwrap_or_else(|| Ok(Vec::new()))
            .map(|rows| ("acme".to_owned(), rows))
    }

    async fn replies(
        &self,
        caller: &SessionSlot,
        conversation: &str,
        ts: &str,
        limit: u32,
    ) -> Result<(String, Vec<SlackHistoryMessage>), SlackReadError> {
        self.replies_calls.lock().push((
            caller.clone(),
            conversation.to_owned(),
            ts.to_owned(),
            limit,
        ));
        self.replies_result
            .lock()
            .clone()
            .unwrap_or_else(|| Ok(Vec::new()))
            .map(|rows| ("acme".to_owned(), rows))
    }

    async fn user(
        &self,
        caller: &SessionSlot,
        user: &str,
    ) -> Result<(String, SlackUser), SlackReadError> {
        self.user_calls.lock().push((caller.clone(), user.to_owned()));
        self.user_result
            .lock()
            .clone()
            .unwrap_or_else(|| {
                Ok(SlackUser {
                    id: user.to_owned(),
                    name: "tester".to_owned(),
                    real_name: None,
                    tz: None,
                    deleted: false,
                    profile: None,
                })
            })
            .map(|user| ("acme".to_owned(), user))
    }

    async fn users(
        &self,
        caller: &SessionSlot,
        query: &str,
    ) -> Result<(String, Vec<SlackUser>), SlackReadError> {
        self.users_calls.lock().push((caller.clone(), query.to_owned()));
        self.users_result
            .lock()
            .clone()
            .unwrap_or_else(|| Ok(Vec::new()))
            .map(|rows| ("acme".to_owned(), rows))
    }

    async fn me(&self, caller: &SessionSlot) -> Result<(String, AuthTest), SlackReadError> {
        self.me_calls.lock().push(caller.clone());
        self.me_result.lock().clone().unwrap_or_else(|| {
            Ok((
                "acme".to_owned(),
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
        caller: &SessionSlot,
        conversation: &str,
    ) -> Result<(String, Vec<SlackPin>), SlackReadError> {
        self.pins_calls.lock().push((caller.clone(), conversation.to_owned()));
        self.pins_result
            .lock()
            .clone()
            .unwrap_or_else(|| Ok(Vec::new()))
            .map(|rows| ("acme".to_owned(), rows))
    }

    async fn bookmarks(
        &self,
        caller: &SessionSlot,
        conversation: &str,
    ) -> Result<(String, Vec<SlackBookmark>), SlackReadError> {
        self.bookmarks_calls.lock().push((caller.clone(), conversation.to_owned()));
        self.bookmarks_result
            .lock()
            .clone()
            .unwrap_or_else(|| Ok(Vec::new()))
            .map(|rows| ("acme".to_owned(), rows))
    }

    async fn fetch_attachment(
        &self,
        _caller: &SessionSlot,
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
            workspace: "acme".to_owned(),
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

    fn subscribed_targets(&self, caller: &SessionSlot, label: &str) -> Vec<SlackSubscription> {
        self.subscribed_targets_calls.lock().push((caller.clone(), label.to_owned()));
        self.subscribed_targets.lock().clone()
    }

    fn subscribe(
        &self,
        caller: &SessionSlot,
        request: SlackSubscribeRequest,
    ) -> Result<Vec<SlackSubscription>, SlackSubscribeError> {
        self.subscribe_calls.lock().push((caller.clone(), request));
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
}

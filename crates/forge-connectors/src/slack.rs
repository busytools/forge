//! The Slack Web API client: one uniform POST per method, form params,
//! Bearer token, and the envelope rules that turn a logical failure into
//! an error.
//!
//! Slack answers `{"ok": false, "error": "..."}` with HTTP 200 for a
//! logical failure, so a 200 is not success. Only a 429 arrives as a
//! status, and it carries `Retry-After`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;

use forge_primitives::slack::{
    SlackConversation, SlackFile, SlackMessage, SlackSearchMatch, SlackSubscription,
    SlackSubscriptionTarget, SlackUser, SlackWatchMode,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// What the pump needs from the workspace. The connector holds no state
/// of its own, so the subscription set, the watermarks and the liveness
/// flag all live behind this.
pub trait SlackHost: Send + Sync {
    /// A ready client for this workspace, or an error when the workspace is
    /// not configured. The host builds it, so the token stays with the
    /// workspace and the connector never asks for the credential itself.
    fn client(&self, workspace: &str, timeout: Duration) -> Result<SlackClient, String>;

    /// The authenticated user's id for a workspace, resolved at boot.
    /// Needed to recognise `<@U...>` mentions.
    fn user_id(&self, workspace: &str) -> Option<String>;

    /// The subscriptions scoped to one workspace.
    fn subscriptions(&self, workspace: &str) -> Vec<SlackSubscription>;

    /// The last timestamp delivered for a conversation, or `None` when it
    /// has never been swept.
    fn watermark(&self, workspace: &str, conversation: &str) -> Option<String>;

    /// Record the last timestamp delivered. A `ts` is a string with a
    /// microsecond fraction; never round-trip it through a number.
    fn set_watermark(&self, workspace: &str, conversation: &str, ts: &str);

    /// Liveness for the Inspector's status line.
    fn set_connected(&self, workspace: &str, connected: bool);

    /// Hand one matched message to its subscriber's session.
    fn deliver(&self, subscription: &SlackSubscription, message: &SlackMessage);
}

/// The Web API calls this connector makes, behind a trait so a sweep or
/// an outbound call can be driven from a test double instead of a live
/// workspace.
///
/// Public because forge-workspace holds one and cannot store a trait it
/// cannot name, the same reason [`SlackHost`] is public. It abstracts
/// this one connector's own surface; it is not a trait shared between
/// connectors.
#[async_trait::async_trait]
pub trait SlackApi: Send + Sync {
    /// Who the token belongs to; the boot probe's only call.
    async fn auth_test(&self) -> Result<AuthTest, SlackError>;
    async fn list_conversations(&self) -> Result<Vec<SlackConversation>, SlackError>;
    async fn history(
        &self,
        channel: &str,
        oldest: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError>;
    async fn replies(
        &self,
        channel: &str,
        ts: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError>;
    /// Post one message, as a root or into an existing thread.
    async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), SlackError>;
    /// Replace the text of one of the authenticated user's own messages.
    async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<(), SlackError>;
    /// Delete one of the authenticated user's own messages.
    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), SlackError>;
    /// Add or remove one reaction on a message.
    async fn set_reaction(
        &self,
        channel: &str,
        ts: &str,
        name: &str,
        add: bool,
    ) -> Result<(), SlackError>;
    /// The author of one message, for the own-message check before an
    /// edit or a delete. `None` when the page did not carry it.
    async fn message_author(&self, channel: &str, ts: &str) -> Result<Option<String>, SlackError>;
    /// GET an absolute URL with the bearer attached, for a private file.
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, SlackError>;
    /// POST raw bytes to an absolute pre-signed URL, with no bearer.
    async fn post_bytes(&self, url: &str, body: Vec<u8>) -> Result<(), SlackError>;
    /// `files.info` for one file id.
    async fn file_info(&self, id: &str) -> Result<SlackFile, SlackError>;
    /// `files.getUploadURLExternal`: the pre-signed URL to POST the bytes
    /// to, plus the file id the completion call needs.
    async fn get_upload_url(
        &self,
        name: &str,
        length: usize,
    ) -> Result<(String, String), SlackError>;
    /// `files.completeUploadExternal`, which can be called once only.
    async fn complete_upload(
        &self,
        file_id: &str,
        channel: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), SlackError>;
    /// `search.messages`, always timestamp-ordered.
    async fn search_messages(
        &self,
        query: &str,
        count: u32,
    ) -> Result<Vec<SlackSearchMatch>, SlackError>;
    /// `users.info` for one user id.
    async fn user_info(&self, user: &str) -> Result<SlackUser, SlackError>;
}

#[async_trait::async_trait]
impl SlackApi for SlackClient {
    async fn auth_test(&self) -> Result<AuthTest, SlackError> {
        SlackClient::auth_test(self).await
    }

    async fn list_conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
        SlackClient::list_conversations(self).await
    }

    async fn history(
        &self,
        channel: &str,
        oldest: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError> {
        SlackClient::history(self, channel, oldest, limit, cursor).await
    }

    async fn replies(
        &self,
        channel: &str,
        ts: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError> {
        SlackClient::replies(self, channel, ts, limit, cursor).await
    }

    async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), SlackError> {
        SlackClient::post_message(self, channel, text, thread_ts).await
    }

    async fn update_message(&self, channel: &str, ts: &str, text: &str) -> Result<(), SlackError> {
        SlackClient::update_message(self, channel, ts, text).await
    }

    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), SlackError> {
        SlackClient::delete_message(self, channel, ts).await
    }

    async fn set_reaction(
        &self,
        channel: &str,
        ts: &str,
        name: &str,
        add: bool,
    ) -> Result<(), SlackError> {
        SlackClient::set_reaction(self, channel, ts, name, add).await
    }

    async fn message_author(&self, channel: &str, ts: &str) -> Result<Option<String>, SlackError> {
        SlackClient::message_author(self, channel, ts).await
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, SlackError> {
        SlackClient::get_bytes(self, url).await
    }

    async fn post_bytes(&self, url: &str, body: Vec<u8>) -> Result<(), SlackError> {
        SlackClient::post_bytes(self, url, body).await
    }

    async fn file_info(&self, id: &str) -> Result<SlackFile, SlackError> {
        SlackClient::file_info(self, id).await
    }

    async fn get_upload_url(
        &self,
        name: &str,
        length: usize,
    ) -> Result<(String, String), SlackError> {
        SlackClient::get_upload_url(self, name, length).await
    }

    async fn complete_upload(
        &self,
        file_id: &str,
        channel: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), SlackError> {
        SlackClient::complete_upload(self, file_id, channel, thread_ts).await
    }

    async fn search_messages(
        &self,
        query: &str,
        count: u32,
    ) -> Result<Vec<SlackSearchMatch>, SlackError> {
        SlackClient::search_messages(self, query, count).await
    }

    async fn user_info(&self, user: &str) -> Result<SlackUser, SlackError> {
        SlackClient::user_info(self, user).await
    }
}

/// The largest text `chat.postMessage` carries without truncating. The
/// probe measured a silent cut at 4000 with `ok: true`, so a longer draft
/// is split and posted in sequence rather than quietly halved.
pub const POST_LIMIT: usize = 4000;

/// Budget per part once a draft is split, leaving room for the `[i/n] `
/// prefix each part carries.
const POST_PART_BUDGET: usize = 3800;

/// Split `text` into parts each under [`POST_LIMIT`], numbered so a
/// reader can see the order. Text at or under the limit is one part.
pub fn split_for_post(text: &str) -> Vec<String> {
    if text.chars().count() <= POST_LIMIT {
        return vec![text.to_owned()];
    }
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len().div_ceil(POST_PART_BUDGET);
    chars
        .chunks(POST_PART_BUDGET)
        .enumerate()
        .map(|(index, chunk)| {
            format!("[{}/{}] {}", index + 1, total, chunk.iter().collect::<String>())
        })
        .collect()
}

/// Whether `text` carries a mention of `user_id`.
pub fn mentions(text: &str, user_id: &str) -> bool {
    text.contains(&format!("<@{user_id}>"))
}

/// Whether one subscription wants this message. A message the user
/// authored himself is never wanted, or an agent answering in Slack would
/// answer itself.
fn matches(
    subscription: &SlackSubscription,
    conversation: &SlackConversation,
    author: Option<&str>,
    text: &str,
    user_id: &str,
) -> bool {
    if author == Some(user_id) {
        return false;
    }
    match &subscription.target {
        SlackSubscriptionTarget::DirectMessages => conversation.is_im || conversation.is_mpim,
        SlackSubscriptionTarget::Conversation { id, mode } => {
            id == &conversation.id
                && match mode {
                    SlackWatchMode::All => true,
                    SlackWatchMode::MentionsOnly => mentions(text, user_id),
                }
        }
    }
}

/// Whether any of `subscriptions` wants this message.
pub fn wants(
    subscriptions: &[SlackSubscription],
    conversation: &SlackConversation,
    author: Option<&str>,
    text: &str,
    user_id: &str,
) -> bool {
    subscriptions.iter().any(|s| matches(s, conversation, author, text, user_id))
}

/// Whether any subscription names this conversation, ignoring the mode and
/// the author. Decides which conversations a sweep fetches history for: a
/// mentions-only subscription still has to look, or it would never see a
/// mention arrive.
fn targets(subscriptions: &[SlackSubscription], conversation: &SlackConversation) -> bool {
    subscriptions.iter().any(|s| match &s.target {
        SlackSubscriptionTarget::DirectMessages => conversation.is_im || conversation.is_mpim,
        SlackSubscriptionTarget::Conversation { id, .. } => id == &conversation.id,
    })
}

const API_ROOT: &str = "https://slack.com/api";

/// Used when the response carries no `Retry-After`.
pub(crate) const RETRY_FALLBACK: Duration = Duration::from_secs(5);
/// Floor for a `Retry-After` that asks for no wait at all. `Retry-After: 0`
/// is legal, and retrying at once is the opposite of what it asked.
pub(crate) const RETRY_FLOOR: Duration = Duration::from_secs(1);
/// Ceiling for a hostile or broken `Retry-After`.
pub(crate) const RETRY_CAP: Duration = Duration::from_secs(60);

/// What to wait after a 429. Slack sends whole seconds.
pub(crate) fn retry_delay(retry_after_secs: Option<u64>) -> Duration {
    match retry_after_secs {
        None => RETRY_FALLBACK,
        Some(secs) => Duration::from_secs(secs).clamp(RETRY_FLOOR, RETRY_CAP),
    }
}

/// Slack's whole-second `Retry-After`, when the response carries one.
fn retry_after_of(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Bounded so a gateway's error page cannot flood the log or the tool output.
const BODY_PREFIX_LEN: usize = 200;

fn body_prefix(body: &str) -> String {
    body.chars().take(BODY_PREFIX_LEN).collect()
}

/// A non-success status other than 429. The body may be an HTML error
/// page, which the envelope decoder would report as a JSON parse failure
/// - the status is what the operator needs, so name it and bound the body.
fn status_failure(method: &str, status: reqwest::StatusCode, body: &str) -> SlackError {
    SlackError::Transport {
        method: method.to_owned(),
        detail: format!("HTTP {status}: {}", body_prefix(body)),
    }
}

/// `application/x-www-form-urlencoded` body for one call.
fn encode_form(params: &[(&str, String)]) -> String {
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

/// `Debug` is hand-written because the token must never be printed.
pub struct SlackClient {
    http: reqwest::Client,
    token: String,
}

impl std::fmt::Debug for SlackClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackClient").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub enum SlackError {
    /// Slack answered `ok: false`. `needed` names the scope when it is a scope failure.
    Api { method: String, error: String, needed: Option<String> },
    /// The transport failed, or the body was not the envelope we expect.
    Transport { method: String, detail: String },
    /// HTTP 429. Slack's Web API surfaces these as a status, not an `ok: false`.
    RateLimited { method: String, retry_after: Duration },
}

impl std::fmt::Display for SlackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api { method, error, needed } => {
                write!(f, "slack {method} failed: {error}")?;
                if let Some(needed) = needed {
                    write!(f, " (needs {needed})")?;
                }
                Ok(())
            }
            Self::Transport { method, detail } => write!(f, "slack {method} transport: {detail}"),
            Self::RateLimited { method, retry_after } => {
                write!(f, "slack {method} rate limited, retry in {}s", retry_after.as_secs())
            }
        }
    }
}

impl std::error::Error for SlackError {}

/// Slack wraps every payload in `ok`, and reports logical failures with HTTP 200.
#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    needed: Option<String>,
    #[serde(flatten)]
    value: T,
}

/// Split out of `call` so the envelope rules are testable without HTTP.
pub(crate) fn decode_envelope<T: DeserializeOwned>(
    method: &str,
    body: &str,
) -> Result<T, SlackError> {
    let envelope: Envelope<T> = serde_json::from_str(body).map_err(|detail| {
        SlackError::Transport { method: method.to_owned(), detail: detail.to_string() }
    })?;
    if !envelope.ok {
        return Err(SlackError::Api {
            method: method.to_owned(),
            error: envelope.error.unwrap_or_else(|| "unknown".to_owned()),
            needed: envelope.needed,
        });
    }
    Ok(envelope.value)
}

/// Messages fetched per `conversations.history` call.
const SWEEP_LIMIT: u32 = 200;

/// A cursor Slack hands back unchanged must not spin the history walk
/// forever, the same guard the conversation walk carries.
const MAX_HISTORY_PAGES: usize = 200;

/// A cursor Slack hands back unchanged must not spin the reply walk
/// forever, the same guard the conversation walk carries.
const MAX_REPLY_PAGES: usize = 200;

/// How long a pump waits on one Web API call.
const PUMP_TIMEOUT: Duration = Duration::from_secs(30);

/// What one sweep did.
#[derive(Debug)]
pub(crate) struct SweepOutcome {
    pub delivered: usize,
    /// Set when a call was throttled, so the caller backs off rather than
    /// hammering. A 429 is a report, not an error.
    pub rate_limited: Option<Duration>,
}

/// Whether `ts` is strictly after `watermark`. Timestamps are compared as
/// the strings Slack sent: they carry a microsecond fraction, and parsing
/// one to a number loses it and silently moves the cursor.
fn is_newer(ts: &str, watermark: Option<&str>) -> bool {
    match watermark {
        None => true,
        Some(watermark) => ts > watermark,
    }
}

/// Every history message after each page boundary, following the cursor.
/// One page is `SWEEP_LIMIT` messages, and a sweep that read only the first
/// would advance the watermark past everything it never fetched, losing
/// those messages silently.
async fn fetch_history(
    api: &dyn SlackApi,
    channel: &str,
    oldest: Option<&str>,
) -> Result<Vec<SlackHistoryMessage>, SlackError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = api.history(channel, oldest, SWEEP_LIMIT, cursor.as_deref()).await?;
        out.extend(page.messages);
        pages += 1;
        match page.next_cursor {
            Some(next) if pages < MAX_HISTORY_PAGES => cursor = Some(next),
            Some(_) => {
                tracing::warn!(
                    target: "forge_connectors::slack",
                    channel,
                    pages,
                    "conversations.history kept handing back a cursor; stopping the walk",
                );
                return Ok(out);
            }
            None => return Ok(out),
        }
    }
}

/// Every reply in a thread, following the cursor. Dedupe is the caller's:
/// Slack pages newest-first and repeats the parent on every page.
async fn fetch_replies(
    api: &dyn SlackApi,
    channel: &str,
    ts: &str,
) -> Result<Vec<SlackHistoryMessage>, SlackError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = api.replies(channel, ts, SWEEP_LIMIT, cursor.as_deref()).await?;
        out.extend(page.messages);
        pages += 1;
        match page.next_cursor {
            Some(next) if pages < MAX_REPLY_PAGES => cursor = Some(next),
            _ => return Ok(out),
        }
    }
}

/// One pass over a workspace's subscribed conversations: fetch what is
/// newer than each conversation's watermark, follow the threads of the
/// parents that report replies, and hand every survivor to the host.
pub(crate) async fn sweep(
    host: &dyn SlackHost,
    api: &dyn SlackApi,
    workspace: &str,
) -> Result<SweepOutcome, SlackError> {
    let subscriptions = host.subscriptions(workspace);
    let user_id = host.user_id(workspace).unwrap_or_default();

    let conversations = match api.list_conversations().await {
        Ok(conversations) => conversations,
        Err(SlackError::RateLimited { retry_after, .. }) => {
            return Ok(SweepOutcome { delivered: 0, rate_limited: Some(retry_after) });
        }
        Err(error) => return Err(error),
    };

    let mut delivered = 0;
    for conversation in &conversations {
        if !targets(&subscriptions, conversation) {
            continue;
        }
        let watermark = host.watermark(workspace, &conversation.id);

        let history = match fetch_history(api, &conversation.id, watermark.as_deref()).await {
            Ok(history) => history,
            Err(SlackError::RateLimited { retry_after, .. }) => {
                return Ok(SweepOutcome { delivered, rate_limited: Some(retry_after) });
            }
            Err(error) => return Err(error),
        };

        let mut seen: HashSet<String> = HashSet::new();
        let mut batch: Vec<SlackHistoryMessage> = history
            .into_iter()
            .filter(|message| {
                is_newer(&message.ts, watermark.as_deref()) && seen.insert(message.ts.clone())
            })
            .collect();

        // Thread replies never appear in `conversations.history`, so a
        // parent that reports replies is the only way to reach them.
        let parents: Vec<String> = batch
            .iter()
            .filter(|message| message.reply_count > 0 && message.thread_ts.is_none())
            .map(|message| message.ts.clone())
            .collect();
        for parent in parents {
            let replies = match fetch_replies(api, &conversation.id, &parent).await {
                Ok(replies) => replies,
                Err(SlackError::RateLimited { retry_after, .. }) => {
                    return Ok(SweepOutcome { delivered, rate_limited: Some(retry_after) });
                }
                Err(error) => return Err(error),
            };
            for reply in replies {
                if is_newer(&reply.ts, watermark.as_deref()) && seen.insert(reply.ts.clone()) {
                    batch.push(reply);
                }
            }
        }

        let label =
            conversation.name.clone().or_else(|| conversation.user.clone()).unwrap_or_default();
        for message in &batch {
            let Some(subscription) = subscriptions.iter().find(|subscription| {
                matches(
                    subscription,
                    conversation,
                    message.user.as_deref(),
                    &message.text,
                    &user_id,
                )
            }) else {
                continue;
            };
            host.deliver(
                subscription,
                &SlackMessage {
                    workspace: workspace.to_owned(),
                    conversation: conversation.id.clone(),
                    conversation_label: label.clone(),
                    ts: message.ts.clone(),
                    thread_ts: message.thread_ts.clone(),
                    user: message.user.clone(),
                    text: message.text.clone(),
                },
            );
            delivered += 1;
        }

        // Advanced last, so a crash before this point re-delivers rather
        // than skips; delivery is idempotent on the timestamp.
        if let Some(newest) = batch.iter().map(|message| message.ts.as_str()).max() {
            host.set_watermark(workspace, &conversation.id, newest);
        }
    }

    Ok(SweepOutcome { delivered, rate_limited: None })
}

/// The wait before the next sweep: the configured interval, or a rate
/// limit's delay when one was reported.
pub(crate) fn next_interval(configured: Duration, rate_limited: Option<Duration>) -> Duration {
    rate_limited.map_or(configured, |delay| delay.min(RETRY_CAP))
}

/// Sweep one workspace forever, until `shutdown` fires. The first sweep is
/// delayed by the interval so a boot with many workspaces does not burst.
pub async fn run_workspace_pump(
    host: Arc<dyn SlackHost>,
    workspace: String,
    poll_seconds: u64,
    mut shutdown: oneshot::Receiver<()>,
) {
    let interval = Duration::from_secs(poll_seconds);
    let Ok(client) = host.client(&workspace, PUMP_TIMEOUT) else {
        tracing::warn!(
            target: "forge_connectors::slack",
            workspace = %workspace,
            "no client for this workspace; its pump stays down",
        );
        return;
    };

    let mut wait = interval;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            () = tokio::time::sleep(wait) => {}
        }
        wait = interval;
        match sweep(host.as_ref(), &client, &workspace).await {
            Ok(outcome) => {
                host.set_connected(&workspace, true);
                tracing::debug!(
                    target: "forge_connectors::slack",
                    workspace = %workspace,
                    delivered = outcome.delivered,
                    "slack sweep finished",
                );
                wait = next_interval(interval, outcome.rate_limited);
            }
            Err(error) => {
                host.set_connected(&workspace, false);
                tracing::warn!(
                    target: "forge_connectors::slack",
                    workspace = %workspace,
                    %error,
                    "slack sweep failed",
                );
            }
        }
    }
    host.set_connected(&workspace, false);
}

/// `auth.test` - who the token belongs to.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthTest {
    pub team: String,
    pub user: String,
    pub team_id: String,
    pub user_id: String,
    pub url: String,
}

/// `types` must be passed explicitly or Slack returns public channels
/// only, and `limit` must be passed or the call gets a stricter cap.
const CONVERSATION_TYPES: &str = "public_channel,private_channel,im,mpim";

/// A cursor Slack hands back unchanged must not spin the walk forever:
/// at 200 per page this is far past any real workspace, and the failure
/// mode is an endless request loop against a rate-limited API.
const MAX_CONVERSATION_PAGES: usize = 200;

/// One page of `users.conversations` as Slack sends it.
#[derive(Debug, Deserialize)]
struct RawConversationsPage {
    channels: Vec<SlackConversation>,
    #[serde(default)]
    response_metadata: Metadata,
}

#[derive(Debug, Default, Deserialize)]
struct Metadata {
    #[serde(default)]
    next_cursor: String,
}

/// One page with the cursor normalised: Slack sends an empty string on
/// the last page, which would otherwise read as a cursor to page from.
#[derive(Debug)]
struct ConversationsPage {
    conversations: Vec<SlackConversation>,
    next_cursor: Option<String>,
}

/// Split out of the async path so the paging rules are testable without HTTP.
fn decode_conversations_page(body: &str) -> Result<ConversationsPage, SlackError> {
    let raw: RawConversationsPage = decode_envelope("users.conversations", body)?;
    Ok(page_from_raw(raw))
}

fn page_from_raw(raw: RawConversationsPage) -> ConversationsPage {
    let cursor = (!raw.response_metadata.next_cursor.is_empty())
        .then_some(raw.response_metadata.next_cursor);
    ConversationsPage { conversations: raw.channels, next_cursor: cursor }
}

/// One message off `conversations.history` or `conversations.replies`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackHistoryMessage {
    pub ts: String,
    pub user: Option<String>,
    pub text: String,
    /// `None` for a top-level message, the parent's `ts` for a reply.
    pub thread_ts: Option<String>,
    /// Non-zero on a parent whose thread has replies.
    pub reply_count: u32,
}

/// One page of a conversation's messages. Both `conversations.history` and
/// `conversations.replies` answer with this shape.
#[derive(Debug)]
pub struct MessagePage {
    pub messages: Vec<SlackHistoryMessage>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawMessagePage {
    #[serde(default)]
    messages: Vec<RawWireMessage>,
    #[serde(default)]
    response_metadata: Metadata,
}

#[derive(Debug, Deserialize)]
struct RawWireMessage {
    ts: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    reply_count: u32,
}

/// Split out of the async path so the decode is testable without HTTP.
fn decode_message_page(method: &str, body: &str) -> Result<MessagePage, SlackError> {
    let raw: RawMessagePage = decode_envelope(method, body)?;
    let cursor = (!raw.response_metadata.next_cursor.is_empty())
        .then_some(raw.response_metadata.next_cursor);
    let messages = raw
        .messages
        .into_iter()
        .map(|message| SlackHistoryMessage {
            ts: message.ts,
            user: message.user,
            text: message.text,
            thread_ts: message.thread_ts,
            reply_count: message.reply_count,
        })
        .collect();
    Ok(MessagePage { messages, next_cursor: cursor })
}

/// The API's ceiling for one search page.
const SEARCH_COUNT_CAP: u32 = 100;

/// The params `search.messages` is always called with. `sort=timestamp`
/// because the default is relevance, which during the probe ranked a
/// 22-hour-old match above a 2.6-hour-old one.
fn search_params(query: &str, count: u32) -> Vec<(&'static str, String)> {
    vec![
        ("query", query.to_owned()),
        ("sort", "timestamp".to_owned()),
        ("count", count.min(SEARCH_COUNT_CAP).to_string()),
    ]
}

/// Split out of the async path so the decode is testable without HTTP.
fn decode_search(body: &str) -> Result<Vec<SlackSearchMatch>, SlackError> {
    #[derive(Default, Deserialize)]
    struct SearchMessages {
        #[serde(default)]
        matches: Vec<RawMatch>,
    }
    #[derive(Default, Deserialize)]
    struct SearchPayload {
        #[serde(default)]
        messages: SearchMessages,
    }
    #[derive(Deserialize)]
    struct RawMatch {
        ts: String,
        #[serde(default)]
        text: String,
        channel: RawChannel,
        #[serde(default)]
        username: Option<String>,
    }
    #[derive(Deserialize)]
    struct RawChannel {
        id: String,
        #[serde(default)]
        name: Option<String>,
    }

    let payload: SearchPayload = decode_envelope("search.messages", body)?;
    Ok(payload
        .messages
        .matches
        .into_iter()
        .map(|hit| SlackSearchMatch {
            ts: hit.ts,
            text: hit.text,
            conversation_id: hit.channel.id,
            conversation_name: hit.channel.name,
            username: hit.username,
        })
        .collect())
}

/// Split out of the async path so the decode is testable without HTTP.
fn decode_user(body: &str) -> Result<SlackUser, SlackError> {
    #[derive(Deserialize)]
    struct Wrapper {
        user: SlackUser,
    }
    let wrapper: Wrapper = decode_envelope("users.info", body)?;
    Ok(wrapper.user)
}

/// Split out of the async path so the decode is testable without HTTP.
fn decode_file(body: &str) -> Result<SlackFile, SlackError> {
    #[derive(Deserialize)]
    struct Wrapper {
        file: SlackFile,
    }
    let wrapper: Wrapper = decode_envelope("files.info", body)?;
    Ok(wrapper.file)
}

impl SlackClient {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, token }
    }

    /// POST one Web API method. Slack is uniform here, so one helper
    /// covers every call the connector makes.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<T, SlackError> {
        let body = self.call_text(method, params).await?;
        decode_envelope(method, &body)
    }

    /// POST one Web API method and hand back the body undecoded, for the
    /// callers whose decode needs the raw text rather than one envelope.
    async fn call_text(
        &self,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<String, SlackError> {
        let url = format!("{API_ROOT}/{method}");
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(encode_form(params))
            .send()
            .await
            .map_err(|err| SlackError::Transport {
                method: method.to_owned(),
                detail: err.to_string(),
            })?;

        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SlackError::RateLimited {
                method: method.to_owned(),
                retry_after: retry_delay(retry_after_of(&response)),
            });
        }

        let body = response.text().await.map_err(|err| SlackError::Transport {
            method: method.to_owned(),
            detail: err.to_string(),
        })?;
        if !status.is_success() {
            return Err(status_failure(method, status, &body));
        }
        Ok(body)
    }

    /// Who this token belongs to. The first thing to prove on a new workspace.
    pub async fn auth_test(&self) -> Result<AuthTest, SlackError> {
        self.call("auth.test", &[]).await
    }

    /// One page of a conversation's history. `oldest` is omitted entirely
    /// on a first sweep: Slack treats an empty `oldest` differently from
    /// an absent one.
    pub async fn history(
        &self,
        channel: &str,
        oldest: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError> {
        let mut params = vec![("channel", channel.to_owned()), ("limit", limit.to_string())];
        if let Some(oldest) = oldest {
            params.push(("oldest", oldest.to_owned()));
        }
        if let Some(cursor) = cursor {
            params.push(("cursor", cursor.to_owned()));
        }
        let body = self.call_text("conversations.history", &params).await?;
        decode_message_page("conversations.history", &body)
    }

    /// One page of a thread's replies. Slack pages newest-first and repeats
    /// the parent on every page, so the caller dedupes by `ts`.
    pub async fn replies(
        &self,
        channel: &str,
        ts: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError> {
        let mut params = vec![
            ("channel", channel.to_owned()),
            ("ts", ts.to_owned()),
            ("limit", limit.to_string()),
        ];
        if let Some(cursor) = cursor {
            params.push(("cursor", cursor.to_owned()));
        }
        let body = self.call_text("conversations.replies", &params).await?;
        decode_message_page("conversations.replies", &body)
    }

    /// Post one message, as a root or into an existing thread.
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), SlackError> {
        let mut params = vec![("channel", channel.to_owned()), ("text", text.to_owned())];
        if let Some(thread_ts) = thread_ts {
            params.push(("thread_ts", thread_ts.to_owned()));
        }
        let _: serde_json::Value = self.call("chat.postMessage", &params).await?;
        Ok(())
    }

    /// Replace the text of one of the user's own messages. Slack drops
    /// existing blocks when `text` is sent without them, which is what
    /// this connector wants: one text body, no stale block.
    pub async fn update_message(
        &self,
        channel: &str,
        ts: &str,
        text: &str,
    ) -> Result<(), SlackError> {
        let params =
            vec![("channel", channel.to_owned()), ("ts", ts.to_owned()), ("text", text.to_owned())];
        let _: serde_json::Value = self.call("chat.update", &params).await?;
        Ok(())
    }

    /// Delete one of the user's own messages.
    pub async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), SlackError> {
        let params = vec![("channel", channel.to_owned()), ("ts", ts.to_owned())];
        let _: serde_json::Value = self.call("chat.delete", &params).await?;
        Ok(())
    }

    /// Add or remove one reaction. `reactions.remove` requires being the
    /// original reaction's author.
    pub async fn set_reaction(
        &self,
        channel: &str,
        ts: &str,
        name: &str,
        add: bool,
    ) -> Result<(), SlackError> {
        let method = if add { "reactions.add" } else { "reactions.remove" };
        let params = vec![
            ("channel", channel.to_owned()),
            ("timestamp", ts.to_owned()),
            ("name", name.to_owned()),
        ];
        let _: serde_json::Value = self.call(method, &params).await?;
        Ok(())
    }

    /// The author of the message at `ts`, or `None` when the page did not
    /// carry it. Backs the own-message check an edit needs.
    pub async fn message_author(
        &self,
        channel: &str,
        ts: &str,
    ) -> Result<Option<String>, SlackError> {
        let params = vec![
            ("channel", channel.to_owned()),
            ("latest", ts.to_owned()),
            ("inclusive", "true".to_owned()),
            ("limit", "1".to_owned()),
        ];
        let body = self.call_text("conversations.history", &params).await?;
        let page = decode_message_page("conversations.history", &body)?;
        Ok(page.messages.into_iter().find(|message| message.ts == ts).and_then(|m| m.user))
    }

    /// GET an absolute URL with the bearer attached. Slack's private file
    /// URLs answer 403 without it, and byte-identically with it.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, SlackError> {
        let response = self.http.get(url).bearer_auth(&self.token).send().await.map_err(|err| {
            SlackError::Transport { method: "download".to_owned(), detail: err.to_string() }
        })?;
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SlackError::RateLimited {
                method: "download".to_owned(),
                retry_after: retry_delay(retry_after_of(&response)),
            });
        }
        let status = response.status();
        let bytes = response.bytes().await.map_err(|err| SlackError::Transport {
            method: "download".to_owned(),
            detail: err.to_string(),
        })?;
        if !status.is_success() {
            // A non-success body is an error page, never a file: treating
            // it as bytes would write an HTML 403 to disk as the file.
            return Err(SlackError::Transport {
                method: "download".to_owned(),
                detail: format!("HTTP {status}"),
            });
        }
        Ok(bytes.to_vec())
    }

    /// POST raw bytes to an absolute URL. Deliberately no bearer: the
    /// upload URL is pre-signed, and it is not the Web API host.
    pub async fn post_bytes(&self, url: &str, body: Vec<u8>) -> Result<(), SlackError> {
        let response = self.http.post(url).body(body).send().await.map_err(|err| {
            SlackError::Transport { method: "upload".to_owned(), detail: err.to_string() }
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SlackError::RateLimited {
                method: "upload".to_owned(),
                retry_after: retry_delay(retry_after_of(&response)),
            });
        }
        if !status.is_success() {
            return Err(SlackError::Transport {
                method: "upload".to_owned(),
                detail: format!("HTTP {status}"),
            });
        }
        Ok(())
    }

    /// `files.info` for one file id.
    pub async fn file_info(&self, id: &str) -> Result<SlackFile, SlackError> {
        let params = vec![("file", id.to_owned())];
        let body = self.call_text("files.info", &params).await?;
        decode_file(&body)
    }

    /// `files.getUploadURLExternal`: the pre-signed URL to POST the bytes
    /// to, plus the file id the completion call needs.
    pub async fn get_upload_url(
        &self,
        name: &str,
        length: usize,
    ) -> Result<(String, String), SlackError> {
        #[derive(Deserialize)]
        struct UploadTarget {
            upload_url: String,
            file_id: String,
        }
        let params = vec![("filename", name.to_owned()), ("length", length.to_string())];
        let target: UploadTarget = self.call("files.getUploadURLExternal", &params).await?;
        Ok((target.upload_url, target.file_id))
    }

    /// `files.completeUploadExternal`. It can be called once only, and the
    /// file is discarded if it never is.
    pub async fn complete_upload(
        &self,
        file_id: &str,
        channel: &str,
        thread_ts: Option<&str>,
    ) -> Result<(), SlackError> {
        let files = serde_json::json!([{ "id": file_id }]).to_string();
        let mut params = vec![("files", files), ("channel_id", channel.to_owned())];
        if let Some(thread_ts) = thread_ts {
            params.push(("thread_ts", thread_ts.to_owned()));
        }
        let _: serde_json::Value = self.call("files.completeUploadExternal", &params).await?;
        Ok(())
    }

    /// `search.messages`. Matches text; it cannot find every message that
    /// mentions a user, which is what the mention subscription is for.
    pub async fn search_messages(
        &self,
        query: &str,
        count: u32,
    ) -> Result<Vec<SlackSearchMatch>, SlackError> {
        let body = self.call_text("search.messages", &search_params(query, count)).await?;
        decode_search(&body)
    }

    /// `users.info` for one user id.
    pub async fn user_info(&self, user: &str) -> Result<SlackUser, SlackError> {
        let params = vec![("user", user.to_owned())];
        let body = self.call_text("users.info", &params).await?;
        decode_user(&body)
    }

    /// Every conversation the token's user is a member of, paging to the end.
    pub async fn list_conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let mut params =
                vec![("types", CONVERSATION_TYPES.to_owned()), ("limit", "200".to_owned())];
            if let Some(cursor) = &cursor {
                params.push(("cursor", cursor.clone()));
            }
            let body = self.call_text("users.conversations", &params).await?;
            let page = decode_conversations_page(&body)?;
            out.extend(page.conversations);
            pages += 1;
            match page.next_cursor {
                Some(_) if pages >= MAX_CONVERSATION_PAGES => {
                    tracing::warn!(
                        target: "forge_connectors::slack",
                        pages,
                        conversations = out.len(),
                        "users.conversations kept handing back a cursor; stopping the walk",
                    );
                    return Ok(out);
                }
                Some(next) => cursor = Some(next),
                None => return Ok(out),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Pinned against literals, not against `RETRY_FALLBACK` / `RETRY_CAP`:
    /// asserting the output equals the constant the function itself applies
    /// holds for any value, so it cannot catch a changed cap.
    #[test]
    fn retry_delay_honours_the_header_and_floors_absurd_values() {
        assert_eq!(retry_delay(Some(7)), Duration::from_secs(7), "an honest header is honoured");
        assert_eq!(retry_delay(None), Duration::from_secs(5), "a missing header falls back");
        // A hostile or broken header must not park the pump for an hour.
        assert_eq!(
            retry_delay(Some(100_000)),
            Duration::from_secs(60),
            "an absurd header is capped"
        );
        // `Retry-After: 0` is legal, and retrying immediately is the one
        // thing the header asked us not to do.
        assert_eq!(retry_delay(Some(0)), Duration::from_secs(1), "a zero header still waits");
    }

    #[test]
    fn a_gateway_failure_reports_the_status_and_a_bounded_body_prefix() {
        let mut body = "bad gateway from the edge".to_owned();
        body.push_str(&"x".repeat(1000));
        let err = status_failure("users.conversations", reqwest::StatusCode::BAD_GATEWAY, &body);
        match err {
            SlackError::Transport { method, detail } => {
                assert!(detail.contains("502"), "the status is named, got: {detail}");
                assert!(detail.contains("bad gateway"), "the body prefix is carried: {detail}");
                assert!(detail.len() < 300, "the body is bounded, got {} chars", detail.len());
                assert_eq!(method, "users.conversations");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn an_ok_false_envelope_is_an_api_error_not_a_decode_error() {
        let body = r#"{"ok":false,"error":"missing_scope","needed":"search:read.public"}"#;
        let err = decode_envelope::<serde_json::Value>("conversations.history", body)
            .expect_err("ok:false must be an error");
        match err {
            SlackError::Api { method, error, needed } => {
                assert_eq!(method, "conversations.history");
                assert_eq!(error, "missing_scope");
                assert_eq!(needed.as_deref(), Some("search:read.public"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn an_ok_true_envelope_decodes_its_payload() {
        let body = r#"{"ok":true,"team":"Trust Machines","user":"ved","team_id":"T1","user_id":"U1","url":"https://x.slack.com/"}"#;
        let got: AuthTest = decode_envelope("auth.test", body).expect("decodes");
        assert_eq!(got.team_id, "T1", "the flattened payload carries team_id");
        assert_eq!(got.user, "ved", "the flattened payload carries user");
    }

    #[test]
    fn list_conversations_decodes_mixed_types_and_keeps_the_cursor() {
        let body = r#"{
          "ok": true,
          "channels": [
            {"id":"C1","name":"general","is_channel":true},
            {"id":"D1","is_im":true,"user":"U9"},
            {"id":"G1","name":"secret","is_private":true}
          ],
          "response_metadata": {"next_cursor": "abc123"}
        }"#;
        let page = decode_conversations_page(body).expect("decodes");
        assert_eq!(page.conversations.len(), 3, "every channel in the page is kept");
        assert_eq!(page.conversations[1].user.as_deref(), Some("U9"), "a DM keeps its partner");
        assert_eq!(page.next_cursor.as_deref(), Some("abc123"), "a non-empty cursor pages on");

        let empty = decode_conversations_page(
            r#"{"ok":true,"channels":[],"response_metadata":{"next_cursor":""}}"#,
        )
        .expect("an empty cursor means the last page");
        assert_eq!(empty.next_cursor, None, "an empty cursor ends the walk");
    }

    /// The input class a message body actually contains, and the class a
    /// hand-rolled encoder silently splits a parameter on.
    #[test]
    fn encode_form_escapes_what_a_message_body_carries() {
        let encoded = encode_form(&[("text", "a&b=c+d\ncafé".to_owned())]);
        assert_eq!(encoded, "text=a%26b%3Dc%2Bd%0Acaf%C3%A9");
        assert_eq!(
            encode_form(&[("text", "two words".to_owned())]),
            "text=two+words",
            "a space is a form `+`, not %20"
        );
    }

    #[test]
    fn encode_form_keeps_an_empty_value_and_emits_nothing_for_no_params() {
        assert_eq!(encode_form(&[]), "", "no params is an empty body");
        assert_eq!(
            encode_form(&[("cursor", String::new())]),
            "cursor=",
            "an empty value still emits its name"
        );
    }

    fn sub_for(target: SlackSubscriptionTarget) -> SlackSubscription {
        SlackSubscription {
            id: uuid::Uuid::new_v4(),
            workspace: "acme".to_owned(),
            project: "forge".to_owned(),
            team_role: None,
            target,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn conversation_channel(id: &str, name: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            is_channel: true,
            is_private: false,
            is_im: false,
            is_mpim: false,
            is_archived: false,
            user: None,
        }
    }

    fn conversation_im(id: &str, user: &str) -> SlackConversation {
        SlackConversation {
            id: id.to_owned(),
            name: None,
            is_channel: false,
            is_private: false,
            is_im: true,
            is_mpim: false,
            is_archived: false,
            user: Some(user.to_owned()),
        }
    }

    #[test]
    fn a_mention_is_the_literal_token_and_nothing_looser() {
        assert!(mentions("hey <@U123> look", "U123"));
        assert!(!mentions("hey U123 look", "U123"), "a bare id is not a mention");
        assert!(!mentions("hey <@U1234> look", "U123"), "a longer id must not match");
        assert!(mentions("<@U123>", "U123"), "a mention with no surrounding text still counts");
    }

    #[test]
    fn a_dm_subscription_wants_every_dm() {
        let subs = vec![sub_for(SlackSubscriptionTarget::DirectMessages)];
        let dm = conversation_im("D1", "U9");
        assert!(wants(&subs, &dm, Some("U9"), "anything", "U1"));
    }

    #[test]
    fn a_mentions_only_channel_filters_on_the_token() {
        let subs = vec![sub_for(SlackSubscriptionTarget::Conversation {
            id: "C1".to_owned(),
            mode: SlackWatchMode::MentionsOnly,
        })];
        let channel = conversation_channel("C1", "general");
        assert!(!wants(&subs, &channel, Some("U9"), "no mention here", "U1"));
        assert!(wants(&subs, &channel, Some("U9"), "ping <@U1>", "U1"));
    }

    #[test]
    fn the_users_own_messages_are_never_delivered() {
        // Otherwise the connector echoes back everything the user says, and an
        // agent answering in Slack would answer itself.
        let subs = vec![sub_for(SlackSubscriptionTarget::Conversation {
            id: "C1".to_owned(),
            mode: SlackWatchMode::All,
        })];
        let channel = conversation_channel("C1", "general");
        assert!(!wants(&subs, &channel, Some("U1"), "my own words", "U1"));
    }

    #[test]
    fn an_unsubscribed_conversation_is_never_wanted() {
        let subs = vec![sub_for(SlackSubscriptionTarget::Conversation {
            id: "C1".to_owned(),
            mode: SlackWatchMode::All,
        })];
        assert!(!wants(&subs, &conversation_channel("C2", "other"), Some("U9"), "hi", "U1"));
    }

    #[test]
    fn a_history_page_decodes_its_messages_and_the_cursor() {
        let body = r#"{
          "ok": true,
          "messages": [
            {"ts":"100.000001","user":"U9","text":"hello","thread_ts":"100.000001","reply_count":3},
            {"ts":"200.000002","user":"U8","text":"plain"}
          ],
          "response_metadata": {"next_cursor": "cur1"}
        }"#;
        let page = decode_message_page("conversations.history", body).expect("decodes");
        assert_eq!(page.messages.len(), 2, "every message in the page is kept");
        assert_eq!(page.messages[0].ts, "100.000001", "the ts survives as the string Slack sent");
        assert_eq!(page.messages[0].reply_count, 3, "the reply count drives thread following");
        assert_eq!(page.messages[0].thread_ts.as_deref(), Some("100.000001"));
        assert_eq!(page.messages[1].thread_ts, None, "a top-level message has no thread");
        assert_eq!(page.next_cursor.as_deref(), Some("cur1"));

        let last = decode_message_page(
            "conversations.history",
            r#"{"ok":true,"messages":[],"response_metadata":{"next_cursor":""}}"#,
        )
        .expect("an empty cursor means the last page");
        assert_eq!(last.next_cursor, None);
    }

    /// The decoder keeps the repeat: Slack returns the parent on every page
    /// of `conversations.replies`, and deduping is the sweep's job.
    #[test]
    fn a_replies_page_keeps_the_repeated_parent_for_the_caller_to_dedupe() {
        let body = r#"{
          "ok": true,
          "messages": [
            {"ts":"100.0","user":"U9","text":"root"},
            {"ts":"200.1","user":"U8","text":"reply one"},
            {"ts":"100.0","user":"U9","text":"root"},
            {"ts":"300.2","user":"U8","text":"reply two"}
          ]
        }"#;
        let page = decode_message_page("conversations.replies", body).expect("decodes");
        assert_eq!(page.messages.len(), 4, "the decoder does not dedupe");
        assert_eq!(
            page.messages.iter().filter(|m| m.ts == "100.0").count(),
            2,
            "the parent arrives twice and the caller is the one that folds it",
        );
    }

    /// Drives both ports from seeded responses, so a sweep runs with no
    /// workspace and no network. One type implements both traits because
    /// the sweep's two arguments are the same double in every test.
    #[derive(Default)]
    struct FakeHost {
        subscriptions: Vec<SlackSubscription>,
        user_id: Option<String>,
        rate_limited: Option<Duration>,
        conversations: std::sync::Mutex<Vec<SlackConversation>>,
        history: std::sync::Mutex<HashMap<String, Vec<Vec<SlackHistoryMessage>>>>,
        replies: std::sync::Mutex<HashMap<String, Vec<SlackHistoryMessage>>>,
        watermarks: std::sync::Mutex<HashMap<String, String>>,
        connected: std::sync::Mutex<Option<bool>>,
        delivered: std::sync::Mutex<Vec<SlackMessage>>,
    }

    fn conversation_for(id: &str) -> SlackConversation {
        let is_im = id.starts_with('D');
        SlackConversation {
            id: id.to_owned(),
            name: (!is_im).then(|| id.to_lowercase()),
            is_channel: !is_im,
            is_private: false,
            is_im,
            is_mpim: false,
            is_archived: false,
            user: is_im.then(|| "U9".to_owned()),
        }
    }

    impl FakeHost {
        fn with_subscriptions(subscriptions: Vec<SlackSubscription>) -> Self {
            Self { subscriptions, user_id: Some("U1".to_owned()), ..Self::default() }
        }

        fn seed_history(&self, channel: &str, messages: Vec<SlackHistoryMessage>) {
            self.seed_history_pages(channel, vec![messages]);
        }

        /// Seed several pages. The fake serves them in order, handing back
        /// the next index as the cursor, so a paging walk is exercised.
        fn seed_history_pages(&self, channel: &str, pages: Vec<Vec<SlackHistoryMessage>>) {
            {
                let mut conversations = self.conversations.lock().expect("lock");
                if !conversations.iter().any(|c| c.id == channel) {
                    conversations.push(conversation_for(channel));
                }
            }
            self.history.lock().expect("lock").insert(channel.to_owned(), pages);
        }

        fn seed_replies(&self, channel: &str, parent: &str, messages: Vec<SlackHistoryMessage>) {
            self.replies.lock().expect("lock").insert(format!("{channel}/{parent}"), messages);
        }

        fn fail_with_rate_limit(&mut self, retry_after: Duration) {
            self.rate_limited = Some(retry_after);
        }

        fn delivered(&self) -> Vec<SlackMessage> {
            self.delivered.lock().expect("lock").clone()
        }

        fn connected(&self) -> Option<bool> {
            *self.connected.lock().expect("lock")
        }
    }

    impl SlackHost for FakeHost {
        fn client(&self, _workspace: &str, _timeout: Duration) -> Result<SlackClient, String> {
            Ok(SlackClient::new(reqwest::Client::new(), "xoxp-test".to_owned()))
        }

        fn user_id(&self, _workspace: &str) -> Option<String> {
            self.user_id.clone()
        }

        fn subscriptions(&self, _workspace: &str) -> Vec<SlackSubscription> {
            self.subscriptions.clone()
        }

        fn watermark(&self, workspace: &str, conversation: &str) -> Option<String> {
            self.watermarks
                .lock()
                .expect("lock")
                .get(&format!("{workspace}/{conversation}"))
                .cloned()
        }

        fn set_watermark(&self, workspace: &str, conversation: &str, ts: &str) {
            self.watermarks
                .lock()
                .expect("lock")
                .insert(format!("{workspace}/{conversation}"), ts.to_owned());
        }

        fn set_connected(&self, _workspace: &str, connected: bool) {
            *self.connected.lock().expect("lock") = Some(connected);
        }

        fn deliver(&self, _subscription: &SlackSubscription, message: &SlackMessage) {
            self.delivered.lock().expect("lock").push(message.clone());
        }
    }

    #[async_trait::async_trait]
    impl SlackApi for FakeHost {
        async fn auth_test(&self) -> Result<AuthTest, SlackError> {
            Ok(AuthTest {
                team: "Test".to_owned(),
                user: "tester".to_owned(),
                team_id: "T1".to_owned(),
                user_id: self.user_id.clone().unwrap_or_default(),
                url: "https://test.slack.com/".to_owned(),
            })
        }

        async fn list_conversations(&self) -> Result<Vec<SlackConversation>, SlackError> {
            if let Some(retry_after) = self.rate_limited {
                return Err(SlackError::RateLimited {
                    method: "users.conversations".to_owned(),
                    retry_after,
                });
            }
            Ok(self.conversations.lock().expect("lock").clone())
        }

        async fn history(
            &self,
            channel: &str,
            _oldest: Option<&str>,
            _limit: u32,
            cursor: Option<&str>,
        ) -> Result<MessagePage, SlackError> {
            if let Some(retry_after) = self.rate_limited {
                return Err(SlackError::RateLimited {
                    method: "conversations.history".to_owned(),
                    retry_after,
                });
            }
            let pages = self.history.lock().expect("lock");
            let Some(seeded) = pages.get(channel) else {
                return Ok(MessagePage { messages: Vec::new(), next_cursor: None });
            };
            let index = cursor.and_then(|cursor| cursor.parse::<usize>().ok()).unwrap_or(0);
            Ok(MessagePage {
                messages: seeded.get(index).cloned().unwrap_or_default(),
                next_cursor: (index + 1 < seeded.len()).then(|| (index + 1).to_string()),
            })
        }

        async fn replies(
            &self,
            channel: &str,
            ts: &str,
            _limit: u32,
            _cursor: Option<&str>,
        ) -> Result<MessagePage, SlackError> {
            Ok(MessagePage {
                messages: self
                    .replies
                    .lock()
                    .expect("lock")
                    .get(&format!("{channel}/{ts}"))
                    .cloned()
                    .unwrap_or_default(),
                next_cursor: None,
            })
        }

        async fn post_message(
            &self,
            _channel: &str,
            _text: &str,
            _thread_ts: Option<&str>,
        ) -> Result<(), SlackError> {
            Ok(())
        }

        async fn update_message(
            &self,
            _channel: &str,
            _ts: &str,
            _text: &str,
        ) -> Result<(), SlackError> {
            Ok(())
        }

        async fn delete_message(&self, _channel: &str, _ts: &str) -> Result<(), SlackError> {
            Ok(())
        }

        async fn set_reaction(
            &self,
            _channel: &str,
            _ts: &str,
            _name: &str,
            _add: bool,
        ) -> Result<(), SlackError> {
            Ok(())
        }

        async fn message_author(
            &self,
            _channel: &str,
            _ts: &str,
        ) -> Result<Option<String>, SlackError> {
            Ok(None)
        }

        async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, SlackError> {
            Ok(Vec::new())
        }

        async fn post_bytes(&self, _url: &str, _body: Vec<u8>) -> Result<(), SlackError> {
            Ok(())
        }

        async fn file_info(&self, _id: &str) -> Result<SlackFile, SlackError> {
            Ok(SlackFile { id: String::new(), name: String::new(), url_private: String::new() })
        }

        async fn get_upload_url(
            &self,
            _name: &str,
            _length: usize,
        ) -> Result<(String, String), SlackError> {
            Ok((String::new(), String::new()))
        }

        async fn complete_upload(
            &self,
            _file_id: &str,
            _channel: &str,
            _thread_ts: Option<&str>,
        ) -> Result<(), SlackError> {
            Ok(())
        }

        async fn search_messages(
            &self,
            _query: &str,
            _count: u32,
        ) -> Result<Vec<SlackSearchMatch>, SlackError> {
            Ok(Vec::new())
        }

        async fn user_info(&self, _user: &str) -> Result<SlackUser, SlackError> {
            Ok(SlackUser { id: String::new(), name: String::new(), real_name: None, tz: None })
        }
    }

    #[test]
    fn a_search_always_asks_for_timestamp_order() {
        // The default is relevance, which ranked a 22-hour-old match above
        // a 2.6-hour-old one during the probe.
        let params = search_params("granite", 20);
        assert_eq!(
            params.iter().find(|(key, _)| *key == "sort").map(|(_, value)| value.as_str()),
            Some("timestamp"),
        );
    }

    #[test]
    fn a_search_decodes_matches_and_their_channel_names() {
        let body = r#"{"ok":true,"messages":{"total":2,"matches":[
            {"ts":"100.1","text":"hello","channel":{"id":"C1","name":"general"},"username":"ved"},
            {"ts":"200.2","text":"world","channel":{"id":"D1","is_im":true},"username":"other"}]}}"#;
        let matches = decode_search(body).expect("decodes");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].conversation_id, "C1");
        assert_eq!(matches[0].conversation_name.as_deref(), Some("general"));
        assert_eq!(matches[0].username.as_deref(), Some("ved"));
        assert_eq!(matches[1].conversation_name, None, "a DM has no name");
    }

    #[test]
    fn a_user_lookup_decodes_the_display_name() {
        let body = r#"{"ok":true,"user":{"id":"U1","name":"ved","real_name":"Vedhavyas S","tz":"Asia/Kolkata"}}"#;
        let user = decode_user(body).expect("decodes");
        assert_eq!(user.name, "ved");
        assert_eq!(user.tz.as_deref(), Some("Asia/Kolkata"));
    }

    #[test]
    fn a_file_lookup_decodes_its_private_url() {
        let body = r#"{"ok":true,"file":{"id":"F1","name":"notes.txt","url_private":"https://files.slack.com/x"}}"#;
        let file = decode_file(body).expect("decodes");
        assert_eq!(file.name, "notes.txt");
        assert_eq!(file.url_private, "https://files.slack.com/x");
    }

    #[test]
    fn a_draft_at_the_limit_is_one_post_and_a_longer_one_is_split() {
        let exact = "a".repeat(POST_LIMIT);
        assert_eq!(split_for_post(&exact).len(), 1, "a draft that fits posts as one message");

        let over = "a".repeat(POST_LIMIT + 1);
        let parts = split_for_post(&over);
        assert!(parts.len() > 1, "one character over must be split, not truncated: {parts:?}");
        assert!(
            parts.iter().all(|part| part.chars().count() <= POST_LIMIT),
            "no part may reach the point Slack truncates at",
        );
        assert_eq!(
            parts.iter().map(|part| part.matches('a').count()).sum::<usize>(),
            POST_LIMIT + 1,
            "splitting must carry every character",
        );
    }

    /// What the one-shot server saw. The URL is on 127.0.0.1 with an
    /// ephemeral port, so the byte helpers need no Slack credential.
    #[derive(Default, Debug)]
    struct SeenRequest {
        authorization: Option<String>,
        body: Vec<u8>,
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|window| window == needle)
    }

    /// Header NAMES are case-insensitive, values are not: lowercasing the
    /// whole head would turn `Bearer xoxp-test` into `bearer xoxp-test`
    /// and hide a real mismatch behind a scaffolding bug.
    fn content_length(head: &str) -> Option<usize> {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse().ok())
    }

    fn header_value(head: &str, wanted: &str) -> Option<String> {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.trim().to_owned())
    }

    /// Serve exactly one request and close, on a background thread, so a
    /// test can assert on what actually went over the wire. `std::net`
    /// rather than `tokio::net` because this workspace's tokio has no
    /// `net` feature and a test is not a reason to add one.
    fn spawn_one_shot_server(
        status: &'static str,
        response_body: &'static str,
    ) -> (String, std::thread::JoinHandle<SeenRequest>) {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a port");
        let addr = listener.local_addr().expect("local addr");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept one connection");
            let mut raw = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let read = stream.read(&mut chunk).expect("read the request");
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..read]);
                if let Some(end) = find_subslice(&raw, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..end]).to_string();
                    let declared = content_length(&head).unwrap_or(0);
                    if raw.len() >= end + 4 + declared {
                        break;
                    }
                }
            }

            let end = find_subslice(&raw, b"\r\n\r\n").expect("a complete request");
            let head = String::from_utf8_lossy(&raw[..end]).to_string();
            let authorization = header_value(&head, "authorization");
            let body = raw[end + 4..].to_vec();

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            SeenRequest { authorization, body }
        });
        (format!("http://{addr}/file"), handle)
    }

    #[tokio::test]
    async fn get_bytes_sends_the_bearer_header() {
        let (url, seen) = spawn_one_shot_server("200 OK", "body-bytes");
        let client = SlackClient::new(reqwest::Client::new(), "xoxp-test".to_owned());

        let got = client.get_bytes(&url).await.expect("fetch");
        assert_eq!(got, b"body-bytes");
        assert_eq!(
            seen.join().expect("server thread").authorization.as_deref(),
            Some("Bearer xoxp-test"),
            "a private file URL is a 403 without this header",
        );
    }

    #[tokio::test]
    async fn a_non_success_download_is_an_error_not_an_empty_body() {
        let (url, seen) = spawn_one_shot_server("403 Forbidden", "");
        let client = SlackClient::new(reqwest::Client::new(), "xoxp-test".to_owned());

        assert!(
            client.get_bytes(&url).await.is_err(),
            "a 403 must not be mistaken for a zero-byte file",
        );
        seen.join().expect("server thread");
    }

    #[tokio::test]
    async fn post_bytes_sends_the_body_unchanged_and_no_bearer() {
        let (url, seen) = spawn_one_shot_server("200 OK", "OK - 5");
        let client = SlackClient::new(reqwest::Client::new(), "xoxp-test".to_owned());

        client.post_bytes(&url, b"hello".to_vec()).await.expect("upload");
        let seen = seen.join().expect("server thread");
        assert_eq!(seen.body, b"hello");
        assert_eq!(
            seen.authorization, None,
            "the upload URL is pre-signed and must not carry the bearer",
        );
    }

    fn history_message(ts: &str, user: &str, text: &str) -> SlackHistoryMessage {
        SlackHistoryMessage {
            ts: ts.to_owned(),
            user: Some(user.to_owned()),
            text: text.to_owned(),
            thread_ts: None,
            reply_count: 0,
        }
    }

    fn sub_dm(workspace: &str) -> SlackSubscription {
        let mut sub = sub_for(SlackSubscriptionTarget::DirectMessages);
        sub.workspace = workspace.to_owned();
        sub
    }

    fn sub_channel(workspace: &str, id: &str, mode: SlackWatchMode) -> SlackSubscription {
        let mut sub = sub_for(SlackSubscriptionTarget::Conversation { id: id.to_owned(), mode });
        sub.workspace = workspace.to_owned();
        sub
    }

    #[tokio::test]
    async fn a_sweep_delivers_only_what_follows_the_watermark() {
        let host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);
        host.seed_history(
            "D1",
            vec![history_message("200.1", "U9", "newer"), history_message("100.1", "U9", "older")],
        );
        host.set_watermark("acme", "D1", "150.0");

        let outcome = sweep(&host, &host, "acme").await.expect("sweep");
        assert_eq!(outcome.delivered, 1);
        assert_eq!(host.delivered()[0].text, "newer");
    }

    #[tokio::test]
    async fn a_sweep_advances_the_watermark_to_the_newest_seen() {
        let host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);
        host.seed_history(
            "D1",
            vec![history_message("200.1", "U9", "a"), history_message("300.2", "U9", "b")],
        );

        sweep(&host, &host, "acme").await.expect("sweep");
        assert_eq!(host.watermark("acme", "D1"), Some("300.2".to_owned()));
    }

    /// Without paging, a conversation that exceeds one page inside a poll
    /// interval loses the surplus: the sweep would advance the watermark
    /// past messages it never fetched, and nothing would ever report it.
    #[tokio::test]
    async fn a_sweep_reads_every_history_page_before_advancing() {
        let host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);
        host.seed_history_pages(
            "D1",
            vec![
                vec![history_message("100.1", "U9", "one")],
                vec![history_message("200.2", "U9", "two")],
                vec![history_message("300.3", "U9", "three")],
            ],
        );

        sweep(&host, &host, "acme").await.expect("sweep");
        let texts: Vec<_> = host.delivered().into_iter().map(|m| m.text).collect();
        assert_eq!(texts, vec!["one".to_owned(), "two".to_owned(), "three".to_owned()]);
        assert_eq!(
            host.watermark("acme", "D1"),
            Some("300.3".to_owned()),
            "the cursor clears every page that was read",
        );
    }

    #[tokio::test]
    async fn a_rate_limit_is_reported_rather_than_retried_inline() {
        let mut host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);
        host.fail_with_rate_limit(Duration::from_secs(7));

        let outcome = sweep(&host, &host, "acme").await.expect("a 429 is not an error");
        assert_eq!(outcome.rate_limited, Some(Duration::from_secs(7)));
    }

    #[tokio::test]
    async fn a_message_in_a_thread_pulls_its_replies_and_dedupes_the_parent() {
        // The measured behaviour: replies page newest-first and repeat the
        // parent on every page, so the parent arrives once per page without
        // dedupe.
        let host =
            FakeHost::with_subscriptions(vec![sub_channel("acme", "C1", SlackWatchMode::All)]);
        let mut root = history_message("100.0", "U9", "root");
        root.reply_count = 2;
        host.seed_history("C1", vec![root]);
        host.seed_replies(
            "C1",
            "100.0",
            vec![
                history_message("100.0", "U9", "root"),
                history_message("200.1", "U8", "reply one"),
                history_message("100.0", "U9", "root"),
                history_message("300.2", "U8", "reply two"),
            ],
        );

        sweep(&host, &host, "acme").await.expect("sweep");
        let texts: Vec<_> = host.delivered().into_iter().map(|m| m.text).collect();
        assert_eq!(
            texts,
            vec!["root".to_owned(), "reply one".to_owned(), "reply two".to_owned()],
            "the parent must not be delivered once per page",
        );
    }

    #[tokio::test]
    async fn the_pump_stops_when_the_shutdown_fires() {
        let host = Arc::new(FakeHost::with_subscriptions(vec![sub_dm("acme")]));
        let (tx, rx) = oneshot::channel();
        let pump = tokio::spawn(run_workspace_pump(host.clone(), "acme".to_owned(), 3600, rx));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tx.send(()).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(2), pump)
            .await
            .expect("the pump must not outlive its shutdown")
            .expect("no panic");
        assert_eq!(host.connected(), Some(false), "the pump marks itself disconnected on exit");
    }

    #[test]
    fn next_interval_takes_the_rate_limit_delay_when_one_was_reported() {
        assert_eq!(
            next_interval(Duration::from_secs(30), Some(Duration::from_secs(7))),
            Duration::from_secs(7),
        );
        assert_eq!(next_interval(Duration::from_secs(30), None), Duration::from_secs(30));
        assert_eq!(
            next_interval(Duration::from_secs(30), Some(Duration::from_secs(600))),
            Duration::from_secs(60),
            "a hostile Retry-After is capped",
        );
    }

    #[test]
    fn a_token_never_reaches_the_debug_output() {
        let client = SlackClient::new(reqwest::Client::new(), "xoxp-supersecret".to_owned());
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("supersecret"), "token leaked: {rendered}");
    }
}

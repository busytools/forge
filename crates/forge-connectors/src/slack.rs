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
    SlackBookmark, SlackConversation, SlackFile, SlackFollowedThread, SlackMessage, SlackPin,
    SlackSearchMatch, SlackSubscription, SlackSubscriptionTarget, SlackThreadOwner, SlackUser,
    SlackWatchMode,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// What the pump needs from the workspace. The connector holds no state
/// of its own, so the subscription set, the watermarks and the liveness
/// flag all live behind this.
pub trait SlackHost: Send + Sync {
    /// A ready API for this workspace, or an error when the workspace is
    /// not configured. The host builds it, so the token stays with the
    /// workspace and the connector never asks for the credential itself.
    /// The trait object is what the pump drives, so a test host can hand
    /// back a double instead of a live client.
    fn client(&self, workspace: &str, timeout: Duration) -> Result<Arc<dyn SlackApi>, String>;

    /// The authenticated user's id for a workspace, resolved at boot.
    /// Needed to recognise `<@U...>` mentions.
    fn user_id(&self, workspace: &str) -> Option<String>;

    /// The subscriptions scoped to one workspace.
    fn subscriptions(&self, workspace: &str) -> Vec<SlackSubscription>;

    /// The last timestamp delivered for a conversation, or `None` when it
    /// has never been swept. An error means the cursor could not be read:
    /// the caller skips the conversation rather than sweeping it from the
    /// beginning, which would deliver its whole history as new.
    fn watermark(&self, workspace: &str, conversation: &str) -> Result<Option<String>, String>;

    /// Record the last timestamp delivered. A `ts` is a string with a
    /// microsecond fraction; never round-trip it through a number.
    fn set_watermark(&self, workspace: &str, conversation: &str, ts: &str);

    /// The threads tracked for a conversation, each with the owners still
    /// subscribed to it. The host prunes while listing: an owner whose
    /// subscription is gone is dropped from the record, a row's last owner
    /// deletion removes the row, and a row idle past the host's drop age
    /// never comes back.
    fn followed_threads(&self, workspace: &str, conversation: &str) -> Vec<SlackFollowedThread>;

    /// The last reply `ts` seen in a thread, or `None` when it has none
    /// yet. An error means the cursor could not be read: the caller skips
    /// the thread this tick rather than walking it from the parent.
    fn thread_watermark(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
    ) -> Result<Option<String>, String>;

    /// Advance a thread's reply cursor, stored as the string Slack sent.
    fn set_thread_watermark(&self, workspace: &str, conversation: &str, parent_ts: &str, ts: &str);

    /// Track a thread from a delivered message, owned by the matching
    /// subscription. A new row's cursor starts at `since` - the delivered
    /// message's own ts, not the parent's: the parent of a mention can be
    /// weeks old, and a cursor seeded from it would idle-drop the thread
    /// before its first reply walk. Following again by another owner adds
    /// that owner and keeps the cursor.
    fn follow_thread(
        &self,
        workspace: &str,
        conversation: &str,
        parent_ts: &str,
        owner: SlackThreadOwner,
        since: &str,
    );

    /// Liveness for the Inspector's status line.
    fn set_connected(&self, workspace: &str, connected: bool);

    /// Hand one matched message to its subscriber's session. Returns
    /// whether it was handed off: a `false` means the caller did not
    /// advance the conversation cursor past this message, so the next
    /// sweep re-delivers it instead of losing it.
    fn deliver(&self, subscription: &SlackSubscription, message: &SlackMessage) -> bool;

    /// Called after a mention is delivered, so the conversation it came
    /// from is watched too and the agent can reply into it rather than
    /// only seeing the ping. Returns whether a record was added: the host
    /// does nothing when that owner already covers the conversation.
    fn auto_subscribe(&self, workspace: &str, message: &SlackMessage) -> bool;
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
        oldest: Option<&str>,
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
    /// `search.messages`, always timestamp-ordered. The cursor follows a
    /// previous page when one sweep has more hits than one page holds.
    async fn search_messages(
        &self,
        query: &str,
        count: u32,
        cursor: Option<&str>,
    ) -> Result<SearchPage, SlackError>;
    /// `users.info` for one user id.
    async fn user_info(&self, user: &str) -> Result<SlackUser, SlackError>;
    /// `pins.list` for one conversation.
    async fn pins(&self, channel: &str) -> Result<Vec<SlackPin>, SlackError>;
    /// `bookmarks.list` for one conversation.
    async fn bookmarks(&self, channel: &str) -> Result<Vec<SlackBookmark>, SlackError>;
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
        oldest: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError> {
        SlackClient::replies(self, channel, ts, oldest, limit, cursor).await
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
        cursor: Option<&str>,
    ) -> Result<SearchPage, SlackError> {
        SlackClient::search_messages(self, query, count, cursor).await
    }

    async fn user_info(&self, user: &str) -> Result<SlackUser, SlackError> {
        SlackClient::user_info(self, user).await
    }

    async fn pins(&self, channel: &str) -> Result<Vec<SlackPin>, SlackError> {
        SlackClient::pins(self, channel).await
    }

    async fn bookmarks(&self, channel: &str) -> Result<Vec<SlackBookmark>, SlackError> {
        SlackClient::bookmarks(self, channel).await
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
        SlackSubscriptionTarget::Conversation { id, mode, .. } => {
            id == &conversation.id
                && match mode {
                    SlackWatchMode::All => true,
                    SlackWatchMode::MentionsOnly => mentions(text, user_id),
                }
        }
        // Swept by search, never by conversation.
        SlackSubscriptionTarget::Mentions => false,
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
        SlackSubscriptionTarget::Mentions => false,
    })
}

/// The thread a delivered message anchors: its own thread when it is a
/// reply, the thread it starts when it is a parent that already has
/// replies, and nothing for a plain top-level message - or every message
/// would grow the tracked set.
fn followed_parent_of(message: &SlackHistoryMessage) -> Option<String> {
    message.thread_ts.clone().or_else(|| (message.reply_count > 0).then(|| message.ts.clone()))
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

#[derive(Debug, Clone)]
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
#[derive(Debug, Default)]
pub(crate) struct SweepOutcome {
    pub delivered: usize,
    /// Set when a call was throttled, so the caller backs off rather than
    /// hammering. A 429 is a report, not an error.
    pub rate_limited: Option<Duration>,
    /// A conversation is gone from Slack's side: the pump folds this into
    /// the glyph write, because the sweep returning `Ok` must not read as
    /// a connected row over a conversation that can never deliver again.
    pub conversation_gone: bool,
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

#[cfg(test)]
mod is_newer_tests {
    use super::is_newer;

    /// Slack ts strings share the fixed-width seconds prefix, so string
    /// order matches time order only while that holds. The disagreeing
    /// pair pins the comparison as string order: numerically 1700000000.1
    /// equals 1700000000.10, and a numeric compare would call the newer
    /// cursor "not newer" and silently skip real messages.
    #[test]
    fn string_order_survives_trailing_zeros_a_numeric_compare_does_not() {
        assert!(
            is_newer("1700000000.000100", Some("1700000000.0001")),
            "string order, not numeric - these are equal as f64",
        );
        assert!(!is_newer("1700000000.0001", Some("1700000000.000100")));
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
    oldest: Option<&str>,
) -> Result<Vec<SlackHistoryMessage>, SlackError> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = api.replies(channel, ts, oldest, SWEEP_LIMIT, cursor.as_deref()).await?;
        out.extend(page.messages);
        pages += 1;
        match page.next_cursor {
            Some(next) if pages < MAX_REPLY_PAGES => cursor = Some(next),
            Some(_) => {
                tracing::warn!(
                    target: "forge_connectors::slack",
                    channel,
                    parent = %ts,
                    pages,
                    "conversations.replies kept handing back a cursor; stopping the walk",
                );
                return Ok(out);
            }
            None => return Ok(out),
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
    if subscriptions.is_empty() {
        return Ok(SweepOutcome { delivered: 0, ..Default::default() });
    }
    let user_id = host.user_id(workspace).unwrap_or_default();
    if user_id.is_empty() {
        // The own-message filter needs the id, and the agent's own posts
        // go out as the user - sweeping without it would echo those
        // replies back into the session. Fail quiet-but-visible rather
        // than sweep half-blind; the host retries the id in the
        // background.
        tracing::warn!(
            target: "forge_connectors::slack",
            workspace,
            "slack user id unresolved; the sweep is skipped this tick",
        );
        return Ok(SweepOutcome { delivered: 0, ..Default::default() });
    }

    let mut conversations = match api.list_conversations().await {
        Ok(conversations) => conversations,
        Err(SlackError::RateLimited { retry_after, .. }) => {
            return Ok(SweepOutcome {
                delivered: 0,
                rate_limited: Some(retry_after),
                ..Default::default()
            });
        }
        Err(error) => return Err(error),
    };
    // A subscription can name a conversation the token's user never joined:
    // a mention pulled us into a public channel, and users.conversations is
    // membership-scoped. Those arrive from the subscription set itself, or
    // the headline mention case dies after its first delivery.
    for subscription in &subscriptions {
        if let SlackSubscriptionTarget::Conversation { id, .. } = &subscription.target
            && !conversations.iter().any(|conversation| &conversation.id == id)
        {
            conversations.push(SlackConversation {
                id: id.clone(),
                name: None,
                is_channel: false,
                is_private: false,
                is_im: false,
                is_mpim: false,
                is_archived: false,
                user: None,
                purpose: None,
                topic: None,
            });
        }
    }

    let mut delivered = 0;
    let mut conversation_gone = false;
    for conversation in &conversations {
        if !targets(&subscriptions, conversation) {
            continue;
        }
        let watermark = match host.watermark(workspace, &conversation.id) {
            Ok(watermark) => watermark,
            Err(error) => {
                // "Could not read the cursor" is not "never swept": sweeping
                // from the beginning would deliver the channel's whole
                // history as new. Skip it for this tick.
                tracing::warn!(
                    target: "forge_connectors::slack",
                    workspace,
                    conversation = %conversation.id,
                    %error,
                    "reading the slack watermark failed; skipping the conversation this tick",
                );
                continue;
            }
        };

        let history = match fetch_history(api, &conversation.id, watermark.as_deref()).await {
            Ok(history) => history,
            Err(SlackError::RateLimited { retry_after, .. }) => {
                return Ok(SweepOutcome {
                    delivered,
                    rate_limited: Some(retry_after),
                    ..Default::default()
                });
            }
            // One conversation's failure is not the workspace's: the
            // sweep continues with the conversations after it. The cursor
            // stays where it was, so nothing is skipped.
            Err(error) => {
                let gone = matches!(&error, SlackError::Api { error: api_error, .. }
                    if api_error == "channel_not_found" || api_error == "invalid_channel_id");
                // A DM id a user token can never read - Slackbot's
                // conversation answers channel_not_found forever - is a
                // Slack quirk, not a dead channel, so it earns no glyph.
                if gone && (conversation.is_im || conversation.is_mpim) {
                    tracing::info!(
                        target: "forge_connectors::slack",
                        workspace,
                        conversation = %conversation.id,
                        %error,
                        "slack withholds this DM's history from user tokens; skipping it every tick",
                    );
                    continue;
                }
                // A conversation deleted on Slack's side is permanent. The
                // flag rides the outcome: the pump owns the glyph, and a
                // write here would be clobbered by its Ok arm.
                if gone {
                    conversation_gone = true;
                }
                tracing::warn!(
                    target: "forge_connectors::slack",
                    workspace,
                    conversation = %conversation.id,
                    %error,
                    "reading a conversation's history failed; skipping it this tick",
                );
                continue;
            }
        };

        // The DM class covers conversations that cannot be pre-seeded per
        // id, so first sight baselines instead of replaying: the newest
        // fetched ts becomes the cursor and the back catalogue is not new.
        // Logged, because the same shape hides a named conversation whose
        // subscribe-time cursor write failed - a swallowed backlog. That
        // is still the right direction: refusing to sweep would deliver
        // nothing forever, and replaying the backlog would spam every
        // session watching it.
        if watermark.is_none() {
            if let Some(newest) = history.iter().map(|message| message.ts.as_str()).max() {
                tracing::info!(
                    target: "forge_connectors::slack",
                    workspace,
                    conversation = %conversation.id,
                    cursor = %newest,
                    "no cursor for this conversation; baselining at the newest message seen",
                );
                host.set_watermark(workspace, &conversation.id, newest);
            }
            continue;
        }

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
            let replies = match fetch_replies(api, &conversation.id, &parent, None).await {
                Ok(replies) => replies,
                Err(SlackError::RateLimited { retry_after, .. }) => {
                    return Ok(SweepOutcome {
                        delivered,
                        rate_limited: Some(retry_after),
                        ..Default::default()
                    });
                }
                // One conversation's failure is not the workspace's: the
                // sweep continues with the conversations after it.
                Err(error) => {
                    tracing::warn!(
                        target: "forge_connectors::slack",
                        workspace,
                        conversation = %conversation.id,
                        parent = %parent,
                        %error,
                        "reading a thread's replies failed; skipping them this tick",
                    );
                    continue;
                }
            };
            for reply in replies {
                if is_newer(&reply.ts, watermark.as_deref()) && seen.insert(reply.ts.clone()) {
                    batch.push(reply);
                }
            }
        }

        let label =
            conversation.name.clone().or_else(|| conversation.user.clone()).unwrap_or_default();
        let mut newest_delivered: Option<String> = None;
        let mut batch_failed = false;
        for message in &batch {
            // Every subscription that wants this message gets it: a lead
            // and a worker watching the same conversation are two owners,
            // and find-first would starve whichever sorted second.
            let mut all_handed = true;
            for subscription in subscriptions.iter().filter(|subscription| {
                matches(
                    subscription,
                    conversation,
                    message.user.as_deref(),
                    &message.text,
                    &user_id,
                )
            }) {
                if host.deliver(
                    subscription,
                    &SlackMessage {
                        workspace: workspace.to_owned(),
                        conversation: conversation.id.clone(),
                        conversation_label: label.clone(),
                        ts: message.ts.clone(),
                        thread_ts: message.thread_ts.clone(),
                        user: message.user.clone(),
                        text: message.text.clone(),
                        files: message.files.clone(),
                    },
                ) {
                    if let Some(parent_ts) = followed_parent_of(message) {
                        host.follow_thread(
                            workspace,
                            &conversation.id,
                            &parent_ts,
                            SlackThreadOwner {
                                project: subscription.project.clone(),
                                team_role: subscription.team_role.clone(),
                            },
                            &message.ts,
                        );
                    }
                } else {
                    all_handed = false;
                }
            }
            if !all_handed {
                // Stop at the failure: the cursor holds at its previous
                // value rather than advancing past the message that did
                // not reach every matching subscription - under the wire's
                // newest-first pages, any advance here would jump ABOVE
                // the failure and lose everything at and below it. Dedupe
                // absorbs the re-delivery on the next sweep.
                batch_failed = true;
                break;
            }
            delivered += 1;
            // Pages arrive newest-first, so the watermark is the newest
            // delivered message, not the last one processed.
            if newest_delivered.as_deref().is_none_or(|current| message.ts.as_str() > current) {
                newest_delivered = Some(message.ts.clone());
            }
        }

        // Advanced last, and only over a batch that was handed off whole.
        if !batch_failed && let Some(newest) = newest_delivered {
            host.set_watermark(workspace, &conversation.id, &newest);
        }

        // Threads followed from earlier deliveries. Replies never appear
        // in history and the parent ages past the conversation watermark,
        // so each tracked thread is walked on its own cursor.
        for thread in host.followed_threads(workspace, &conversation.id) {
            let cursor = match host.thread_watermark(workspace, &conversation.id, &thread.parent_ts)
            {
                Ok(cursor) => cursor,
                Err(error) => {
                    tracing::warn!(
                        target: "forge_connectors::slack",
                        workspace,
                        conversation = %conversation.id,
                        parent = %thread.parent_ts,
                        %error,
                        "reading a thread cursor failed; skipping the thread this tick",
                    );
                    continue;
                }
            };
            let replies =
                match fetch_replies(api, &conversation.id, &thread.parent_ts, cursor.as_deref())
                    .await
                {
                    Ok(replies) => replies,
                    Err(SlackError::RateLimited { retry_after, .. }) => {
                        return Ok(SweepOutcome {
                            delivered,
                            rate_limited: Some(retry_after),
                            ..Default::default()
                        });
                    }
                    // One thread's failure is not the workspace's: the
                    // walk continues with the threads after it, and the
                    // cursor stays where it was.
                    Err(error) => {
                        tracing::warn!(
                            target: "forge_connectors::slack",
                            workspace,
                            conversation = %conversation.id,
                            parent = %thread.parent_ts,
                            %error,
                            "walking a followed thread failed; skipping it this tick",
                        );
                        continue;
                    }
                };
            let mut newest_reply: Option<String> = None;
            let mut walk_failed = false;
            for reply in replies {
                // Slack repeats the parent on every page of the walk.
                if reply.ts == thread.parent_ts
                    || !is_newer(&reply.ts, cursor.as_deref())
                    || !seen.insert(reply.ts.clone())
                {
                    continue;
                }
                // Replies never appear in history, so this walk is the one
                // path that sees the agent's own answers to the threads it
                // anchors - and they must not come back, or an agent
                // answering the echo answers itself.
                if reply.user.as_deref() == Some(user_id.as_str()) {
                    continue;
                }
                let mut all_handed = true;
                for owner in &thread.owners {
                    // Delivery routes by the owner fields alone, so any
                    // subscription of the same owner carries the reply.
                    let Some(subscription) = subscriptions.iter().find(|subscription| {
                        subscription.project == owner.project
                            && subscription.team_role == owner.team_role
                    }) else {
                        all_handed = false;
                        continue;
                    };
                    if !host.deliver(
                        subscription,
                        &SlackMessage {
                            workspace: workspace.to_owned(),
                            conversation: conversation.id.clone(),
                            conversation_label: label.clone(),
                            ts: reply.ts.clone(),
                            thread_ts: Some(thread.parent_ts.clone()),
                            user: reply.user.clone(),
                            text: reply.text.clone(),
                            files: reply.files.clone(),
                        },
                    ) {
                        all_handed = false;
                    }
                }
                if !all_handed {
                    // Replies arrive newest-first, so any advance here
                    // would jump ABOVE the reply that did not reach every
                    // owner and never fetch it again. The cursor holds at
                    // its previous value; dedupe absorbs the re-delivery.
                    walk_failed = true;
                    break;
                }
                delivered += 1;
                // Pages arrive newest-first, so the cursor is the newest
                // delivered reply, not the last one processed.
                if newest_reply.as_deref().is_none_or(|current| reply.ts.as_str() > current) {
                    newest_reply = Some(reply.ts.clone());
                }
            }
            if !walk_failed && let Some(newest) = newest_reply {
                host.set_thread_watermark(workspace, &conversation.id, &thread.parent_ts, &newest);
            }
        }
    }

    Ok(SweepOutcome { delivered, conversation_gone, ..Default::default() })
}

/// The watermark key for a workspace's whole mention stream. One cursor,
/// not one per conversation: both sides share this constant so a typo
/// cannot silently reset it and re-deliver everything.
pub const MENTION_CURSOR: &str = "__mentions__";

/// Mentions fetched per search. The workspace's newest, since the query is
/// timestamp-sorted.
const MENTION_SEARCH_COUNT: u32 = 20;

/// A cursor Slack hands back unchanged must not spin the mention walk
/// forever, the same guard the other walks carry.
const MAX_SEARCH_PAGES: usize = 200;

/// Sweep the workspace's mention stream. Returns at once when no
/// `Mentions` target exists, so a workspace with only conversation
/// subscriptions never spends a search call on every tick.
pub(crate) async fn sweep_mentions(
    host: &dyn SlackHost,
    api: &dyn SlackApi,
    workspace: &str,
) -> Result<SweepOutcome, SlackError> {
    let subscriptions = host.subscriptions(workspace);
    // Every Mentions target gets the hit: a lead and a worker each asking
    // to be told are two owners, and find-first would starve whichever
    // sorted second. Same-owner re-delivery is absorbed by the
    // destination dedupe.
    let mention_subscriptions: Vec<&SlackSubscription> = subscriptions
        .iter()
        .filter(|s| matches!(s.target, SlackSubscriptionTarget::Mentions))
        .collect();
    if mention_subscriptions.is_empty() {
        return Ok(SweepOutcome { delivered: 0, ..Default::default() });
    }
    // Without the authenticated user's id there is no token to search
    // for, and no way to tell the user's own messages from anyone else's.
    // Fail quiet-but-visible rather than sweeping half-blind.
    let Some(user_id) = host.user_id(workspace).filter(|id| !id.is_empty()) else {
        tracing::warn!(
            target: "forge_connectors::slack",
            workspace,
            "slack user id unresolved; the mention sweep is skipped this tick",
        );
        return Ok(SweepOutcome { delivered: 0, ..Default::default() });
    };

    let watermark = match host.watermark(workspace, MENTION_CURSOR) {
        Ok(watermark) => watermark,
        Err(error) => {
            tracing::warn!(
                target: "forge_connectors::slack",
                workspace,
                %error,
                "reading the mention cursor failed; skipping the mention sweep this tick",
            );
            return Ok(SweepOutcome { delivered: 0, ..Default::default() });
        }
    };
    let query = format!("<@{user_id}>");

    // Follow the search cursor while the page comes back full: a page
    // holds the NEWEST hits, so advancing the cursor past a full page
    // would silently discard everything older on it. Catch-up costs extra
    // calls only while a backlog exists, which is exactly when it matters.
    let mut delivered = 0;
    let mut newest: Option<String> = None;
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = match api.search_messages(&query, MENTION_SEARCH_COUNT, cursor.as_deref()).await
        {
            Ok(page) => page,
            Err(SlackError::RateLimited { retry_after, .. }) => {
                // The cursor stays where it was: advancing it here would
                // drop the older undelivered pages behind it. Re-delivery
                // is the cheap failure.
                return Ok(SweepOutcome {
                    delivered,
                    rate_limited: Some(retry_after),
                    ..Default::default()
                });
            }
            Err(error) => return Err(error),
        };
        pages += 1;

        for hit in &page.matches {
            // The query is the literal mention token, so it matches the
            // user's own outgoing messages too - and every reply the agent
            // posts that quotes one. Without this the sweep self-echoes.
            if hit.user.as_deref() == Some(user_id.as_str()) {
                continue;
            }
            if !is_newer(&hit.ts, watermark.as_deref()) {
                continue;
            }
            let message = SlackMessage {
                workspace: workspace.to_owned(),
                conversation: hit.conversation_id.clone(),
                conversation_label: hit
                    .conversation_name
                    .clone()
                    .unwrap_or_else(|| hit.conversation_id.clone()),
                ts: hit.ts.clone(),
                thread_ts: hit.thread_ts.clone(),
                user: hit.user.clone(),
                text: hit.text.clone(),
                files: hit.files.clone(),
            };
            let mut all_handed = true;
            for subscription in &mention_subscriptions {
                if !host.deliver(subscription, &message) {
                    all_handed = false;
                }
            }
            if !all_handed {
                // Hits arrive newest-first, so a failed delivery means
                // everything behind it is undelivered too: hold the
                // cursor where it was, skip the auto-subscribe, and let
                // the next tick retry the whole window.
                return Ok(SweepOutcome { delivered, ..Default::default() });
            }
            // Ved's ask: a mention pulls the agent into the conversation, so
            // the reply back and forth does not need another subscription.
            host.auto_subscribe(workspace, &message);
            // A mention inside a thread anchors that thread the same way,
            // once per owner that received it: the back and forth
            // continues under the parent, not the channel.
            for subscription in &mention_subscriptions {
                if let Some(parent_ts) = hit.thread_ts.clone() {
                    host.follow_thread(
                        workspace,
                        &hit.conversation_id,
                        &parent_ts,
                        SlackThreadOwner {
                            project: subscription.project.clone(),
                            team_role: subscription.team_role.clone(),
                        },
                        &hit.ts,
                    );
                }
            }
            delivered += 1;
            if newest.as_deref().is_none_or(|current| hit.ts.as_str() > current) {
                newest = Some(hit.ts.clone());
            }
        }

        match page.next_cursor {
            Some(next) if pages < MAX_SEARCH_PAGES => cursor = Some(next),
            Some(_) => {
                tracing::warn!(
                    target: "forge_connectors::slack",
                    workspace,
                    pages,
                    "search.messages kept handing back a cursor; stopping the mention walk",
                );
                break;
            }
            None => break,
        }
    }

    if let Some(newest) = newest {
        host.set_watermark(workspace, MENTION_CURSOR, &newest);
    }

    Ok(SweepOutcome { delivered, ..Default::default() })
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
        match sweep(host.as_ref(), client.as_ref(), &workspace).await {
            Ok(outcome) => {
                // A dead conversation rides the outcome so the Ok arm
                // writes the glyph down; an unconditional write here
                // would flip it back up in the same tick.
                host.set_connected(&workspace, !outcome.conversation_gone);
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
        // The mention stream is one search per workspace, not one call per
        // conversation, so it costs the same whether one channel is watched
        // or fifty. It returns at once when nothing subscribes to mentions.
        match sweep_mentions(host.as_ref(), client.as_ref(), &workspace).await {
            Ok(outcome) => {
                if let Some(delay) = outcome.rate_limited {
                    // The conversation sweep may already have asked for a
                    // longer wait; the longer backoff wins.
                    wait = wait.max(next_interval(interval, Some(delay)));
                }
            }
            Err(error) => {
                // The conversation sweep alone decides the glyph, so a
                // mention stream failing every tick (a token without
                // search scope, say) would read connected forever. When
                // anyone subscribes to mentions, a failing mention sweep
                // is exactly the condition the row is for.
                if host
                    .subscriptions(&workspace)
                    .iter()
                    .any(|sub| matches!(sub.target, SlackSubscriptionTarget::Mentions))
                {
                    host.set_connected(&workspace, false);
                }
                tracing::warn!(
                    target: "forge_connectors::slack",
                    workspace = %workspace,
                    %error,
                    "slack mention sweep failed",
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
    /// Files shared on the message, ids and all.
    pub files: Vec<SlackFile>,
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
    #[serde(default)]
    files: Vec<SlackFile>,
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
            files: message.files,
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

/// One page of `search.messages`, newest first.
#[derive(Debug, Default)]
pub struct SearchPage {
    pub matches: Vec<SlackSearchMatch>,
    pub next_cursor: Option<String>,
}

/// Split out of the async path so the decode is testable without HTTP.
fn decode_search(body: &str) -> Result<SearchPage, SlackError> {
    #[derive(Default, Deserialize)]
    struct SearchMessages {
        #[serde(default)]
        matches: Vec<RawMatch>,
        #[serde(default)]
        next_cursor: String,
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
        /// The author's id, which the mention sweep's own-message filter
        /// needs - `username` is only a handle.
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        thread_ts: Option<String>,
        #[serde(default)]
        files: Vec<SlackFile>,
    }
    #[derive(Deserialize)]
    struct RawChannel {
        id: String,
        #[serde(default)]
        name: Option<String>,
    }

    let payload: SearchPayload = decode_envelope("search.messages", body)?;
    let next_cursor =
        (!payload.messages.next_cursor.is_empty()).then_some(payload.messages.next_cursor);
    let matches = payload
        .messages
        .matches
        .into_iter()
        .map(|hit| SlackSearchMatch {
            ts: hit.ts,
            text: hit.text,
            conversation_id: hit.channel.id,
            conversation_name: hit.channel.name,
            username: hit.username,
            user: hit.user,
            thread_ts: hit.thread_ts,
            files: hit.files,
        })
        .collect();
    Ok(SearchPage { matches, next_cursor })
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

/// Split out of the async path so the decode is testable without HTTP.
fn decode_pins(body: &str) -> Result<Vec<SlackPin>, SlackError> {
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        items: Vec<SlackPin>,
    }
    let wrapper: Wrapper = decode_envelope("pins.list", body)?;
    Ok(wrapper.items)
}

/// Split out of the async path so the decode is testable without HTTP.
fn decode_bookmarks(body: &str) -> Result<Vec<SlackBookmark>, SlackError> {
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        bookmarks: Vec<SlackBookmark>,
    }
    let wrapper: Wrapper = decode_envelope("bookmarks.list", body)?;
    Ok(wrapper.bookmarks)
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
        oldest: Option<&str>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<MessagePage, SlackError> {
        let mut params = vec![
            ("channel", channel.to_owned()),
            ("ts", ts.to_owned()),
            ("limit", limit.to_string()),
        ];
        if let Some(oldest) = oldest {
            params.push(("oldest", oldest.to_owned()));
        }
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
        cursor: Option<&str>,
    ) -> Result<SearchPage, SlackError> {
        let mut params = search_params(query, count);
        if let Some(cursor) = cursor {
            params.push(("cursor", cursor.to_owned()));
        }
        let body = self.call_text("search.messages", &params).await?;
        decode_search(&body)
    }

    /// `users.info` for one user id.
    pub async fn user_info(&self, user: &str) -> Result<SlackUser, SlackError> {
        let params = vec![("user", user.to_owned())];
        let body = self.call_text("users.info", &params).await?;
        decode_user(&body)
    }

    /// `pins.list` for one conversation.
    pub async fn pins(&self, channel: &str) -> Result<Vec<SlackPin>, SlackError> {
        let params = vec![("channel", channel.to_owned())];
        let body = self.call_text("pins.list", &params).await?;
        decode_pins(&body)
    }

    /// `bookmarks.list` for one conversation. The param is `channel_id`
    /// here against `channel` on pins.list.
    pub async fn bookmarks(&self, channel: &str) -> Result<Vec<SlackBookmark>, SlackError> {
        let params = vec![("channel_id", channel.to_owned())];
        let body = self.call_text("bookmarks.list", &params).await?;
        decode_bookmarks(&body)
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
    use forge_primitives::slack::SlackThreadRecord;
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

    #[test]
    fn purpose_and_topic_decode_and_a_dm_omits_them() {
        let page = decode_conversations_page(
            r#"{"ok":true,"channels":[{"id":"C1","name":"general","is_channel":true,
                "purpose":{"value":"Deploy chatter","creator":"U1","last_set":1700000000},
                "topic":{"value":"release coordination"}}]}"#,
        )
        .expect("decodes");
        let channel = &page.conversations[0];
        assert_eq!(
            channel.purpose.as_ref().expect("a channel carries its purpose").value,
            "Deploy chatter",
        );
        assert_eq!(
            channel.topic.as_ref().expect("a channel carries its topic").value,
            "release coordination",
            "only the value is read; the metadata stays behind",
        );

        let dm = decode_conversations_page(r#"{"ok":true,"channels":[{"id":"D1","is_im":true}]}"#)
            .expect("decodes");
        assert!(
            dm.conversations[0].purpose.is_none() && dm.conversations[0].topic.is_none(),
            "a DM carries neither object",
        );
    }

    #[test]
    fn pins_list_decodes_the_pinned_rows() {
        let body = r#"{"ok":true,"items":[
            {"type":"message","created":1700000000,"created_by":"U1",
             "message":{"type":"message","ts":"1700000000.000100","user":"U2",
                        "text":"the pinned text"}}]}"#;
        let pins = decode_pins(body).expect("decodes");
        assert_eq!(pins.len(), 1, "one row per pinned message");
        let message = pins[0].message.as_ref().expect("a pin carries its message");
        assert_eq!(message.ts, "1700000000.000100", "the ts survives verbatim");
        assert_eq!(message.text, "the pinned text");
        assert_eq!(message.user.as_deref(), Some("U2"));
        assert_eq!(pins[0].created_by.as_deref(), Some("U1"), "who pinned it comes along");
    }

    #[test]
    fn bookmarks_list_decodes_the_saved_links() {
        let body = r#"{"ok":true,"bookmarks":[
            {"id":"Bk1","type":"link","channel_id":"C1","title":"Runbook",
             "link":"https://example.com","date_created":1700000000}]}"#;
        let bookmarks = decode_bookmarks(body).expect("decodes");
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].id, "Bk1");
        assert_eq!(bookmarks[0].title.as_deref(), Some("Runbook"));
        assert_eq!(bookmarks[0].link.as_deref(), Some("https://example.com"));
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
            purpose: None,
            topic: None,
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
            purpose: None,
            topic: None,
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
            name: None,
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
            name: None,
            mode: SlackWatchMode::All,
        })];
        let channel = conversation_channel("C1", "general");
        assert!(!wants(&subs, &channel, Some("U1"), "my own words", "U1"));
    }

    /// A mention target sweeps by search rather than by conversation, so
    /// the per-conversation path must never claim it - claiming it would
    /// deliver the same mention twice.
    #[test]
    fn a_mention_target_is_not_a_conversation_target() {
        let subs = vec![sub_for(SlackSubscriptionTarget::Mentions)];
        let channel = conversation_channel("C1", "general");
        assert!(!wants(&subs, &channel, Some("U9"), "no mention here", "U1"));
        assert!(!wants(&subs, &channel, Some("U9"), "a mention <@U1>", "U1"));
    }

    #[test]
    fn an_unsubscribed_conversation_is_never_wanted() {
        let subs = vec![sub_for(SlackSubscriptionTarget::Conversation {
            id: "C1".to_owned(),
            name: None,
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
        subscriptions: std::sync::Mutex<Vec<SlackSubscription>>,
        user_id_slot: std::sync::Mutex<Option<String>>,
        /// The fake's own API trait object, set post-construction so the
        /// pump's `client()` drives the seeded double instead of a live
        /// client.
        api_self: std::sync::Mutex<Option<Arc<dyn SlackApi>>>,
        rate_limited: Option<Duration>,
        conversations: std::sync::Mutex<Vec<SlackConversation>>,
        history: std::sync::Mutex<HashMap<String, Vec<Vec<SlackHistoryMessage>>>>,
        /// Seeded failures: channel -> the error history returns.
        history_errors: std::sync::Mutex<HashMap<String, SlackError>>,
        replies: std::sync::Mutex<HashMap<String, Vec<Vec<SlackHistoryMessage>>>>,
        watermarks: std::sync::Mutex<HashMap<String, String>>,
        connected: std::sync::Mutex<Option<bool>>,
        delivered: std::sync::Mutex<Vec<SlackMessage>>,
        /// The owner of each successful delivery, index-aligned with
        /// `delivered`.
        delivered_owners: std::sync::Mutex<Vec<SlackThreadOwner>>,
        search: std::sync::Mutex<Vec<Vec<SlackSearchMatch>>>,
        search_calls: std::sync::Mutex<usize>,
        auto_subscribed: std::sync::Mutex<Vec<String>>,
        deliver_failures: std::sync::Mutex<std::collections::HashSet<usize>>,
        deliver_attempts: std::sync::Mutex<usize>,
        /// Followed threads keyed `workspace/conversation/parent`.
        threads: std::sync::Mutex<HashMap<String, SlackThreadRecord>>,
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
            purpose: None,
            topic: None,
        }
    }

    impl FakeHost {
        fn with_subscriptions(subscriptions: Vec<SlackSubscription>) -> Self {
            Self {
                subscriptions: std::sync::Mutex::new(subscriptions),
                user_id_slot: std::sync::Mutex::new(Some("U1".to_owned())),
                ..Self::default()
            }
        }

        /// Expose the fake's own API impl for the pump's `client()`. Must
        /// be called on the Arc before a pump test runs.
        fn set_api_self(me: &Arc<Self>) {
            *me.api_self.lock().expect("lock") = Some(Arc::clone(me) as Arc<dyn SlackApi>);
        }

        fn seed_history(&self, channel: &str, messages: Vec<SlackHistoryMessage>) {
            self.seed_history_pages(channel, vec![messages]);
        }

        /// Seed history WITHOUT registering the conversation in the
        /// membership list, the shape of a channel the token's user is
        /// not a member of.
        fn seed_history_unlisted(&self, channel: &str, messages: Vec<SlackHistoryMessage>) {
            self.history.lock().expect("lock").insert(channel.to_owned(), vec![messages]);
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
            self.seed_replies_pages(channel, parent, vec![messages]);
        }

        /// Seed several reply pages. The fake serves them in order,
        /// handing back the next index as the cursor, mirroring Slack's
        /// newest-first pages that repeat the parent.
        fn seed_replies_pages(
            &self,
            channel: &str,
            parent: &str,
            pages: Vec<Vec<SlackHistoryMessage>>,
        ) {
            self.replies.lock().expect("lock").insert(format!("{channel}/{parent}"), pages);
        }

        /// Make history for `channel` fail with a seeded error.
        fn fail_history_with(&self, channel: &str, error: SlackError) {
            self.history_errors.lock().expect("lock").insert(channel.to_owned(), error);
        }

        /// Resolve the user id late, or leave it unresolved with `None` -
        /// the shape of a boot whose auth probe has not come back yet.
        fn set_user_id(&self, id: Option<String>) {
            *self.user_id_slot.lock().expect("lock") = id;
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

        fn seed_search(&self, matches: Vec<SlackSearchMatch>) {
            self.seed_search_pages(vec![matches]);
        }

        /// Seed several pages. The fake serves them in order, handing back
        /// the next index as the cursor, so a paging walk is exercised.
        fn seed_search_pages(&self, pages: Vec<Vec<SlackSearchMatch>>) {
            *self.search.lock().expect("lock") = pages;
        }

        /// How many times `method` was called, for the tests that assert a
        /// sweep did or did not spend a call.
        fn count_for(&self, method: &str) -> usize {
            match method {
                "search.messages" => *self.search_calls.lock().expect("lock"),
                other => panic!("the fake does not count {other}"),
            }
        }

        fn auto_subscribed(&self) -> Vec<String> {
            self.auto_subscribed.lock().expect("lock").clone()
        }

        /// Make the Nth delivery attempt fail, so a test can assert how
        /// the sweep treats a partial failure.
        fn fail_delivery_at(&self, index: usize) {
            self.deliver_failures.lock().expect("lock").insert(index);
        }

        fn delivered_owners(&self) -> Vec<SlackThreadOwner> {
            self.delivered_owners.lock().expect("lock").clone()
        }

        fn thread_cursor(
            &self,
            workspace: &str,
            conversation: &str,
            parent: &str,
        ) -> Option<String> {
            self.threads
                .lock()
                .expect("lock")
                .get(&format!("{workspace}/{conversation}/{parent}"))
                .map(|record| record.cursor.clone())
        }
    }

    impl SlackHost for FakeHost {
        fn client(
            &self,
            _workspace: &str,
            _timeout: Duration,
        ) -> Result<Arc<dyn SlackApi>, String> {
            let exposed = self.api_self.lock().expect("lock").clone();
            exposed.ok_or("the fake exposes no API; call set_api_self first".to_owned())
        }

        fn user_id(&self, _workspace: &str) -> Option<String> {
            self.user_id_slot.lock().expect("lock").clone()
        }

        fn subscriptions(&self, _workspace: &str) -> Vec<SlackSubscription> {
            self.subscriptions.lock().expect("lock").clone()
        }

        fn watermark(&self, workspace: &str, conversation: &str) -> Result<Option<String>, String> {
            Ok(self
                .watermarks
                .lock()
                .expect("lock")
                .get(&format!("{workspace}/{conversation}"))
                .cloned())
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

        fn deliver(&self, subscription: &SlackSubscription, message: &SlackMessage) -> bool {
            let attempt = {
                let mut attempts = self.deliver_attempts.lock().expect("lock");
                *attempts += 1;
                *attempts - 1
            };
            if self.deliver_failures.lock().expect("lock").contains(&attempt) {
                return false;
            }
            self.delivered.lock().expect("lock").push(message.clone());
            self.delivered_owners.lock().expect("lock").push(SlackThreadOwner {
                project: subscription.project.clone(),
                team_role: subscription.team_role.clone(),
            });
            true
        }

        fn followed_threads(
            &self,
            workspace: &str,
            conversation: &str,
        ) -> Vec<SlackFollowedThread> {
            let prefix = format!("{workspace}/{conversation}/");
            self.threads
                .lock()
                .expect("lock")
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(key, record)| SlackFollowedThread {
                    parent_ts: key
                        .rsplit('/')
                        .next()
                        .expect("the parent is the last part")
                        .to_owned(),
                    owners: record.owners.clone(),
                })
                .collect()
        }

        fn thread_watermark(
            &self,
            workspace: &str,
            conversation: &str,
            parent_ts: &str,
        ) -> Result<Option<String>, String> {
            Ok(self
                .threads
                .lock()
                .expect("lock")
                .get(&format!("{workspace}/{conversation}/{parent_ts}"))
                .map(|record| record.cursor.clone()))
        }

        fn set_thread_watermark(
            &self,
            workspace: &str,
            conversation: &str,
            parent_ts: &str,
            ts: &str,
        ) {
            let mut threads = self.threads.lock().expect("lock");
            let record = threads
                .entry(format!("{workspace}/{conversation}/{parent_ts}"))
                .or_insert_with(|| SlackThreadRecord { cursor: String::new(), owners: Vec::new() });
            record.cursor = ts.to_owned();
        }

        fn follow_thread(
            &self,
            workspace: &str,
            conversation: &str,
            parent_ts: &str,
            owner: SlackThreadOwner,
            since: &str,
        ) {
            let mut threads = self.threads.lock().expect("lock");
            let record = threads
                .entry(format!("{workspace}/{conversation}/{parent_ts}"))
                .or_insert_with(|| {
                    // The real host starts a new thread at the caller's
                    // since, so the first replies walk reaches everything
                    // after the message that anchored it.
                    SlackThreadRecord { cursor: since.to_owned(), owners: Vec::new() }
                });
            if !record.owners.contains(&owner) {
                record.owners.push(owner);
            }
        }

        fn auto_subscribe(&self, workspace: &str, message: &SlackMessage) -> bool {
            let already =
                self.subscriptions.lock().expect("lock").iter().any(|sub| match &sub.target {
                    SlackSubscriptionTarget::Conversation { id, .. } => id == &message.conversation,
                    SlackSubscriptionTarget::DirectMessages | SlackSubscriptionTarget::Mentions => {
                        false
                    }
                });
            if already {
                return false;
            }
            // Mirrors the real host: the new subscription's cursor starts
            // at the mention, so the channel's past is not swept.
            self.watermarks
                .lock()
                .expect("lock")
                .insert(format!("{}/{}", workspace, message.conversation), message.ts.clone());
            self.subscriptions.lock().expect("lock").push(SlackSubscription {
                id: uuid::Uuid::new_v4(),
                workspace: workspace.to_owned(),
                project: "forge".to_owned(),
                team_role: None,
                target: SlackSubscriptionTarget::Conversation {
                    id: message.conversation.clone(),
                    name: None,
                    mode: SlackWatchMode::All,
                },
                created_at: std::time::SystemTime::UNIX_EPOCH,
            });
            self.auto_subscribed.lock().expect("lock").push(message.conversation.clone());
            true
        }
    }

    #[async_trait::async_trait]
    impl SlackApi for FakeHost {
        async fn auth_test(&self) -> Result<AuthTest, SlackError> {
            Ok(AuthTest {
                team: "Test".to_owned(),
                user: "tester".to_owned(),
                team_id: "T1".to_owned(),
                user_id: self.user_id_slot.lock().expect("lock").clone().unwrap_or_default(),
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
            if let Some(error) = self.history_errors.lock().expect("lock").get(channel) {
                return Err(error.clone());
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
            _oldest: Option<&str>,
            _limit: u32,
            cursor: Option<&str>,
        ) -> Result<MessagePage, SlackError> {
            let pages = self.replies.lock().expect("lock");
            let Some(seeded) = pages.get(&format!("{channel}/{ts}")) else {
                return Ok(MessagePage { messages: Vec::new(), next_cursor: None });
            };
            let index = cursor.and_then(|cursor| cursor.parse::<usize>().ok()).unwrap_or(0);
            Ok(MessagePage {
                messages: seeded.get(index).cloned().unwrap_or_default(),
                next_cursor: (index + 1 < seeded.len()).then(|| (index + 1).to_string()),
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
            query: &str,
            _count: u32,
            cursor: Option<&str>,
        ) -> Result<SearchPage, SlackError> {
            *self.search_calls.lock().expect("lock") += 1;
            let seeded = self.search.lock().expect("lock");
            let index = cursor.and_then(|cursor| cursor.parse::<usize>().ok()).unwrap_or(0);
            let _ = query;
            let page = seeded.get(index).cloned().unwrap_or_default();
            Ok(SearchPage {
                matches: page,
                next_cursor: (index + 1 < seeded.len()).then(|| (index + 1).to_string()),
            })
        }

        async fn user_info(&self, _user: &str) -> Result<SlackUser, SlackError> {
            Ok(SlackUser { id: String::new(), name: String::new(), real_name: None, tz: None })
        }

        async fn pins(&self, _channel: &str) -> Result<Vec<SlackPin>, SlackError> {
            Ok(Vec::new())
        }

        async fn bookmarks(&self, _channel: &str) -> Result<Vec<SlackBookmark>, SlackError> {
            Ok(Vec::new())
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

    /// A file-share mention carries its files in the hit: the ids are
    /// what slack__attachment fetches with, so delivery loses nothing on
    /// a no-text file share.
    #[test]
    fn a_search_hit_carries_its_files() {
        let body = r#"{"ok":true,"messages":{"matches":[
            {"ts":"100.1","text":"","channel":{"id":"C1"},"user":"U9",
             "files":[{"id":"F1","name":"notes.txt","url_private":"https://files/x"}]}]}}"#;
        let page = decode_search(body).expect("decodes");
        assert_eq!(page.matches.len(), 1);
        assert_eq!(page.matches[0].files.len(), 1, "the file rides on the hit");
        assert_eq!(page.matches[0].files[0].id, "F1");
    }

    #[test]
    fn a_search_decodes_matches_and_their_channel_names() {
        let body = r#"{"ok":true,"messages":{"total":2,"matches":[
            {"ts":"100.1","text":"hello","channel":{"id":"C1","name":"general"},"username":"ved"},
            {"ts":"200.2","text":"world","channel":{"id":"D1","is_im":true},"username":"other"}]}}"#;
        let page = decode_search(body).expect("decodes");
        let matches = &page.matches;
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].conversation_id, "C1");
        assert_eq!(matches[0].conversation_name.as_deref(), Some("general"));
        assert_eq!(matches[0].username.as_deref(), Some("ved"));
        assert_eq!(matches[1].conversation_name, None, "a DM has no name");
        assert_eq!(page.next_cursor, None);
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
            files: Vec::new(),
        }
    }

    fn sub_dm(workspace: &str) -> SlackSubscription {
        let mut sub = sub_for(SlackSubscriptionTarget::DirectMessages);
        sub.workspace = workspace.to_owned();
        sub
    }

    fn sub_mentions(workspace: &str) -> SlackSubscription {
        let mut sub = sub_for(SlackSubscriptionTarget::Mentions);
        sub.workspace = workspace.to_owned();
        sub
    }

    fn sub_conversation_owned_by(
        workspace: &str,
        project: &str,
        team_role: Option<&str>,
        id: &str,
    ) -> SlackSubscription {
        let mut sub = sub_for(SlackSubscriptionTarget::Conversation {
            id: id.to_owned(),
            name: None,
            mode: SlackWatchMode::All,
        });
        sub.workspace = workspace.to_owned();
        sub.project = project.to_owned();
        sub.team_role = team_role.map(str::to_owned);
        sub
    }

    fn thread_owner(project: &str, team_role: Option<&str>) -> SlackThreadOwner {
        SlackThreadOwner { project: project.to_owned(), team_role: team_role.map(str::to_owned) }
    }

    fn search_match(ts: &str, conversation: &str, text: &str) -> SlackSearchMatch {
        SlackSearchMatch {
            ts: ts.to_owned(),
            text: text.to_owned(),
            conversation_id: conversation.to_owned(),
            conversation_name: None,
            username: Some("U9".to_owned()),
            user: Some("U9".to_owned()),
            thread_ts: None,
            files: Vec::new(),
        }
    }

    /// The whole reason this target is affordable: one search covers the
    /// workspace, where a per-conversation mention poll would blow the
    /// budget.
    #[tokio::test]
    async fn a_mention_sweep_makes_exactly_one_search_call_for_the_workspace() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        host.seed_search(vec![search_match("200.1", "C1", "ping <@U1>")]);

        sweep_mentions(&host, &host, "acme").await.expect("sweep");
        assert_eq!(host.count_for("search.messages"), 1);
    }

    #[tokio::test]
    async fn only_mention_matches_newer_than_the_cursor_are_delivered() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        host.set_watermark("acme", MENTION_CURSOR, "150.0");
        host.seed_search(vec![
            search_match("200.1", "C1", "newer <@U1>"),
            search_match("100.1", "C1", "older <@U1>"),
        ]);

        let outcome = sweep_mentions(&host, &host, "acme").await.expect("sweep");
        assert_eq!(outcome.delivered, 1);
        assert_eq!(host.delivered()[0].text, "newer <@U1>");
    }

    #[tokio::test]
    async fn the_mention_cursor_advances_to_the_newest_ts() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        host.seed_search(vec![
            search_match("200.1", "C1", "a <@U1>"),
            search_match("300.2", "C2", "b <@U1>"),
        ]);

        sweep_mentions(&host, &host, "acme").await.expect("sweep");
        assert_eq!(
            host.watermark("acme", MENTION_CURSOR).expect("watermark"),
            Some("300.2".to_owned())
        );
    }

    /// If this ever runs for a workspace with no mention target, it spends
    /// a search call for nothing on every tick and the cost of the feature
    /// silently becomes per-workspace-per-poll.
    #[tokio::test]
    async fn a_mention_sweep_with_no_targets_does_not_search_at_all() {
        let host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);

        sweep_mentions(&host, &host, "acme").await.expect("sweep");
        assert_eq!(
            host.count_for("search.messages"),
            0,
            "an unsubscribed workspace must not spend a call",
        );
    }

    /// Without a cursor seeded at the mention, the first sweep of the new
    /// subscription delivers the newest page of the channel's history as
    /// if it were all new.
    #[tokio::test]
    async fn a_freshly_auto_subscribed_conversation_starts_from_the_mention() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        host.seed_search(vec![search_match("200.1", "C1", "ping <@U1>")]);
        sweep_mentions(&host, &host, "acme").await.expect("sweep");

        // The channel's own past still exists; none of it may arrive.
        host.seed_history(
            "C1",
            vec![
                history_message("250.1", "U9", "after the mention"),
                history_message("150.1", "U9", "before the mention"),
            ],
        );
        sweep(&host, &host, "acme").await.expect("sweep");

        let texts: Vec<_> = host.delivered().into_iter().map(|m| m.text).collect();
        assert_eq!(
            texts,
            vec!["ping <@U1>".to_owned(), "after the mention".to_owned()],
            "the subscription starts at the mention, not at the channel's history",
        );
    }

    /// A page holds the newest hits, so a backlog wider than one page is
    /// lost if the cursor advances past the page. The walk follows the
    /// cursor and delivers all of them.
    #[tokio::test]
    async fn a_mention_backlog_wider_than_one_page_is_delivered_without_loss() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        let all: Vec<SlackSearchMatch> = (0..25)
            .rev()
            .map(|n| search_match(&format!("1{n:02}.1"), "C1", "ping <@U1>"))
            .collect();
        let newest = all[0].ts.clone();
        let oldest = all[24].ts.clone();
        let (page_one, page_two) = all.split_at(20);
        host.seed_search_pages(vec![page_one.to_vec(), page_two.to_vec()]);

        let outcome = sweep_mentions(&host, &host, "acme").await.expect("sweep");
        assert_eq!(outcome.delivered, 25, "every mention in the backlog arrives");
        assert_eq!(host.watermark("acme", MENTION_CURSOR).expect("watermark"), Some(newest));
        assert_ne!(
            host.watermark("acme", MENTION_CURSOR).expect("watermark"),
            Some(oldest),
            "nothing is skipped"
        );
    }

    /// The search query is the literal mention token, so it matches the
    /// user's own outgoing messages as well - and every reply the agent
    /// posts that quotes one. With the measured indexing lag that is a
    /// self-echo loop.
    #[tokio::test]
    async fn the_mentions_own_author_is_never_delivered() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        let mut own = search_match("300.2", "C1", "mine <@U1>");
        own.user = Some("U1".to_owned());
        host.seed_search(vec![search_match("200.1", "C1", "theirs <@U1>"), own]);

        let outcome = sweep_mentions(&host, &host, "acme").await.expect("sweep");
        assert_eq!(outcome.delivered, 1, "only the other author's mention arrives");
        let texts: Vec<_> = host.delivered().into_iter().map(|m| m.text).collect();
        assert!(!texts.iter().any(|t| t == "mine <@U1>"), "no self-echo: {texts:?}");
    }

    /// A dispatch failure must leave the cursor where it was, so the next
    /// sweep re-fetches the whole window; dedupe absorbs what already
    /// reached a session.
    #[tokio::test]
    async fn a_failed_delivery_holds_the_cursor_at_its_previous_value() {
        let host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);
        host.set_watermark("acme", "D1", "050.0");
        host.seed_history(
            "D1",
            // The wire's order: newest first.
            vec![
                history_message("300.1", "U9", "newest"),
                history_message("200.1", "U9", "fails"),
                history_message("100.1", "U9", "oldest"),
            ],
        );
        host.fail_delivery_at(1);

        let outcome = sweep(&host, &host, "acme").await.expect("sweep");
        assert_eq!(outcome.delivered, 1, "the sweep stops at the failed message");
        assert_eq!(host.delivered()[0].text, "newest");
        assert_eq!(
            host.watermark("acme", "D1"),
            Ok(Some("050.0".to_owned())),
            "the cursor holds below the failure instead of jumping past it",
        );
    }

    #[tokio::test]
    async fn a_mention_creates_a_conversation_subscription() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        host.seed_search(vec![search_match("200.1", "C1", "ping <@U1>")]);

        sweep_mentions(&host, &host, "acme").await.expect("sweep");
        assert_eq!(host.auto_subscribed(), vec!["C1".to_owned()]);
    }

    #[tokio::test]
    async fn an_already_watched_conversation_is_not_subscribed_twice() {
        let host = FakeHost::with_subscriptions(vec![
            sub_mentions("acme"),
            sub_channel("acme", "C1", SlackWatchMode::All),
        ]);
        host.seed_search(vec![search_match("200.1", "C1", "ping <@U1>")]);

        sweep_mentions(&host, &host, "acme").await.expect("sweep");
        assert!(host.auto_subscribed().is_empty(), "C1 is already watched");
    }

    fn sub_channel(workspace: &str, id: &str, mode: SlackWatchMode) -> SlackSubscription {
        let mut sub =
            sub_for(SlackSubscriptionTarget::Conversation { id: id.to_owned(), name: None, mode });
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
        host.set_watermark("acme", "D1", "050.0");
        host.seed_history(
            "D1",
            // The wire's order: newest first.
            vec![history_message("300.2", "U9", "newest"), history_message("200.1", "U9", "older")],
        );

        sweep(&host, &host, "acme").await.expect("sweep");
        assert_eq!(host.watermark("acme", "D1").expect("watermark"), Some("300.2".to_owned()));
    }

    /// Without paging, a conversation that exceeds one page inside a poll
    /// interval loses the surplus: the sweep would advance the watermark
    /// past messages it never fetched, and nothing would ever report it.
    #[tokio::test]
    async fn a_sweep_reads_every_history_page_before_advancing() {
        let host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);
        host.set_watermark("acme", "D1", "050.0");
        host.seed_history_pages(
            "D1",
            // The wire's order: newest page first.
            vec![
                vec![history_message("300.3", "U9", "three")],
                vec![history_message("200.2", "U9", "two")],
                vec![history_message("100.1", "U9", "one")],
            ],
        );

        sweep(&host, &host, "acme").await.expect("sweep");
        let texts: Vec<_> = host.delivered().into_iter().map(|m| m.text).collect();
        assert_eq!(texts, vec!["three".to_owned(), "two".to_owned(), "one".to_owned()]);
        assert_eq!(
            host.watermark("acme", "D1").expect("watermark"),
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
        host.set_watermark("acme", "C1", "050.0");
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

    /// The pin that would have caught the original Critical: the trigger
    /// sweep delivers a parent, and a reply that arrives on a LATER sweep
    /// still reaches the owning session - the parent has aged past the
    /// conversation watermark by then, so only the thread cursor reaches
    /// it.
    #[tokio::test]
    async fn a_reply_arriving_on_the_sweep_after_the_trigger_delivers_to_the_owning_session() {
        let host = FakeHost::with_subscriptions(vec![sub_conversation_owned_by(
            "acme",
            "forge",
            Some("tester"),
            "C1",
        )]);
        let mut parent = history_message("100.0", "U9", "trigger");
        parent.reply_count = 2;
        host.set_watermark("acme", "C1", "050.0");
        host.seed_history("C1", vec![parent.clone()]);

        sweep(&host, &host, "acme").await.expect("trigger sweep");
        assert_eq!(host.delivered()[0].ts, "100.0", "the trigger itself is delivered");
        assert_eq!(host.thread_cursor("acme", "C1", "100.0"), Some("100.0".to_owned()));

        // The reply arrives afterwards. History has nothing new; only the
        // followed thread's own walk can reach it.
        host.seed_replies("C1", "100.0", vec![parent, history_message("150.0", "U8", "the reply")]);
        sweep(&host, &host, "acme").await.expect("follow-up sweep");

        let delivered = host.delivered();
        let replies: Vec<&SlackMessage> =
            delivered.iter().filter(|message| message.ts == "150.0").collect();
        assert_eq!(replies.len(), 1, "the reply is delivered once");
        assert_eq!(replies[0].thread_ts.as_deref(), Some("100.0"), "as a reply of its thread");
        assert_eq!(
            host.delivered_owners().last().map(|owner| owner.team_role.clone()),
            Some(Some("tester".to_owned())),
            "the reply reaches the OWNING session",
        );
        assert_eq!(
            delivered.iter().filter(|message| message.ts == "100.0").count(),
            1,
            "the repeated parent is deduped",
        );
        assert_eq!(
            host.thread_cursor("acme", "C1", "100.0"),
            Some("150.0".to_owned()),
            "the thread cursor advances over the delivered reply",
        );
    }

    /// Fan-out on threads delivers to every owner in the record, the way
    /// a conversation's batch delivers to every matching subscription -
    /// and one owner's failed delivery holds the cursor below the reply
    /// instead of losing it.
    #[tokio::test]
    async fn every_owner_gets_a_thread_reply_and_a_failure_holds_the_cursor() {
        let host = FakeHost::with_subscriptions(vec![
            sub_conversation_owned_by("acme", "forge", Some("a"), "C1"),
            sub_conversation_owned_by("acme", "forge", Some("b"), "C1"),
        ]);
        let mut parent = history_message("100.0", "U9", "trigger");
        parent.reply_count = 1;
        host.set_watermark("acme", "C1", "050.0");
        host.seed_history("C1", vec![parent.clone()]);
        sweep(&host, &host, "acme").await.expect("trigger sweep");
        assert_eq!(
            host.followed_threads("acme", "C1").first().map(|thread| thread.owners.len()),
            Some(2),
            "both sessions own the thread after both were delivered the trigger",
        );

        host.seed_replies("C1", "100.0", vec![parent, history_message("150.0", "U8", "the reply")]);
        // Attempts so far: the parent went to a then b. The reply's FIRST
        // owner delivery (a) fails, so the cursor must not advance even
        // though b receives it.
        host.fail_delivery_at(2);
        sweep(&host, &host, "acme").await.expect("follow-up sweep");

        assert_eq!(
            host.delivered().iter().filter(|message| message.ts == "150.0").count(),
            1,
            "the owner whose delivery succeeded still received the reply",
        );
        assert_eq!(
            host.thread_cursor("acme", "C1", "100.0"),
            Some("100.0".to_owned()),
            "the cursor stays below a reply that did not reach every owner",
        );
    }

    /// A plain top-level message anchors nothing, or every message would
    /// grow the tracked set.
    #[tokio::test]
    async fn a_plain_message_follows_no_thread() {
        let host = FakeHost::with_subscriptions(vec![sub_conversation_owned_by(
            "acme", "forge", None, "C1",
        )]);
        host.set_watermark("acme", "C1", "050.0");
        host.seed_history("C1", vec![history_message("100.0", "U9", "plain")]);

        sweep(&host, &host, "acme").await.expect("sweep");
        assert_eq!(host.delivered().len(), 1, "the message itself is delivered");
        assert!(
            host.followed_threads("acme", "C1").is_empty(),
            "a message with no thread and no replies anchors nothing",
        );
    }

    /// A mention inside a thread anchors that thread for the mention
    /// subscription's owner, so the back and forth continues under the
    /// parent.
    #[tokio::test]
    async fn a_mention_in_a_thread_anchors_that_thread() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        host.set_watermark("acme", MENTION_CURSOR, "100.0");
        let mut hit = search_match("150.0", "C1", "ping <@U1>");
        hit.thread_ts = Some("99.0".to_owned());
        host.seed_search(vec![hit]);

        sweep_mentions(&host, &host, "acme").await.expect("mention sweep");
        let threads = host.followed_threads("acme", "C1");
        assert_eq!(threads.len(), 1, "the thread of the mention is tracked");
        assert_eq!(threads[0].parent_ts, "99.0");
        assert_eq!(threads[0].owners, vec![thread_owner("forge", None)]);
        assert_eq!(
            host.thread_cursor("acme", "C1", "99.0"),
            Some("150.0".to_owned()),
            "the cursor starts at the mention's ts, never the weeks-old parent",
        );
    }

    /// Slack pages a thread newest-first and repeats the parent on every
    /// page (measured live at limit=3). The walk must page to the end,
    /// dedupe the parent on each page, and lose nothing at the boundary.
    #[tokio::test]
    async fn a_thread_walk_pages_to_the_end_deduping_the_parent() {
        let host = FakeHost::with_subscriptions(vec![sub_conversation_owned_by(
            "acme",
            "forge",
            Some("tester"),
            "C1",
        )]);
        let mut parent = history_message("100.0", "U9", "trigger");
        parent.reply_count = 3;
        host.set_watermark("acme", "C1", "050.0");
        host.seed_history("C1", vec![parent.clone()]);
        sweep(&host, &host, "acme").await.expect("trigger sweep");

        host.seed_replies_pages(
            "C1",
            "100.0",
            vec![
                vec![parent.clone(), history_message("300.0", "U8", "reply three")],
                vec![
                    parent,
                    history_message("200.0", "U8", "reply two"),
                    history_message("150.0", "U8", "reply one"),
                ],
            ],
        );
        sweep(&host, &host, "acme").await.expect("follow-up sweep");

        let delivered = host.delivered();
        let seen_ts = |ts: &str| delivered.iter().filter(|message| message.ts == ts).count();
        assert_eq!(seen_ts("100.0"), 1, "the parent is delivered once, never per page");
        assert_eq!(seen_ts("300.0"), 1, "the newest page's reply arrives");
        assert_eq!(seen_ts("200.0"), 1, "the older page's replies arrive too");
        assert_eq!(seen_ts("150.0"), 1, "nothing is lost at the page boundary");
        assert_eq!(
            host.thread_cursor("acme", "C1", "100.0"),
            Some("300.0".to_owned()),
            "the cursor lands on the newest reply",
        );
    }

    /// Replies never appear in history, so this walk is the one path that
    /// sees the agent's own answers to the threads it anchors - and they
    /// must not come back, or an agent answering the echo answers itself.
    #[tokio::test]
    async fn the_agents_own_replies_are_never_delivered_back_by_the_thread_walk() {
        let host = FakeHost::with_subscriptions(vec![sub_conversation_owned_by(
            "acme",
            "forge",
            Some("tester"),
            "C1",
        )]);
        let mut parent = history_message("100.0", "U9", "trigger");
        parent.reply_count = 2;
        host.set_watermark("acme", "C1", "050.0");
        host.seed_history("C1", vec![parent.clone()]);
        sweep(&host, &host, "acme").await.expect("trigger sweep");

        host.seed_replies(
            "C1",
            "100.0",
            vec![
                parent,
                history_message("150.0", "U1", "the agent's own reply"),
                history_message("140.0", "U8", "someone else's reply"),
            ],
        );
        sweep(&host, &host, "acme").await.expect("follow-up sweep");

        let delivered = host.delivered();
        assert!(
            !delivered.iter().any(|message| message.text == "the agent's own reply"),
            "the agent's own reply must not echo back: {delivered:?}",
        );
        assert!(
            delivered.iter().any(|message| message.text == "someone else's reply"),
            "and everyone else's replies still arrive",
        );
    }

    /// The thread walk's failure hold: replies arrive newest-first, so a
    /// failure on the OLDER reply means any cursor advance would jump
    /// above the reply that did not reach every owner.
    #[tokio::test]
    async fn a_thread_walk_failure_holds_the_cursor_at_its_previous_value() {
        let host = FakeHost::with_subscriptions(vec![sub_conversation_owned_by(
            "acme",
            "forge",
            Some("tester"),
            "C1",
        )]);
        let mut parent = history_message("100.0", "U9", "trigger");
        parent.reply_count = 2;
        host.set_watermark("acme", "C1", "050.0");
        host.seed_history("C1", vec![parent.clone()]);
        sweep(&host, &host, "acme").await.expect("trigger sweep");
        assert_eq!(host.thread_cursor("acme", "C1", "100.0"), Some("100.0".to_owned()));

        host.seed_replies(
            "C1",
            "100.0",
            vec![
                parent,
                history_message("300.0", "U8", "newer reply"),
                history_message("200.0", "U8", "older reply fails"),
            ],
        );
        // Attempts: the trigger (0), then the newer reply (1). The older
        // reply's delivery (2) fails.
        host.fail_delivery_at(2);
        sweep(&host, &host, "acme").await.expect("follow-up sweep");

        assert_eq!(
            host.thread_cursor("acme", "C1", "100.0"),
            Some("100.0".to_owned()),
            "the walk's cursor holds below the failed reply instead of jumping past it",
        );
    }

    /// Two owners asking to be told are two owners: every Mentions
    /// subscription gets the hit, not whichever sorted first.
    #[tokio::test]
    async fn a_mention_reaches_every_mentions_subscription() {
        let host = FakeHost::with_subscriptions(vec![
            sub_conversation_owned_by("acme", "forge", Some("a"), "C1"),
            {
                let mut sub = sub_mentions("acme");
                sub.team_role = Some("a".to_owned());
                sub
            },
            {
                let mut sub = sub_mentions("acme");
                sub.team_role = Some("b".to_owned());
                sub
            },
        ]);
        host.set_watermark("acme", MENTION_CURSOR, "100.0");
        host.seed_search(vec![search_match("150.0", "C9", "ping <@U1>")]);

        sweep_mentions(&host, &host, "acme").await.expect("sweep");
        let owners: Vec<Option<String>> =
            host.delivered_owners().iter().map(|owner| owner.team_role.clone()).collect();
        assert!(
            owners.iter().any(|role| role.as_deref() == Some("a")),
            "the first owner receives the mention: {owners:?}",
        );
        assert!(
            owners.iter().any(|role| role.as_deref() == Some("b")),
            "the second mention subscription is not starved: {owners:?}",
        );
    }

    /// Without the id there is no own-message filter, and the agent's own
    /// posts go out as the user: sweeping would echo those replies back.
    /// The sweep fails quiet-but-visible instead.
    #[tokio::test]
    async fn an_unresolved_user_id_skips_the_conversation_sweep() {
        let host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);
        host.set_user_id(None);
        host.seed_history("D1", vec![history_message("300.0", "U9", "pending")]);

        let outcome = sweep(&host, &host, "acme").await.expect("sweep");
        assert_eq!(outcome.delivered, 0, "nothing is delivered half-blind");
        assert!(host.delivered().is_empty(), "the pending window is not swept without the id");
        assert_eq!(
            host.watermark("acme", "D1").expect("watermark"),
            None,
            "and the cursor is untouched, so the next sweep picks the window up",
        );
    }

    /// A conversation deleted on Slack's side never delivers again, and
    /// the pump's Ok arm must not write the glyph straight back up after
    /// the sweep has flipped it down - pinned through the PUMP, where the
    /// clobbering happens, never through `sweep()` directly.
    #[tokio::test]
    async fn a_deleted_conversation_flips_the_glyph_down_through_the_pump() {
        let host = Arc::new(FakeHost::with_subscriptions(vec![sub_channel(
            "acme",
            "C1",
            SlackWatchMode::All,
        )]));
        FakeHost::set_api_self(&host);
        host.set_watermark("acme", "C1", "050.0");
        host.fail_history_with(
            "C1",
            SlackError::Api {
                method: "conversations.history".to_owned(),
                error: "channel_not_found".to_owned(),
                needed: None,
            },
        );

        let (tx, rx) = oneshot::channel();
        let pump = tokio::spawn(run_workspace_pump(host.clone(), "acme".to_owned(), 1, rx));
        // The pump's first tick fires after one interval; wait until the
        // glyph write has landed.
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if host.connected().is_some() {
                break;
            }
        }

        assert_eq!(
            host.connected(),
            Some(false),
            "a dead conversation must not read as a connected row after a wired tick",
        );

        tx.send(()).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(2), pump)
            .await
            .expect("the pump must not outlive its shutdown")
            .expect("no panic");
    }

    /// The Slackbot conversation answers channel_not_found to a user
    /// token forever, a Slack quirk rather than a dead channel: a
    /// workspace whose only sweep failure is that DM reads connected,
    /// and its other DMs still deliver. Pinned through the PUMP, where
    /// the glyph write happens.
    #[tokio::test]
    async fn a_slackbot_dm_channel_not_found_leaves_the_glyph_connected() {
        let host = Arc::new(FakeHost::with_subscriptions(vec![sub_dm("acme")]));
        FakeHost::set_api_self(&host);
        // The Slackbot-shaped DM: listed, watermarked, never readable.
        host.seed_history("D1", vec![]);
        host.set_watermark("acme", "D1", "050.0");
        host.fail_history_with(
            "D1",
            SlackError::Api {
                method: "conversations.history".to_owned(),
                error: "channel_not_found".to_owned(),
                needed: None,
            },
        );
        // A readable DM beside it, so the tick provably sweeps instead
        // of passing on an unexercised outcome.
        host.seed_history("D2", vec![history_message("200.0", "U9", "hello from a dm")]);
        host.set_watermark("acme", "D2", "100.0");

        let (tx, rx) = oneshot::channel();
        let pump = tokio::spawn(run_workspace_pump(host.clone(), "acme".to_owned(), 1, rx));
        for _ in 0..60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if host.connected().is_some() {
                break;
            }
        }

        assert!(
            !host.delivered().is_empty(),
            "the tick ran and delivered the readable DM: {:?}",
            host.delivered(),
        );
        assert_eq!(
            host.connected(),
            Some(true),
            "the unreadable DM is a known quirk, not a dead connector",
        );

        tx.send(()).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(2), pump)
            .await
            .expect("the pump must not outlive its shutdown")
            .expect("no panic");
    }

    #[derive(Clone, Default)]
    struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The unreadable-DM skip is expected, not a failure: the log names
    /// the conversation and states the reason, so the tick line is
    /// never silent.
    #[tokio::test]
    async fn the_unreadable_dm_skip_logs_its_reason() {
        let host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);
        host.seed_history("D1", vec![]);
        host.set_watermark("acme", "D1", "050.0");
        host.fail_history_with(
            "D1",
            SlackError::Api {
                method: "conversations.history".to_owned(),
                error: "channel_not_found".to_owned(),
                needed: None,
            },
        );

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let outcome = sweep(&host, &host, "acme").await.expect("sweep");
        assert!(!outcome.conversation_gone, "the quirk does not flip the glyph");

        let log = String::from_utf8_lossy(&capture.0.lock().expect("lock")).into_owned();
        assert!(log.contains("D1"), "the skip names the conversation: {log}");
        assert!(log.contains("withholds this DM's history"), "the skip states its reason: {log}");
    }

    /// The headline mention case: a mention from a public channel the
    /// user has not joined auto-subscribes it, and users.conversations
    /// (membership-scoped) never lists it - the sweep must still fetch
    /// its messages and thread replies.
    #[tokio::test]
    async fn an_auto_subscribed_channel_the_user_never_joined_is_swept() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        host.set_watermark("acme", MENTION_CURSOR, "100.0");
        host.seed_search(vec![search_match("150.0", "C9", "ping <@U1>")]);
        sweep_mentions(&host, &host, "acme").await.expect("mention sweep");
        assert_eq!(host.auto_subscribed(), vec!["C9".to_owned()], "the mention subscribes C9");

        // C9 is absent from the membership list; only the subscription
        // set names it.
        host.seed_history_unlisted("C9", vec![history_message("200.0", "U8", "the follow-up")]);
        sweep(&host, &host, "acme").await.expect("conversation sweep");

        let delivered = host.delivered();
        assert!(
            delivered.iter().any(|message| message.text == "the follow-up"),
            "the non-member channel's messages still arrive: {delivered:?}",
        );
    }

    /// A DM-class subscription cannot be pre-seeded per conversation, so
    /// first sight baselines: the newest fetched ts becomes the cursor
    /// and the existing DM history is not delivered as new.
    #[tokio::test]
    async fn a_dm_class_subscription_baselines_on_first_sight_instead_of_replaying() {
        let host = FakeHost::with_subscriptions(vec![sub_dm("acme")]);
        // The class covers D1, which the fake registers when history is
        // seeded; its cursor is unset, the shape a fresh DM arrives in.
        host.seed_history(
            "D1",
            // The wire's order: newest first.
            vec![
                history_message("300.1", "U9", "old one"),
                history_message("200.1", "U9", "older two"),
            ],
        );

        sweep(&host, &host, "acme").await.expect("first sweep");
        assert!(
            host.delivered().is_empty(),
            "the DM's existing history is baselined, not replayed: {:?}",
            host.delivered(),
        );
        assert_eq!(
            host.watermark("acme", "D1").expect("watermark"),
            Some("300.1".to_owned()),
            "the cursor starts at the newest message seen",
        );

        host.seed_history(
            "D1",
            vec![
                history_message("400.1", "U9", "new one"),
                history_message("300.1", "U9", "old one"),
            ],
        );
        sweep(&host, &host, "acme").await.expect("second sweep");
        let texts: Vec<_> = host.delivered().into_iter().map(|m| m.text).collect();
        assert_eq!(texts, vec!["new one".to_owned()], "only what follows the baseline arrives");
    }

    /// A failed mention delivery holds the mention cursor and skips the
    /// auto-subscribe, so the next tick retries the whole window instead
    /// of losing the trigger message.
    #[tokio::test]
    async fn a_failed_mention_delivery_holds_the_cursor_and_the_follow() {
        let host = FakeHost::with_subscriptions(vec![sub_mentions("acme")]);
        host.set_watermark("acme", MENTION_CURSOR, "100.0");
        host.seed_search(vec![search_match("150.0", "C9", "ping <@U1>")]);
        host.fail_delivery_at(0);

        let outcome = sweep_mentions(&host, &host, "acme").await.expect("sweep");
        assert_eq!(outcome.delivered, 0, "the failed mention is not counted");
        assert_eq!(
            host.watermark("acme", MENTION_CURSOR).expect("watermark"),
            Some("100.0".to_owned()),
            "the cursor holds at its previous value",
        );
        assert!(
            host.auto_subscribed().is_empty(),
            "a mention that did not reach a session subscribes nothing",
        );
    }

    /// Two owners on one conversation: a partial fan-out failure holds
    /// the cursor at its previous value, the same semantics the thread
    /// walk enforces.
    #[tokio::test]
    async fn a_conversation_fanout_failure_holds_the_cursor_for_every_owner() {
        let host = FakeHost::with_subscriptions(vec![
            sub_conversation_owned_by("acme", "forge", Some("a"), "C1"),
            sub_conversation_owned_by("acme", "forge", Some("b"), "C1"),
        ]);
        host.set_watermark("acme", "C1", "050.0");
        host.seed_history("C1", vec![history_message("300.0", "U9", "the message")]);
        // Attempts: a's delivery (0) succeeds, b's (1) fails.
        host.fail_delivery_at(1);

        sweep(&host, &host, "acme").await.expect("sweep");
        let delivered = host.delivered();
        assert_eq!(
            delivered.len(),
            1,
            "the owner whose delivery succeeded still received it: {delivered:?}",
        );
        assert_eq!(
            host.watermark("acme", "C1").expect("watermark"),
            Some("050.0".to_owned()),
            "the cursor holds below a message that did not reach every owner",
        );
    }

    #[tokio::test]
    async fn the_pump_stops_when_the_shutdown_fires() {
        let host = Arc::new(FakeHost::with_subscriptions(vec![sub_dm("acme")]));
        FakeHost::set_api_self(&host);
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

//! Slack connector shapes that cross a crate boundary: the `[[slack]]`
//! config entry an operator writes, the conversation shape
//! `users.conversations` reports, and the subscription record a session
//! creates.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One `[[slack]]` entry. The token is a credential, so it lives in
/// `forge.toml` rather than in the state DB beside the subscriptions.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackConfig {
    /// Tells workspaces apart in the MCP tools and the Inspector.
    pub workspace: String,
    /// User token, `xoxp-...`.
    pub token: String,
    /// Sweep interval for this workspace, in seconds.
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
}

fn default_poll_seconds() -> u64 {
    30
}

/// Hand-written because the token must never be printed, the same reason
/// `SlackClient` writes its own.
impl std::fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConfig")
            .field("workspace", &self.workspace)
            .field("token", &"[redacted]")
            .field("poll_seconds", &self.poll_seconds)
            .finish()
    }
}

/// Slack sends `purpose` and `topic` as objects carrying their text in
/// `value`, and omits both on a DM.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SlackConversationText {
    #[serde(default)]
    pub value: String,
}

/// A conversation as `users.conversations` reports it. Fields Slack omits
/// per conversation type default rather than failing the whole decode.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackConversation {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub is_channel: bool,
    #[serde(default)]
    pub is_private: bool,
    #[serde(default)]
    pub is_im: bool,
    #[serde(default)]
    pub is_mpim: bool,
    #[serde(default)]
    pub is_archived: bool,
    /// The other member's user id, present on a DM.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub purpose: Option<SlackConversationText>,
    #[serde(default)]
    pub topic: Option<SlackConversationText>,
}

/// What one subscription watches inside a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlackSubscriptionTarget {
    /// Every DM in the workspace, group DMs included - the DM class the
    /// design settled on, so a new DM needs no new subscription.
    DirectMessages,
    /// One conversation, with the mode that decides what reaches the session.
    /// `name` is the display name captured when the record was written;
    /// records from before it was captured decode as `None` and render
    /// the raw id.
    Conversation {
        id: String,
        #[serde(default)]
        name: Option<String>,
        mode: SlackWatchMode,
    },
    /// Every message in this workspace that mentions the user, including
    /// public channels they are not in. Not private channels they are not
    /// in, which they could not read anyway.
    ///
    /// Swept by one search per workspace rather than by conversation, so
    /// the per-conversation matching path never claims it - letting both
    /// claim a mention would deliver it twice.
    Mentions,
}

/// What a conversation subscription lets through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlackWatchMode {
    /// Every message.
    All,
    /// Only messages that mention the user.
    MentionsOnly,
}

/// One watched target, owned by one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackSubscription {
    pub id: Uuid,
    /// The `[[slack]]` label this watches.
    pub workspace: String,
    /// Owning project. Named `project` and `team_role` to match
    /// `GotifySubscription`; a rename would decode to `None` and silently
    /// reroute every worker's subscription to the lead.
    pub project: String,
    pub team_role: Option<String>,
    pub target: SlackSubscriptionTarget,
    pub created_at: SystemTime,
}

/// A file as `files.info` reports it. `url_private` is a bearer fetch, not
/// a URL, and `name` is whatever the uploader chose to call it - untrusted
/// input as far as this code is concerned.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlackFile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url_private: String,
}

/// One hit from `search.messages`. `conversation_name` is absent on a DM,
/// which has no name rather than an empty one. `user` is the author's id,
/// which the mention sweep needs for the own-message filter - `username`
/// is only a handle. A file-share with no text is a real hit, so the
/// files ride along for delivery and `slack__attachment`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlackSearchMatch {
    pub ts: String,
    #[serde(default)]
    pub text: String,
    pub conversation_id: String,
    #[serde(default)]
    pub conversation_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub files: Vec<SlackFile>,
}

/// One user as `users.info` reports it. Every optional field is optional
/// because Slack omits fields per user type rather than sending nulls.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlackUser {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub real_name: Option<String>,
    #[serde(default)]
    pub tz: Option<String>,
}

/// One pinned message as `pins.list` reports it. Pins are messages, and
/// Slack omits the message object on any other row, which carries nothing
/// readable.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlackPin {
    #[serde(default)]
    pub created: u64,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub message: Option<SlackPinMessage>,
}

/// The message body of a pinned row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlackPinMessage {
    pub ts: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub text: String,
}

/// One channel bookmark as `bookmarks.list` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SlackBookmark {
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub link: Option<String>,
}

/// One session's ownership of a followed thread, named after the
/// `SlackSubscription` fields it is taken from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackThreadOwner {
    pub project: String,
    pub team_role: Option<String>,
}

/// What the store keeps about one followed thread: the last-seen reply
/// `ts` as the verbatim string Slack sent, and the sessions whose
/// subscriptions put the thread on their radar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlackThreadRecord {
    pub cursor: String,
    pub owners: Vec<SlackThreadOwner>,
}

/// One followed thread as the pump sees it, minus the cursor it reads
/// through its own port method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackFollowedThread {
    pub parent_ts: String,
    pub owners: Vec<SlackThreadOwner>,
}

/// A composed but unsent Slack message. The workspace holds it until the
/// user approves it in the dock prompt; nothing reaches Slack otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackDraft {
    pub id: Uuid,
    pub workspace: String,
    pub conversation: String,
    /// `None` posts a root message; `Some(ts)` replies into that thread.
    pub thread_ts: Option<String>,
    pub text: String,
    /// The MCP tool that composed the draft, so a held edit, reaction or
    /// upload does not read as `slack__post` in the session list.
    pub tool: String,
}

/// One message as the pump sees it, after matching. Carries what delivery
/// needs and nothing the wire happened to include.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackMessage {
    pub workspace: String,
    pub conversation: String,
    /// Conversation name for display; the DM partner's id when unnamed.
    /// Not `Option`, so a caller never has to invent a fallback.
    pub conversation_label: String,
    pub ts: String,
    /// `None` for a top-level message, the thread's parent `ts` otherwise.
    pub thread_ts: Option<String>,
    pub user: Option<String>,
    pub text: String,
    /// Files shared on the message. A file-share with no text is a real
    /// message, and the ids are what `slack__attachment` fetches with.
    pub files: Vec<SlackFile>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_never_reaches_the_config_debug_output() {
        let config = SlackConfig {
            workspace: "acme".to_owned(),
            token: "xoxp-supersecret".to_owned(),
            poll_seconds: 30,
        };
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("supersecret"), "token leaked: {rendered}");
        assert!(rendered.contains("acme"), "the label stays readable: {rendered}");
    }

    /// Records written before conversation names were captured carry no
    /// `name` key; dropping them at decode would silently unsubscribe
    /// every session on restart.
    #[test]
    fn a_target_recorded_without_a_name_still_decodes() {
        let old = r#"{"Conversation":{"id":"C0C0T5E6RM1","mode":"All"}}"#;
        let target: SlackSubscriptionTarget =
            serde_json::from_str(old).expect("the old shape decodes");
        assert_eq!(
            target,
            SlackSubscriptionTarget::Conversation {
                id: "C0C0T5E6RM1".to_owned(),
                name: None,
                mode: SlackWatchMode::All,
            },
            "a missing name decodes as None, never as a decode failure",
        );
    }
}

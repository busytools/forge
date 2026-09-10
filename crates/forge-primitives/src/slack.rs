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
}

/// What one subscription watches inside a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlackSubscriptionTarget {
    /// Every DM in the workspace, group DMs included - the DM class the
    /// design settled on, so a new DM needs no new subscription.
    DirectMessages,
    /// One conversation, with the mode that decides what reaches the session.
    Conversation { id: String, mode: SlackWatchMode },
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
}

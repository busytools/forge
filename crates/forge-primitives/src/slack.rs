//! Slack connector shapes that cross a crate boundary: the `[[slack]]`
//! config entry an operator writes, and the conversation shape
//! `users.conversations` reports.

use serde::Deserialize;

/// One `[[slack]]` entry. The token is a credential, so it lives in
/// `forge.toml` rather than in the state DB beside the subscriptions.
#[derive(Debug, Clone, Deserialize)]
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

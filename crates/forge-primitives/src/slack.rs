//! Slack connector shapes that cross a crate boundary: the `[[slack]]`
//! config entry an operator writes, and the conversation shape
//! `users.conversations` reports.

use serde::Deserialize;

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

//! Wire-shape type for a matched Gotify notification on its way into a
//! subscriber's chat.
//!
//! Mirrors [`crate::mcp::peers::types::WrappedPrompt`]: the workspace
//! threads the typed fields through `Command::DeliverGotifyMessage` +
//! `SessionUpdate::GotifyNotificationAppended`, and [`to_prose`] is the
//! single source of the user-turn prose that both the session's LLM
//! (via `Command::Prompt`) and the TUI chat echo (via
//! `forge-tui::ui::peer_block::detect_inbound`) consume. Workspace-
//! internal - only forge-workspace builds it and forge-tui reads it
//! through the protocol enums, so it stays out of forge-primitives
//! (logic-free wire types only).
//!
//! [`to_prose`]: GotifyNotification::to_prose

/// A matched Gotify notification resolved for delivery: the application
/// display name (resolved from the numeric appid, or the id as a string
/// when the app index hasn't seen it), title, message, and priority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GotifyNotification {
    pub app: String,
    pub title: String,
    pub message: String,
    pub priority: u8,
}

impl GotifyNotification {
    /// The user-turn prose injected into the subscriber's session. The
    /// format MUST match the prefix
    /// `forge-tui::ui::peer_block::detect_inbound` keys on to render the
    /// notification chat block.
    pub fn to_prose(&self) -> String {
        format!(
            "[Gotify - app '{}', priority {}]\n{}\n{}",
            self.app, self.priority, self.title, self.message,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_prose_matches_the_detect_inbound_prefix() {
        let n = GotifyNotification {
            app: "Backups".to_owned(),
            title: "Nightly backup complete".to_owned(),
            message: "All volumes backed up".to_owned(),
            priority: 3,
        };
        assert_eq!(
            n.to_prose(),
            "[Gotify - app 'Backups', priority 3]\nNightly backup complete\nAll volumes backed up",
        );
    }

    #[test]
    fn to_prose_preserves_a_multiline_message_body() {
        let n = GotifyNotification {
            app: "CI".to_owned(),
            title: "Build failed".to_owned(),
            message: "step 1 ok\nstep 2 failed".to_owned(),
            priority: 8,
        };
        assert_eq!(
            n.to_prose(),
            "[Gotify - app 'CI', priority 8]\nBuild failed\nstep 1 ok\nstep 2 failed",
        );
    }
}

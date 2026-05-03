//! Permission-mode enum used across the agent stack.
//!
//! Pre-bridge-collapse this module also held a `BridgeSession`
//! struct that accumulated per-session bookkeeping for the bridge
//! translation layer. Post collapse, that state lives in
//! `app::state::types::SessionTurnState` (App-side) and the worker
//! (when it needs the Connected event payload, computed locally).
//! All that's left here is `PermissionMode` itself.

/// Mirrors upstream's permission-mode enum — distinct from the
/// wire-string `current_mode_id` shipped on `ModeState`. Used to track
/// which modes are supported / runtime-unavailable per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    DontAsk,
    Auto,
    BypassPermissions,
}

impl PermissionMode {
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
            Self::Auto => "auto",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "default" | "ask" => Self::Default,
            "acceptEdits" | "accept_edits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "dontAsk" | "dont_ask" | "deny" => Self::DontAsk,
            "auto" => Self::Auto,
            "bypassPermissions" | "bypass_permissions" => Self::BypassPermissions,
            _ => return None,
        })
    }

    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::AcceptEdits => "Accept Edits",
            Self::Plan => "Plan",
            Self::DontAsk => "Don't Ask",
            Self::Auto => "Auto",
            Self::BypassPermissions => "Bypass Permissions",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_mode_round_trips_through_wire() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::DontAsk,
            PermissionMode::Auto,
            PermissionMode::BypassPermissions,
        ] {
            assert_eq!(PermissionMode::from_wire(mode.as_wire()), Some(mode));
        }
    }

    #[test]
    fn permission_mode_aliases() {
        assert_eq!(PermissionMode::from_wire("ask"), Some(PermissionMode::Default));
        assert_eq!(PermissionMode::from_wire("accept_edits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(PermissionMode::from_wire("dont_ask"), Some(PermissionMode::DontAsk));
        assert_eq!(PermissionMode::from_wire("deny"), Some(PermissionMode::DontAsk));
        assert_eq!(PermissionMode::from_wire("bypass_permissions"), Some(PermissionMode::BypassPermissions));
    }
}

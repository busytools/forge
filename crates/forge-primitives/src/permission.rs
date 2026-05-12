//! Canonical `PermissionMode` enum, used by:
//!  - `forge_sdk::Options` builder for `--permission-mode` CLI argv.
//!  - `forge_agent::state` for runtime mode tracking.
//!  - `forge_tui` for permission-mode UI / settings.
//!
//! Unified in Phase 0 of the MVVM refactor (#102). Previously there
//! were two enums — one in `forge_primitives::options` with `Ask` /
//! `DenyPermissions` variant names, and one in `forge_agent::state`
//! with `Default` / `DontAsk` variant names. They mapped to the same
//! wire strings; the variant names diverged for historical reasons.

use serde::{Deserialize, Serialize};

/// Which permission flow the `claude` binary should use for tool
/// invocations. Mirrors the upstream CLI's six-variant set.
///
/// Variant `Ask` maps to wire `"default"`; the rest match wire strings
/// 1:1 via `#[serde(rename = ...)]`:
///  - `Ask` -> "default"
///  - `AcceptEdits` -> "acceptEdits"
///  - `Plan` -> "plan"
///  - `DontAsk` -> "dontAsk"
///  - `Auto` -> "auto"
///  - `BypassPermissions` -> "bypassPermissions"
///
/// `from_wire` also accepts the snake-case aliases the upstream
/// Python SDK and earlier CLIs emitted (`accept_edits`, `dont_ask`,
/// `bypass_permissions`, `ask`, `deny`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionMode {
    #[serde(rename = "default")]
    Ask,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "dontAsk")]
    DontAsk,
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl PermissionMode {
    /// The string the `claude` binary expects via `--permission-mode`
    /// and the JSON wire string.
    #[must_use]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Ask => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
            Self::Auto => "auto",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    /// Alias for [`Self::as_wire`]. The SDK options builder calls
    /// this to drive `--permission-mode <arg>` on the CLI.
    #[must_use]
    pub fn as_cli_arg(self) -> &'static str {
        self.as_wire()
    }

    /// Parse a wire string into a `PermissionMode`. Accepts both the
    /// canonical camelCase forms (`"acceptEdits"`, `"dontAsk"`,
    /// `"bypassPermissions"`) and the snake_case + legacy aliases
    /// (`"accept_edits"`, `"dont_ask"`, `"bypass_permissions"`,
    /// `"ask"` -> Ask, `"deny"` -> DontAsk).
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "default" | "ask" => Self::Ask,
            "acceptEdits" | "accept_edits" => Self::AcceptEdits,
            "plan" => Self::Plan,
            "dontAsk" | "dont_ask" | "deny" => Self::DontAsk,
            "auto" => Self::Auto,
            "bypassPermissions" | "bypass_permissions" => Self::BypassPermissions,
            _ => return None,
        })
    }

    /// Human-readable display name (used by forge-tui's mode chip
    /// + settings UI).
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
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
            PermissionMode::Ask,
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
        assert_eq!(PermissionMode::from_wire("ask"), Some(PermissionMode::Ask));
        assert_eq!(PermissionMode::from_wire("accept_edits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(PermissionMode::from_wire("dont_ask"), Some(PermissionMode::DontAsk));
        assert_eq!(PermissionMode::from_wire("deny"), Some(PermissionMode::DontAsk));
        assert_eq!(
            PermissionMode::from_wire("bypass_permissions"),
            Some(PermissionMode::BypassPermissions)
        );
    }

    #[test]
    fn permission_mode_as_cli_arg_matches_as_wire() {
        assert_eq!(PermissionMode::Ask.as_cli_arg(), "default");
        assert_eq!(PermissionMode::DontAsk.as_cli_arg(), "dontAsk");
    }

    #[test]
    fn permission_mode_serde_round_trips() {
        let value = serde_json::to_string(&PermissionMode::DontAsk).expect("serialize");
        assert_eq!(value, "\"dontAsk\"");
        let decoded: PermissionMode = serde_json::from_str("\"acceptEdits\"").expect("deserialize");
        assert_eq!(decoded, PermissionMode::AcceptEdits);
    }
}

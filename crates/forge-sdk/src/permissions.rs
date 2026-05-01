//! Permission callback types.
//!
//! SDK's `PermissionResultAllow`, `PermissionResultDeny`,
//! `ToolPermissionContext`, `CanUseTool` callable, plus the `PermissionUpdate`
//! family carried on allow results.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::options::PermissionMode;

/// Context passed to a [`CanUseToolCallback`] when the `claude` binary
/// asks for permission.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ToolPermissionContext {
    /// The tool the model wants to invoke (e.g. `"Edit"`, `"Bash"`).
    pub tool_name: String,
    /// The JSON input the model generated for the call.
    pub tool_input: Value,
    /// Identifier of THIS tool call (always present — used to correlate
    /// with the subsequent `ToolResult` in the stream). Matches the wire
    /// `tool_use_id` field.
    pub tool_use_id: String,
    /// Sub-agent identifier when the request came from a Task-spawned
    /// child agent. `None` for main-agent tool calls.
    pub agent_id: Option<String>,
    /// Permission-rule suggestions the CLI attached to this request.
    /// Typically populated when the user has in-flight workspace
    /// permission prompts. Wire shape:
    /// `ToolPermissionContext.suggestions` — populated
    /// from the `control_request`'s `permission_suggestions` list, with
    /// unrecognised entries dropped.
    pub suggestions: Vec<PermissionUpdate>,
    /// Path the CLI considered out-of-bounds when rejecting the tool
    /// call (e.g. an `Edit` against a file outside the workspace).
    /// `None` when the request is not path-scoped or the CLI didn't
    /// supply a value. UIs render it as "Claude wanted to touch `<path>`".
    pub blocked_path: Option<String>,
    /// Free-form reason the CLI surfaced for why the request needs
    /// human review (e.g. `"workspace not yet trusted"`). Pass-through
    /// for UIs that show it verbatim.
    pub decision_reason: Option<String>,
    /// Short title the CLI suggests for the prompt (e.g. `"Run tests"`).
    pub title: Option<String>,
    /// Display name for the tool call (often a humanised tool name).
    pub display_name: Option<String>,
    /// Long-form description the CLI suggests as additional context
    /// in the prompt body.
    pub description: Option<String>,
}

impl ToolPermissionContext {
    /// Construct a context. Public constructor needed because the struct is
    /// `#[non_exhaustive]` (struct-literal construction is blocked across
    /// crate boundaries). `suggestions` defaults to empty; use
    /// [`with_suggestions`](Self::with_suggestions) to attach parsed
    /// permission-rule hints.
    #[must_use]
    pub fn new(
        tool_name: impl Into<String>,
        tool_input: Value,
        tool_use_id: impl Into<String>,
        agent_id: Option<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            tool_input,
            tool_use_id: tool_use_id.into(),
            agent_id,
            suggestions: Vec::new(),
            blocked_path: None,
            decision_reason: None,
            title: None,
            display_name: None,
            description: None,
        }
    }

    /// Attach permission-rule suggestions parsed from the control
    /// request's `permission_suggestions` field.
    #[must_use]
    pub fn with_suggestions(mut self, suggestions: Vec<PermissionUpdate>) -> Self {
        self.suggestions = suggestions;
        self
    }

    /// Attach the optional display fields the CLI surfaces alongside
    /// `tool_name` / `tool_input` so UIs can render rich permission
    /// prompts ("Claude wants to `<title>`: `<description>`"). Each field
    /// is independently optional; callers pass `None` for ones the CLI
    /// didn't populate.
    #[must_use]
    pub fn with_display(
        mut self,
        blocked_path: Option<String>,
        decision_reason: Option<String>,
        title: Option<String>,
        display_name: Option<String>,
        description: Option<String>,
    ) -> Self {
        self.blocked_path = blocked_path;
        self.decision_reason = decision_reason;
        self.title = title;
        self.display_name = display_name;
        self.description = description;
        self
    }
}

/// The decision a callback returns.
///
/// Wraps the CLI's `PermissionResultAllow` / `PermissionResultDeny` as a
/// single enum with constructor helpers.
#[derive(Debug, Clone)]
pub struct PermissionDecision {
    inner: DecisionKind,
}

#[derive(Debug, Clone)]
enum DecisionKind {
    Allow {
        updated_input: Option<Value>,
        updated_permissions: Vec<PermissionUpdate>,
    },
    Deny {
        reason: String,
    },
}

impl PermissionDecision {
    /// Approve the tool call as-is.
    #[must_use]
    pub fn allow() -> Self {
        Self {
            inner: DecisionKind::Allow {
                updated_input: None,
                updated_permissions: Vec::new(),
            },
        }
    }

    /// Approve the tool call with a modified input payload. The `claude`
    /// binary will receive the modified input in place of the model's
    /// original.
    #[must_use]
    pub fn allow_with_input(updated_input: Value) -> Self {
        Self {
            inner: DecisionKind::Allow {
                updated_input: Some(updated_input),
                updated_permissions: Vec::new(),
            },
        }
    }

    /// Deny the tool call. `reason` is forwarded to the model as feedback.
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            inner: DecisionKind::Deny {
                reason: reason.into(),
            },
        }
    }

    /// Attach a list of [`PermissionUpdate`]s to an allow decision. These
    /// are forwarded to the `claude` binary as `updatedPermissions` on the
    /// wire and applied to the session's permission state. No-op on a
    /// deny decision — the CLI's `PermissionResultDeny` has no equivalent
    /// channel.
    #[must_use]
    pub fn with_updated_permissions(mut self, updates: Vec<PermissionUpdate>) -> Self {
        if let DecisionKind::Allow {
            updated_permissions,
            ..
        } = &mut self.inner
        {
            *updated_permissions = updates;
        }
        self
    }

    /// True if this is an allow decision.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self.inner, DecisionKind::Allow { .. })
    }

    /// For an allow decision with modified input, returns the modified input.
    #[must_use]
    pub fn updated_input(&self) -> Option<&Value> {
        match &self.inner {
            DecisionKind::Allow { updated_input, .. } => updated_input.as_ref(),
            DecisionKind::Deny { .. } => None,
        }
    }

    /// Permission updates attached to an allow decision. Empty slice for
    /// deny, or for an allow that carries no updates.
    #[must_use]
    pub fn updated_permissions(&self) -> &[PermissionUpdate] {
        match &self.inner {
            DecisionKind::Allow {
                updated_permissions,
                ..
            } => updated_permissions,
            DecisionKind::Deny { .. } => &[],
        }
    }

    /// For a deny decision, returns the reason string.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match &self.inner {
            DecisionKind::Deny { reason } => Some(reason.as_str()),
            DecisionKind::Allow { .. } => None,
        }
    }
}

/// Where a [`PermissionUpdate`] should be persisted. Wire shape:
/// `PermissionUpdateDestination` literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionUpdateDestination {
    /// User-level `~/.claude/settings.json`.
    UserSettings,
    /// Project-level `.claude/settings.json`.
    ProjectSettings,
    /// Project-local `.claude/settings.local.json`.
    LocalSettings,
    /// Session-scoped (in-memory, per-client) — discarded on disconnect.
    Session,
}

/// Policy a rule-based [`PermissionUpdate`] applies. Wire shape:
/// `PermissionBehavior` literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    /// Auto-approve matches.
    Allow,
    /// Auto-deny matches.
    Deny,
    /// Prompt on matches.
    Ask,
}

/// One tool-rule entry inside a rule-based [`PermissionUpdate`].
/// Wraps the CLI's `PermissionRuleValue`. Wire uses
/// camelCase (`toolName`, `ruleContent`) per the CLI's `to_dict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRuleValue {
    /// Tool the rule targets (e.g. `"Edit"`, `"Bash"`).
    pub tool_name: String,
    /// Optional rule-content pattern (tool-specific). `None` means "any
    /// invocation of this tool".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_content: Option<String>,
}

/// One permission-state mutation attached to an allow decision. Mirrors
/// the CLI's `PermissionUpdate`. Dispatched on `type`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum PermissionUpdate {
    /// Append rules to the active permission set.
    #[serde(rename = "addRules")]
    AddRules {
        /// Rules to add.
        rules: Vec<PermissionRuleValue>,
        /// Policy for matching invocations.
        behavior: PermissionBehavior,
        /// Where to persist. `None` = in-memory only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Replace the entire rule set with `rules`.
    #[serde(rename = "replaceRules")]
    ReplaceRules {
        /// Rules to install.
        rules: Vec<PermissionRuleValue>,
        /// Policy for matching invocations.
        behavior: PermissionBehavior,
        /// Where to persist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Remove the listed rules from the active set.
    #[serde(rename = "removeRules")]
    RemoveRules {
        /// Rules to drop.
        rules: Vec<PermissionRuleValue>,
        /// Policy the rules were registered under.
        behavior: PermissionBehavior,
        /// Where to persist the removal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Switch the session's [`PermissionMode`].
    #[serde(rename = "setMode")]
    SetMode {
        /// Target mode.
        mode: PermissionMode,
        /// Where to persist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Widen the additional-directories allowlist.
    #[serde(rename = "addDirectories")]
    AddDirectories {
        /// Absolute paths to add.
        directories: Vec<String>,
        /// Where to persist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
    /// Shrink the additional-directories allowlist.
    #[serde(rename = "removeDirectories")]
    RemoveDirectories {
        /// Absolute paths to remove.
        directories: Vec<String>,
        /// Where to persist the removal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        destination: Option<PermissionUpdateDestination>,
    },
}

/// Trait for permission callbacks.
///
/// Implementations are typically async closures or plain functions. The
/// SDK wraps them in a boxed trait object inside `Options`.
///
/// # Panics and error handling
///
/// Callbacks are expected to never panic. If a callback does panic, the
/// `tokio` task running the current `next_event` call is aborted; the next
/// call to `next_event` returns an [`Error::Io`](crate::Error::Io) with a
/// broken-pipe or similar message. Authors should return
/// [`PermissionDecision::deny`] to signal rejection rather than panicking.
///
/// Callbacks cannot signal I/O or other errors. If your callback performs
/// fallible work (e.g., consulting a policy server), handle the failure
/// internally and translate to `allow` or `deny(reason)` — the SDK does not
/// surface callback errors to the `claude` binary separately from a deny
/// response.
pub trait CanUseToolCallback: Send + Sync {
    /// Called by the SDK when the `claude` binary requests permission.
    fn call<'a>(
        &'a self,
        ctx: ToolPermissionContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PermissionDecision> + Send + 'a>>;
}

impl<F, Fut> CanUseToolCallback for F
where
    F: Fn(ToolPermissionContext) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = PermissionDecision> + Send + 'static,
{
    fn call<'a>(
        &'a self,
        ctx: ToolPermissionContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = PermissionDecision> + Send + 'a>> {
        Box::pin(self(ctx))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    #[allow(unused_imports)]
    use super::*;

    use crate::{PermissionDecision, ToolPermissionContext};
    use serde_json::json;

    #[test]
    fn allow_decision_no_modifications() {
        let d = PermissionDecision::allow();
        assert!(d.is_allow());
        assert!(d.updated_input().is_none());
        assert!(d.reason().is_none());
    }

    #[test]
    fn allow_decision_with_updated_input() {
        let d = PermissionDecision::allow_with_input(json!({"file_path": "/tmp/safe.txt"}));
        assert!(d.is_allow());
        assert_eq!(
            d.updated_input().unwrap(),
            &json!({"file_path": "/tmp/safe.txt"})
        );
    }

    #[test]
    fn deny_decision_carries_reason() {
        let d = PermissionDecision::deny("not today");
        assert!(!d.is_allow());
        assert_eq!(d.reason(), Some("not today"));
    }

    #[test]
    fn context_carries_tool_name_and_input() {
        let ctx =
            ToolPermissionContext::new("Edit", json!({"file_path": "/tmp/x"}), "toolu_01", None);
        assert_eq!(ctx.tool_name, "Edit");
        assert_eq!(ctx.tool_use_id, "toolu_01");
    }

    use crate::PermissionMode;
    use crate::{
        PermissionBehavior, PermissionRuleValue, PermissionUpdate, PermissionUpdateDestination,
    };

    #[test]
    fn add_rules_serialises_with_camel_case_wire_keys() {
        let update = PermissionUpdate::AddRules {
            rules: vec![
                PermissionRuleValue {
                    tool_name: "Edit".into(),
                    rule_content: Some("*.py".into()),
                },
                PermissionRuleValue {
                    tool_name: "Bash".into(),
                    rule_content: None,
                },
            ],
            behavior: PermissionBehavior::Allow,
            destination: Some(PermissionUpdateDestination::ProjectSettings),
        };
        let v = serde_json::to_value(&update).expect("ser");
        assert_eq!(
            v,
            json!({
                "type": "addRules",
                "rules": [
                    {"toolName": "Edit", "ruleContent": "*.py"},
                    {"toolName": "Bash"}
                ],
                "behavior": "allow",
                "destination": "projectSettings"
            })
        );
    }

    #[test]
    fn replace_and_remove_rules_variants_roundtrip() {
        for (variant, wire_type) in [
            (
                PermissionUpdate::ReplaceRules {
                    rules: vec![PermissionRuleValue {
                        tool_name: "Edit".into(),
                        rule_content: None,
                    }],
                    behavior: PermissionBehavior::Deny,
                    destination: None,
                },
                "replaceRules",
            ),
            (
                PermissionUpdate::RemoveRules {
                    rules: vec![PermissionRuleValue {
                        tool_name: "Bash".into(),
                        rule_content: Some("rm *".into()),
                    }],
                    behavior: PermissionBehavior::Ask,
                    destination: Some(PermissionUpdateDestination::Session),
                },
                "removeRules",
            ),
        ] {
            let v = serde_json::to_value(&variant).expect("ser");
            assert_eq!(v["type"], wire_type);
            let back: PermissionUpdate = serde_json::from_value(v).expect("de");
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                serde_json::to_value(&variant).unwrap()
            );
        }
    }

    #[test]
    fn set_mode_serialises_with_camel_case_mode_string() {
        let update = PermissionUpdate::SetMode {
            mode: PermissionMode::AcceptEdits,
            destination: Some(PermissionUpdateDestination::UserSettings),
        };
        let v = serde_json::to_value(&update).expect("ser");
        assert_eq!(
            v,
            json!({
                "type": "setMode",
                "mode": "acceptEdits",
                "destination": "userSettings"
            })
        );
    }

    #[test]
    fn directory_variants_roundtrip() {
        let add = PermissionUpdate::AddDirectories {
            directories: vec!["/tmp/a".into(), "/tmp/b".into()],
            destination: None,
        };
        let v = serde_json::to_value(&add).expect("ser");
        assert_eq!(
            v,
            json!({
                "type": "addDirectories",
                "directories": ["/tmp/a", "/tmp/b"]
            })
        );

        let rem = PermissionUpdate::RemoveDirectories {
            directories: vec!["/tmp/a".into()],
            destination: Some(PermissionUpdateDestination::LocalSettings),
        };
        let v = serde_json::to_value(&rem).expect("ser");
        assert_eq!(v["type"], "removeDirectories");
        assert_eq!(v["destination"], "localSettings");
    }

    #[test]
    fn permission_behavior_values_match_wire() {
        for (b, wire) in [
            (PermissionBehavior::Allow, "allow"),
            (PermissionBehavior::Deny, "deny"),
            (PermissionBehavior::Ask, "ask"),
        ] {
            assert_eq!(serde_json::to_value(b).unwrap(), json!(wire));
        }
    }

    #[test]
    fn permission_update_destination_values_match_wire() {
        for (d, wire) in [
            (PermissionUpdateDestination::UserSettings, "userSettings"),
            (
                PermissionUpdateDestination::ProjectSettings,
                "projectSettings",
            ),
            (PermissionUpdateDestination::LocalSettings, "localSettings"),
            (PermissionUpdateDestination::Session, "session"),
        ] {
            assert_eq!(serde_json::to_value(d).unwrap(), json!(wire));
        }
    }

    #[test]
    fn decision_allow_has_empty_permissions_by_default() {
        let d = PermissionDecision::allow();
        assert!(d.updated_permissions().is_empty());
    }

    #[test]
    fn decision_allow_with_input_has_empty_permissions_by_default() {
        let d = PermissionDecision::allow_with_input(json!({"foo": "bar"}));
        assert!(d.updated_permissions().is_empty());
        assert_eq!(d.updated_input(), Some(&json!({"foo": "bar"})));
    }

    #[test]
    fn with_updated_permissions_attaches_updates() {
        let updates = vec![
            PermissionUpdate::SetMode {
                mode: PermissionMode::Plan,
                destination: None,
            },
            PermissionUpdate::AddDirectories {
                directories: vec!["/workspace".into()],
                destination: None,
            },
        ];
        let d = PermissionDecision::allow().with_updated_permissions(updates.clone());
        let got = d.updated_permissions();
        assert_eq!(got.len(), 2);
        assert_eq!(
            serde_json::to_value(got).unwrap(),
            serde_json::to_value(&updates).unwrap()
        );
    }

    #[test]
    fn with_updated_permissions_is_noop_on_deny() {
        let d = PermissionDecision::deny("nope").with_updated_permissions(vec![
            PermissionUpdate::SetMode {
                mode: PermissionMode::Ask,
                destination: None,
            },
        ]);
        // Still a deny; permissions are not readable.
        assert!(d.updated_permissions().is_empty());
        assert_eq!(d.reason(), Some("nope"));
    }
}

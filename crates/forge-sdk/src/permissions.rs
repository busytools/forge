//! Permission callback types.
//!
//! Mirrors Python SDK's `PermissionResultAllow`, `PermissionResultDeny`,
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
    /// permission prompts. Mirrors Python's
    /// `ToolPermissionContext.suggestions` (`types.py:180`) — populated
    /// from the `control_request`'s `permission_suggestions` list, with
    /// unrecognised entries dropped.
    pub suggestions: Vec<PermissionUpdate>,
    /// Abort signal placeholder — Python reserves this field for future
    /// abort-signal support (`types.py:178`). forge-sdk carries it
    /// through as an opaque [`Value`] so callbacks can introspect the
    /// payload once it's wired end-to-end.
    pub signal: Option<Value>,
}

impl ToolPermissionContext {
    /// Construct a context. Public constructor needed because the struct is
    /// `#[non_exhaustive]` (struct-literal construction is blocked across
    /// crate boundaries). `suggestions` and `signal` default to empty /
    /// `None`; use [`with_suggestions`](Self::with_suggestions) and
    /// [`with_signal`](Self::with_signal) to attach them.
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
            signal: None,
        }
    }

    /// Attach permission-rule suggestions parsed from the control
    /// request's `permission_suggestions` field.
    #[must_use]
    pub fn with_suggestions(mut self, suggestions: Vec<PermissionUpdate>) -> Self {
        self.suggestions = suggestions;
        self
    }

    /// Attach the abort-signal placeholder payload. Python reserves this
    /// for future abort-signal wiring upstream and currently passes
    /// `None` everywhere (`types.py:178`). Hidden from the public doc
    /// surface until Anthropic wires the field end-to-end — forge-sdk
    /// keeps the builder only to preserve source-level compatibility
    /// when that day arrives.
    #[doc(hidden)]
    #[must_use]
    pub fn with_signal(mut self, signal: Value) -> Self {
        self.signal = Some(signal);
        self
    }
}

/// The decision a callback returns.
///
/// Mirrors Python's `PermissionResultAllow` / `PermissionResultDeny` as a
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
    /// deny decision — Python's `PermissionResultDeny` has no equivalent
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

/// Where a [`PermissionUpdate`] should be persisted. Mirrors Python's
/// `PermissionUpdateDestination` literal (`types.py:103-105`).
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

/// Policy a rule-based [`PermissionUpdate`] applies. Mirrors Python's
/// `PermissionBehavior` literal (`types.py:107`).
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
/// Mirrors Python's `PermissionRuleValue` (`types.py:110-115`). Wire uses
/// camelCase (`toolName`, `ruleContent`) per Python's `to_dict`.
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
/// Python's `PermissionUpdate` (`types.py:118-170`). Dispatched on `type`.
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

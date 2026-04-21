//! Permission callback types.
//!
//! Mirrors Python SDK's `PermissionResultAllow`, `PermissionResultDeny`,
//! `ToolPermissionContext`, `CanUseTool` callable.

use serde_json::Value;

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
}

impl ToolPermissionContext {
    /// Construct a context. Public constructor needed because the struct is
    /// `#[non_exhaustive]` (struct-literal construction is blocked across
    /// crate boundaries).
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
        }
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
    Allow { updated_input: Option<Value> },
    Deny { reason: String },
}

impl PermissionDecision {
    /// Approve the tool call as-is.
    #[must_use]
    pub fn allow() -> Self {
        Self {
            inner: DecisionKind::Allow {
                updated_input: None,
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

    /// True if this is an allow decision.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self.inner, DecisionKind::Allow { .. })
    }

    /// For an allow decision with modified input, returns the modified input.
    #[must_use]
    pub fn updated_input(&self) -> Option<&Value> {
        match &self.inner {
            DecisionKind::Allow { updated_input } => updated_input.as_ref(),
            DecisionKind::Deny { .. } => None,
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

/// Trait for permission callbacks.
///
/// Implementations are typically async closures or plain functions. The
/// SDK wraps them in a boxed trait object inside `Options`.
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

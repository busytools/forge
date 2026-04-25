//! Bridge `forge-sdk` callbacks (`can_use_tool`, hook handlers) into the
//! daemon's reverse-RPC issuer.
//!
//! Both bridges are constructed at session-spawn time and handed to the
//! SDK's [`forge_sdk::Options`] before [`forge_sdk::Client::spawn`]. The
//! SDK invokes them from inside its read loop when the CLI emits the
//! corresponding `control_request`. Each invocation marshals the
//! callback's typed input back to a JSON `params` blob, issues a
//! reverse-RPC over [`crate::reverse_rpc::issue_to_primary`], and
//! decodes the client's response into the callback's expected return
//! type.
//!
//! The bridges run inside the SDK's task context (not the session
//! actor's). They don't access the [`forge_sdk::Client`] itself — only
//! the daemon's connection-management plumbing — so they don't deadlock
//! against the actor.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use forge_sdk::{CanUseToolCallback, HookCallback, HookContext, HookDecision};
use forge_sdk::{PermissionDecision, ToolPermissionContext};
use serde::Serialize;
use serde_json::Value;

use crate::Error;
use crate::prompt_queue::PromptKind;
use crate::registry::DaemonState;
use crate::session_state::SessionId;

/// D13 — 1 hour timeout for reverse-RPCs.
pub const HOOK_TIMEOUT_SECS: u64 = 3600;

/// `CanUseToolCallback` impl that issues a `permission.request` reverse-RPC.
///
/// Constructed in `methods::session::spawn` before [`forge_sdk::Client::spawn`].
pub struct ForgedPermissionBridge {
    state: Arc<DaemonState>,
    session_id: SessionId,
}

impl ForgedPermissionBridge {
    /// Construct a bridge tied to a specific session.
    #[must_use]
    pub fn new(state: Arc<DaemonState>, session_id: SessionId) -> Self {
        Self { state, session_id }
    }

    /// Issue the reverse-RPC and decode the wire-shape response into a
    /// [`PermissionDecision`]. Falls back to a deny on transport failure
    /// (the SDK can't surface I/O errors out of the callback).
    async fn issue_and_decode(&self, ctx: ToolPermissionContext) -> PermissionDecision {
        let params = serde_json::json!({
            "tool_name": ctx.tool_name,
            "tool_input": ctx.tool_input,
            "context": {
                "tool_use_id": ctx.tool_use_id,
                "agent_id": ctx.agent_id,
                "suggestions": ctx.suggestions,
            },
        });
        let value = match crate::reverse_rpc::issue_to_primary(
            &self.state,
            &self.session_id,
            "permission.request",
            params,
            PromptKind::Permission,
            Duration::from_secs(HOOK_TIMEOUT_SECS),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "permission bridge: issue_to_primary failed; denying");
                return PermissionDecision::deny(format!("permission bridge: {e}"));
            }
        };

        // Wire shape: `{ "decision": "allow"|"deny", "updated_input": ?, "reason": ? }`
        let decision_str = value
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or("deny");
        let updated_input = value.get("updated_input").cloned();
        let reason = value
            .get("reason")
            .and_then(Value::as_str)
            .map(String::from);

        match (decision_str, updated_input) {
            ("allow", None) => PermissionDecision::allow(),
            ("allow", Some(updates)) => PermissionDecision::allow_with_input(updates),
            ("deny", _) => PermissionDecision::deny(reason.unwrap_or_default()),
            (other, _) => PermissionDecision::deny(format!("unknown decision: {other}")),
        }
    }
}

impl CanUseToolCallback for ForgedPermissionBridge {
    fn call<'a>(
        &'a self,
        ctx: ToolPermissionContext,
    ) -> Pin<Box<dyn std::future::Future<Output = PermissionDecision> + Send + 'a>> {
        Box::pin(self.issue_and_decode(ctx))
    }
}

/// `HookCallback<I>` impl that issues a `hook.<kind>` reverse-RPC. The
/// concrete input type `I` is parameterised so the bridge plugs into
/// every typed [`forge_sdk::HooksBuilder`] method.
///
/// Reverse-RPC params shape: `{ "input": <serialised I>, "context": {…} }`.
pub struct ForgedHookBridge {
    state: Arc<DaemonState>,
    session_id: SessionId,
    /// Snake-case hook kind (e.g. `"pre_tool_use"`) used to construct the
    /// JSON-RPC method name `hook.<kind>`.
    kind: String,
}

impl ForgedHookBridge {
    /// Construct a bridge tied to a specific session and hook kind.
    #[must_use]
    pub fn new(state: Arc<DaemonState>, session_id: SessionId, kind: impl Into<String>) -> Self {
        Self {
            state,
            session_id,
            kind: kind.into(),
        }
    }

    /// Marshal the callback's input + context into a `params` JSON blob,
    /// issue the reverse-RPC, and decode the response.
    async fn issue_and_decode<I>(&self, input: I, context: HookContext) -> HookDecision
    where
        I: Serialize + Send + 'static,
    {
        let method = format!("hook.{}", self.kind);
        let input_v = match serde_json::to_value(&input) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, hook = %self.kind, "hook bridge: failed to encode input; passthrough");
                return HookDecision::passthrough();
            }
        };
        let params = serde_json::json!({
            "input": input_v,
            "context": {
                "kind": context.kind.as_str(),
                "tool_name": context.tool_name,
                "session_id": context.session_id,
                "tool_use_id": context.tool_use_id,
            },
        });
        let value = match crate::reverse_rpc::issue_to_primary(
            &self.state,
            &self.session_id,
            &method,
            params,
            PromptKind::Hook {
                kind: self.kind.clone(),
            },
            Duration::from_secs(HOOK_TIMEOUT_SECS),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, hook = %self.kind, "hook bridge: issue_to_primary failed; passthrough");
                return HookDecision::passthrough();
            }
        };

        // Wire shape: `{ "decision": "allow"|"deny"|"passthrough", … }`
        decode_hook_decision(&value)
    }
}

/// Decode the wire-shape `decision` blob returned from a `hook.<kind>`
/// reverse-RPC into a [`HookDecision`].
///
/// Wire shape (see §7.4.13 of the wire spec):
/// ```json
/// {
///   "decision": "allow" | "deny" | "passthrough" | "replace_input",
///   "updated_input": ?,
///   "reason": ?,
///   "continue": ?,
///   "suppressOutput": ?,
///   "stopReason": ?,
///   "systemMessage": ?
/// }
/// ```
fn decode_hook_decision(value: &Value) -> HookDecision {
    let decision_str = value
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("passthrough");
    let updated_input = value.get("updated_input").cloned();
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .map(String::from);

    let mut d = match (decision_str, updated_input) {
        ("allow", None) => HookDecision::allow(),
        ("allow" | "replace_input", Some(input)) => HookDecision::replace_input(input),
        ("deny", _) => HookDecision::deny(reason.clone().unwrap_or_default()),
        ("passthrough", _) => HookDecision::passthrough(),
        (other, _) => {
            tracing::warn!(decision = %other, "hook bridge: unknown decision; passthrough");
            HookDecision::passthrough()
        }
    };

    // Optional control fields — Python's SyncHookJSONOutput shape.
    if let Some(b) = value.get("continue").and_then(Value::as_bool) {
        d = d.with_continue(b);
    }
    if let Some(b) = value.get("suppressOutput").and_then(Value::as_bool) {
        d = d.with_suppress_output(b);
    }
    if let Some(s) = value.get("stopReason").and_then(Value::as_str) {
        d = d.with_stop_reason(s);
    }
    if let Some(s) = value.get("systemMessage").and_then(Value::as_str) {
        d = d.with_system_message(s);
    }

    d
}

// HookCallback<I> is parameterised over the concrete input type. Each
// hook kind has its own concrete callback, so we provide a blanket
// implementation that works for any `Serialize` input. The trait's
// blanket impl over `Fn(I, HookContext) -> Future` in forge-sdk doesn't
// apply here because we want to share state across invocations — the
// bridge holds an `Arc<DaemonState>`. Implementing `HookCallback<I>`
// directly on `ForgedHookBridge` lets us pass the same Arc to every
// typed hook builder method.

impl<I> HookCallback<I> for ForgedHookBridge
where
    I: Serialize + Send + 'static,
{
    fn call<'a>(
        &'a self,
        input: I,
        context: HookContext,
    ) -> Pin<Box<dyn std::future::Future<Output = HookDecision> + Send + 'a>> {
        Box::pin(self.issue_and_decode(input, context))
    }
}

/// Wire-shape mirror of one hook registration entry. Mirrors
/// `WireHookSpec` in §7.4 of the wire spec.
///
/// Returned by the `WireOptions::hooks` deserialiser; consumed by
/// `attach_hooks` to build a [`forge_sdk::HooksBuilder`].
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WireHookSpec {
    /// Snake-case hook kind, e.g. `"pre_tool_use"`. Must match one of
    /// the kinds [`HooksBuilder`](forge_sdk::HooksBuilder) exposes.
    pub kind: String,
    /// Optional matcher glob (only used for `pre_tool_use`,
    /// `post_tool_use`, `post_tool_use_failure`, `permission_request`).
    /// Defaults to `"*"` when omitted.
    #[serde(default)]
    pub matcher: Option<String>,
}

/// Attach a [`ForgedHookBridge`] for every entry in `specs` to a fresh
/// [`forge_sdk::HooksBuilder`]. Returns the configured builder.
///
/// Sets `default_timeout_secs(HOOK_TIMEOUT_SECS)` per D13 — the CLI
/// will wait up to 1 hour for a hook reply before giving up.
///
/// # Errors
///
/// [`Error::InvalidParams`] if a hook spec names an unknown kind.
pub fn attach_hooks(
    state_arc: &Arc<DaemonState>,
    session_id: &SessionId,
    specs: &[WireHookSpec],
) -> Result<forge_sdk::Hooks, Error> {
    let mut builder = forge_sdk::HooksBuilder::new().default_timeout_secs(HOOK_TIMEOUT_SECS);
    for spec in specs {
        let bridge =
            ForgedHookBridge::new(state_arc.clone(), session_id.clone(), spec.kind.clone());
        let matcher = spec.matcher.clone().unwrap_or_else(|| "*".to_string());
        builder = match spec.kind.as_str() {
            "pre_tool_use" => builder.pre_tool_use(matcher, bridge),
            "post_tool_use" => builder.post_tool_use(matcher, bridge),
            "post_tool_use_failure" => builder.post_tool_use_failure(matcher, bridge),
            "user_prompt_submit" => builder.user_prompt_submit(bridge),
            "stop" => builder.stop(bridge),
            "subagent_stop" => builder.subagent_stop(bridge),
            "subagent_start" => builder.subagent_start(bridge),
            "pre_compact" => builder.pre_compact(bridge),
            "notification" => builder.notification(bridge),
            "permission_request" => builder.permission_request(matcher, bridge),
            other => {
                return Err(Error::InvalidParams(format!(
                    "hooks: unknown kind '{other}'"
                )));
            }
        };
    }
    Ok(builder.build())
}

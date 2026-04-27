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

        decode_permission_response(&value)
    }
}

/// Decode the wire-shape response of a `permission.request` reverse-RPC
/// into a [`PermissionDecision`]. Recognises every documented sentinel
/// (`_jsonrpc_error`, `_client_disconnected`, `_session_closed`) and
/// falls through to a typed deny when the wire shape is malformed
/// (missing `decision`, unknown variant, etc.).
///
/// Public so integration tests can drive it directly with synthesised
/// values without going through reverse-RPC issuance — the function is
/// referentially transparent (input → output, no side effects beyond
/// a single `tracing::warn` on unknown decisions) so exposing it has
/// no API-shape risk.
#[must_use]
pub fn decode_permission_response(value: &Value) -> PermissionDecision {
    // Sentinel: client returned a JSON-RPC error response. Surfaces
    // as a typed deny with the upstream code+message in the reason,
    // so logs distinguish "client said deny" from "client errored".
    if let Some(err) = value.get("_jsonrpc_error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let message = err.get("message").and_then(Value::as_str).unwrap_or("");
        return PermissionDecision::deny(format!("client error {code}: {message}"));
    }
    // Sentinel: client disconnected before answering.
    if value.get("_client_disconnected").is_some() {
        return PermissionDecision::deny(String::from(
            "answering client disconnected before responding",
        ));
    }
    // Sentinel: session closed before the prompt was answered.
    if value.get("_session_closed").is_some() {
        return PermissionDecision::deny(String::from("session closed before prompt answered"));
    }

    // Wire shape: `{ "decision": "allow"|"deny", "updated_input": ?, "reason": ? }`
    // Round 4 — fix M4. Previously a missing `decision` field silently
    // collapsed into "deny" with an empty reason — symmetrical with the
    // hook bridge's missing-decision branch, but without a log it was
    // indistinguishable from a deliberate deny in operator traces. Now
    // we warn on the malformed-shape path so the silent fallback is
    // visible (e.g. a buggy client sending `{}` instead of
    // `{"decision":"deny"}`).
    let Some(decision_str) = value.get("decision").and_then(Value::as_str) else {
        tracing::warn!(?value, "permission bridge: missing decision field; denying");
        return PermissionDecision::deny("missing decision field");
    };
    let updated_input = value.get("updated_input").cloned();
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .map(String::from);

    match (decision_str, updated_input) {
        ("allow", None) => PermissionDecision::allow(),
        ("allow", Some(updates)) => PermissionDecision::allow_with_input(updates),
        ("deny", _) => PermissionDecision::deny(reason.unwrap_or_default()),
        (other, _) => {
            tracing::warn!(decision = %other, "permission bridge: unknown decision; denying");
            PermissionDecision::deny(format!("unknown decision: {other}"))
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
                tracing::warn!(error = %e, hook = %self.kind, "hook bridge: failed to encode input; failing closed");
                return decode_hook_response(
                    &self.kind,
                    &serde_json::json!({
                        "_encode_error": { "message": e.to_string() },
                    }),
                );
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
                tracing::warn!(error = %e, hook = %self.kind, "hook bridge: issue_to_primary failed; failing closed");
                return decode_hook_response(
                    &self.kind,
                    &serde_json::json!({
                        "_transport_error": { "message": e.to_string() },
                    }),
                );
            }
        };

        decode_hook_response(&self.kind, &value)
    }
}

/// Hook kinds where transport / decode failure must DENY rather than
/// passthrough. These are security-critical: a failure-mode passthrough
/// is equivalent to "approve any tool call", which would silently
/// disable the user's safety controls.
#[must_use]
fn is_security_critical(kind: &str) -> bool {
    matches!(kind, "pre_tool_use" | "permission_request")
}

/// Failure mode for `kind`. Security-critical hooks deny; observational
/// hooks (`post_tool_use`, `stop`, `notification`, etc.) passthrough so
/// a hook outage doesn't grind the agent to a halt.
#[must_use]
fn fail_closed_decision(kind: &str, reason: &str) -> HookDecision {
    if is_security_critical(kind) {
        HookDecision::deny(reason.to_owned())
    } else {
        HookDecision::passthrough()
    }
}

/// Decode the wire-shape response of a `hook.<kind>` reverse-RPC into a
/// [`HookDecision`]. Recognises every documented sentinel
/// (`_jsonrpc_error`, `_client_disconnected`, `_session_closed`,
/// `_encode_error`, `_transport_error`) and falls through to either a
/// fail-closed deny (security-critical kinds) or a passthrough
/// (observational kinds).
///
/// Public so integration tests can drive it directly with synthesised
/// values without going through reverse-RPC issuance — the function
/// is referentially transparent.
#[must_use]
pub fn decode_hook_response(kind: &str, value: &Value) -> HookDecision {
    // Sentinel: encode failure on the request side (input couldn't be
    // serialised). Local synthetic — never appears on the wire.
    if let Some(err) = value.get("_encode_error") {
        let message = err.get("message").and_then(Value::as_str).unwrap_or("");
        return fail_closed_decision(kind, &format!("hook bridge encode failure: {message}"));
    }
    // Sentinel: transport failure (timeout, disconnect, etc.). Local
    // synthetic — never appears on the wire.
    if let Some(err) = value.get("_transport_error") {
        let message = err.get("message").and_then(Value::as_str).unwrap_or("");
        return fail_closed_decision(kind, &format!("hook bridge: {message}"));
    }
    // Sentinel: client returned a JSON-RPC error.
    if let Some(err) = value.get("_jsonrpc_error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let message = err.get("message").and_then(Value::as_str).unwrap_or("");
        return fail_closed_decision(kind, &format!("client error {code}: {message}"));
    }
    // Sentinel: client disconnected before answering.
    if value.get("_client_disconnected").is_some() {
        return fail_closed_decision(kind, "answering client disconnected before responding");
    }
    // Sentinel: session closed before the prompt was answered.
    if value.get("_session_closed").is_some() {
        return fail_closed_decision(kind, "session closed before prompt answered");
    }

    // Wire shape: `{ "decision": "allow"|"deny"|"passthrough", … }`
    decode_hook_decision(kind, value)
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
///
/// `kind` drives the fail-closed / fail-open decision when the wire
/// shape is malformed (missing `decision` field, unknown decision
/// variant). Security-critical kinds (`pre_tool_use`,
/// `permission_request`) deny on malformed input; observational kinds
/// (`post_tool_use`, `stop`, `notification`, …) passthrough so a
/// flapping client doesn't grind the agent to a halt.
fn decode_hook_decision(kind: &str, value: &Value) -> HookDecision {
    // Missing decision key — security-critical hooks must NOT default
    // to passthrough; fail closed instead. Round 3 — fix I1.
    let Some(decision_str) = value.get("decision").and_then(Value::as_str) else {
        tracing::warn!(
            hook = %kind,
            "hook bridge: missing decision field; failing per kind policy"
        );
        return fail_closed_decision(kind, "hook bridge: missing decision field");
    };
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
            tracing::warn!(
                decision = %other,
                hook = %kind,
                "hook bridge: unknown decision; failing per kind policy"
            );
            return fail_closed_decision(kind, &format!("hook bridge: unknown decision '{other}'"));
        }
    };

    // Optional control fields — match the SyncHookJSONOutput shape.
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

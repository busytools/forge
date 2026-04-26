//! Inbound `control_request` dispatch: `Client::handle_control` routes
//! permission checks, MCP JSON-RPC, and hook callbacks to the appropriate
//! handler and writes the matching `control_response`.
//!
//! Split out from `client.rs` (audit finding I5) to separate inbound
//! dispatch from outbound control issuance and from the lifecycle /
//! `next_event` loop.
//!
//! ## Two dispatch paths
//!
//! 1. **Inline** (`Client::handle_control`) — used by
//!    [`Client::next_event`] for single-task consumers. Writes via
//!    `&mut self.sub`.
//! 2. **Detached** ([`ControlDispatchHandle::dispatch`]) — used by
//!    consumers that want the actor pattern (long-running `next_event`
//!    in one task + concurrent commands in another). Writes via a
//!    cloned [`AsyncWriter`] handle. Each dispatch is `tokio::spawn`'d
//!    so a slow callback can't block the actor's command loop. Closes
//!    the audit 2026-04-26 G1 hazard (cancel-mid-write deadlock).
//!
//! The two paths share their logic: each terminal `write_line` call
//! goes through an internal trait, and both code paths call the same
//! orchestration.

use std::collections::HashMap;
use std::sync::Arc;

use crate::Error;
use crate::client::Client;
use crate::control::{
    AllowBehavior, ControlRequest, ControlRequestKind, ControlResponse, ControlResponseKind,
    ControlResponseType,
};
use crate::hooks::callback::ErasedHookCallback;
use crate::hooks::outputs::encode_updated_input_wrapper;
use crate::hooks::{HookContext, HookDecision, HookKind};
use crate::mcp::orchestration::McpHosts;
use crate::mcp::protocol::JsonRpcRequest;
use crate::permissions::{CanUseToolCallback, PermissionDecision, ToolPermissionContext};
use crate::transport::AsyncWriter;

impl Client {
    /// Dispatch an inbound `control_request`: MCP messages to the
    /// in-process host, hook callbacks to the minted callback id, and
    /// `can_use_tool` requests to the permission callback. Any other
    /// subtype gets an `unsupported control-request subtype` error.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn handle_control(&mut self, req: ControlRequest) -> Result<(), Error> {
        // Capture the original input here — we need it to echo into
        // `updatedInput` when the callback supplies no override.
        let original_input = req.original_tool_input().cloned();

        // Forward-compat: unknown subtype from the CLI. Log loudly so
        // drift is visible, then return an error response so the CLI
        // doesn't hang waiting for our decision. The session continues.
        if let ControlRequestKind::Unknown { subtype, raw } = &req.request {
            tracing::warn!(
                %subtype,
                raw = %raw,
                request_id = %req.request_id,
                "unknown control_request subtype — responding with error, session continues"
            );
            return self.write_unsupported_control_error(&req).await;
        }

        // MCP message routing — in-process dispatch to the named server.
        if let ControlRequestKind::McpMessage {
            server_name,
            message,
        } = &req.request
        {
            return self.handle_mcp_message(&req, server_name, message).await;
        }

        // Hook callback — dispatch by opaque callback_id.
        if let ControlRequestKind::HookCallback {
            callback_id,
            input,
            tool_use_id,
        } = &req.request
        {
            return self
                .handle_hook_callback(&req, callback_id, input, tool_use_id.as_deref())
                .await;
        }

        let decision = match (&self.can_use_tool, &req.request) {
            (
                Some(cb),
                ControlRequestKind::CanUseTool {
                    tool_name,
                    input,
                    tool_use_id,
                    agent_id,
                    permission_suggestions,
                    ..
                },
            ) => {
                // Decode Python's typed `PermissionUpdate` suggestions out
                // of the raw `Vec<Value>` the decoder captured. CLI schema
                // evolution can introduce new variants — log loudly when
                // dropping so drift is visible (mirrors the Unknown-variant
                // pattern used elsewhere) instead of silently emptying the
                // suggestions list and breaking permission UX.
                let suggestions: Vec<crate::permissions::PermissionUpdate> = permission_suggestions
                    .iter()
                    .filter_map(|v| match serde_json::from_value(v.clone()) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                raw = %v,
                                "permission_suggestion failed to decode; dropping (CLI schema drift?)"
                            );
                            None
                        }
                    })
                    .collect();
                let ctx = ToolPermissionContext::new(
                    tool_name.clone(),
                    input.clone(),
                    tool_use_id.clone(),
                    agent_id.clone(),
                )
                .with_suggestions(suggestions);
                cb.call(ctx).await
            }
            (None, ControlRequestKind::CanUseTool { .. }) => {
                // No callback registered — default to deny. This matches
                // what the binary would see if the SDK had no business
                // being in the loop.
                PermissionDecision::deny("no permission callback registered")
            }
            _ => {
                return self.write_unsupported_control_error(&req).await;
            }
        };

        let behavior = if decision.is_allow() {
            let perms = decision.updated_permissions();
            let updated_permissions = if perms.is_empty() {
                None
            } else {
                Some(
                    serde_json::to_value(perms)
                        .map_err(|e| Error::encode("updated_permissions", e))?,
                )
            };
            AllowBehavior::Allow {
                updated_input: decision
                    .updated_input()
                    .cloned()
                    .or(original_input)
                    .unwrap_or(serde_json::Value::Null),
                updated_permissions,
            }
        } else {
            AllowBehavior::Deny {
                message: decision.reason().unwrap_or("denied").to_string(),
                interrupt: false,
            }
        };

        let resp = req
            .build_response(behavior)
            .map_err(|e| Error::message_parse(format!("could not build control response: {e}")))?;
        let mut line = serde_json::to_string(&resp).map_err(|e| {
            Error::message_parse(format!("could not serialise control response: {e}"))
        })?;
        line.push('\n');
        self.sub.write_line(&line).await?;
        Ok(())
    }

    async fn write_unsupported_control_error(&mut self, req: &ControlRequest) -> Result<(), Error> {
        let resp = ControlResponse {
            ty: ControlResponseType::ControlResponse,
            response: ControlResponseKind::Error {
                request_id: req.request_id.clone(),
                error: "unsupported control-request subtype".into(),
            },
        };
        let mut line = serde_json::to_string(&resp)
            .map_err(|e| Error::message_parse(format!("error response serialise: {e}")))?;
        line.push('\n');
        self.sub.write_line(&line).await
    }

    /// Handle an MCP JSON-RPC `mcp_message` control request — dispatch to
    /// the registered in-process server and write the wrapped response.
    async fn handle_mcp_message(
        &mut self,
        req: &ControlRequest,
        server_name: &str,
        message: &serde_json::Value,
    ) -> Result<(), Error> {
        if !self.mcp_hosts.has(server_name) {
            let resp = ControlResponse {
                ty: ControlResponseType::ControlResponse,
                response: ControlResponseKind::Error {
                    request_id: req.request_id.clone(),
                    error: format!("unknown MCP server: {server_name}"),
                },
            };
            let mut line = serde_json::to_string(&resp)
                .map_err(|e| Error::message_parse(format!("error response serialise: {e}")))?;
            line.push('\n');
            return self.sub.write_line(&line).await;
        }

        // Parse the inner JSON-RPC request.
        let jsonrpc: JsonRpcRequest = serde_json::from_value(message.clone())
            .map_err(|e| Error::message_parse(format!("bad JSON-RPC envelope: {e}")))?;

        // Dispatch. Notifications (no id) return None; synthesise an empty
        // result wrapper so a control_response is always emitted (matches
        // Python SDK behaviour where `control_response` is always written).
        let jsonrpc_response = match self.mcp_hosts.dispatch(server_name, &jsonrpc).await {
            Some(r) => serde_json::to_value(&r)
                .map_err(|e| Error::message_parse(format!("mcp response serialise: {e}")))?,
            None => serde_json::json!({"jsonrpc": "2.0", "result": {}}),
        };

        let wrapper = serde_json::json!({"mcp_response": jsonrpc_response});
        let resp = ControlResponse {
            ty: ControlResponseType::ControlResponse,
            response: ControlResponseKind::Success {
                request_id: req.request_id.clone(),
                response: wrapper,
            },
        };
        let mut line = serde_json::to_string(&resp)
            .map_err(|e| Error::message_parse(format!("mcp control response serialise: {e}")))?;
        line.push('\n');
        self.sub.write_line(&line).await
    }

    /// Handle a `hook_callback` control request — dispatch by opaque
    /// `callback_id` and emit the appropriate `hookSpecificOutput` wrapper
    /// per event kind.
    async fn handle_hook_callback(
        &mut self,
        req: &ControlRequest,
        callback_id: &str,
        input: &serde_json::Value,
        tool_use_id: Option<&str>,
    ) -> Result<(), Error> {
        let event_name = input
            .get("hook_event_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Unknown")
            .to_string();
        let kind = HookKind::from_wire(&event_name);

        let ctx = HookContext {
            kind,
            tool_name: input
                .get("tool_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            session_id: self.session_id.clone(),
            tool_use_id: tool_use_id.map(str::to_string),
        };

        let decision = if let Some(cb) = self.hook_callbacks.get(callback_id) {
            cb.call_erased(input.clone(), ctx).await
        } else {
            tracing::warn!(%callback_id, "hook_callback for unknown id; passthrough");
            HookDecision::passthrough()
        };

        // Deferred hooks short-circuit the normal body-building. Python
        // `types.py:448-460` defines `AsyncHookJSONOutput` as exactly
        // `{"async": true, "asyncTimeout": <ms>?}` — no `decision` /
        // `hookSpecificOutput` / control fields. Emit that shape and
        // return.
        if decision.is_deferred() {
            let mut defer_body = serde_json::Map::new();
            defer_body.insert("async".into(), serde_json::Value::Bool(true));
            if let Some(timeout) = decision.defer_timeout_ms() {
                defer_body.insert("asyncTimeout".into(), serde_json::json!(timeout));
            }
            let ctrl = ControlResponse {
                ty: ControlResponseType::ControlResponse,
                response: ControlResponseKind::Success {
                    request_id: req.request_id.clone(),
                    response: serde_json::Value::Object(defer_body),
                },
            };
            let mut line = serde_json::to_string(&ctrl)
                .map_err(|e| Error::encode("deferred hook payload", e))?;
            line.push('\n');
            self.sub.write_line(&line).await?;
            return Ok(());
        }

        let mut response_body = match (decision.is_allow(), decision.reason()) {
            (true, _) => serde_json::json!({}),
            (false, Some(reason)) => serde_json::json!({"decision": "block", "reason": reason}),
            (false, None) => serde_json::json!({"decision": "block"}),
        };

        if let Some(map) = response_body.as_object_mut() {
            // Python SDK ships these as top-level fields alongside
            // `decision`, with `_convert_hook_output_for_cli` mapping
            // `continue_` → `continue` on the wire. Match the wire
            // names exactly (`_internal/query.py:40-55`).
            if let Some(cont) = decision.continue_execution() {
                map.insert("continue".into(), serde_json::Value::Bool(cont));
            }
            if let Some(suppress) = decision.suppress_output() {
                map.insert("suppressOutput".into(), serde_json::Value::Bool(suppress));
            }
            if let Some(stop) = decision.stop_reason() {
                map.insert("stopReason".into(), serde_json::Value::String(stop.into()));
            }
            if let Some(msg) = decision.system_message() {
                map.insert(
                    "systemMessage".into(),
                    serde_json::Value::String(msg.into()),
                );
            }
        }

        if let Some(updated) = decision.updated_input()
            && let Some(wrapper) = encode_updated_input_wrapper(kind, updated)
            && let Some(map) = response_body.as_object_mut()
        {
            map.insert("hookSpecificOutput".into(), wrapper);
        }

        let ctrl = ControlResponse {
            ty: ControlResponseType::ControlResponse,
            response: ControlResponseKind::Success {
                request_id: req.request_id.clone(),
                response: response_body,
            },
        };
        let mut line =
            serde_json::to_string(&ctrl).map_err(|e| Error::encode("hook response", e))?;
        line.push('\n');
        self.sub.write_line(&line).await?;
        Ok(())
    }
}

// =============================================================================
// Detached dispatch — closes audit 2026-04-26 G1 hazard.
//
// `Client::handle_control` writes via `&mut self.sub` and runs inline
// inside `next_event`. When the daemon's session actor select!s
// `next_event` against a command channel, a cmd preempting an
// in-flight `handle_control` cancels it mid-callback and the CLI
// never gets its `control_response` (deadlock for HOOK_TIMEOUT_SECS).
//
// `ControlDispatchHandle` is a clonable bundle of writer + callbacks
// + state. Its `dispatch` method runs the same logic but via an
// `AsyncWriter` clone, so the daemon's actor can `tokio::spawn` the
// dispatch and the cancel preemption no longer matters — the spawned
// task runs to completion regardless of select! cancellation.
//
// Available only on transports that override
// [`Transport::try_clone_writer`]. Subprocess (the SDK default) does
// not; use the daemon's `BridgedTransport` for the actor pattern.
// =============================================================================

/// Clonable bundle of state + writer that dispatches a single
/// `control_request`. Constructed via [`Client::try_dispatch_handle`].
///
/// Each field is `Arc`-backed (or `Clone`); cloning the handle is
/// cheap. Designed to be moved into a `tokio::spawn`'d task per
/// inbound `control_request`.
#[derive(Clone)]
pub struct ControlDispatchHandle {
    writer: Arc<dyn AsyncWriter>,
    can_use_tool: Option<Arc<dyn CanUseToolCallback>>,
    mcp_hosts: McpHosts,
    hook_callbacks: HashMap<String, Arc<dyn ErasedHookCallback>>,
    session_id: String,
}

impl std::fmt::Debug for ControlDispatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlDispatchHandle")
            .field("writer", &self.writer)
            .field(
                "can_use_tool",
                &self.can_use_tool.as_ref().map(|_| "<callback>"),
            )
            .field("mcp_hosts", &self.mcp_hosts)
            .field(
                "hook_callbacks",
                &format!("<{} hooks>", self.hook_callbacks.len()),
            )
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl ControlDispatchHandle {
    pub(crate) fn new(
        writer: Arc<dyn AsyncWriter>,
        can_use_tool: Option<Arc<dyn CanUseToolCallback>>,
        mcp_hosts: McpHosts,
        hook_callbacks: HashMap<String, Arc<dyn ErasedHookCallback>>,
        session_id: String,
    ) -> Self {
        Self {
            writer,
            can_use_tool,
            mcp_hosts,
            hook_callbacks,
            session_id,
        }
    }

    /// Dispatch one inbound `control_request`. Mirrors
    /// `Client::handle_control`'s logic but writes via the cloned
    /// [`AsyncWriter`] instead of `&mut self.sub`. Safe to call from
    /// a `tokio::spawn`'d task — runs to completion regardless of
    /// caller cancellation.
    ///
    /// # Errors
    ///
    /// Same shape as `Client::handle_control`: encode/serialise
    /// failures from [`Error::message_parse`] / [`Error::encode`],
    /// and write failures from the underlying writer.
    #[allow(clippy::too_many_lines)]
    pub async fn dispatch(&self, req: ControlRequest) -> Result<(), Error> {
        let original_input = req.original_tool_input().cloned();

        if let ControlRequestKind::Unknown { subtype, raw } = &req.request {
            tracing::warn!(
                %subtype,
                raw = %raw,
                request_id = %req.request_id,
                "unknown control_request subtype — responding with error, session continues"
            );
            return self.write_unsupported(&req).await;
        }

        if let ControlRequestKind::McpMessage {
            server_name,
            message,
        } = &req.request
        {
            return self.handle_mcp(&req, server_name, message).await;
        }

        if let ControlRequestKind::HookCallback {
            callback_id,
            input,
            tool_use_id,
        } = &req.request
        {
            return self
                .handle_hook(&req, callback_id, input, tool_use_id.as_deref())
                .await;
        }

        let decision = match (&self.can_use_tool, &req.request) {
            (
                Some(cb),
                ControlRequestKind::CanUseTool {
                    tool_name,
                    input,
                    tool_use_id,
                    agent_id,
                    permission_suggestions,
                    ..
                },
            ) => {
                let suggestions: Vec<crate::permissions::PermissionUpdate> = permission_suggestions
                    .iter()
                    .filter_map(|v| match serde_json::from_value(v.clone()) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                raw = %v,
                                "permission_suggestion failed to decode; dropping (CLI schema drift?)"
                            );
                            None
                        }
                    })
                    .collect();
                let ctx = ToolPermissionContext::new(
                    tool_name.clone(),
                    input.clone(),
                    tool_use_id.clone(),
                    agent_id.clone(),
                )
                .with_suggestions(suggestions);
                cb.call(ctx).await
            }
            (None, ControlRequestKind::CanUseTool { .. }) => {
                PermissionDecision::deny("no permission callback registered")
            }
            _ => {
                return self.write_unsupported(&req).await;
            }
        };

        let behavior = if decision.is_allow() {
            let perms = decision.updated_permissions();
            let updated_permissions = if perms.is_empty() {
                None
            } else {
                Some(
                    serde_json::to_value(perms)
                        .map_err(|e| Error::encode("updated_permissions", e))?,
                )
            };
            AllowBehavior::Allow {
                updated_input: decision
                    .updated_input()
                    .cloned()
                    .or(original_input)
                    .unwrap_or(serde_json::Value::Null),
                updated_permissions,
            }
        } else {
            AllowBehavior::Deny {
                message: decision.reason().unwrap_or("denied").to_string(),
                interrupt: false,
            }
        };

        let resp = req
            .build_response(behavior)
            .map_err(|e| Error::message_parse(format!("could not build control response: {e}")))?;
        let mut line = serde_json::to_string(&resp).map_err(|e| {
            Error::message_parse(format!("could not serialise control response: {e}"))
        })?;
        line.push('\n');
        self.writer.write_line(&line).await
    }

    async fn write_unsupported(&self, req: &ControlRequest) -> Result<(), Error> {
        let resp = ControlResponse {
            ty: ControlResponseType::ControlResponse,
            response: ControlResponseKind::Error {
                request_id: req.request_id.clone(),
                error: "unsupported control-request subtype".into(),
            },
        };
        let mut line = serde_json::to_string(&resp)
            .map_err(|e| Error::message_parse(format!("error response serialise: {e}")))?;
        line.push('\n');
        self.writer.write_line(&line).await
    }

    async fn handle_mcp(
        &self,
        req: &ControlRequest,
        server_name: &str,
        message: &serde_json::Value,
    ) -> Result<(), Error> {
        if !self.mcp_hosts.has(server_name) {
            let resp = ControlResponse {
                ty: ControlResponseType::ControlResponse,
                response: ControlResponseKind::Error {
                    request_id: req.request_id.clone(),
                    error: format!("unknown MCP server: {server_name}"),
                },
            };
            let mut line = serde_json::to_string(&resp)
                .map_err(|e| Error::message_parse(format!("error response serialise: {e}")))?;
            line.push('\n');
            return self.writer.write_line(&line).await;
        }

        let jsonrpc: JsonRpcRequest = serde_json::from_value(message.clone())
            .map_err(|e| Error::message_parse(format!("bad JSON-RPC envelope: {e}")))?;

        let jsonrpc_response = match self.mcp_hosts.dispatch(server_name, &jsonrpc).await {
            Some(r) => serde_json::to_value(&r)
                .map_err(|e| Error::message_parse(format!("mcp response serialise: {e}")))?,
            None => serde_json::json!({"jsonrpc": "2.0", "result": {}}),
        };

        let wrapper = serde_json::json!({"mcp_response": jsonrpc_response});
        let resp = ControlResponse {
            ty: ControlResponseType::ControlResponse,
            response: ControlResponseKind::Success {
                request_id: req.request_id.clone(),
                response: wrapper,
            },
        };
        let mut line = serde_json::to_string(&resp)
            .map_err(|e| Error::message_parse(format!("mcp control response serialise: {e}")))?;
        line.push('\n');
        self.writer.write_line(&line).await
    }

    async fn handle_hook(
        &self,
        req: &ControlRequest,
        callback_id: &str,
        input: &serde_json::Value,
        tool_use_id: Option<&str>,
    ) -> Result<(), Error> {
        let event_name = input
            .get("hook_event_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Unknown")
            .to_string();
        let kind = HookKind::from_wire(&event_name);

        let ctx = HookContext {
            kind,
            tool_name: input
                .get("tool_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            session_id: self.session_id.clone(),
            tool_use_id: tool_use_id.map(str::to_string),
        };

        let decision = if let Some(cb) = self.hook_callbacks.get(callback_id) {
            cb.call_erased(input.clone(), ctx).await
        } else {
            tracing::warn!(%callback_id, "hook_callback for unknown id; passthrough");
            HookDecision::passthrough()
        };

        if decision.is_deferred() {
            let mut defer_body = serde_json::Map::new();
            defer_body.insert("async".into(), serde_json::Value::Bool(true));
            if let Some(timeout) = decision.defer_timeout_ms() {
                defer_body.insert("asyncTimeout".into(), serde_json::json!(timeout));
            }
            let ctrl = ControlResponse {
                ty: ControlResponseType::ControlResponse,
                response: ControlResponseKind::Success {
                    request_id: req.request_id.clone(),
                    response: serde_json::Value::Object(defer_body),
                },
            };
            let mut line = serde_json::to_string(&ctrl)
                .map_err(|e| Error::encode("deferred hook payload", e))?;
            line.push('\n');
            self.writer.write_line(&line).await?;
            return Ok(());
        }

        let mut response_body = match (decision.is_allow(), decision.reason()) {
            (true, _) => serde_json::json!({}),
            (false, Some(reason)) => serde_json::json!({"decision": "block", "reason": reason}),
            (false, None) => serde_json::json!({"decision": "block"}),
        };

        if let Some(map) = response_body.as_object_mut() {
            if let Some(cont) = decision.continue_execution() {
                map.insert("continue".into(), serde_json::Value::Bool(cont));
            }
            if let Some(suppress) = decision.suppress_output() {
                map.insert("suppressOutput".into(), serde_json::Value::Bool(suppress));
            }
            if let Some(stop) = decision.stop_reason() {
                map.insert("stopReason".into(), serde_json::Value::String(stop.into()));
            }
            if let Some(msg) = decision.system_message() {
                map.insert(
                    "systemMessage".into(),
                    serde_json::Value::String(msg.into()),
                );
            }
        }

        if let Some(updated) = decision.updated_input()
            && let Some(wrapper) = encode_updated_input_wrapper(kind, updated)
            && let Some(map) = response_body.as_object_mut()
        {
            map.insert("hookSpecificOutput".into(), wrapper);
        }

        let ctrl = ControlResponse {
            ty: ControlResponseType::ControlResponse,
            response: ControlResponseKind::Success {
                request_id: req.request_id.clone(),
                response: response_body,
            },
        };
        let mut line =
            serde_json::to_string(&ctrl).map_err(|e| Error::encode("hook response", e))?;
        line.push('\n');
        self.writer.write_line(&line).await?;
        Ok(())
    }
}

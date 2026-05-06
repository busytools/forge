//! Inbound `control_request` dispatch: [`ControlDispatchHandle`]
//! routes permission checks, MCP JSON-RPC, and hook callbacks to the
//! appropriate handler and writes the matching `control_response` via
//! a clonable [`AsyncWriter`].
//!
//! Internal: built once during [`Client::spawn`], cloned and moved
//! into a `tokio::spawn`'d task per inbound `control_request` by the
//! reader task in [`crate::client::runtime`]. The clonable writer + the
//! `tokio::spawn` together close audit 2026-04-26 G1 — a slow callback
//! cannot block the read loop, and the actor's `select!` cancellation
//! cannot drop a `control_response` write mid-flight.
//!
//! Used the same way during the synchronous init handshake — the
//! init loop calls [`ControlDispatchHandle::dispatch`] directly for
//! interleaved `control_request`s, since no concurrent reader exists
//! yet.

use std::collections::HashMap;
use std::sync::Arc;

use crate::Error;
use crate::control::{
    AllowBehavior, ControlRequest, ControlRequestKind, ControlResponse, ControlResponseKind,
    ControlResponseType,
};
use crate::hooks::callback::ErasedHookCallback;
use crate::hooks::{HookContext, HookDecision, HookKind};
use crate::mcp::orchestration::McpHosts;
use crate::mcp::protocol::JsonRpcRequest;
use crate::permissions::{CanUseToolCallback, PermissionDecision, ToolPermissionContext};
use crate::transport::AsyncWriter;
use forge_primitives::hooks::outputs::encode_updated_input_wrapper;

// =============================================================================
// Detached dispatch — closes audit 2026-04-26 G1 hazard.
//
// Inbound `control_request`s go through `dispatch`, which writes the
// matching `control_response` via a clonable [`AsyncWriter`]. The
// reader task in [`crate::client::runtime`] `tokio::spawn`s a fresh
// task per inbound request so a slow callback can't block the read
// loop AND cancellation of the actor's `select!` over a command
// channel + `next_event` cannot drop the response write mid-flight.
//
// Available on any transport that overrides
// [`Transport::try_clone_writer`]. The shipped Subprocess does — its
// writer task accepts mpsc clones — so any client built via
// [`Client::spawn`] gets the cancel-safe behaviour out of the box.
// =============================================================================

/// Clonable bundle of state + writer that dispatches a single
/// `control_request`. Internal — built once during
/// [`Client::spawn`](crate::Client::spawn) and cloned by the reader
/// task per inbound request.
///
/// Each field is `Arc`-backed (or `Clone`); cloning the handle is
/// cheap. Designed to be moved into a `tokio::spawn`'d task per
/// inbound `control_request`.
#[derive(Clone)]
pub(crate) struct ControlDispatchHandle {
    writer: Arc<dyn AsyncWriter>,
    can_use_tool: Option<Arc<dyn CanUseToolCallback>>,
    mcp_hosts: McpHosts,
    hook_callbacks: HashMap<String, Arc<dyn ErasedHookCallback>>,
    session_id: crate::client::runtime::SharedSessionId,
}

impl std::fmt::Debug for ControlDispatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlDispatchHandle")
            .field("writer", &self.writer)
            .field("can_use_tool", &self.can_use_tool.as_ref().map(|_| "<callback>"))
            .field("mcp_hosts", &self.mcp_hosts)
            .field("hook_callbacks", &format!("<{} hooks>", self.hook_callbacks.len()))
            .field("session_id", &"<shared>")
            .finish()
    }
}

impl ControlDispatchHandle {
    pub(crate) fn new(
        writer: Arc<dyn AsyncWriter>,
        can_use_tool: Option<Arc<dyn CanUseToolCallback>>,
        mcp_hosts: McpHosts,
        hook_callbacks: HashMap<String, Arc<dyn ErasedHookCallback>>,
        session_id: crate::client::runtime::SharedSessionId,
    ) -> Self {
        Self { writer, can_use_tool, mcp_hosts, hook_callbacks, session_id }
    }

    /// Capture a session id from an incoming Message. No-op once a
    /// non-empty id has been bound. Called by the reader task on every
    /// message.
    pub(crate) fn capture_session_id_from(&self, msg: &forge_primitives::Message) {
        if let Some(id) = msg.session_id()
            && !id.is_empty()
        {
            let mut current = self.session_id.write();
            if current.is_empty() {
                *current = id.to_string();
                tracing::debug!(session_id = %*current, "client session_id bound");
            }
        }
    }

    /// Dispatch one inbound `control_request`. Routes the request
    /// to the right handler (MCP, hook callback, `can_use_tool`) and
    /// writes the matching `control_response` via the cloned
    /// [`AsyncWriter`]. Safe to call from a `tokio::spawn`'d task —
    /// runs to completion regardless of caller cancellation.
    ///
    /// # Errors
    ///
    /// Same shape as `Client::handle_control`: encode/serialise
    /// failures from [`Error::message_parse`] / [`Error::encode`],
    /// and write failures from the underlying writer.
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn dispatch(&self, req: ControlRequest) -> Result<(), Error> {
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

        if let ControlRequestKind::McpMessage { server_name, message } = &req.request {
            return self.handle_mcp(&req, server_name, message).await;
        }

        if let ControlRequestKind::HookCallback { callback_id, input, tool_use_id } = &req.request {
            return self.handle_hook(&req, callback_id, input, tool_use_id.as_deref()).await;
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
                    blocked_path,
                    decision_reason,
                    title,
                    display_name,
                    description,
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
                .with_suggestions(suggestions)
                .with_display(
                    blocked_path.clone(),
                    decision_reason.clone(),
                    title.clone(),
                    display_name.clone(),
                    description.clone(),
                );
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
            session_id: self.session_id.read().clone(),
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
                map.insert("systemMessage".into(), serde_json::Value::String(msg.into()));
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

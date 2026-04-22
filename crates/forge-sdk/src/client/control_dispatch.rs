//! Inbound `control_request` dispatch: [`Client::handle_control`] routes
//! permission checks, MCP JSON-RPC, and hook callbacks to the appropriate
//! handler and writes the matching `control_response`.
//!
//! Split out from `client.rs` (audit finding I5) to separate inbound
//! dispatch from outbound control issuance and from the lifecycle /
//! `next_event` loop.

use crate::Error;
use crate::client::Client;
use crate::control::{
    AllowBehavior, ControlRequest, ControlRequestKind, ControlResponse, ControlResponseKind,
    ControlResponseType,
};
use crate::hooks::outputs::encode_updated_input_wrapper;
use crate::hooks::{HookContext, HookDecision, HookKind};
use crate::mcp::protocol::JsonRpcRequest;
use crate::permissions::{PermissionDecision, ToolPermissionContext};

impl Client {
    /// Dispatch an inbound `control_request`: MCP messages to the
    /// in-process host, hook callbacks to the minted callback id, and
    /// `can_use_tool` requests to the permission callback. Any other
    /// subtype gets an `unsupported control-request subtype` error.
    pub(super) async fn handle_control(&mut self, req: ControlRequest) -> Result<(), Error> {
        // Capture the original input here — we need it to echo into
        // `updatedInput` when the callback supplies no override.
        let original_input = req.original_tool_input().cloned();

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
                    ..
                },
            ) => {
                let ctx = ToolPermissionContext::new(
                    tool_name.clone(),
                    input.clone(),
                    tool_use_id.clone(),
                    agent_id.clone(),
                );
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
                    serde_json::to_value(perms).map_err(|e| Error::MessageParse {
                        reason: format!("could not encode updated_permissions: {e}"),
                    })?,
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
            .map_err(|e| Error::MessageParse {
                reason: format!("could not build control response: {e}"),
            })?;
        let mut line = serde_json::to_string(&resp).map_err(|e| Error::MessageParse {
            reason: format!("could not serialise control response: {e}"),
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
        let mut line = serde_json::to_string(&resp).map_err(|e| Error::MessageParse {
            reason: format!("error response serialise: {e}"),
        })?;
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
            let mut line = serde_json::to_string(&resp).map_err(|e| Error::MessageParse {
                reason: format!("error response serialise: {e}"),
            })?;
            line.push('\n');
            return self.sub.write_line(&line).await;
        }

        // Parse the inner JSON-RPC request.
        let jsonrpc: JsonRpcRequest =
            serde_json::from_value(message.clone()).map_err(|e| Error::MessageParse {
                reason: format!("bad JSON-RPC envelope: {e}"),
            })?;

        // Dispatch. Notifications (no id) return None; synthesise an empty
        // result wrapper so a control_response is always emitted (matches
        // Python SDK behaviour where `control_response` is always written).
        let jsonrpc_response = match self.mcp_hosts.dispatch(server_name, &jsonrpc).await {
            Some(r) => serde_json::to_value(&r).map_err(|e| Error::MessageParse {
                reason: format!("mcp response serialise: {e}"),
            })?,
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
        let mut line = serde_json::to_string(&resp).map_err(|e| Error::MessageParse {
            reason: format!("mcp control response serialise: {e}"),
        })?;
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
        let mut line = serde_json::to_string(&ctrl).map_err(|e| Error::MessageParse {
            reason: format!("hook response encode: {e}"),
        })?;
        line.push('\n');
        self.sub.write_line(&line).await?;
        Ok(())
    }
}

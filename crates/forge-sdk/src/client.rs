//! Public [`Client`] — the entry point consumers hold.

use std::sync::Arc;

use tracing::debug;

use std::collections::HashMap;

use crate::Error;
use crate::control::{
    AllowBehavior, ControlRequest, ControlRequestKind, ControlResponse, ControlResponseKind,
    ControlResponseType,
};
use crate::hooks::{ErasedHookCallback, HookContext, HookDecision, HookKind};
use crate::mcp::orchestration::McpHosts;
use crate::mcp::protocol::JsonRpcRequest;
use crate::messages::Message;
use crate::options::Options;
use crate::permissions::{CanUseToolCallback, PermissionDecision, ToolPermissionContext};
use crate::transport::codec::{DecodedLine, decode_dispatch, decode_line, encode_user_prompt};
use crate::transport::process::Subprocess;

/// An active `claude` binary subprocess.
///
/// Construct via [`spawn`](Self::spawn). The first line the binary emits is
/// always a `system`/`init` message carrying the session id — `spawn`
/// consumes it so callers start clean at the first `assistant` turn.
pub struct Client {
    sub: Subprocess,
    session_id: String,
    line_number: u64,
    can_use_tool: Option<Arc<dyn CanUseToolCallback>>,
    mcp_hosts: McpHosts,
    hook_callbacks: HashMap<String, Arc<dyn ErasedHookCallback>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("sub", &self.sub)
            .field("session_id", &self.session_id)
            .field("line_number", &self.line_number)
            .field(
                "can_use_tool",
                &self.can_use_tool.as_ref().map(|_| "<callback>"),
            )
            .field("mcp_hosts", &self.mcp_hosts)
            .field(
                "hook_callbacks",
                &format!("<{} hooks>", self.hook_callbacks.len()),
            )
            .finish()
    }
}

impl Client {
    /// Spawn `claude` with the given options and drain the init line.
    ///
    /// # Errors
    ///
    /// Any [`Error`] variant; see field docs.
    pub async fn spawn(options: Options) -> Result<Self, Error> {
        let can_use_tool = options.can_use_tool.clone();
        let mcp_hosts = McpHosts::new(options.mcp_servers.clone());
        let hook_registry = options.hooks.mint_registry();
        let hook_payload = hook_registry.to_initialize_payload();
        let hook_callbacks = hook_registry.by_id;
        // Concrete-list skills populate `initialize.skills`. `"all"` marker
        // travels via `--allowedTools` only and does NOT appear in the
        // initialize payload (matches Python SDK).
        let skills_payload: Vec<String> = options
            .skills
            .iter()
            .filter(|s| s.as_str() != "all")
            .cloned()
            .collect();
        let exclude_dynamic_sections = options.exclude_dynamic_sections;

        let mut sub = Subprocess::spawn(&options).await?;
        let init_line = sub.read_line().await?.ok_or_else(|| Error::Connection {
            reason: "subprocess closed stdout before init line".into(),
        })?;
        let init = decode_line(&init_line, 1)?;
        let session_id = match &init {
            Message::System {
                session_id: Some(id),
                subtype,
                ..
            } if subtype == "init" => id.clone(),
            other => {
                return Err(Error::MessageParse {
                    reason: format!("expected system/init, got: {other:?}"),
                });
            }
        };
        debug!(session_id, "client init");

        let mut client = Self {
            sub,
            session_id,
            line_number: 1,
            can_use_tool,
            mcp_hosts,
            hook_callbacks,
        };
        client
            .send_initialize(hook_payload, skills_payload, exclude_dynamic_sections)
            .await?;
        Ok(client)
    }

    /// Send the `initialize` `control_request` and await its matching
    /// `control_response`. Python SDK does this right after receiving the
    /// system/init message, before any user input.
    ///
    /// The body carries:
    /// - `hooks` — `{event_name: [{matcher, hookCallbackIds, timeout}]}`
    /// - `excludeDynamicSections` — bool
    /// - `skills` — list of concrete skill names (the `"all"` sentinel
    ///   travels via `--allowedTools` only).
    async fn send_initialize(
        &mut self,
        hooks: serde_json::Value,
        skills: Vec<String>,
        exclude_dynamic_sections: bool,
    ) -> Result<(), Error> {
        let request_id = crate::request_id::next();
        let body = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "initialize",
                "hooks": hooks,
                "excludeDynamicSections": exclude_dynamic_sections,
                "skills": skills,
                "agents": {},
            }
        });
        let mut line = serde_json::to_string(&body).map_err(|e| Error::MessageParse {
            reason: format!("initialize serialise: {e}"),
        })?;
        line.push('\n');
        self.sub.write_line(&line).await?;

        // Await the matching response. Control responses arrive on the
        // same stdio as regular messages — typically the very next line
        // with no user prompts racing.
        loop {
            let Some(response_line) = self.sub.read_line().await? else {
                return Err(Error::Connection {
                    reason: "subprocess closed before initialize response".into(),
                });
            };
            self.line_number += 1;
            let value: serde_json::Value =
                serde_json::from_str(&response_line).map_err(|source| Error::JsonDecode {
                    line: self.line_number,
                    source,
                })?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("control_response") {
                let resp_request_id = value
                    .pointer("/response/request_id")
                    .and_then(serde_json::Value::as_str);
                if resp_request_id == Some(&request_id) {
                    let subtype = value
                        .pointer("/response/subtype")
                        .and_then(serde_json::Value::as_str);
                    if subtype == Some("success") {
                        return Ok(());
                    }
                    let err = value
                        .pointer("/response/error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(Error::MessageParse {
                        reason: format!("initialize failed: {err}"),
                    });
                }
            }
            // Not our response — if it's some other control_request we
            // don't yet know how to handle at init time, log and drop.
            // Real CLI behaviour: the initialize response is always the
            // next frame; hitting this branch in production means a
            // protocol bug worth surfacing.
            tracing::warn!(line = %response_line, "unexpected frame during initialize");
        }
    }

    /// The session id captured from the init message.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Send a user prompt as a stream-json user turn.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on pipe write failure.
    pub async fn send_user_message(&mut self, prompt: &str) -> Result<(), Error> {
        let line = encode_user_prompt(prompt, &self.session_id)?;
        self.sub.write_line(&line).await
    }

    /// Read the next stream-json **regular** message from the subprocess.
    ///
    /// Control requests (permission checks, MCP messages, hook callbacks)
    /// are handled transparently: when one arrives, the client dispatches
    /// to the right callback and writes a `control_response` back, then
    /// loops to the next line. Callers only ever see regular `Message`s.
    ///
    /// Returns `Ok(None)` at end-of-stream (subprocess exited).
    ///
    /// # Errors
    ///
    /// - [`Error::JsonDecode`] / [`Error::MessageParse`] per line.
    /// - [`Error::Io`] on pipe read/write failure.
    pub async fn next_event(&mut self) -> Result<Option<Message>, Error> {
        loop {
            let Some(line) = self.sub.read_line().await? else {
                return Ok(None);
            };
            self.line_number += 1;
            match decode_dispatch(&line, self.line_number)? {
                DecodedLine::Message(msg) => return Ok(Some(msg)),
                DecodedLine::Control(req) => {
                    self.handle_control(req).await?;
                }
            }
        }
    }

    async fn handle_control(&mut self, req: ControlRequest) -> Result<(), Error> {
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
                // HookCallback / other subtypes handled elsewhere
                // (see Plan 3 hooks). Until they're wired up, respond
                // with an unsupported error.
                return self.write_unsupported_control_error(&req).await;
            }
        };

        let behavior = if decision.is_allow() {
            AllowBehavior::Allow {
                updated_input: decision
                    .updated_input()
                    .cloned()
                    .or(original_input)
                    .unwrap_or(serde_json::Value::Null),
                updated_permissions: None,
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

        if let Some(updated) = decision.updated_input() {
            let wrapper = match kind {
                HookKind::PreToolUse => Some(serde_json::json!({
                    "hookEventName": "PreToolUse",
                    "updatedInput": updated,
                })),
                HookKind::UserPromptSubmit => Some(serde_json::json!({
                    "hookEventName": "UserPromptSubmit",
                    "updatedPrompt": updated,
                })),
                _ => {
                    tracing::warn!(
                        ?kind,
                        "hook returned updated_input but hook kind doesn't support it; ignoring"
                    );
                    None
                }
            };
            if let Some(w) = wrapper {
                if let Some(map) = response_body.as_object_mut() {
                    map.insert("hookSpecificOutput".into(), w);
                }
            }
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

    /// Issue an outbound `control_request` with the given `subtype` and
    /// body, await the matching `control_response`, and return the inner
    /// `response` value on success.
    ///
    /// # Errors
    ///
    /// - [`Error::MessageParse`] when the CLI replies with an error
    ///   subtype or a malformed frame.
    /// - [`Error::Io`] on pipe read/write failure.
    async fn send_control(
        &mut self,
        subtype: &str,
        extra: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        let request_id = crate::request_id::next();
        let mut request_body = serde_json::Map::new();
        request_body.insert(
            "subtype".into(),
            serde_json::Value::String(subtype.to_string()),
        );
        if let serde_json::Value::Object(extra_map) = extra {
            for (k, v) in extra_map {
                request_body.insert(k, v);
            }
        }
        let envelope = serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": serde_json::Value::Object(request_body),
        });
        let mut line = serde_json::to_string(&envelope).map_err(|e| Error::MessageParse {
            reason: format!("control encode: {e}"),
        })?;
        line.push('\n');
        self.sub.write_line(&line).await?;

        loop {
            let Some(response_line) = self.sub.read_line().await? else {
                return Err(Error::Connection {
                    reason: format!("subprocess closed before {subtype} response"),
                });
            };
            self.line_number += 1;
            let value: serde_json::Value =
                serde_json::from_str(&response_line).map_err(|source| Error::JsonDecode {
                    line: self.line_number,
                    source,
                })?;
            if value.get("type").and_then(serde_json::Value::as_str) == Some("control_response") {
                let resp_request_id = value
                    .pointer("/response/request_id")
                    .and_then(serde_json::Value::as_str);
                if resp_request_id == Some(&request_id) {
                    let resp_subtype = value
                        .pointer("/response/subtype")
                        .and_then(serde_json::Value::as_str);
                    if resp_subtype == Some("success") {
                        return Ok(value
                            .pointer("/response/response")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null));
                    }
                    let err = value
                        .pointer("/response/error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(Error::MessageParse {
                        reason: format!("{subtype} failed: {err}"),
                    });
                }
            }
            tracing::warn!(
                line = %response_line,
                %subtype,
                "unexpected frame while awaiting control response"
            );
        }
    }

    /// Ask the CLI to interrupt the current turn (cancel in-flight tool
    /// calls and return control to the SDK).
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn interrupt(&mut self) -> Result<(), Error> {
        self.send_control("interrupt", serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Switch the permission mode mid-session.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn set_permission_mode(
        &mut self,
        mode: crate::options::PermissionMode,
    ) -> Result<(), Error> {
        self.send_control(
            "set_permission_mode",
            serde_json::json!({"mode": mode.as_cli_arg()}),
        )
        .await?;
        Ok(())
    }

    /// Ask the CLI to revert file edits made in the current turn.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn rewind_files(&mut self) -> Result<(), Error> {
        self.send_control("rewind_files", serde_json::json!({}))
            .await?;
        Ok(())
    }

    /// Reconnect a named MCP server (asks the CLI to drop + re-establish
    /// its connection to the named server).
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_reconnect(&mut self, server_name: &str) -> Result<(), Error> {
        self.send_control(
            "mcp_reconnect",
            serde_json::json!({"server_name": server_name}),
        )
        .await?;
        Ok(())
    }

    /// Toggle a named MCP server on/off.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_toggle(&mut self, server_name: &str, enabled: bool) -> Result<(), Error> {
        self.send_control(
            "mcp_toggle",
            serde_json::json!({"server_name": server_name, "enabled": enabled}),
        )
        .await?;
        Ok(())
    }

    /// Kill an in-flight sub-agent task by its tool-use id.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn stop_task(&mut self, tool_use_id: &str) -> Result<(), Error> {
        self.send_control("stop_task", serde_json::json!({"tool_use_id": tool_use_id}))
            .await?;
        Ok(())
    }

    /// Query MCP server status. Returns the raw response payload.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_status(&mut self) -> Result<serde_json::Value, Error> {
        self.send_control("mcp_status", serde_json::json!({})).await
    }

    /// Query current context usage (tokens consumed vs. budget). Returns
    /// the raw response payload.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn get_context_usage(&mut self) -> Result<serde_json::Value, Error> {
        self.send_control("get_context_usage", serde_json::json!({}))
            .await
    }

    /// Fork the current session into a new one at the given tool-use id
    /// boundary. Returns the new `session_id` the CLI assigned.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn fork_session(&mut self, at_tool_use_id: Option<&str>) -> Result<String, Error> {
        let mut body = serde_json::Map::new();
        if let Some(id) = at_tool_use_id {
            body.insert(
                "tool_use_id".into(),
                serde_json::Value::String(id.to_string()),
            );
        }
        let resp = self
            .send_control("fork_session", serde_json::Value::Object(body))
            .await?;
        resp.get("session_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::MessageParse {
                reason: format!("fork_session response missing session_id: {resp}"),
            })
    }

    /// Graceful shutdown. Closes stdin, waits for the subprocess to exit.
    ///
    /// # Errors
    ///
    /// [`Error::Process`] when the subprocess exits non-zero, [`Error::Io`]
    /// for I/O failure.
    pub async fn disconnect(self) -> Result<(), Error> {
        self.sub.shutdown().await
    }
}

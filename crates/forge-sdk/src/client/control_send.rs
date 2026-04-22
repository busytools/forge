//! Outbound `control_request` senders: the generic [`send_control`]
//! primitive plus the nine typed wrappers consumers call
//! (`interrupt`, `set_permission_mode`, `set_model`, `rewind_files`,
//! `mcp_reconnect`, `mcp_toggle`, `stop_task`, `mcp_status`,
//! `get_context_usage`), including `_raw` escape hatches for
//! `mcp_status` and `get_context_usage`.
//!
//! Session forking lives elsewhere: the spawn-time
//! [`Options::fork_session`](crate::Options) flag (surfaced via
//! `--fork-session`) and the offline
//! [`fork_session`](crate::session_mutations::fork_session) free
//! function. Python SDK v0.1.64 does not define a runtime
//! `fork_session` `control_request` subtype.
//!
//! Split out from `client.rs` (audit finding I5) to separate outbound
//! control issuance from inbound dispatch / lifecycle.

use crate::Error;
use crate::client::Client;

impl Client {
    /// Issue an outbound `control_request` with the given `subtype` and
    /// body, await the matching `control_response`, and return the inner
    /// `response` value on success.
    ///
    /// # Errors
    ///
    /// - [`Error::MessageParse`] when the CLI replies with an error
    ///   subtype or a malformed frame.
    /// - [`Error::Io`] on pipe read/write failure.
    pub(super) async fn send_control(
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
        let mut line = serde_json::to_string(&envelope)
            .map_err(|e| Error::message_parse(format!("control encode: {e}")))?;
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
                    return Err(Error::message_parse(format!("{subtype} failed: {err}")));
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

    /// Switch the model mid-session. Pass `Some("claude-sonnet-4-6")` or
    /// similar to pick a specific model; `None` reverts to the CLI
    /// default. Wire shape mirrors Python SDK `_internal/query.py:688-695`
    /// — `{"subtype": "set_model", "model": <string or null>}`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn set_model(&mut self, model: Option<&str>) -> Result<(), Error> {
        self.send_control("set_model", serde_json::json!({"model": model}))
            .await?;
        Ok(())
    }

    /// Ask the CLI to revert file edits made since the given user message.
    /// Required field shape matches Python SDK `types.py:1497` —
    /// `{"subtype": "rewind_files", "user_message_id": "..."}`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn rewind_files(&mut self, user_message_id: &str) -> Result<(), Error> {
        self.send_control(
            "rewind_files",
            serde_json::json!({"user_message_id": user_message_id}),
        )
        .await?;
        Ok(())
    }

    /// Reconnect a named MCP server (asks the CLI to drop + re-establish
    /// its connection to the named server). Wire shape uses camelCase
    /// `serverName` per Python SDK `types.py:1505`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_reconnect(&mut self, server_name: &str) -> Result<(), Error> {
        self.send_control(
            "mcp_reconnect",
            serde_json::json!({"serverName": server_name}),
        )
        .await?;
        Ok(())
    }

    /// Toggle a named MCP server on/off. Wire shape uses camelCase
    /// `serverName` per Python SDK `types.py:1513`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_toggle(&mut self, server_name: &str, enabled: bool) -> Result<(), Error> {
        self.send_control(
            "mcp_toggle",
            serde_json::json!({"serverName": server_name, "enabled": enabled}),
        )
        .await?;
        Ok(())
    }

    /// Kill an in-flight sub-agent task by its `task_id` (from the
    /// `TaskStarted` system message). Matches Python SDK
    /// `types.py:1519` — `{"subtype": "stop_task", "task_id": "..."}`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn stop_task(&mut self, task_id: &str) -> Result<(), Error> {
        self.send_control("stop_task", serde_json::json!({"task_id": task_id}))
            .await?;
        Ok(())
    }

    /// Query MCP server status. Returns the typed response.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases, plus [`Error::MessageParse`]
    /// when the CLI payload doesn't match
    /// [`McpStatusResponse`](crate::public_types::McpStatusResponse).
    pub async fn mcp_status(&mut self) -> Result<crate::public_types::McpStatusResponse, Error> {
        let raw = self
            .send_control("mcp_status", serde_json::json!({}))
            .await?;
        serde_json::from_value(raw).map_err(|e| Error::message_parse(format!("mcp_status: {e}")))
    }

    /// Query MCP server status, returning the raw JSON payload. Use this
    /// escape hatch when the CLI returns fields not yet modelled by
    /// [`McpStatusResponse`](crate::public_types::McpStatusResponse).
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_status_raw(&mut self) -> Result<serde_json::Value, Error> {
        self.send_control("mcp_status", serde_json::json!({})).await
    }

    /// Query current context usage (tokens consumed vs. budget). Returns
    /// the typed response.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases, plus [`Error::MessageParse`]
    /// when the CLI payload doesn't match
    /// [`ContextUsageResponse`](crate::public_types::ContextUsageResponse).
    pub async fn get_context_usage(
        &mut self,
    ) -> Result<crate::public_types::ContextUsageResponse, Error> {
        let raw = self
            .send_control("get_context_usage", serde_json::json!({}))
            .await?;
        serde_json::from_value(raw)
            .map_err(|e| Error::message_parse(format!("get_context_usage: {e}")))
    }

    /// Query current context usage, returning the raw JSON payload. Use
    /// this when the CLI returns fields not yet modelled by
    /// [`ContextUsageResponse`](crate::public_types::ContextUsageResponse).
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn get_context_usage_raw(&mut self) -> Result<serde_json::Value, Error> {
        self.send_control("get_context_usage", serde_json::json!({}))
            .await
    }
}

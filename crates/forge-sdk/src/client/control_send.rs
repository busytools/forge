//! Outbound `control_request` typed wrappers — the user-facing
//! commands (`interrupt`, `set_permission_mode`, `set_model`,
//! `rewind_files`, `mcp_reconnect`, `mcp_toggle`, `stop_task`,
//! `mcp_status`, `get_context_usage`) plus the `_raw` escape hatches
//! for `mcp_status` and `get_context_usage`.
//!
//! All methods take `&self` and route through
//! [`Client::send_control`] (which lives in `client.rs`); the reader
//! task routes the matching `control_response` back via the
//! `pending_controls` map. Concurrent calls are safe — the writer
//! mpsc serialises onto stdin in arrival order, and each
//! `request_id` gets its own oneshot waiter.
//!
//! Session forking lives elsewhere: the spawn-time
//! [`Options::fork_session`](crate::Options) flag (surfaced via
//! `--fork-session`) and the offline
//! [`fork_session`](crate::session::mutations::fork_session) free
//! function.

use crate::Error;
use crate::client::Client;

impl Client {
    /// Ask the CLI to interrupt the current turn (cancel in-flight tool
    /// calls and return control to the SDK).
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn interrupt(&self) -> Result<(), Error> {
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
        &self,
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
    /// default. Wire shape: `{"subtype": "set_model", "model": <string or null>}`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn set_model(&self, model: Option<&str>) -> Result<(), Error> {
        self.send_control("set_model", serde_json::json!({"model": model}))
            .await?;
        Ok(())
    }

    /// Ask the CLI to revert file edits made since the given user message.
    /// Required field shape: `{"subtype": "rewind_files", "user_message_id": "..."}`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn rewind_files(&self, user_message_id: &str) -> Result<(), Error> {
        self.send_control(
            "rewind_files",
            serde_json::json!({"user_message_id": user_message_id}),
        )
        .await?;
        Ok(())
    }

    /// Reconnect a named MCP server (asks the CLI to drop + re-establish
    /// its connection to the named server). Wire shape uses camelCase
    /// `serverName`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_reconnect(&self, server_name: &str) -> Result<(), Error> {
        self.send_control(
            "mcp_reconnect",
            serde_json::json!({"serverName": server_name}),
        )
        .await?;
        Ok(())
    }

    /// Toggle a named MCP server on/off. Wire shape uses camelCase
    /// `serverName`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_toggle(&self, server_name: &str, enabled: bool) -> Result<(), Error> {
        self.send_control(
            "mcp_toggle",
            serde_json::json!({"serverName": server_name, "enabled": enabled}),
        )
        .await?;
        Ok(())
    }

    /// Kill an in-flight sub-agent task by its `task_id` (from the
    /// `TaskStarted` system message).
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn stop_task(&self, task_id: &str) -> Result<(), Error> {
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
    pub async fn mcp_status(&self) -> Result<crate::public_types::McpStatusResponse, Error> {
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
    pub async fn mcp_status_raw(&self) -> Result<serde_json::Value, Error> {
        self.send_control("mcp_status", serde_json::json!({})).await
    }

    /// Query current context usage (tokens consumed vs. budget).
    /// Returns the typed response.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases, plus [`Error::MessageParse`]
    /// when the CLI payload doesn't match
    /// [`ContextUsageResponse`](crate::public_types::ContextUsageResponse).
    pub async fn get_context_usage(
        &self,
    ) -> Result<crate::public_types::ContextUsageResponse, Error> {
        let raw = self
            .send_control("get_context_usage", serde_json::json!({}))
            .await?;
        serde_json::from_value(raw)
            .map_err(|e| Error::message_parse(format!("get_context_usage: {e}")))
    }

    /// Query current context usage, returning the raw JSON payload.
    /// Use this when the CLI returns fields not yet modelled by
    /// [`ContextUsageResponse`](crate::public_types::ContextUsageResponse).
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn get_context_usage_raw(&self) -> Result<serde_json::Value, Error> {
        self.send_control("get_context_usage", serde_json::json!({}))
            .await
    }

    /// Ask the CLI to generate a short session title from `description`.
    /// Wire shape: `{"subtype": "generate_session_title",
    /// "description": <string>, "persist": true}`. The CLI responds with
    /// the generated string.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases, plus [`Error::MessageParse`]
    /// when the CLI response isn't a string title.
    pub async fn generate_session_title(&self, description: &str) -> Result<String, Error> {
        let raw = self
            .send_control(
                "generate_session_title",
                serde_json::json!({"description": description, "persist": true}),
            )
            .await?;
        match raw {
            serde_json::Value::String(s) => Ok(s),
            serde_json::Value::Object(map) => map
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .ok_or_else(|| {
                    Error::message_parse("generate_session_title: response missing 'title'")
                }),
            other => Err(Error::message_parse(format!(
                "generate_session_title: unexpected response shape {other}"
            ))),
        }
    }

    /// Ask the CLI to reload session plugins (slash commands, agents,
    /// MCP servers). Wire shape: `{"subtype": "reload_plugins"}`. The
    /// CLI replies with the refreshed plugin inventory; returns the
    /// raw JSON for forge-daemon to forward.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn reload_plugins(&self) -> Result<serde_json::Value, Error> {
        self.send_control("reload_plugins", serde_json::json!({}))
            .await
    }

    /// Replace the active MCP server set. Wire shape:
    /// `{"subtype": "mcp_set_servers", "servers": <map of name → config>}`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_set_servers(&self, servers: serde_json::Value) -> Result<(), Error> {
        self.send_control("mcp_set_servers", serde_json::json!({"servers": servers}))
            .await?;
        Ok(())
    }

    /// Begin OAuth authentication for an MCP server. Wire shape:
    /// `{"subtype": "mcp_authenticate", "server_name": <string>}`. The
    /// CLI may return an authorization URL or an immediate completion
    /// status; returns the raw JSON.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_authenticate(
        &self,
        server_name: &str,
    ) -> Result<serde_json::Value, Error> {
        self.send_control(
            "mcp_authenticate",
            serde_json::json!({"server_name": server_name}),
        )
        .await
    }

    /// Clear stored OAuth credentials for an MCP server. Wire shape:
    /// `{"subtype": "mcp_clear_auth", "server_name": <string>}`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_clear_auth(&self, server_name: &str) -> Result<(), Error> {
        self.send_control(
            "mcp_clear_auth",
            serde_json::json!({"server_name": server_name}),
        )
        .await?;
        Ok(())
    }

    /// Forward an OAuth callback URL to the CLI to complete an MCP
    /// authentication flow. Wire shape:
    /// `{"subtype": "mcp_oauth_callback_url", "server_name": <string>,
    /// "callback_url": <string>}`.
    ///
    /// # Errors
    ///
    /// See the outbound control error cases.
    pub async fn mcp_oauth_callback_url(
        &self,
        server_name: &str,
        callback_url: &str,
    ) -> Result<(), Error> {
        self.send_control(
            "mcp_oauth_callback_url",
            serde_json::json!({
                "server_name": server_name,
                "callback_url": callback_url,
            }),
        )
        .await?;
        Ok(())
    }
}

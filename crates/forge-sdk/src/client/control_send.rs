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
}

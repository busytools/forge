use forge_primitives as types;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLaunchSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_progress_summaries: Option<bool>,
    /// Charter appended to the spawned `claude` subprocess's system
    /// prompt via `--append-system-prompt`. Set for worker spawns (the
    /// LLM-supplied inline persona, via `handle_spawn_worker`) and for
    /// lead spawns (the lead charter, via `apply_lead_charter`); `None`
    /// only when the caller supplies none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charter: Option<String>,
    /// Lead-session delegation preamble: when `Some`, appended to the
    /// system prompt after the forge MCP trust block so a Lead session
    /// knows how to spawn and drive workers. Built by the workspace for
    /// Lead spawns only; `None` for workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_preamble: Option<String>,
    /// Spawn-path-specific extra CLI args to thread through to the
    /// `claude` subprocess via `OptionsBuilder::extra_arg`. Each pair
    /// becomes `--<flag> <value>` (or `--<flag>` when the value is
    /// `None`). Workers in git-repo projects populate
    /// `("worktree", Some(label))` so claude forks a worktree at
    /// `<repo>/.claude/worktrees/<label>/`; lead/resume paths leave
    /// this empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<(String, Option<String>)>,
    /// Boot-wave fresh-start routing flag (`--new`). When `true`, the
    /// project-target spawn arms skip resuming the catalog lead session
    /// and start a fresh one, and the lead cascades it to its workers.
    /// Pure workspace-side routing - `#[serde(skip)]` keeps it out of
    /// every serialized form, so it never reaches the `settings` JSON
    /// sent to `claude`. Defaults `false`; only the boot dispatch sets it.
    #[serde(skip)]
    pub force_new: bool,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Connected {
        session_id: String,
        cwd: String,
        current_model: types::CurrentModel,
        available_models: Vec<types::AvailableModel>,
        mode: Option<types::ModeState>,
        history_updates: Option<Vec<types::Message>>,
        /// Compactions the resumed transcript records, seeding the
        /// per-session count. `0` for a fresh session.
        compaction_count: u32,
    },
    AuthRequired {
        method_name: String,
        method_description: String,
    },
    ConnectionFailed {
        message: String,
    },
    PermissionRequest {
        session_id: String,
        request: types::PermissionRequest,
    },
    QuestionRequest {
        session_id: String,
        request: types::QuestionRequest,
    },
    McpOperationError {
        session_id: String,
        error: types::McpOperationError,
    },
    RuntimeReloadCompleted {
        session_id: String,
    },
    RuntimeReloadFailed {
        session_id: String,
        message: String,
    },
    SessionsListed {
        sessions: Vec<types::SessionListEntry>,
    },
    StatusSnapshot {
        session_id: String,
        account: forge_primitives::AccountInfo,
        forge_account: Option<forge_primitives::ForgeAccountIdentity>,
    },
    OauthCredentialsSnapshot {
        session_id: String,
        credentials: Option<crate::cloud::oauth_credentials::OauthCredentials>,
    },
    ContextUsage {
        session_id: String,
        percentage: Option<u8>,
        /// Raw model context-window size in tokens (e.g. 200_000 for
        /// Sonnet's default cap, 1_000_000 for the 1M-context variant).
        /// `None` when the upstream probe hasn't reported it yet.
        /// Sourced from `ContextUsageResponse.raw_max_tokens`.
        max_tokens: Option<u64>,
    },
    McpSnapshot {
        session_id: String,
        servers: Vec<forge_primitives::McpServerStatus>,
        error: Option<String>,
    },
    /// Raw `forge_primitives::Message` envelope from the underlying
    /// SDK Client, forwarded to the consumer (e.g. forge-tui's App)
    /// for per-variant dispatch and state mutation.
    SdkMessage {
        session_id: String,
        msg: forge_primitives::Message,
    },
    /// Observation of CLI runtime state captured from a hook input
    /// payload as it passes through the SDK's hook-callback dispatch.
    /// Hooks fire on every tool use, prompt submit, etc., so this is
    /// a high-fidelity signal compared to the lower-frequency
    /// `system/status` events. Fields are `Option` because the CLI
    /// only populates the relevant subset on each hook event (e.g.
    /// `agent_id` / `agent_type` are absent for main-agent tool calls).
    HookObservation {
        /// Session id the hook fired in.
        session_id: String,
        /// `tool_use_id` when the hook event is tool-lifecycle scoped.
        /// `None` for events that aren't bound to a specific tool call
        /// (`UserPromptSubmit`, `Stop`, etc.).
        tool_use_id: Option<String>,
        /// Permission mode active at the moment the hook fired. Wire
        /// value as a string (e.g. `"acceptEdits"`, `"plan"`); the
        /// consumer types it with `forge_primitives::PermissionMode`.
        permission_mode: Option<String>,
        /// Effort level active at the moment the hook fired (CLI
        /// 2.1.133+, absent for older CLIs). Wire value as a string
        /// (`"low"` / `"medium"` / `"high"` / `"xhigh"` / `"max"`);
        /// the consumer maps to `forge_primitives::EffortLevel`.
        effort: Option<String>,
        /// Sub-agent identifier when the hook fired inside a
        /// `Task`-spawned worker. Matches the `agent_id` from the
        /// subagent's `SubagentStart` / `SubagentStop` hooks.
        agent_id: Option<String>,
        /// Sub-agent type name (e.g. `"general-purpose"`,
        /// `"code-reviewer"`).
        agent_type: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::SessionLaunchSettings;

    #[test]
    fn session_launch_settings_serializes_agent_progress_summaries() {
        let settings = SessionLaunchSettings {
            settings: Some(serde_json::json!({ "model": "haiku" })),
            agent_progress_summaries: Some(true),
            ..SessionLaunchSettings::default()
        };

        let json = serde_json::to_value(&settings).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "settings": { "model": "haiku" },
                "agent_progress_summaries": true
            })
        );
    }
}

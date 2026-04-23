//! Configuration for spawning a `Client`.
//!
//! Mirrors Python SDK's `ClaudeAgentOptions`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::hooks::Hooks;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agents::{AgentDefinition, EffortLevel};
use crate::mcp::McpServer;
use crate::permissions::CanUseToolCallback;

/// Which permission flow the `claude` binary should use for tool invocations.
///
/// Mirrors Python SDK's `permission_mode` values (all six).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PermissionMode {
    /// Prompt on every tool use (the default).
    #[serde(rename = "default")]
    Default,
    /// Auto-allow edits / writes; prompt on destructive ops.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// Read-only mode; block tools that would mutate the workspace.
    #[serde(rename = "plan")]
    Plan,
    /// Auto-allow all tools (use with care).
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    /// Let the binary decide based on tool + context heuristics (Python v0.1.57+).
    #[serde(rename = "auto")]
    Auto,
    /// Never prompt; silently deny anything that would require approval.
    #[serde(rename = "dontAsk")]
    DontAsk,
}

impl PermissionMode {
    /// The string the `claude` binary expects via `--permission-mode`.
    #[must_use]
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
            Self::Auto => "auto",
            Self::DontAsk => "dontAsk",
        }
    }
}

/// Configuration for one `Client` invocation.
///
/// Construct via [`OptionsBuilder`] rather than populating directly; the
/// struct is `#[non_exhaustive]` so new fields (permission callback, MCP
/// servers, hooks) can be added in later milestones without breaking
/// callers.
#[derive(Clone)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)] // mirrors Python's `ClaudeAgentOptions` verbatim
pub struct Options {
    /// Path or name of the `claude` binary to spawn.
    pub binary: String,
    /// Working directory for the subprocess. None → inherit current cwd.
    pub cwd: Option<PathBuf>,
    /// Session id to resume. Passed as `--resume <id>`. None → new session.
    pub resume: Option<String>,
    /// Model name. Passed as `--model <name>`. None → binary default.
    pub model: Option<String>,
    /// Permission flow.
    pub permission_mode: PermissionMode,
    /// Optional permission callback. When set, the `claude` binary asks
    /// this callback before invoking any tool.
    pub can_use_tool: Option<Arc<dyn CanUseToolCallback>>,
    /// In-process MCP servers. Each entry maps a server name (used in the
    /// `mcp__<server>__<tool>` prefix the model sees) to a built
    /// [`McpServer`].
    pub mcp_servers: Vec<(String, McpServer)>,
    /// External MCP servers — stdio / SSE / HTTP. Mirrors the non-SDK
    /// variants of Python's `ClaudeAgentOptions.mcp_servers`
    /// (`types.py:549-572`). Registered alongside in-process servers in
    /// the `--mcp-config` JSON.
    pub external_mcp_servers:
        std::collections::HashMap<String, crate::public_types::McpServerConfig>,
    /// Registered hooks. Empty by default.
    pub hooks: Hooks,
    /// Tool names the model is allowed to invoke. Passed to the CLI as
    /// `--allowedTools <comma,list>`. Empty means "no explicit allowlist".
    pub allowed_tools: Vec<String>,
    /// Skills to enable. Python SDK supports `"all"` plus concrete names.
    /// Three-channel delivery:
    /// 1. Injected into `--allowedTools` as `Skill` (for `"all"`) or
    ///    `Skill(<name>)`.
    /// 2. If `setting_sources` is unset, defaulted to `user,project` and
    ///    emitted as `--setting-sources=user,project`.
    /// 3. Concrete (non-`"all"`) skills also populate the `skills` field
    ///    in the `initialize` `control_request` (deferred until C2.9 lands).
    pub skills: Vec<String>,
    /// CLI `--setting-sources` value. When `None`, the default derives
    /// from whether `skills` is set.
    pub setting_sources: Option<Vec<String>>,
    /// Whether to exclude dynamic sections from the system prompt. Wire
    /// shape: `excludeDynamicSections` field in the `initialize`
    /// `control_request` (NOT a CLI flag — Python SDK delivers this via
    /// the control channel).
    pub exclude_dynamic_sections: Option<bool>,
    /// Orthogonal permission-prompt tool. When set, passed as
    /// `--permission-prompt-tool <name>`. Mirrors Python
    /// `ClaudeAgentOptions.permission_prompt_tool_name`.
    pub permission_prompt_tool_name: Option<String>,
    /// Minimum `claude` binary version required. Default `>= 2.0.0`
    /// (matches Python SDK v0.1.64 pin at `subprocess_cli.py:29`). When
    /// `Some`, `Client::spawn` runs `<binary> --version` once and checks
    /// the reported major version is at least the first component.
    pub minimum_cli_version: Option<String>,
    /// Override the directory used by `session::scan::*` to resolve
    /// project keys. When `None`, forge-sdk defaults to
    /// `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`. Matches
    /// Python SDK's `_internal/sessions.py` `_get_projects_dir()`.
    pub projects_dir: Option<PathBuf>,
    /// Subagent definitions forwarded via the `initialize` `control_request`'s
    /// `agents` field. Key is the subagent name the model picks; value is
    /// the [`AgentDefinition`]. Empty by default — matching Python SDK
    /// v0.1.64 `ClaudeAgentOptions.agents` (`types.py:1355`).
    pub agents: HashMap<String, AgentDefinition>,
    /// System prompt configuration. `None` = inherit CLI default. `Some`
    /// emits `--system-prompt`, `--system-prompt-file`, or
    /// `--append-system-prompt` depending on variant. Ported from Python
    /// SDK `SystemPromptPreset` / `SystemPromptFile` (`types.py:35-78`).
    pub system_prompt: Option<SystemPromptKind>,
    /// Base tool set. `None` = CLI default. `Some(ToolsPreset::Default)`
    /// emits `--tools default`; `Some(List(...))` emits `--tools <csv>`.
    pub tools: Option<ToolsPreset>,
    /// Denylist passed via `--disallowedTools`. Empty = no flag.
    pub disallowed_tools: Vec<String>,
    /// Turn limit. `--max-turns <n>`.
    pub max_turns: Option<u64>,
    /// USD budget. `--max-budget-usd <n>`.
    pub max_budget_usd: Option<f64>,
    /// Backup model when the primary is unavailable. `--fallback-model`.
    pub fallback_model: Option<String>,
    /// Experimental beta flags. `--betas <csv>`.
    pub betas: Vec<String>,
    /// Resume the most recent conversation. `--continue`.
    pub continue_conversation: bool,
    /// Explicit session id for a new session (distinct from `resume`).
    /// `--session-id <id>`.
    pub session_id: Option<String>,
    /// Surface streaming chunks rather than coalesced turns.
    /// `--include-partial-messages`.
    pub include_partial_messages: bool,
    /// Spawn-time fork — duplicate `resume`'s session on the first turn.
    /// `--fork-session` (distinct from the offline JSONL-level
    /// [`fork_session`](crate::session::mutations::fork_session) helper;
    /// Python SDK v0.1.64 has no runtime `fork_session` `control_request`).
    pub fork_session: bool,
    /// Extra directories surfaced to the CLI via repeated `--add-dir`.
    pub add_dirs: Vec<std::path::PathBuf>,
    /// Local plugins. Python SDK's `list[SdkPluginConfig]` (`types.py:771-778`).
    pub plugins: Vec<SdkPluginConfig>,
    /// Environment variables added to the subprocess env.
    pub env: HashMap<String, String>,
    /// Override `$USER` in the subprocess env.
    pub user: Option<String>,
    /// Arbitrary forward flags — `{"flag": Some("v")}` emits `--flag v`,
    /// `{"flag": None}` emits a bare `--flag`. Mirrors Python's
    /// `extra_args: dict[str, str | None]` (`types.py:1417`).
    pub extra_args: HashMap<String, Option<String>>,
    /// Reasoning-effort hint. `--effort <level>` — Python's `effort` is a
    /// literal or integer; forge-sdk reuses [`EffortLevel`].
    pub effort: Option<EffortLevel>,
    /// Extended-thinking configuration. Takes precedence over
    /// `max_thinking_tokens`.
    pub thinking: Option<ThinkingConfig>,
    /// Deprecated. Use `thinking` instead. `--max-thinking-tokens <n>`
    /// when `thinking` is None.
    pub max_thinking_tokens: Option<u64>,
    /// Task budget: total sub-agent token budget per turn. `--task-budget`.
    pub task_budget: Option<u64>,
    /// Structured output schema. Python's `output_format` accepts
    /// `{"type": "json_schema", "schema": {...}}`; forge-sdk accepts the
    /// schema JSON directly for simplicity.
    pub output_format: Option<Value>,
    /// Internal stdout buffer upper bound. `None` = default 1 MiB.
    pub max_buffer_size: Option<usize>,
    /// Stderr line callback. When set, each line from the subprocess
    /// stderr is forwarded to `callback(line)`. Drained in the
    /// background so the pipe never blocks.
    pub stderr: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
    /// Enable file-checkpoint tracking (required for
    /// [`Client::rewind_files`](crate::Client::rewind_files)). Python
    /// delivers this via the `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING`
    /// env var (`_internal/transport/subprocess_cli.py:436-437`), NOT a
    /// CLI flag — forge-sdk matches. Field name mirrors Python
    /// `ClaudeAgentOptions.enable_file_checkpointing`
    /// (`types.py:1408`).
    pub enable_file_checkpointing: bool,
    /// Settings: either a file path or an inline JSON string. When
    /// combined with [`sandbox`](Self::sandbox), forge-sdk parses the JSON
    /// (or reads the file) and merges `{"sandbox": <sandbox>}` in. Mirrors
    /// Python's `ClaudeAgentOptions.settings` (`types.py:1410`) +
    /// `_build_settings_value` in `subprocess_cli.py:111-163`.
    pub settings: Option<String>,
    /// Sandbox configuration — merged into [`settings`](Self::settings)
    /// JSON when emitted via `--settings`. Mirrors Python's
    /// `ClaudeAgentOptions.sandbox` (`types.py:1412`).
    pub sandbox: Option<crate::public_types::SandboxSettings>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            binary: "claude".into(),
            cwd: None,
            resume: None,
            model: None,
            permission_mode: PermissionMode::Default,
            can_use_tool: None,
            mcp_servers: Vec::new(),
            external_mcp_servers: HashMap::new(),
            hooks: Hooks::default(),
            allowed_tools: Vec::new(),
            skills: Vec::new(),
            setting_sources: None,
            exclude_dynamic_sections: None,
            permission_prompt_tool_name: None,
            minimum_cli_version: Some("2.0.0".into()),
            projects_dir: None,
            agents: HashMap::new(),
            system_prompt: None,
            tools: None,
            disallowed_tools: Vec::new(),
            max_turns: None,
            max_budget_usd: None,
            fallback_model: None,
            betas: Vec::new(),
            continue_conversation: false,
            session_id: None,
            include_partial_messages: false,
            fork_session: false,
            add_dirs: Vec::new(),
            plugins: Vec::new(),
            env: HashMap::new(),
            user: None,
            extra_args: HashMap::new(),
            effort: None,
            thinking: None,
            max_thinking_tokens: None,
            task_budget: None,
            output_format: None,
            max_buffer_size: None,
            stderr: None,
            enable_file_checkpointing: false,
            settings: None,
            sandbox: None,
        }
    }
}

impl Options {
    /// Return the inner schema JSON of a `{"type":"json_schema","schema":...}`
    /// `output_format` entry, if present. Mirrors Python's extraction at
    /// `subprocess_cli.py:366-375`.
    pub(crate) fn output_format_json_schema(&self) -> Option<String> {
        let format = self.output_format.as_ref()?;
        if format.get("type")?.as_str()? != "json_schema" {
            return None;
        }
        serde_json::to_string(format.get("schema")?).ok()
    }

    /// Resolve `settings` + `sandbox` into the single string passed via
    /// `--settings`. Mirrors Python's `_build_settings_value`
    /// (`subprocess_cli.py:111-163`), but surfaces sandbox serialisation
    /// failures rather than silently dropping the sandbox config. Parse
    /// failures on the user-supplied settings blob log a `warn` and
    /// continue (Python semantics).
    ///
    /// # Errors
    ///
    /// [`crate::Error::MessageParse`] when the `sandbox` struct fails to
    /// serialise (shouldn't happen for well-formed types, but we refuse
    /// to spawn un-sandboxed when the caller asked for sandboxing).
    pub(crate) fn build_settings_value(&self) -> Result<Option<String>, crate::Error> {
        let has_settings = self.settings.is_some();
        let has_sandbox = self.sandbox.is_some();
        if !has_settings && !has_sandbox {
            return Ok(None);
        }
        // Only a settings path / inline JSON, no sandbox merge needed:
        // pass through verbatim (CLI accepts both forms).
        if has_settings && !has_sandbox {
            return Ok(self.settings.clone());
        }

        // Need to merge. Parse existing settings (if any) into a JSON
        // object, then attach "sandbox".
        let mut settings_obj = serde_json::Map::new();
        if let Some(raw) = self.settings.as_deref() {
            let trimmed = raw.trim();
            if trimmed.starts_with('{') && trimmed.ends_with('}') {
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(parsed) => {
                        if let Some(obj) = parsed.as_object() {
                            settings_obj.clone_from(obj);
                        } else {
                            tracing::warn!(
                                "inline --settings JSON parsed but is not an object; ignoring"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "inline --settings JSON failed to parse; ignoring and merging sandbox only"
                        );
                    }
                }
            } else {
                match std::fs::read(trimmed) {
                    Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
                        Ok(parsed) => {
                            if let Some(obj) = parsed.as_object() {
                                settings_obj.clone_from(obj);
                            } else {
                                tracing::warn!(
                                    path = %trimmed,
                                    "settings file JSON is not an object; ignoring"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                path = %trimmed,
                                error = %e,
                                "settings file JSON failed to parse; ignoring and merging sandbox only"
                            );
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            path = %trimmed,
                            error = %e,
                            "settings file read failed; ignoring and merging sandbox only"
                        );
                    }
                }
            }
        }
        if let Some(sandbox) = &self.sandbox {
            let v = serde_json::to_value(sandbox).map_err(|e| {
                crate::Error::message_parse(format!("could not serialise sandbox config: {e}"))
            })?;
            settings_obj.insert("sandbox".into(), v);
        }
        serde_json::to_string(&settings_obj).map(Some).map_err(|e| {
            crate::Error::message_parse(format!("could not serialise merged settings: {e}"))
        })
    }
}

/// System-prompt configuration. Mirrors Python's discriminated union of
/// `str | SystemPromptPreset | SystemPromptFile` (`types.py:35-78`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPromptKind {
    /// Plain string override — `--system-prompt <text>`.
    Inline(String),
    /// Preset (currently only `claude_code`) with optional append + the
    /// `exclude_dynamic_sections` signal that rides along inside the
    /// `initialize` `control_request` instead of argv. Mirrors Python
    /// `SystemPromptPreset` (`types.py:43-66`).
    Preset {
        /// Optional append text that lands on argv as
        /// `--append-system-prompt <text>`.
        append: Option<String>,
        /// When `Some`, sent in the `initialize` body as
        /// `excludeDynamicSections`. `None` omits the field, matching
        /// Python's `_internal/query.py:204` conditional.
        exclude_dynamic_sections: Option<bool>,
    },
    /// File-backed prompt — `--system-prompt-file <path>`.
    File(std::path::PathBuf),
}

impl SystemPromptKind {
    /// Convenience constructor for the `claude_code` preset with an
    /// append string. Python `{"type": "preset", "preset":
    /// "claude_code", "append": ...}`.
    #[must_use]
    pub fn preset_append(append: impl Into<String>) -> Self {
        Self::Preset {
            append: Some(append.into()),
            exclude_dynamic_sections: None,
        }
    }
}

/// Tool-base selector. Python's `ToolsPreset` is a dict `{"type":"default"}`;
/// forge-sdk normalises to an enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolsPreset {
    /// `claude_code` preset — emits `--tools default`.
    Default,
    /// Explicit list — emits `--tools <csv>`.
    List(Vec<String>),
}

/// Extended-thinking configuration. Mirrors Python's union of
/// `Adaptive`, `Enabled`, `Disabled` (`types.py:1325-1338`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingConfig {
    /// CLI picks per-turn — `--thinking adaptive`.
    Adaptive,
    /// Thinking on with a per-turn token cap —
    /// `--max-thinking-tokens <n>`.
    Enabled {
        /// Per-turn budget.
        budget_tokens: u64,
    },
    /// Thinking off — `--thinking disabled`.
    Disabled,
}

/// Plugin config. Mirrors Python's `SdkPluginConfig`
/// (`{"type": "local", "path": str}`, `types.py:771-778`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SdkPluginConfig {
    /// Local filesystem plugin — emits `--plugin-dir <path>`.
    Local {
        /// Plugin directory path.
        path: std::path::PathBuf,
    },
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("binary", &self.binary)
            .field("cwd", &self.cwd)
            .field("resume", &self.resume)
            .field("model", &self.model)
            .field("permission_mode", &self.permission_mode)
            .field(
                "can_use_tool",
                &self.can_use_tool.as_ref().map(|_| "<callback>"),
            )
            .field(
                "mcp_servers",
                &format!("<{} servers>", self.mcp_servers.len()),
            )
            .field(
                "external_mcp_servers",
                &format!("<{} external>", self.external_mcp_servers.len()),
            )
            .field("hooks", &self.hooks)
            .field("allowed_tools", &self.allowed_tools)
            .field("skills", &self.skills)
            .field("setting_sources", &self.setting_sources)
            .field("exclude_dynamic_sections", &self.exclude_dynamic_sections)
            .field(
                "permission_prompt_tool_name",
                &self.permission_prompt_tool_name,
            )
            .field("minimum_cli_version", &self.minimum_cli_version)
            .field("projects_dir", &self.projects_dir)
            .field("agents", &format!("<{} agents>", self.agents.len()))
            .field("system_prompt", &self.system_prompt)
            .field("tools", &self.tools)
            .field("disallowed_tools", &self.disallowed_tools)
            .field("max_turns", &self.max_turns)
            .field("max_budget_usd", &self.max_budget_usd)
            .field("fallback_model", &self.fallback_model)
            .field("betas", &self.betas)
            .field("continue_conversation", &self.continue_conversation)
            .field("session_id", &self.session_id)
            .field("include_partial_messages", &self.include_partial_messages)
            .field("fork_session", &self.fork_session)
            .field("add_dirs", &self.add_dirs)
            .field("plugins", &self.plugins)
            .field("env", &format!("<{} vars>", self.env.len()))
            .field("user", &self.user)
            .field("extra_args", &format!("<{} flags>", self.extra_args.len()))
            .field("effort", &self.effort)
            .field("thinking", &self.thinking)
            .field("max_thinking_tokens", &self.max_thinking_tokens)
            .field("task_budget", &self.task_budget)
            .field("output_format", &self.output_format)
            .field("max_buffer_size", &self.max_buffer_size)
            .field("stderr", &self.stderr.as_ref().map(|_| "<callback>"))
            .field("enable_file_checkpointing", &self.enable_file_checkpointing)
            .field("settings", &self.settings)
            .field("sandbox", &self.sandbox)
            .finish()
    }
}

/// Builder for [`Options`].
#[derive(Clone, Default)]
pub struct OptionsBuilder {
    inner: Options,
}

impl std::fmt::Debug for OptionsBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OptionsBuilder")
            .field("inner", &self.inner)
            .finish()
    }
}

impl OptionsBuilder {
    /// Start from defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the `claude` binary path or name.
    #[must_use]
    pub fn binary(mut self, binary: impl Into<String>) -> Self {
        self.inner.binary = binary.into();
        self
    }

    /// Alias for [`binary`](Self::binary) — matches Python SDK's
    /// `ClaudeAgentOptions.cli_path` field name so snippets porting
    /// directly from Python compile unchanged. Accepts any path-like
    /// value; forge-sdk stores it as the string the binary is
    /// launched with.
    #[must_use]
    pub fn cli_path(self, cli_path: impl AsRef<std::path::Path>) -> Self {
        self.binary(cli_path.as_ref().to_string_lossy().into_owned())
    }

    /// Set the working directory for the subprocess.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.inner.cwd = Some(cwd.into());
        self
    }

    /// Resume an existing session.
    #[must_use]
    pub fn resume(mut self, session_id: impl Into<String>) -> Self {
        self.inner.resume = Some(session_id.into());
        self
    }

    /// Override the model.
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.inner.model = Some(model.into());
        self
    }

    /// Set the permission mode.
    #[must_use]
    pub fn permission_mode(mut self, mode: PermissionMode) -> Self {
        self.inner.permission_mode = mode;
        self
    }

    /// Register a permission callback. Any type implementing
    /// [`CanUseToolCallback`] works — including plain async functions
    /// via the blanket impl.
    #[must_use]
    pub fn can_use_tool<C>(mut self, callback: C) -> Self
    where
        C: CanUseToolCallback + 'static,
    {
        self.inner.can_use_tool = Some(Arc::new(callback));
        self
    }

    /// Register an in-process MCP server under the given name. The model
    /// sees tools as `mcp__<name>__<tool>`.
    #[must_use]
    pub fn mcp_server(mut self, name: impl Into<String>, server: McpServer) -> Self {
        self.inner.mcp_servers.push((name.into(), server));
        self
    }

    /// Register an external (stdio / SSE / HTTP) MCP server under the
    /// given name. Non-SDK variants of Python's `mcp_servers` dict.
    #[must_use]
    pub fn external_mcp_server(
        mut self,
        name: impl Into<String>,
        config: crate::public_types::McpServerConfig,
    ) -> Self {
        self.inner.external_mcp_servers.insert(name.into(), config);
        self
    }

    /// Attach hooks.
    #[must_use]
    pub fn hooks(mut self, hooks: Hooks) -> Self {
        self.inner.hooks = hooks;
        self
    }

    /// Set the `--allowedTools` list explicitly.
    #[must_use]
    pub fn allowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.inner.allowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    /// Enable skills. Use `"all"` to enable all skills, or list names.
    /// Python SDK defaults `setting_sources` to `["user", "project"]` when
    /// this is set and `setting_sources` is not explicitly provided.
    #[must_use]
    pub fn skills<I, S>(mut self, skills: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.inner.skills = skills.into_iter().map(Into::into).collect();
        self
    }

    /// Override the `--setting-sources` list.
    #[must_use]
    pub fn setting_sources<I, S>(mut self, sources: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.inner.setting_sources = Some(sources.into_iter().map(Into::into).collect());
        self
    }

    /// Exclude dynamic sections from the system prompt. Delivered via
    /// the `initialize` `control_request`, not a CLI flag.
    #[must_use]
    pub fn exclude_dynamic_sections(mut self, yes: bool) -> Self {
        self.inner.exclude_dynamic_sections = Some(yes);
        self
    }

    /// Set an orthogonal permission-prompt tool name (CLI flag
    /// `--permission-prompt-tool`). Alternative to `can_use_tool` — the
    /// CLI invokes the named tool (typically via MCP) instead of routing
    /// permission requests to the SDK callback.
    #[must_use]
    pub fn permission_prompt_tool_name(mut self, name: impl Into<String>) -> Self {
        self.inner.permission_prompt_tool_name = Some(name.into());
        self
    }

    /// Override the minimum `claude` binary version check. Pass `None` to
    /// disable the check entirely.
    #[must_use]
    pub fn minimum_cli_version(mut self, version: Option<String>) -> Self {
        self.inner.minimum_cli_version = version;
        self
    }

    /// Override the projects directory used by `session::scan::*` helpers
    /// to resolve project keys. When unset, defaults to
    /// `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`.
    #[must_use]
    pub fn projects_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.inner.projects_dir = Some(path.into());
        self
    }

    /// Register a subagent under `name`. Forwards to the CLI via the
    /// `initialize` `control_request`'s `agents` field. Mirrors Python
    /// SDK's `ClaudeAgentOptions.agents` dict (`types.py:1355`).
    #[must_use]
    pub fn agent(mut self, name: impl Into<String>, def: AgentDefinition) -> Self {
        self.inner.agents.insert(name.into(), def);
        self
    }

    /// Replace the whole subagent map in one go.
    #[must_use]
    pub fn agents(mut self, agents: HashMap<String, AgentDefinition>) -> Self {
        self.inner.agents = agents;
        self
    }

    /// Set the system prompt.
    #[must_use]
    pub fn system_prompt(mut self, sp: SystemPromptKind) -> Self {
        self.inner.system_prompt = Some(sp);
        self
    }

    /// Set the base tool preset / list.
    #[must_use]
    pub fn tools(mut self, tools: ToolsPreset) -> Self {
        self.inner.tools = Some(tools);
        self
    }

    /// Override the disallowed-tools list.
    #[must_use]
    pub fn disallowed_tools(mut self, tools: Vec<String>) -> Self {
        self.inner.disallowed_tools = tools;
        self
    }

    /// Cap the turn count.
    #[must_use]
    pub fn max_turns(mut self, n: u64) -> Self {
        self.inner.max_turns = Some(n);
        self
    }

    /// Cap total USD spend.
    #[must_use]
    pub fn max_budget_usd(mut self, usd: f64) -> Self {
        self.inner.max_budget_usd = Some(usd);
        self
    }

    /// Specify the fallback model.
    #[must_use]
    pub fn fallback_model(mut self, m: impl Into<String>) -> Self {
        self.inner.fallback_model = Some(m.into());
        self
    }

    /// Set experimental beta flags.
    #[must_use]
    pub fn betas(mut self, betas: Vec<String>) -> Self {
        self.inner.betas = betas;
        self
    }

    /// Resume the most recent conversation (`--continue`).
    #[must_use]
    pub fn continue_conversation(mut self, yes: bool) -> Self {
        self.inner.continue_conversation = yes;
        self
    }

    /// Set an explicit session id for a new session (distinct from `resume`).
    #[must_use]
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.inner.session_id = Some(id.into());
        self
    }

    /// Toggle `--include-partial-messages`.
    #[must_use]
    pub fn include_partial_messages(mut self, yes: bool) -> Self {
        self.inner.include_partial_messages = yes;
        self
    }

    /// Toggle `--fork-session` (spawn-time).
    #[must_use]
    pub fn fork_session(mut self, yes: bool) -> Self {
        self.inner.fork_session = yes;
        self
    }

    /// Append a directory to `--add-dir` list.
    #[must_use]
    pub fn add_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.inner.add_dirs.push(dir.into());
        self
    }

    /// Replace the whole `--add-dir` list.
    #[must_use]
    pub fn add_dirs(mut self, dirs: Vec<std::path::PathBuf>) -> Self {
        self.inner.add_dirs = dirs;
        self
    }

    /// Register a local plugin directory.
    #[must_use]
    pub fn plugin_dir(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.inner
            .plugins
            .push(SdkPluginConfig::Local { path: path.into() });
        self
    }

    /// Replace the whole plugin list.
    #[must_use]
    pub fn plugins(mut self, plugins: Vec<SdkPluginConfig>) -> Self {
        self.inner.plugins = plugins;
        self
    }

    /// Add one env var to the subprocess environment.
    #[must_use]
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.inner.env.insert(k.into(), v.into());
        self
    }

    /// Replace the whole env map.
    #[must_use]
    pub fn envs(mut self, env: HashMap<String, String>) -> Self {
        self.inner.env = env;
        self
    }

    /// Override `$USER` in the subprocess env.
    #[must_use]
    pub fn user(mut self, u: impl Into<String>) -> Self {
        self.inner.user = Some(u.into());
        self
    }

    /// Add one extra argv flag (pass `None` for bare flags).
    #[must_use]
    pub fn extra_arg(mut self, flag: impl Into<String>, value: Option<String>) -> Self {
        self.inner.extra_args.insert(flag.into(), value);
        self
    }

    /// Replace the whole extra-args map.
    #[must_use]
    pub fn extra_args(mut self, args: HashMap<String, Option<String>>) -> Self {
        self.inner.extra_args = args;
        self
    }

    /// Set the reasoning-effort hint.
    #[must_use]
    pub fn effort(mut self, e: EffortLevel) -> Self {
        self.inner.effort = Some(e);
        self
    }

    /// Configure extended thinking.
    #[must_use]
    pub fn thinking(mut self, t: ThinkingConfig) -> Self {
        self.inner.thinking = Some(t);
        self
    }

    /// Deprecated — prefer `thinking(ThinkingConfig::Enabled{..})`.
    #[must_use]
    pub fn max_thinking_tokens(mut self, n: u64) -> Self {
        self.inner.max_thinking_tokens = Some(n);
        self
    }

    /// Cap total sub-agent token budget per turn.
    #[must_use]
    pub fn task_budget(mut self, n: u64) -> Self {
        self.inner.task_budget = Some(n);
        self
    }

    /// Attach a structured-output schema. Python form:
    /// `{"type":"json_schema","schema":{...}}`.
    #[must_use]
    pub fn output_format(mut self, value: Value) -> Self {
        self.inner.output_format = Some(value);
        self
    }

    /// Cap the stdout buffer size (bytes). `None` = default 1 MiB.
    #[must_use]
    pub fn max_buffer_size(mut self, n: usize) -> Self {
        self.inner.max_buffer_size = Some(n);
        self
    }

    /// Attach a stderr line callback.
    #[must_use]
    pub fn stderr(mut self, cb: impl Fn(String) + Send + Sync + 'static) -> Self {
        self.inner.stderr = Some(Arc::new(cb));
        self
    }

    /// Enable file-checkpoint tracking. Delivered via the
    /// `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING` env var (matches
    /// Python).
    #[must_use]
    pub fn enable_file_checkpointing(mut self, yes: bool) -> Self {
        self.inner.enable_file_checkpointing = yes;
        self
    }

    /// Set `--settings` value — either a file path or an inline JSON
    /// string. Combined with [`sandbox`](Self::sandbox) if both are set.
    #[must_use]
    pub fn settings(mut self, s: impl Into<String>) -> Self {
        self.inner.settings = Some(s.into());
        self
    }

    /// Attach sandbox settings. Merged into `--settings` JSON if
    /// `settings` is also set.
    #[must_use]
    pub fn sandbox(mut self, sandbox: crate::public_types::SandboxSettings) -> Self {
        self.inner.sandbox = Some(sandbox);
        self
    }

    /// Finalise and return the `Options`.
    #[must_use]
    pub fn build(self) -> Options {
        self.inner
    }
}

//! Configuration for spawning a `Client`.
//!
//! Mirrors Python SDK's `ClaudeAgentOptions`. Fields present for M1/M2 only;
//! hooks, MCP servers, session store, skills arrive in later milestones
//! and are added to this struct at that point.

use std::path::PathBuf;
use std::sync::Arc;

use crate::hooks::Hooks;
use crate::mcp::McpServer;
use crate::permissions::CanUseToolCallback;
use crate::session_store::SessionStore;

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
    pub exclude_dynamic_sections: bool,
    /// Orthogonal permission-prompt tool. When set, passed as
    /// `--permission-prompt-tool <name>`. Mirrors Python
    /// `ClaudeAgentOptions.permission_prompt_tool_name`.
    pub permission_prompt_tool_name: Option<String>,
    /// Transcript-mirror session store. When `Some`, forge-sdk passes
    /// `--session-mirror` to the CLI so it emits `transcript_mirror`
    /// frames to the SDK, which batches them to `store.append(...)` at
    /// ~100ms cadence.
    pub session_store: Option<Arc<dyn SessionStore>>,
    /// Minimum `claude` binary version required. Default `>= 2.0.0`
    /// (matches Python SDK v0.1.64 pin at `subprocess_cli.py:29`). When
    /// `Some`, `Client::spawn` runs `<binary> --version` once and checks
    /// the reported major version is at least the first component.
    pub minimum_cli_version: Option<String>,
    /// Override the directory used to resolve `transcript_mirror.filePath`
    /// into a [`SessionKey`](crate::session_store::SessionKey). When `None`,
    /// forge-sdk defaults to `$CLAUDE_CONFIG_DIR/projects` or
    /// `~/.claude/projects`. Matches Python SDK's `_internal/sessions.py`
    /// `_get_projects_dir()`.
    pub projects_dir: Option<PathBuf>,
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
            hooks: Hooks::default(),
            allowed_tools: Vec::new(),
            skills: Vec::new(),
            setting_sources: None,
            exclude_dynamic_sections: false,
            permission_prompt_tool_name: None,
            session_store: None,
            minimum_cli_version: Some("2.0.0".into()),
            projects_dir: None,
        }
    }
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
            .field("hooks", &self.hooks)
            .field("allowed_tools", &self.allowed_tools)
            .field("skills", &self.skills)
            .field("setting_sources", &self.setting_sources)
            .field("exclude_dynamic_sections", &self.exclude_dynamic_sections)
            .field(
                "permission_prompt_tool_name",
                &self.permission_prompt_tool_name,
            )
            .field(
                "session_store",
                &self.session_store.as_ref().map(|_| "<store>"),
            )
            .field("minimum_cli_version", &self.minimum_cli_version)
            .field("projects_dir", &self.projects_dir)
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
        self.inner.exclude_dynamic_sections = yes;
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

    /// Attach a transcript-mirror session store. When set, the SDK spawns
    /// the CLI with `--session-mirror` and batches `transcript_mirror`
    /// frames to `store.append(...)`.
    #[must_use]
    pub fn session_store<S>(mut self, store: S) -> Self
    where
        S: SessionStore + 'static,
    {
        self.inner.session_store = Some(Arc::new(store));
        self
    }

    /// Attach an already-`Arc`-wrapped transcript-mirror session store —
    /// useful when the caller wants to keep a handle on the store (e.g.
    /// to inspect it after the client returns). Behaviour is otherwise
    /// identical to [`session_store`](Self::session_store).
    #[must_use]
    pub fn session_store_arc(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.inner.session_store = Some(store);
        self
    }

    /// Override the minimum `claude` binary version check. Pass `None` to
    /// disable the check entirely.
    #[must_use]
    pub fn minimum_cli_version(mut self, version: Option<String>) -> Self {
        self.inner.minimum_cli_version = version;
        self
    }

    /// Override the projects directory used to resolve `transcript_mirror`
    /// `filePath` frames into
    /// [`SessionKey`](crate::session_store::SessionKey) values. When unset,
    /// defaults to `$CLAUDE_CONFIG_DIR/projects` or `~/.claude/projects`.
    #[must_use]
    pub fn projects_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.inner.projects_dir = Some(path.into());
        self
    }

    /// Finalise and return the `Options`.
    #[must_use]
    pub fn build(self) -> Options {
        self.inner
    }
}

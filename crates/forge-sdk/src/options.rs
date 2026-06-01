//! Configuration for spawning a `Client`.
//!
//! SDK's `ClaudeAgentOptions`.

use std::path::PathBuf;
use std::sync::Arc;

use crate::hooks::Hooks;
use std::collections::HashMap;

use crate::mcp::McpServer;
use crate::permissions::CanUseToolCallback;
// Pure-data option enums live in forge-primitives.
use forge_primitives::subagents::SubagentEffort;
pub use forge_primitives::{PermissionMode, SdkPluginConfig, SubagentDefinition, SystemPromptKind};

/// Per-line callback used by [`Options::tee_inbound`] and
/// [`Options::tee_outbound`] to capture the wire bytes the SDK
/// exchanges with the `claude` subprocess. The callback receives
/// lines without a trailing newline.
pub type WireTee = Arc<dyn Fn(&str) + Send + Sync>;

/// Predicate used by [`Options::auto_approve_tool`] to short-circuit
/// the permission flow for trusted tool names (e.g. forge's own
/// in-process MCP server prefixes).
pub type AutoApproveToolPredicate = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Configuration for one `Client` invocation.
///
/// Construct via [`OptionsBuilder`] rather than populating directly.
#[derive(Clone)]
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
    /// Optional auto-approve predicate. When the CLI asks via
    /// `can_use_tool` whether a particular tool can run, forge-sdk
    /// consults this predicate first; tools that match are
    /// auto-approved without invoking the `can_use_tool` callback.
    /// Caller (forge-workspace via forge-agent) typically supplies a
    /// closure that auto-approves any tool whose name starts with the
    /// `mcp__<server>__` prefix of one of its in-process MCP servers
    /// (e.g. `mcp__forge__peers__ask_agent`) - those servers are
    /// already opted into by the user's forge.toml so a permission
    /// prompt would just be noise.
    pub auto_approve_tool: Option<AutoApproveToolPredicate>,
    /// In-process MCP servers. Each entry maps a server name (used in the
    /// `mcp__<server>__<tool>` prefix the model sees) to a built
    /// [`McpServer`].
    pub mcp_servers: Vec<(String, McpServer)>,
    /// External MCP servers - stdio / SSE / HTTP. Mirrors the non-SDK
    /// variants of the CLI's `ClaudeAgentOptions.mcp_servers`
    ///. Registered alongside in-process servers in
    /// the `--mcp-config` JSON.
    pub external_mcp_servers: std::collections::HashMap<String, forge_primitives::McpServerConfig>,
    /// Registered hooks. Empty by default.
    pub hooks: Hooks,
    /// Tool names the model is allowed to invoke. Passed to the CLI as
    /// `--allowedTools <comma,list>`. Empty means "no explicit allowlist".
    pub allowed_tools: Vec<String>,
    /// CLI `--setting-sources` value. When `None`, the CLI uses its own
    /// default.
    pub setting_sources: Option<Vec<String>>,
    /// Orthogonal permission-prompt tool. When set, passed as
    /// `--permission-prompt-tool <name>`.
    pub permission_prompt_tool_name: Option<String>,
    /// Minimum `claude` binary version required. Default `>= 2.0.0`.
    /// When `Some`, `Client::spawn` runs `<binary> --version` once
    /// and checks the reported major version is at least the first
    /// component.
    pub minimum_cli_version: Option<String>,
    /// Override the directory used to resolve project keys. When
    /// `None`, forge-sdk defaults to `<config_dir>/projects`.
    pub projects_dir: Option<PathBuf>,
    /// Subagent definitions forwarded via the `initialize`
    /// `control_request`'s `agents` field. Key is the subagent name
    /// the model picks; value is the [`SubagentDefinition`]. Empty by
    /// default.
    pub subagents: HashMap<String, SubagentDefinition>,
    /// System prompt configuration. `None` = inherit CLI default.
    /// `Some` emits `--system-prompt`, `--system-prompt-file`, or
    /// `--append-system-prompt` depending on variant.
    pub system_prompt: Option<SystemPromptKind>,
    /// Turn limit. `--max-turns <n>`.
    pub max_turns: Option<u64>,
    /// Explicit session id for a new session (distinct from `resume`).
    /// `--session-id <id>`.
    pub session_id: Option<String>,
    /// Local plugins. Wire shape: `list[SdkPluginConfig]`.
    pub plugins: Vec<SdkPluginConfig>,
    /// Environment variables added to the subprocess env.
    pub env: HashMap<String, String>,
    /// Override `$USER` in the subprocess env.
    pub user: Option<String>,
    /// Arbitrary forward flags - `{"flag": Some("v")}` emits
    /// `--flag v`, `{"flag": None}` emits a bare `--flag`.
    pub extra_args: HashMap<String, Option<String>>,
    /// Reasoning-effort hint. `--effort <level>` - the CLI's `effort`
    /// is a literal-or-integer carried via [`SubagentEffort`] (named
    /// for its origin on the subagent declaration shape, but the
    /// session-level effort uses the same wire enum).
    pub effort: Option<SubagentEffort>,
    /// Internal stdout buffer upper bound. `None` = default 1 MiB.
    pub max_buffer_size: Option<usize>,
    /// Stderr line callback. When set, each line from the subprocess
    /// stderr is forwarded to `callback(line)`. Drained in the
    /// background so the pipe never blocks.
    pub stderr: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
    /// Inbound wire tee. When set, the SDK invokes
    /// `callback(line)` for every stream-json line read from the
    /// subprocess stdout BEFORE decoding. Used by
    /// `forge-test-harness` to capture the raw wire bytes for
    /// conformance baselines. The callback receives lines without a
    /// trailing newline.
    pub tee_inbound: Option<WireTee>,
    /// Outbound wire tee. When set, the SDK invokes
    /// `callback(line)` for every stream-json line about to be
    /// written to the subprocess stdin. Counterpart to
    /// [`tee_inbound`](Self::tee_inbound). The callback receives
    /// lines without a trailing newline (the SDK strips it before
    /// invoking).
    pub tee_outbound: Option<WireTee>,
    /// Settings: either a file path or an inline JSON string. When
    /// combined with [`sandbox`](Self::sandbox), forge-sdk parses
    /// the JSON (or reads the file) and merges
    /// `{"sandbox": <sandbox>}` in.
    pub settings: Option<String>,
    /// Sandbox configuration - merged into
    /// [`settings`](Self::settings) JSON when emitted via
    /// `--settings`.
    pub sandbox: Option<forge_primitives::SandboxSettings>,
    /// Wire-classification rewriter proxy. When set, the subprocess
    /// is launched with `HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS` env
    /// vars pointing at this proxy so its outbound HTTPS traffic
    /// flows through the rewriter (which normalises the 6 sdk-cli
    /// classification signals to cli shape).
    ///
    /// forge-workspace boots one proxy per process at startup and
    /// stamps the handle onto every Options it constructs. forge-sdk
    /// itself stays proxy-agnostic - leaving this None spawns
    /// without rewriting (useful for unit/integration tests).
    pub proxy: Option<crate::transport::proxy::ProxyHandle>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            binary: "claude".into(),
            cwd: None,
            resume: None,
            model: None,
            permission_mode: PermissionMode::Ask,
            can_use_tool: None,
            auto_approve_tool: None,
            mcp_servers: Vec::new(),
            external_mcp_servers: HashMap::new(),
            hooks: Hooks::default(),
            allowed_tools: Vec::new(),
            setting_sources: None,
            permission_prompt_tool_name: None,
            minimum_cli_version: Some("2.0.0".into()),
            projects_dir: None,
            subagents: HashMap::new(),
            system_prompt: None,
            max_turns: None,
            session_id: None,
            plugins: Vec::new(),
            env: HashMap::new(),
            user: None,
            extra_args: HashMap::new(),
            effort: None,
            max_buffer_size: None,
            stderr: None,
            tee_inbound: None,
            tee_outbound: None,
            settings: None,
            sandbox: None,
            proxy: None,
        }
    }
}

impl Options {
    /// Resolve `settings` + `sandbox` into the single string passed via
    /// `--settings`. Surfaces sandbox serialisation failures rather
    /// than silently dropping the sandbox config. Parse failures on
    /// the user-supplied settings blob log a `warn` and continue.
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

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("binary", &self.binary)
            .field("cwd", &self.cwd)
            .field("resume", &self.resume)
            .field("model", &self.model)
            .field("permission_mode", &self.permission_mode)
            .field("can_use_tool", &self.can_use_tool.as_ref().map(|_| "<callback>"))
            .field("auto_approve_tool", &self.auto_approve_tool.as_ref().map(|_| "<predicate>"))
            .field("mcp_servers", &format!("<{} servers>", self.mcp_servers.len()))
            .field(
                "external_mcp_servers",
                &format!("<{} external>", self.external_mcp_servers.len()),
            )
            .field("hooks", &self.hooks)
            .field("allowed_tools", &self.allowed_tools)
            .field("setting_sources", &self.setting_sources)
            .field("permission_prompt_tool_name", &self.permission_prompt_tool_name)
            .field("minimum_cli_version", &self.minimum_cli_version)
            .field("projects_dir", &self.projects_dir)
            .field("subagents", &format!("<{} subagents>", self.subagents.len()))
            .field("system_prompt", &self.system_prompt)
            .field("max_turns", &self.max_turns)
            .field("session_id", &self.session_id)
            .field("plugins", &self.plugins)
            .field("env", &format!("<{} vars>", self.env.len()))
            .field("user", &self.user)
            .field("extra_args", &format!("<{} flags>", self.extra_args.len()))
            .field("effort", &self.effort)
            .field("max_buffer_size", &self.max_buffer_size)
            .field("stderr", &self.stderr.as_ref().map(|_| "<callback>"))
            .field("tee_inbound", &self.tee_inbound.as_ref().map(|_| "<callback>"))
            .field("tee_outbound", &self.tee_outbound.as_ref().map(|_| "<callback>"))
            .field("settings", &self.settings)
            .field("sandbox", &self.sandbox)
            .field("proxy", &self.proxy.as_ref().map(|p| format!("<rewriter@{}>", p.listen_addr())))
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
        f.debug_struct("OptionsBuilder").field("inner", &self.inner).finish()
    }
}

impl OptionsBuilder {
    /// Start from defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the `claude` binary path or name.
    pub fn binary(mut self, binary: impl Into<String>) -> Self {
        self.inner.binary = binary.into();
        self
    }

    /// Set the working directory for the subprocess.
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.inner.cwd = Some(cwd.into());
        self
    }

    /// Resume an existing session.
    pub fn resume(mut self, session_id: impl Into<String>) -> Self {
        self.inner.resume = Some(session_id.into());
        self
    }

    /// Override the model.
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.inner.model = Some(model.into());
        self
    }

    /// Set the permission mode.
    pub fn permission_mode(mut self, mode: PermissionMode) -> Self {
        self.inner.permission_mode = mode;
        self
    }

    /// Register a permission callback. Any type implementing
    /// [`CanUseToolCallback`] works - including plain async functions
    /// via the blanket impl.
    pub fn can_use_tool<C>(mut self, callback: C) -> Self
    where
        C: CanUseToolCallback + 'static,
    {
        self.inner.can_use_tool = Some(Arc::new(callback));
        self
    }

    /// Auto-approve any tool whose name satisfies the predicate.
    /// forge-sdk consults this before invoking `can_use_tool`; the
    /// permission flow short-circuits with an allow decision for
    /// matches. Caller typically passes a closure that checks
    /// `mcp__<server>__` prefixes for its in-process MCP servers.
    pub fn auto_approve_tool<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.inner.auto_approve_tool = Some(Arc::new(predicate));
        self
    }

    /// Register an in-process MCP server under the given name. The model
    /// sees tools as `mcp__<name>__<tool>`.
    pub fn mcp_server(mut self, name: impl Into<String>, server: McpServer) -> Self {
        self.inner.mcp_servers.push((name.into(), server));
        self
    }

    /// Attach hooks.
    pub fn hooks(mut self, hooks: Hooks) -> Self {
        self.inner.hooks = hooks;
        self
    }

    /// Set the `--allowedTools` list explicitly.
    pub fn allowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.inner.allowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    /// Set an orthogonal permission-prompt tool name (CLI flag
    /// `--permission-prompt-tool`). Alternative to `can_use_tool` - the
    /// CLI invokes the named tool (typically via MCP) instead of routing
    /// permission requests to the SDK callback.
    pub fn permission_prompt_tool_name(mut self, name: impl Into<String>) -> Self {
        self.inner.permission_prompt_tool_name = Some(name.into());
        self
    }

    /// Register a subagent under `name`. Forwards to the CLI via the
    /// `initialize` `control_request`'s `agents` field.
    pub fn subagent(mut self, name: impl Into<String>, def: SubagentDefinition) -> Self {
        self.inner.subagents.insert(name.into(), def);
        self
    }

    /// Set the system prompt.
    pub fn system_prompt(mut self, sp: SystemPromptKind) -> Self {
        self.inner.system_prompt = Some(sp);
        self
    }

    /// Set the worker charter as an appended system prompt. Sugar for
    /// `system_prompt(SystemPromptKind::Preset { append: Some(text), ... })`
    /// used by the workers MCP spawn path where the caller never wants
    /// to replace the CLI's default prompt, only append to it.
    pub fn append_system_prompt(mut self, text: impl Into<String>) -> Self {
        self.inner.system_prompt = Some(SystemPromptKind::Preset {
            append: Some(text.into()),
            exclude_dynamic_sections: None,
        });
        self
    }

    /// Cap the turn count.
    pub fn max_turns(mut self, n: u64) -> Self {
        self.inner.max_turns = Some(n);
        self
    }

    /// Set an explicit session id for a new session (distinct from `resume`).
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.inner.session_id = Some(id.into());
        self
    }

    /// Replace the whole plugin list.
    pub fn plugins(mut self, plugins: Vec<SdkPluginConfig>) -> Self {
        self.inner.plugins = plugins;
        self
    }

    /// Add one env var to the subprocess environment.
    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.inner.env.insert(k.into(), v.into());
        self
    }

    /// Add one extra argv flag (pass `None` for bare flags).
    pub fn extra_arg(mut self, flag: impl Into<String>, value: Option<String>) -> Self {
        self.inner.extra_args.insert(flag.into(), value);
        self
    }

    /// Set the reasoning-effort hint.
    pub fn effort(mut self, e: SubagentEffort) -> Self {
        self.inner.effort = Some(e);
        self
    }

    /// Attach a stderr line callback.
    pub fn stderr(mut self, cb: impl Fn(String) + Send + Sync + 'static) -> Self {
        self.inner.stderr = Some(Arc::new(cb));
        self
    }

    /// Attach an inbound wire-tee callback. Receives every
    /// stream-json line read from the subprocess stdout (without
    /// trailing newline) before the SDK decodes it. Used by
    /// `forge-test-harness` to capture conformance baselines.
    pub fn tee_inbound(mut self, cb: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.inner.tee_inbound = Some(Arc::new(cb));
        self
    }

    /// Attach an outbound wire-tee callback. Receives every
    /// stream-json line about to be written to the subprocess stdin
    /// (without trailing newline). Counterpart to
    /// [`tee_inbound`](Self::tee_inbound).
    pub fn tee_outbound(mut self, cb: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.inner.tee_outbound = Some(Arc::new(cb));
        self
    }

    /// Set `--settings` value - either a file path or an inline JSON
    /// string. Combined with [`sandbox`](Self::sandbox) if both are set.
    pub fn settings(mut self, s: impl Into<String>) -> Self {
        self.inner.settings = Some(s.into());
        self
    }

    /// Attach sandbox settings. Merged into `--settings` JSON if
    /// `settings` is also set.
    pub fn sandbox(mut self, sandbox: forge_primitives::SandboxSettings) -> Self {
        self.inner.sandbox = Some(sandbox);
        self
    }

    /// Attach a wire-classification rewriter proxy. Causes the
    /// spawned subprocess to inherit `HTTPS_PROXY` +
    /// `NODE_EXTRA_CA_CERTS` env vars pointing at the proxy. See
    /// [`crate::transport::proxy`].
    pub fn proxy(mut self, handle: crate::transport::proxy::ProxyHandle) -> Self {
        self.inner.proxy = Some(handle);
        self
    }

    /// Finalise and return the `Options`.
    pub fn build(self) -> Options {
        self.inner
    }
}

#[cfg(test)]
mod tests_skills_option {
    // Test-mod `use super::*;` brings the parent's full surface in; not every test consumes every item.
    #[allow(unused_imports)]
    use super::*;

    use crate::OptionsBuilder;

    #[test]
    fn allowed_tools_round_trip() {
        let opts = OptionsBuilder::new().allowed_tools(["Read", "Grep"]).build();
        assert_eq!(opts.allowed_tools, vec!["Read".to_string(), "Grep".into()]);
    }
}

#[cfg(test)]
mod tests_options_build {
    // Test-mod `use super::*;` brings the parent's full surface in; not every test consumes every item.
    #[allow(unused_imports)]
    use super::*;

    use std::path::PathBuf;

    use crate::{OptionsBuilder, PermissionMode};

    #[test]
    fn default_options() {
        let opts = OptionsBuilder::new().build();
        assert_eq!(opts.binary, "claude");
        assert!(opts.cwd.is_none());
        assert!(opts.resume.is_none());
        assert_eq!(opts.permission_mode, PermissionMode::Ask);
        assert!(opts.model.is_none());
    }

    #[test]
    fn builder_sets_model_and_cwd() {
        let opts = OptionsBuilder::new().model("claude-opus-4-5").cwd("/tmp/project").build();
        assert_eq!(opts.model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(opts.cwd, Some(PathBuf::from("/tmp/project")));
    }

    #[test]
    fn builder_sets_resume_session() {
        let opts = OptionsBuilder::new().resume("sess_abc").build();
        assert_eq!(opts.resume.as_deref(), Some("sess_abc"));
    }

    #[test]
    fn builder_sets_permission_mode() {
        let opts = OptionsBuilder::new().permission_mode(PermissionMode::AcceptEdits).build();
        assert_eq!(opts.permission_mode, PermissionMode::AcceptEdits);
    }

    #[test]
    fn builder_sets_custom_binary() {
        let opts = OptionsBuilder::new().binary("/usr/local/bin/claude").build();
        assert_eq!(opts.binary, "/usr/local/bin/claude");
    }

    #[test]
    fn builder_stores_can_use_tool_callback() {
        use forge_primitives::{PermissionDecision, ToolPermissionContext};

        let opts = OptionsBuilder::new()
            .can_use_tool(|_ctx: ToolPermissionContext| async move { PermissionDecision::allow() })
            .build();
        assert!(opts.can_use_tool.is_some());
    }

    #[test]
    fn auto_approve_tool_predicate_exact_prefix_match() {
        // The control_dispatch fast-path invokes the predicate with
        // the raw tool name. Verify a prefix-based predicate matches
        // exactly the names the caller intends and nothing else.
        let opts = OptionsBuilder::new()
            .auto_approve_tool(|name: &str| name.starts_with("mcp__forge__"))
            .build();
        let pred = opts.auto_approve_tool.expect("predicate stored");
        assert!(pred("mcp__forge__peers__whoami"));
        assert!(pred("mcp__forge__peers__ask_agent"));
        // Workers tools live under the same `mcp__forge__` namespace
        // - auto-approve must cover them with one predicate.
        assert!(pred("mcp__forge__workers__spawn"));
        assert!(pred("mcp__forge__workers__list"));
        assert!(pred("mcp__forge__workers__tell"));
        assert!(pred("mcp__forge__workers__ask"));
        assert!(pred("mcp__forge__"));
        // Sibling prefixes must NOT match (no partial-string fuzz).
        assert!(!pred("mcp__forgery__steal_secrets"));
        assert!(!pred("mcp__forge"));
        assert!(!pred("Bash"));
        assert!(!pred("Read"));
        // Empty string mustn't accidentally match.
        assert!(!pred(""));
    }

    #[test]
    fn auto_approve_tool_default_is_none() {
        let opts = OptionsBuilder::new().build();
        assert!(opts.auto_approve_tool.is_none());
    }

    #[test]
    fn append_system_prompt_builder_sets_preset_append() {
        let opts = OptionsBuilder::new().append_system_prompt("charter text").build();
        match opts.system_prompt {
            Some(SystemPromptKind::Preset { append: Some(text), .. }) => {
                assert_eq!(text, "charter text");
            }
            other => panic!("expected Preset{{ append: Some(_) }}, got {other:?}"),
        }
    }

    #[test]
    fn append_system_prompt_builder_overrides_prior_system_prompt() {
        let opts = OptionsBuilder::new()
            .system_prompt(SystemPromptKind::Inline("ignored".into()))
            .append_system_prompt("wins")
            .build();
        match opts.system_prompt {
            Some(SystemPromptKind::Preset { append: Some(text), .. }) => {
                assert_eq!(text, "wins");
            }
            other => panic!("expected Preset overriding Inline, got {other:?}"),
        }
    }
}

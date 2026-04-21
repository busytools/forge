//! Configuration for spawning a `Client`.
//!
//! Mirrors Python SDK's `ClaudeAgentOptions`. Fields present for M1 only;
//! permission callback, hooks, MCP servers, session store, skills arrive in
//! later milestones and are added to this struct at that point.

use std::path::PathBuf;

/// Which permission flow the `claude` binary should use for tool invocations.
///
/// Mirrors Python SDK's `permission_mode` values (all six).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Prompt on every tool use (the default).
    Default,
    /// Auto-allow edits / writes; prompt on destructive ops.
    AcceptEdits,
    /// Read-only mode; block tools that would mutate the workspace.
    Plan,
    /// Auto-allow all tools (use with care).
    BypassPermissions,
    /// Let the binary decide based on tool + context heuristics (Python v0.1.57+).
    Auto,
    /// Never prompt; silently deny anything that would require approval.
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
#[derive(Debug, Clone)]
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
}

impl Default for Options {
    fn default() -> Self {
        Self {
            binary: "claude".into(),
            cwd: None,
            resume: None,
            model: None,
            permission_mode: PermissionMode::Default,
        }
    }
}

/// Builder for [`Options`].
#[derive(Debug, Clone, Default)]
pub struct OptionsBuilder {
    inner: Options,
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

    /// Finalise and return the `Options`.
    #[must_use]
    pub fn build(self) -> Options {
        self.inner
    }
}

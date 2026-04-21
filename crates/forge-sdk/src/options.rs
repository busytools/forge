//! Configuration for spawning a `Client`.
//!
//! Mirrors Python SDK's `ClaudeAgentOptions`. Fields present for M1/M2 only;
//! hooks, MCP servers, session store, skills arrive in later milestones
//! and are added to this struct at that point.

use std::path::PathBuf;
use std::sync::Arc;

use crate::permissions::CanUseToolCallback;

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

    /// Finalise and return the `Options`.
    #[must_use]
    pub fn build(self) -> Options {
        self.inner
    }
}

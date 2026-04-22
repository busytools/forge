//! `tokio::process` wrapping of the `claude` binary.

use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::Error;
use crate::mcp::orchestration::McpHosts;
use crate::options::{
    Options, PermissionMode, SdkPluginConfig, SystemPromptKind, ThinkingConfig, ToolsPreset,
};

/// Run `<binary> --version` synchronously and return the stdout.
///
/// # Errors
///
/// [`Error::CliNotFound`] when the binary isn't on PATH; [`Error::Io`]
/// on any other spawn/wait failure; [`Error::Process`] when the version
/// probe exits non-zero.
pub fn query_cli_version(binary: &str) -> Result<String, Error> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::CliNotFound {
                binary: binary.to_string(),
            },
            _ => Error::Io(e),
        })?;
    if !output.status.success() {
        return Err(Error::Process {
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Check that the reported `claude` version is at least `min_version`
/// (semver-style major.minor.patch, only major component compared).
///
/// # Errors
///
/// [`Error::Connection`] when the reported version is below the minimum
/// or can't be parsed.
pub fn check_cli_version(reported: &str, min_version: &str) -> Result<(), Error> {
    // `reported` is typically like `2.1.116 (anthropic)` or `claude 2.1.116`.
    let token = reported
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .ok_or_else(|| Error::Connection {
            reason: format!("could not parse claude version from: {reported}"),
        })?;
    let major: u32 = token
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Connection {
            reason: format!("could not parse major version from: {token}"),
        })?;
    let min_major: u32 = min_version
        .split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Connection {
            reason: format!("could not parse minimum major from: {min_version}"),
        })?;
    if major < min_major {
        return Err(Error::Connection {
            reason: format!("claude CLI version {reported} below minimum required {min_version}"),
        });
    }
    Ok(())
}

/// Build the subprocess argv from [`Options`], matching Python SDK's
/// `_build_command` byte-for-byte where possible. Exposed for tests and
/// for advanced callers that want to inspect the argv without spawning.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_args(options: &Options) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    // Python SDK passes only `--output-format stream-json --verbose`
    // (streaming input is signalled by sending stream-json on stdin,
    // not by a flag). Leaving `--input-format` off keeps us
    // byte-compatible with Python's argv composition.
    args.push("--output-format".into());
    args.push("stream-json".into());
    args.push("--verbose".into());

    // system_prompt — Python always emits a form of this flag. We match
    // only when the caller set an explicit value; None means "inherit
    // CLI default" which is closer to Rust idiom.
    if let Some(sp) = &options.system_prompt {
        match sp {
            SystemPromptKind::Inline(text) => {
                args.push("--system-prompt".into());
                args.push(text.clone());
            }
            SystemPromptKind::File(path) => {
                args.push("--system-prompt-file".into());
                args.push(path.to_string_lossy().into_owned());
            }
            SystemPromptKind::PresetAppend(append) => {
                args.push("--append-system-prompt".into());
                args.push(append.clone());
            }
        }
    }

    // tools (base set). Python emits `--tools default` for the preset,
    // `--tools <csv>` for a concrete list, `--tools ""` for an empty list.
    if let Some(tools) = &options.tools {
        match tools {
            ToolsPreset::Default => {
                args.push("--tools".into());
                args.push("default".into());
            }
            ToolsPreset::List(names) => {
                args.push("--tools".into());
                args.push(names.join(","));
            }
        }
    }

    // --allowedTools (camelCase per Python SDK). Combines explicit
    // allowed_tools + Skill injection.
    let mut allowed: Vec<String> = options.allowed_tools.clone();
    for skill in &options.skills {
        if skill == "all" {
            allowed.push("Skill".into());
        } else {
            allowed.push(format!("Skill({skill})"));
        }
    }
    if !allowed.is_empty() {
        args.push("--allowedTools".into());
        args.push(allowed.join(","));
    }

    if let Some(n) = options.max_turns {
        args.push("--max-turns".into());
        args.push(n.to_string());
    }
    if let Some(budget) = options.max_budget_usd {
        args.push("--max-budget-usd".into());
        args.push(budget.to_string());
    }
    if !options.disallowed_tools.is_empty() {
        args.push("--disallowedTools".into());
        args.push(options.disallowed_tools.join(","));
    }
    if let Some(tb) = options.task_budget {
        args.push("--task-budget".into());
        args.push(tb.to_string());
    }
    if let Some(model) = &options.model {
        args.push("--model".into());
        args.push(model.clone());
    }
    if let Some(fb) = &options.fallback_model {
        args.push("--fallback-model".into());
        args.push(fb.clone());
    }
    if !options.betas.is_empty() {
        args.push("--betas".into());
        args.push(options.betas.join(","));
    }
    if let Some(name) = &options.permission_prompt_tool_name {
        args.push("--permission-prompt-tool".into());
        args.push(name.clone());
    }
    // Python SDK only emits `--permission-mode` when the caller set
    // one explicitly. We mirror that: the CLI default is already
    // `default`, so omitting the flag on the default variant avoids
    // argv drift and also lets the CLI honour any user-level override.
    if options.permission_mode != PermissionMode::Default {
        args.push("--permission-mode".into());
        args.push(options.permission_mode.as_cli_arg().into());
    }
    if options.continue_conversation {
        args.push("--continue".into());
    }
    if let Some(resume) = &options.resume {
        args.push("--resume".into());
        args.push(resume.clone());
    }
    if let Some(sid) = &options.session_id {
        args.push("--session-id".into());
        args.push(sid.clone());
    }
    // --settings (with optional sandbox merge). Python's
    // `_build_settings_value` — resolves settings + sandbox into one CLI
    // argument, either a file path or an inline JSON string.
    if let Some(value) = options.build_settings_value() {
        args.push("--settings".into());
        args.push(value);
    }

    for dir in &options.add_dirs {
        args.push("--add-dir".into());
        args.push(dir.to_string_lossy().into_owned());
    }

    // MCP: pass --mcp-config '<inline-json>' when servers are registered.
    // Python SDK uses inline JSON (not a temp file) with {"type": "sdk"}
    // entries to signal in-process hosting; external servers carry their
    // own stdio / SSE / HTTP config verbatim.
    let hosts = McpHosts::new(
        options.mcp_servers.clone(),
        options.external_mcp_servers.clone(),
    );
    if !hosts.is_empty() {
        args.push("--mcp-config".into());
        args.push(hosts.config_argv());
    }

    if options.include_partial_messages {
        args.push("--include-partial-messages".into());
    }
    if options.fork_session {
        args.push("--fork-session".into());
    }
    if options.enable_file_checkpointing {
        args.push("--enable-file-checkpointing".into());
    }
    if options.session_store.is_some() {
        args.push("--session-mirror".into());
    }

    // --setting-sources: explicit override wins; otherwise default to
    // user,project when skills is set (per Python SDK behaviour).
    let setting_sources: Option<Vec<String>> = options.setting_sources.clone().or_else(|| {
        if options.skills.is_empty() {
            None
        } else {
            Some(vec!["user".into(), "project".into()])
        }
    });
    if let Some(sources) = setting_sources {
        args.push(format!("--setting-sources={}", sources.join(",")));
    }

    for plugin in &options.plugins {
        match plugin {
            SdkPluginConfig::Local { path } => {
                args.push("--plugin-dir".into());
                args.push(path.to_string_lossy().into_owned());
            }
        }
    }

    // extra_args — arbitrary CLI flags. `None` value = bare flag.
    for (flag, maybe_val) in &options.extra_args {
        args.push(format!("--{flag}"));
        if let Some(v) = maybe_val {
            args.push(v.clone());
        }
    }

    // Resolve thinking config → --thinking / --max-thinking-tokens.
    // `thinking` takes precedence over the deprecated `max_thinking_tokens`.
    if let Some(t) = &options.thinking {
        match t {
            ThinkingConfig::Adaptive => {
                args.push("--thinking".into());
                args.push("adaptive".into());
            }
            ThinkingConfig::Enabled { budget_tokens } => {
                args.push("--max-thinking-tokens".into());
                args.push(budget_tokens.to_string());
            }
            ThinkingConfig::Disabled => {
                args.push("--thinking".into());
                args.push("disabled".into());
            }
        }
    } else if let Some(n) = options.max_thinking_tokens {
        args.push("--max-thinking-tokens".into());
        args.push(n.to_string());
    }

    if let Some(effort) = &options.effort {
        args.push("--effort".into());
        args.push(effort.as_cli_arg());
    }

    if let Some(schema) = options.output_format_json_schema() {
        args.push("--json-schema".into());
        args.push(schema);
    }

    // Always use streaming mode with stdin (matching TypeScript SDK).
    // This allows agents and other large configs to be sent via
    // `initialize` request rather than argv.
    args.push("--input-format".into());
    args.push("stream-json".into());

    args
}

/// A live subprocess with owned stdin/stdout handles.
///
/// Drop takes best-effort cleanup (sends SIGKILL if still alive). Prefer
/// [`shutdown`](Self::shutdown) for graceful termination.
#[derive(Debug)]
pub struct Subprocess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_task: Option<JoinHandle<()>>,
    line_buf: String,
}

impl Subprocess {
    /// Spawn the `claude` binary with stream-json flags.
    ///
    /// # Errors
    ///
    /// - [`Error::CliNotFound`] when the binary isn't on PATH or the given path
    ///   doesn't exist.
    /// - [`Error::Io`] for other spawn failures.
    #[allow(clippy::unused_async)] // kept async for API symmetry + future runtime hooks
    pub async fn spawn(options: &Options) -> Result<Self, Error> {
        // Optional CLI-version guard. Runs `<binary> --version` once.
        if let Some(min) = &options.minimum_cli_version {
            match query_cli_version(&options.binary) {
                Ok(reported) => check_cli_version(&reported, min)?,
                // Tolerate probe failure: spawn may still succeed on a
                // freshly-available binary, but surface the error when
                // spawn itself fails below.
                Err(e) => {
                    tracing::warn!(?e, "claude --version probe failed; skipping version check");
                }
            }
        }
        let mut cmd = Command::new(&options.binary);
        cmd.args(build_args(options));
        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &options.env {
            cmd.env(k, v);
        }
        if let Some(user) = &options.user {
            cmd.env("USER", user);
        }

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        debug!(?cmd, "spawning claude subprocess");
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::CliNotFound {
                binary: options.binary.clone(),
            },
            _ => Error::Io(e),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| Error::Connection {
            reason: "stdin pipe missing".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| Error::Connection {
            reason: "stdout pipe missing".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| Error::Connection {
            reason: "stderr pipe missing".into(),
        })?;

        // Drain stderr on a background task so the pipe never blocks.
        // When options.stderr is Some, forward each line to the callback;
        // otherwise discard silently.
        let stderr_callback = options.stderr.clone();
        let stderr_task = tokio::spawn(async move {
            drain_stderr(stderr, stderr_callback).await;
        });

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_task: Some(stderr_task),
            line_buf: String::new(),
        })
    }

    /// Read one line (without the trailing `\n`) from the subprocess stdout.
    ///
    /// Returns `Ok(None)` at end-of-stream.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on read failure.
    pub async fn read_line(&mut self) -> Result<Option<String>, Error> {
        self.line_buf.clear();
        let n = self.stdout.read_line(&mut self.line_buf).await?;
        if n == 0 {
            return Ok(None);
        }
        // Trim the trailing newline(s).
        while matches!(self.line_buf.chars().last(), Some('\n' | '\r')) {
            self.line_buf.pop();
        }
        Ok(Some(std::mem::take(&mut self.line_buf)))
    }

    /// Write one line of stream-json to the subprocess stdin.
    ///
    /// The caller is responsible for including the trailing `\n` — this
    /// matches the contract of
    /// [`codec::encode_user_prompt`](super::codec::encode_user_prompt).
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on write failure.
    pub async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Graceful shutdown: close stdin, wait for the subprocess to exit.
    ///
    /// # Errors
    ///
    /// [`Error::Process`] when the subprocess exits non-zero, [`Error::Io`]
    /// for I/O failure.
    pub async fn shutdown(mut self) -> Result<(), Error> {
        // Close stdin first — signals EOF to the subprocess so it can exit cleanly.
        drop(self.stdin);
        let status = self.child.wait().await?;
        // Give the stderr drain task a chance to finish emitting lines
        // after the subprocess exits. The task self-terminates on EOF.
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }
        if !status.success() {
            warn!(?status, "claude subprocess exited non-zero during shutdown");
            return Err(Error::Process {
                exit_code: status.code(),
                stderr: String::new(), // stderr capture is best-effort; expanded in M1 polish
            });
        }
        Ok(())
    }
}

/// Background drain for the subprocess stderr pipe. Reads lines as UTF-8
/// (lossy on invalid bytes) and forwards each to the caller-supplied
/// callback when set. Silently consumes lines otherwise so the pipe
/// never blocks.
async fn drain_stderr(
    stderr: tokio::process::ChildStderr,
    callback: Option<Arc<dyn Fn(String) + Send + Sync>>,
) {
    use tokio::io::AsyncBufReadExt as _;
    let mut reader = BufReader::new(stderr);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = match reader.read_line(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                debug!(?e, "stderr read failed");
                return;
            }
        };
        if n == 0 {
            return;
        }
        while matches!(buf.chars().last(), Some('\n' | '\r')) {
            buf.pop();
        }
        if let Some(cb) = callback.as_ref() {
            cb(buf.clone());
        }
    }
}

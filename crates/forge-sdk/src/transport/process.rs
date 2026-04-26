//! `tokio::process` wrapping of the `claude` binary.

use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::Error;
use crate::argv::build_args;
use crate::options::Options;

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

/// A live subprocess with owned stdin/stdout handles.
///
/// Drop takes best-effort cleanup (sends SIGKILL if still alive). Prefer
/// [`shutdown`](Self::shutdown) for graceful termination.
#[derive(Debug)]
pub struct Subprocess {
    child: Child,
    /// `None` after [`Subprocess::end_input`] closes the write half.
    /// All subsequent [`Subprocess::write_line`] calls error out.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr_task: Option<JoinHandle<()>>,
    line_buf: String,
    /// Captured `Child::wait` result. Populated by the first call to
    /// [`Subprocess::close`] so subsequent calls are idempotent no-ops.
    closed: bool,
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
        // The caller asked for a floor — if the probe fails, surface it
        // rather than silently skipping (they won't know the check was
        // bypassed otherwise).
        if let Some(min) = &options.minimum_cli_version {
            let reported = query_cli_version(&options.binary).map_err(|e| Error::Connection {
                reason: format!("minimum_cli_version set but --version probe failed: {e}"),
            })?;
            check_cli_version(&reported, min)?;
        }
        let mut cmd = Command::new(&options.binary);
        cmd.args(build_args(options)?);
        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        }

        // Env setup — mirrors Python
        // `_internal/transport/subprocess_cli.py:395-437`:
        //
        // 1. Start from parent env, filtering out `CLAUDECODE` so SDK-
        //    spawned subprocesses don't think they're nested inside a
        //    Claude Code parent (upstream issue #573).
        // 2. Inject `CLAUDE_CODE_ENTRYPOINT=sdk-rs` (overridable via
        //    `options.env`). Python SDK stamps `sdk-py` here; forge-sdk
        //    identifies as Rust for honest attribution.
        // 3. Let `options.env` override anything the SDK would
        //    otherwise default, EXCEPT `CLAUDE_AGENT_SDK_VERSION` —
        //    that one the SDK always stamps last.
        // 4. Stamp `CLAUDE_AGENT_SDK_VERSION` as the final write.
        // 5. Gate `CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING=true` on
        //    `enable_file_checkpointing` (NOT a CLI flag — env var).
        // 6. Set `PWD` to the chosen cwd when present.
        cmd.env_remove("CLAUDECODE");
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "sdk-rs");
        for (k, v) in &options.env {
            cmd.env(k, v);
        }
        cmd.env("CLAUDE_AGENT_SDK_VERSION", env!("CARGO_PKG_VERSION"));
        if options.enable_file_checkpointing {
            cmd.env("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "true");
        }
        if let Some(cwd) = &options.cwd {
            cmd.env("PWD", cwd);
        }

        // `options.user` must setuid the child — Python
        // `subprocess_cli.py:458` passes it to `anyio.open_process`'s
        // `user=` kwarg. Rust's analogue is `tokio::process::Command`'s
        // inherent `uid()` method on Unix; no-op on other targets so
        // the option stays a Unix-only knob.
        #[cfg(unix)]
        if let Some(user) = &options.user {
            match user.parse::<u32>() {
                Ok(uid) => {
                    cmd.uid(uid);
                }
                Err(_) => {
                    tracing::warn!(
                        %user,
                        "Options::user did not parse as a uid; ignoring (Python accepts a numeric uid)"
                    );
                }
            }
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

        let stdout_reader = match options.max_buffer_size {
            Some(n) if n > 0 => BufReader::with_capacity(n, stdout),
            _ => BufReader::new(stdout),
        };

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: stdout_reader,
            stderr_task: Some(stderr_task),
            line_buf: String::new(),
            closed: false,
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
    /// [`Error::Io`] on write failure, including writes issued after
    /// [`end_input`](Self::end_input).
    pub async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "subprocess stdin already closed (end_input called)",
            )));
        };
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Close the subprocess's stdin. Safe to call multiple times.
    /// Subsequent [`write_line`](Self::write_line) calls will fail
    /// with a `BrokenPipe` `Io` error.
    ///
    /// # Errors
    ///
    /// None today — kept `Result` for [`Transport`](super::Transport)
    /// trait compatibility.
    #[allow(clippy::unused_async)] // Trait symmetry: Transport::end_input is async.
    pub async fn end_input(&mut self) -> Result<(), Error> {
        // Dropping the ChildStdin closes the write half.
        self.stdin = None;
        Ok(())
    }

    /// Graceful shutdown: close stdin, wait for the subprocess to
    /// exit, drain the stderr task. Idempotent — subsequent calls
    /// are no-ops.
    ///
    /// # Errors
    ///
    /// [`Error::Process`] when the subprocess exits non-zero, [`Error::Io`]
    /// for I/O failure.
    pub async fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        // Close stdin first — signals EOF to the subprocess so it can exit cleanly.
        self.stdin = None;
        let status = self.child.wait().await?;
        // Give the stderr drain task a chance to finish emitting lines
        // after the subprocess exits. The task self-terminates on EOF.
        if let Some(task) = self.stderr_task.take() {
            if let Err(e) = task.await {
                warn!(error = %e, "stderr drain task ended abnormally");
            }
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

    /// Consuming alias for [`close`](Self::close). Kept for backward
    /// compatibility with pre-`Transport`-trait callers.
    ///
    /// # Errors
    ///
    /// Same as [`close`](Self::close).
    pub async fn shutdown(mut self) -> Result<(), Error> {
        self.close().await
    }
}

#[async_trait::async_trait]
impl super::Transport for Subprocess {
    async fn read_line(&mut self) -> Result<Option<String>, Error> {
        Subprocess::read_line(self).await
    }
    async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        Subprocess::write_line(self, line).await
    }
    async fn end_input(&mut self) -> Result<(), Error> {
        Subprocess::end_input(self).await
    }
    async fn close(&mut self) -> Result<(), Error> {
        Subprocess::close(self).await
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

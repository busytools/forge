//! `tokio::process` wrapping of the `claude` binary.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tracing::{debug, warn};

use crate::Error;
use crate::mcp::orchestration::McpHosts;
use crate::options::Options;

/// A live subprocess with owned stdin/stdout handles.
///
/// Drop takes best-effort cleanup (sends SIGKILL if still alive). Prefer
/// [`shutdown`](Self::shutdown) for graceful termination.
#[derive(Debug)]
pub struct Subprocess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    _stderr: ChildStderr,
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
        let mut cmd = Command::new(&options.binary);
        cmd.arg("--output-format").arg("stream-json");
        cmd.arg("--input-format").arg("stream-json");
        cmd.arg("--verbose");
        cmd.arg("--permission-mode")
            .arg(options.permission_mode.as_cli_arg());

        if let Some(model) = &options.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(resume) = &options.resume {
            cmd.arg("--resume").arg(resume);
        }
        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        }

        // MCP: pass --mcp-config '<inline-json>' when servers are registered.
        // Python SDK uses inline JSON (not a temp file) with {"type": "sdk"}
        // entries to signal in-process hosting.
        if !options.mcp_servers.is_empty() {
            let hosts = McpHosts::new(options.mcp_servers.clone());
            cmd.arg("--mcp-config").arg(hosts.config_argv());
        }

        // --allowedTools (camelCase per Python SDK). Combines explicit
        // allowed_tools + Skill injection per C3.4.
        let mut allowed: Vec<String> = options.allowed_tools.clone();
        for skill in &options.skills {
            if skill == "all" {
                allowed.push("Skill".into());
            } else {
                allowed.push(format!("Skill({skill})"));
            }
        }
        if !allowed.is_empty() {
            cmd.arg("--allowedTools").arg(allowed.join(","));
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
            cmd.arg(format!("--setting-sources={}", sources.join(",")));
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

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            _stderr: stderr,
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

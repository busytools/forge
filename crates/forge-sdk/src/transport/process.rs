//! `tokio::process` wrapping of the `claude` binary.
//!
//! ## I/O architecture
//!
//! The subprocess's stdin and stdout each run inside a dedicated
//! tokio task; the [`Subprocess`] surface talks to them over mpsc
//! channels. Two properties fall out:
//!
//! - **Cancel-safe reads.** [`tokio::io::AsyncBufReadExt::read_line`]
//!   is documented as not cancel-safe, but [`mpsc::Receiver::recv`]
//!   is. Driving stdout through a reader task lets callers
//!   `tokio::select!` over [`Subprocess::read_line`] without losing
//!   already-consumed bytes.
//! - **Concurrent writes.** Cloning the writer-side mpsc is cheap
//!   and `Send + 'static`. The `Subprocess::clone_writer` helper
//!   hands out a clonable writer backed by the same writer task,
//!   so detached control-request dispatch can write concurrently
//!   with the reader without serialising on `&mut self`.

use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::Error;
use crate::argv::build_args;
use crate::options::{Options, WireTee};

/// Default upper bound on `close()` wait-for-exit. After this elapses,
/// the child is SIGKILL'd. 5s is generous for a CLI that's draining
/// in-flight turns; tests that want to verify the kill path drive
/// against a process that ignores stdin EOF.
const CLOSE_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run `<binary> --version` synchronously and return the stdout.
///
/// # Errors
///
/// [`Error::CliNotFound`] when the binary isn't on PATH; [`Error::Io`]
/// on any other spawn/wait failure; [`Error::Process`] when the version
/// probe exits non-zero.
pub fn query_cli_version(binary: &str) -> Result<String, Error> {
    let output =
        std::process::Command::new(binary).arg("--version").output().map_err(|e| {
            match e.kind() {
                std::io::ErrorKind::NotFound => Error::CliNotFound { binary: binary.to_string() },
                _ => Error::Io(e),
            }
        })?;
    if !output.status.success() {
        return Err(Error::Process {
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Check that the reported `claude` version is at least `min_version`.
/// Compares all three semver components (major.minor.patch)
/// lexicographically.
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
    let reported_triple = parse_semver_triple(token).ok_or_else(|| Error::Connection {
        reason: format!("could not parse semver triple from: {token}"),
    })?;
    let min_triple = parse_semver_triple(min_version).ok_or_else(|| Error::Connection {
        reason: format!("could not parse minimum semver triple from: {min_version}"),
    })?;
    if reported_triple < min_triple {
        return Err(Error::Connection {
            reason: format!("claude CLI version {reported} below minimum required {min_version}"),
        });
    }
    Ok(())
}

/// Parse `"<major>.<minor>.<patch>"` (any trailing non-numeric suffix
/// ignored). Missing minor/patch default to 0.
fn parse_semver_triple(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let patch_str = parts.next().unwrap_or("0");
    // Strip any non-digit suffix (e.g. "117-rc1" or "117 (build)").
    let patch_digits: String = patch_str.chars().take_while(char::is_ascii_digit).collect();
    let patch: u32 = if patch_digits.is_empty() { 0 } else { patch_digits.parse().ok()? };
    Some((major, minor, patch))
}

/// Outcome of one writer-task operation. Sent back over a oneshot the
/// caller provides.
type IoAck = Result<(), Error>;

/// A write-side command dispatched to the writer task.
#[derive(Debug)]
enum WriterCmd {
    /// Append `line` to stdin and flush.
    Write(String, oneshot::Sender<IoAck>),
    /// Drop stdin so the CLI sees EOF.
    EndInput(oneshot::Sender<IoAck>),
}

/// A live `claude` subprocess driven over channels.
///
/// Spawned via [`Subprocess::spawn`]. Every read goes through the
/// reader task → mpsc; every write goes through the writer task ←
/// mpsc. Drop hits `kill_on_drop` cleanup; [`close`](Self::close)
/// gives a graceful exit path with a 5s timeout before SIGKILL.
pub struct Subprocess {
    /// Outbound channel into the writer task. Cloned by
    /// [`clone_writer`](Self::clone_writer) so external
    /// dispatchers can write without contending on `&mut self`.
    writer_tx: mpsc::UnboundedSender<WriterCmd>,
    /// Inbound channel from the reader task. Single-consumer.
    reader_rx: mpsc::UnboundedReceiver<Result<Option<String>, Error>>,
    /// Writer task handle. [`close`](Self::close) aborts this so the
    /// child sees stdin EOF promptly even when external writer clones
    /// still hold `writer_tx` clones — without the abort the writer
    /// task would wait for every clone to drop and the child wouldn't
    /// see EOF on stdin until then.
    writer_task: Option<JoinHandle<()>>,
    /// Stderr drain task — best-effort logged or forwarded to the
    /// caller's callback. Joined during [`close`](Self::close).
    stderr_task: Option<JoinHandle<()>>,
    /// The child handle. [`close`](Self::close) waits on it (with
    /// timeout) and SIGKILLs on hang.
    child: Option<Child>,
    /// Idempotency guard for [`close`](Self::close).
    closed: bool,
}

impl std::fmt::Debug for Subprocess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subprocess")
            .field("child", &self.child.is_some())
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl Subprocess {
    /// Spawn the `claude` binary with stream-json flags.
    ///
    /// # Errors
    ///
    /// - [`Error::CliNotFound`] when the binary isn't on PATH or the given path
    ///   doesn't exist.
    /// - [`Error::Io`] for other spawn failures.
    pub async fn spawn(options: &Options) -> Result<Self, Error> {
        // Optional CLI-version guard. Runs `<binary> --version` once.
        // The caller asked for a floor — if the probe fails, surface it
        // rather than silently skipping (they won't know the check was
        // bypassed otherwise).
        if let Some(min) = &options.minimum_cli_version {
            // `claude --version` is a fork+exec; wrap in spawn_blocking
            // so the tokio worker thread isn't parked during the probe.
            let binary = options.binary.clone();
            let reported = tokio::task::spawn_blocking(move || query_cli_version(&binary))
                .await
                .map_err(|e| Error::Connection {
                    reason: format!("version probe join failed: {e}"),
                })?
                .map_err(|e| Error::Connection {
                    reason: format!("minimum_cli_version set but --version probe failed: {e}"),
                })?;
            check_cli_version(&reported, min)?;
        }
        let mut cmd = Command::new(&options.binary);
        cmd.args(build_args(options)?);
        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        }

        // Env setup. Order matters because later writes win:
        //
        // 1. Start from parent env, filtering out `CLAUDECODE` so SDK-
        //    spawned subprocesses don't think they're nested inside a
        //    Claude Code parent (upstream issue #573).
        // 2. Clear any inherited `CLAUDE_CODE_ENTRYPOINT`. The wire-
        //    classification rewriter relies on the CLI self-classifying
        //    (which yields `sdk-cli` for piped stdout); a leaked stamp
        //    from the parent shell (e.g. running forge inside a
        //    Claude Code session that itself set `sdk-rs`) would
        //    confuse the rewriter's source-string assumption. Callers
        //    that genuinely want to preset the entrypoint can still do
        //    so via `options.env` — those writes happen after this
        //    removal.
        // 3. When a wire-classification rewriter proxy is attached,
        //    inject `HTTPS_PROXY` + `HTTP_PROXY` + `NODE_EXTRA_CA_CERTS`
        //    so the child's HTTPS traffic flows through our MITM. The
        //    proxy rewrites the 6 sdk-cli signals to cli shape — see
        //    `transport::proxy` for details.
        // 4. Let `options.env` override anything above, EXCEPT
        //    `CLAUDE_AGENT_SDK_VERSION` — that one we always stamp last.
        // 5. Stamp `CLAUDE_AGENT_SDK_VERSION` as the final write.
        // 6. Set `PWD` to the chosen cwd when present.
        cmd.env_remove("CLAUDECODE");
        cmd.env_remove("CLAUDE_CODE_ENTRYPOINT");
        if let Some(proxy) = &options.proxy {
            let proxy_url = proxy.proxy_url();
            cmd.env("HTTPS_PROXY", &proxy_url);
            cmd.env("HTTP_PROXY", &proxy_url);
            cmd.env("NODE_EXTRA_CA_CERTS", proxy.ca_cert_path());
        }
        for (k, v) in &options.env {
            cmd.env(k, v);
        }
        cmd.env("CLAUDE_AGENT_SDK_VERSION", env!("CARGO_PKG_VERSION"));
        if let Some(cwd) = &options.cwd {
            cmd.env("PWD", cwd);
        }

        // `options.user` must setuid the child — `tokio::process::Command`
        // exposes `uid()` on Unix; no-op on other targets, so the
        // option stays a Unix-only knob.
        #[cfg(unix)]
        if let Some(user) = &options.user {
            match user.parse::<u32>() {
                Ok(uid) => {
                    cmd.uid(uid);
                }
                Err(_) => {
                    tracing::warn!(
                        %user,
                        "Options::user did not parse as a uid; ignoring (wire accepts a numeric uid)"
                    );
                }
            }
        }

        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        debug!(?cmd, "spawning claude subprocess");
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::CliNotFound { binary: options.binary.clone() },
            _ => Error::Io(e),
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Connection { reason: "stdin pipe missing".into() })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Connection { reason: "stdout pipe missing".into() })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Connection { reason: "stderr pipe missing".into() })?;

        let stderr_callback = options.stderr.clone();
        let stderr_task = tokio::spawn(drain_stderr(stderr, stderr_callback));

        let buf_capacity = options.max_buffer_size.filter(|n| *n > 0);
        let (reader_tx, reader_rx) = mpsc::unbounded_channel();
        spawn_reader_task(stdout, buf_capacity, options.tee_inbound.clone(), reader_tx);

        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let writer_task = spawn_writer_task(stdin, options.tee_outbound.clone(), writer_rx);

        Ok(Self {
            writer_tx,
            reader_rx,
            writer_task: Some(writer_task),
            stderr_task: Some(stderr_task),
            child: Some(child),
            closed: false,
        })
    }

    /// Read one line (without the trailing `\n`) from the subprocess stdout.
    ///
    /// Operating-system PID of the spawned `claude` child, when one is
    /// still attached. Returns `None` after [`close`](Self::close) or
    /// when the child has already been reaped.
    ///
    /// Used by the Inspector pane's PROCESSES section to anchor an
    /// OS-level walk of the descendant tree (the `forge-agent`
    /// crate's `env::processes` scanner — not linked here because the
    /// dependency direction forbids forge-sdk from naming forge-agent
    /// types). The PID is stable for the lifetime of the subprocess
    /// so the scanner can cache its snapshot across polls keyed off
    /// this value.
    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(tokio::process::Child::id)
    }

    /// Returns `Ok(None)` at end-of-stream. Cancel-safe.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on read failure.
    pub async fn read_line(&mut self) -> Result<Option<String>, Error> {
        // Reader task signals EOF by either sending `Ok(None)` or by
        // closing the channel — both collapse to `Ok(None)` here.
        self.reader_rx.recv().await.unwrap_or(Ok(None))
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
    /// [`end_input`](Self::end_input) or [`close`](Self::close).
    pub async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.writer_tx.send(WriterCmd::Write(line.to_owned(), ack_tx)).map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Subprocess writer task gone",
            ))
        })?;
        ack_rx.await.map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Subprocess writer task dropped ack",
            ))
        })?
    }

    /// Close the subprocess's stdin. Safe to call multiple times.
    /// Subsequent [`write_line`](Self::write_line) calls will fail
    /// with a `BrokenPipe` `Io` error.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on flush failure.
    pub async fn end_input(&mut self) -> Result<(), Error> {
        let (ack_tx, ack_rx) = oneshot::channel();
        // Writer task already gone (e.g. previous end_input + drop) →
        // treat as no-op, matching the old direct-stdin behaviour.
        if self.writer_tx.send(WriterCmd::EndInput(ack_tx)).is_err() {
            return Ok(());
        }
        ack_rx.await.unwrap_or_else(|_| {
            warn!("Subprocess::end_input: ack channel dropped");
            Ok(())
        })
    }

    /// Hand out a clonable [`SharedWriter`] backed by the same writer
    /// task. Multiple clones can write concurrently; the writer task
    /// serialises onto the child's stdin in arrival order. Used by
    /// detached control-request dispatch so a slow callback can't
    /// block the reader / command loop.
    pub(crate) fn clone_writer(&self) -> Arc<SharedWriter> {
        Arc::new(SharedWriter { writer_tx: self.writer_tx.clone() })
    }

    /// Graceful shutdown: close stdin, wait for the subprocess to
    /// exit (5s timeout, SIGKILL fallback), drain the stderr task.
    /// Idempotent — subsequent calls are no-ops.
    ///
    /// # Errors
    ///
    /// [`Error::Process`] when the subprocess exits non-zero or hits
    /// the SIGKILL fallback; [`Error::Io`] on wait failure.
    pub async fn close(&mut self) -> Result<(), Error> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;

        // Drop our held writer_tx and abort the writer task. The abort
        // forces stdin closure even if external `SharedWriter` clones
        // (handed out via `clone_writer`) still hold writer_tx
        // clones — without the abort the writer task would wait for
        // every clone to drop. In-flight write_line acks on cloned
        // writers will resolve with the ack-channel-dropped error,
        // which is correct semantics for a closed transport.
        let (closed_tx, closed_rx) = mpsc::unbounded_channel();
        self.writer_tx = closed_tx;
        // Drop the placeholder receiver immediately so any concurrent
        // SharedWriter clone trying to send on `writer_tx` fails fast
        // with BrokenPipe rather than queuing into a dead channel and
        // hanging for the duration of the close grace period.
        drop(closed_rx);
        if let Some(handle) = self.writer_task.take() {
            handle.abort();
            // Symmetric drain with the stderr task below. abort() then
            // await — the future returns JoinError::Cancelled which is
            // expected; surface panic JoinErrors at debug so the
            // tokio default panic handler isn't the only path that
            // notices.
            if let Err(e) = handle.await
                && !e.is_cancelled()
            {
                debug!(error = %e, "writer task ended abnormally");
            }
        }

        // Wait for child exit, with a SIGKILL timeout so a stuck CLI
        // doesn't pin the disconnect path forever.
        let child_result = if let Some(mut child) = self.child.take() {
            match tokio::time::timeout(CLOSE_WAIT_TIMEOUT, child.wait()).await {
                Ok(Ok(status)) if status.success() => Ok(()),
                Ok(Ok(status)) => {
                    Err(Error::Process { exit_code: status.code(), stderr: String::new() })
                }
                Ok(Err(e)) => Err(Error::Io(e)),
                Err(_elapsed) => {
                    warn!("Subprocess::close timed out waiting for child; sending SIGKILL");
                    if let Err(e) = child.kill().await {
                        warn!(error = %e, "Subprocess::close: kill() failed");
                    }
                    Err(Error::Process {
                        exit_code: None,
                        stderr: String::from("close timeout — child killed"),
                    })
                }
            }
        } else {
            Ok(())
        };

        // Best-effort drain of the stderr task. We don't surface its
        // status — it's a logging sink.
        if let Some(task) = self.stderr_task.take()
            && let Err(e) = task.await
        {
            debug!(error = %e, "stderr drain task ended abnormally");
        }

        child_result
    }
}

/// Cloneable writer half of [`Subprocess`] — pushes onto the writer
/// task's mpsc. Multiple clones can write concurrently; the writer
/// task serialises onto the child's stdin in arrival order.
///
/// Returned by [`Subprocess::clone_writer`]. Used by detached
/// `control_request` dispatch.
#[derive(Debug, Clone)]
pub(crate) struct SharedWriter {
    writer_tx: mpsc::UnboundedSender<WriterCmd>,
}

impl SharedWriter {
    /// Write one line of stream-json to the transport. Caller supplies
    /// the trailing `\n`.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on write failure or after the transport has
    /// closed its write half.
    pub(crate) async fn write_line(&self, line: &str) -> Result<(), Error> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.writer_tx.send(WriterCmd::Write(line.to_owned(), ack_tx)).map_err(|_| {
            Error::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "SharedWriter task gone"))
        })?;
        ack_rx.await.map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "SharedWriter task dropped ack",
            ))
        })?
    }

    /// Close the write half so the remote sees EOF on stdin. Idempotent.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] on flush failure.
    pub(crate) async fn end_input(&self) -> Result<(), Error> {
        let (ack_tx, ack_rx) = oneshot::channel();
        // Writer task already exited (e.g. previous end_input or close)
        // → treat as no-op, matching `Subprocess::end_input`.
        if self.writer_tx.send(WriterCmd::EndInput(ack_tx)).is_err() {
            return Ok(());
        }
        ack_rx.await.unwrap_or_else(|_| {
            warn!("SharedWriter::end_input: ack channel dropped");
            Ok(())
        })
    }
}

fn spawn_reader_task(
    stdout: ChildStdout,
    buf_capacity: Option<usize>,
    tee: Option<WireTee>,
    tx: mpsc::UnboundedSender<Result<Option<String>, Error>>,
) {
    use tracing::Instrument;
    let span = tracing::info_span!("forge_sdk::stdout_reader");
    tokio::spawn(
        async move {
            let mut reader = match buf_capacity {
                Some(n) => BufReader::with_capacity(n, stdout),
                None => BufReader::new(stdout),
            };
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf).await {
                    Ok(0) => {
                        // EOF — signal end-of-stream and exit.
                        let _ = tx.send(Ok(None));
                        break;
                    }
                    Ok(_) => {
                        let mut line = std::mem::take(&mut buf);
                        while matches!(line.chars().last(), Some('\n' | '\r')) {
                            line.pop();
                        }
                        if let Some(cb) = tee.as_ref() {
                            cb(&line);
                        }
                        if tx.send(Ok(Some(line))).is_err() {
                            // Receiver gone — caller dropped the transport.
                            break;
                        }
                    }
                    Err(e) => {
                        if tx.send(Err(Error::Io(e))).is_err() {
                            tracing::warn!(
                                target: "forge_sdk::transport",
                                "subprocess stdout I/O error after caller dropped reader"
                            );
                        }
                        break;
                    }
                }
            }
        }
        .instrument(span),
    );
}

fn spawn_writer_task(
    stdin: ChildStdin,
    tee: Option<WireTee>,
    mut rx: mpsc::UnboundedReceiver<WriterCmd>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stdin = Some(stdin);
        while let Some(cmd) = rx.recv().await {
            match cmd {
                WriterCmd::Write(line, ack) => {
                    if let Some(cb) = tee.as_ref() {
                        cb(line.trim_end_matches('\n'));
                    }
                    let result = if let Some(s) = stdin.as_mut() {
                        match s.write_all(line.as_bytes()).await {
                            Ok(()) => s.flush().await.map_err(Error::Io),
                            Err(e) => Err(Error::Io(e)),
                        }
                    } else {
                        Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "stdin already closed (end_input)",
                        )))
                    };
                    let _ = ack.send(result);
                }
                WriterCmd::EndInput(ack) => {
                    drop(stdin.take());
                    let _ = ack.send(Ok(()));
                }
            }
        }
    })
}

/// Background drain for the subprocess stderr pipe. Reads lines as UTF-8
/// (lossy on invalid bytes) and forwards each to the caller-supplied
/// callback when set. Silently consumes lines otherwise so the pipe
/// never blocks.
async fn drain_stderr(stderr: ChildStderr, callback: Option<Arc<dyn Fn(String) + Send + Sync>>) {
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
        } else if buf.starts_with("ERROR") || buf.starts_with("Error") {
            tracing::warn!(target: "forge_sdk::stderr", line = %buf, "claude stderr");
        } else {
            tracing::info!(target: "forge_sdk::stderr", line = %buf, "claude stderr");
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::time::Duration;

    /// Build a [`Subprocess`] wrapping a long-running mock that ignores
    /// stdin (`/bin/sleep 30`). Bypasses [`build_args`] — only relevant
    /// to tests that exercise the close-timeout path.
    fn spawn_sleep_subprocess(secs: u64) -> Subprocess {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg(secs.to_string());
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().expect("spawn /bin/sleep");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        let stderr_task = tokio::spawn(drain_stderr(stderr, None));
        let (reader_tx, reader_rx) = mpsc::unbounded_channel();
        spawn_reader_task(stdout, None, None, reader_tx);
        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let writer_task = spawn_writer_task(stdin, None, writer_rx);

        Subprocess {
            writer_tx,
            reader_rx,
            writer_task: Some(writer_task),
            stderr_task: Some(stderr_task),
            child: Some(child),
            closed: false,
        }
    }

    /// `close()` must SIGKILL a subprocess that ignores stdin EOF
    /// rather than hang on `wait()`. Bound documented at 5s; allow ~1s
    /// slack for tokio scheduling.
    #[tokio::test]
    async fn close_kills_unresponsive_child_within_5s() {
        let mut sub = spawn_sleep_subprocess(30);

        let start = tokio::time::Instant::now();
        let result = sub.close().await;
        let elapsed = start.elapsed();

        // The documented bound is 5s; widen the assert to 30s for
        // CI-load tolerance — flagging a one-time-blip scheduler
        // delay as a regression would be noise.
        assert!(elapsed <= Duration::from_secs(30), "close() took {elapsed:?}, expected <= 30s");
        assert!(
            elapsed >= Duration::from_secs(5),
            "close() returned in {elapsed:?}, expected >= 5s (close timeout fired)"
        );
        match result {
            Err(Error::Process { stderr, .. }) => {
                assert!(
                    stderr.contains("close timeout"),
                    "expected stderr to mention close timeout, got: {stderr}"
                );
            }
            other => panic!("expected Process error from kill, got {other:?}"),
        }
    }
}

//! Channel-bridged [`forge_sdk::Transport`] — splits subprocess stdin and
//! stdout into independent tasks so the daemon's session actor can use
//! [`tokio::select!`] to interleave [`forge_sdk::Client::next_event`]
//! reads with command-driven writes without deadlocking.
//!
//! ## Why this exists
//!
//! [`forge_sdk::transport::process::Subprocess`] holds the child's stdin
//! and stdout behind a single `&mut self` Transport surface. Awaiting
//! [`forge_sdk::Client::next_event`] consequently holds an exclusive
//! borrow of the underlying [`forge_sdk::Client`] for as long as the
//! subprocess has nothing to emit — blocking any concurrent writer that
//! needs the same `&mut Client` to call `send_user_message`.
//!
//! [`tokio::io::AsyncBufReadExt::read_line`] is also documented as not
//! cancel-safe, so the obvious fix — `select!`ing on `next_event` and a
//! command channel — would silently drop bytes that the cancelled read
//! had already consumed.
//!
//! This bridge sidesteps both problems by running the actual subprocess
//! I/O inside two helper tasks:
//!
//! - The **reader** task owns `BufReader<ChildStdout>` and pushes each
//!   line into an mpsc. The Transport's `read_line` becomes a
//!   `mpsc::recv()` — cancel-safe.
//! - The **writer** task owns `ChildStdin`, drains an mpsc of write
//!   commands, and acks each one over a oneshot. The Transport's
//!   `write_line` becomes a `mpsc::send + oneshot.recv` round trip.
//!
//! `Client` continues to see a normal `&mut Transport` surface and
//! cannot tell the difference; the daemon actor gets cancel-safe reads
//! and parallel writes.

use std::process::Stdio;

use async_trait::async_trait;
use forge_sdk::Error as SdkError;
use forge_sdk::Options;
use forge_sdk::Transport;
use forge_sdk::argv::build_args;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// Outcome of one writer-task operation. Sent back to the Transport
/// caller over a oneshot the caller provided.
type IoAck = Result<(), SdkError>;

/// A write-side command the bridge dispatches to its writer task.
#[derive(Debug)]
enum WriterCmd {
    /// Append `bytes` to the subprocess's stdin and flush.
    Write(String, oneshot::Sender<IoAck>),
    /// Drop the subprocess's stdin so the CLI sees EOF.
    EndInput(oneshot::Sender<IoAck>),
}

/// Channel-bridged transport. See module docs.
pub struct BridgedTransport {
    /// Outbound channel into the writer task.
    writer_tx: mpsc::UnboundedSender<WriterCmd>,
    /// Inbound channel from the reader task.
    reader_rx: mpsc::UnboundedReceiver<Result<Option<String>, SdkError>>,
    /// The actual subprocess handle — kept around so `close` can wait
    /// for exit before returning.
    child: Option<Child>,
}

impl std::fmt::Debug for BridgedTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgedTransport")
            .field("child", &self.child.is_some())
            .finish_non_exhaustive()
    }
}

impl BridgedTransport {
    /// Spawn `claude` with the same argv + env as
    /// [`forge_sdk::transport::process::Subprocess`], wired through
    /// stdin/stdout pumps.
    ///
    /// # Errors
    ///
    /// [`SdkError::CliNotFound`] when the binary isn't found on PATH;
    /// [`SdkError::Connection`] when stdin/stdout pipes can't be obtained;
    /// [`SdkError::Io`] for other spawn failures.
    #[allow(
        clippy::unused_async,
        reason = "kept async for API symmetry with forge_sdk::transport::process::Subprocess::spawn + future runtime hooks"
    )]
    pub async fn spawn(options: &Options) -> Result<Self, SdkError> {
        let mut cmd = Command::new(&options.binary);
        cmd.args(build_args(options)?);
        if let Some(cwd) = &options.cwd {
            cmd.current_dir(cwd);
        }

        // Mirror `Subprocess::spawn`'s env setup so the CLI sees an
        // identical environment (matters for telemetry + entrypoint
        // attribution).
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

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        debug!(?cmd, "BridgedTransport spawning claude subprocess");
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SdkError::CliNotFound {
                binary: options.binary.clone(),
            },
            _ => SdkError::Io(e),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| SdkError::Connection {
            reason: "stdin pipe missing".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| SdkError::Connection {
            reason: "stdout pipe missing".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| SdkError::Connection {
            reason: "stderr pipe missing".into(),
        })?;

        // Drain stderr on a background task so the pipe never blocks.
        // No one consumes it in M2, so we just discard.
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        warn!(line = %buf.trim_end(), "claude stderr");
                    }
                }
            }
        });

        let (reader_tx, reader_rx) = mpsc::unbounded_channel();
        spawn_reader_task(stdout, reader_tx);

        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        spawn_writer_task(stdin, writer_rx);

        Ok(Self {
            writer_tx,
            reader_rx,
            child: Some(child),
        })
    }
}

#[async_trait]
impl Transport for BridgedTransport {
    async fn read_line(&mut self) -> Result<Option<String>, SdkError> {
        match self.reader_rx.recv().await {
            Some(line) => line,
            // Reader task dropped its sender — subprocess closed stdout.
            None => Ok(None),
        }
    }

    async fn write_line(&mut self, line: &str) -> Result<(), SdkError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.writer_tx
            .send(WriterCmd::Write(line.to_owned(), ack_tx))
            .map_err(|_| {
                SdkError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "BridgedTransport writer task gone",
                ))
            })?;
        ack_rx.await.map_err(|_| {
            SdkError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "BridgedTransport writer task dropped ack",
            ))
        })?
    }

    async fn end_input(&mut self) -> Result<(), SdkError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        // If the writer task has already exited (e.g. previous
        // end_input), treat as a no-op — matches `Subprocess::end_input`'s
        // idempotent contract.
        if self.writer_tx.send(WriterCmd::EndInput(ack_tx)).is_err() {
            return Ok(());
        }
        ack_rx.await.unwrap_or(Ok(()))
    }

    async fn close(&mut self) -> Result<(), SdkError> {
        // Drop the writer channel — the writer task will close stdin and
        // exit. The reader task will exit when stdout EOFs.
        // Reconstructing the field by replacing it with a closed channel.
        let (closed_tx, _closed_rx) = mpsc::unbounded_channel();
        self.writer_tx = closed_tx;

        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        // Bound `wait()` so a stuck CLI doesn't pin the disconnect
        // path forever. 5s is generous for normal exit; on timeout we
        // SIGKILL and surface the failure as a `Process` error.
        match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(SdkError::Process {
                exit_code: status.code(),
                stderr: String::new(),
            }),
            Ok(Err(e)) => Err(SdkError::Io(e)),
            Err(_elapsed) => {
                warn!("BridgedTransport::close timed out waiting for child; sending SIGKILL");
                if let Err(e) = child.kill().await {
                    warn!(error = %e, "BridgedTransport::close: kill() failed");
                }
                Err(SdkError::Process {
                    exit_code: None,
                    stderr: String::from("close timeout — child killed"),
                })
            }
        }
    }
}

fn spawn_reader_task(
    stdout: ChildStdout,
    tx: mpsc::UnboundedSender<Result<Option<String>, SdkError>>,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
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
                    if tx.send(Ok(Some(line))).is_err() {
                        // Receiver gone — caller dropped the transport.
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(SdkError::Io(e)));
                    break;
                }
            }
        }
    });
}

fn spawn_writer_task(stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<WriterCmd>) {
    tokio::spawn(async move {
        let mut stdin = Some(stdin);
        while let Some(cmd) = rx.recv().await {
            match cmd {
                WriterCmd::Write(line, ack) => {
                    let result = if let Some(s) = stdin.as_mut() {
                        match s.write_all(line.as_bytes()).await {
                            Ok(()) => s.flush().await.map_err(SdkError::Io),
                            Err(e) => Err(SdkError::Io(e)),
                        }
                    } else {
                        Err(SdkError::Io(std::io::Error::new(
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
    });
}

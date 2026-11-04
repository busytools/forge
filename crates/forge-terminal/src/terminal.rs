//! One terminal: the PTY, the screen model, the read loop and the small
//! interface a seat drives.
//!
//! **The event loop is forge's own.** `alacritty_terminal`'s `EventLoop`
//! reads the PTY internally and only reports `Wakeup`, so the raw bytes never
//! reach a listener - and the client's stream is exactly those bytes. This
//! module runs the same loop with the tap: poll, read, scan for OSC 133, send
//! the bytes on, parse them into `Term`, and write the terminal's own
//! responses back.
//!
//! **One thread per terminal.** The read loop owns the `Pty`; writes go
//! through a cloned master fd, and a resize is a message to the loop because
//! `TIOCSWINSZ` is the `Pty`'s to make.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, OnResize, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite, Options, Shell};
use alacritty_terminal::vte::ansi::Processor;
use parking_lot::Mutex;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::sync::{broadcast, watch};

use crate::osc::Osc133;
use crate::parser;
use crate::screen::{self, ReadResult, Screen};
use crate::shell;
use crate::state::{self, ClassifyInput, Exit, State};

/// How long the screen must be quiet before the fallback reads it as a
/// prompt.
const QUIET: Duration = Duration::from_millis(150);

/// The most output one broadcast keeps for a viewer that is not reading.
const OUTPUT_BACKLOG: usize = 256;

/// How many times a nonblocking write retries before it gives up.
const WRITE_RETRIES: u32 = 100;

/// The smallest and largest size a terminal may take. Zero columns underflows
/// alacritty's grid arithmetic.
pub const MIN_COLS: u16 = 2;
pub const MAX_COLS: u16 = 500;
pub const MIN_ROWS: u16 = 2;
pub const MAX_ROWS: u16 = 300;

/// What a terminal is spawned with.
#[derive(Clone, Debug)]
pub struct TerminalConfig {
    /// The shell to run, e.g. `/bin/zsh`.
    pub shell: String,
    pub cwd: PathBuf,
    /// The seat's environment, exactly what its `claude` child gets.
    pub env: HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    pub scrollback_lines: usize,
}

/// One chunk of the terminal's output, with its per-terminal sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputChunk {
    pub seq: u32,
    pub bytes: Vec<u8>,
}

/// Why a terminal could not be spawned or driven.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the terminal's PTY could not be opened: {0}")]
    Spawn(String),
    #[error("the terminal is closed")]
    Closed,
    #[error("the terminal's input could not be written: {0}")]
    Write(String),
    #[error(
        "{cols}x{rows} is outside the terminal's bounds ({MIN_COLS}..={MAX_COLS} columns, \
         {MIN_ROWS}..={MAX_ROWS} rows)"
    )]
    Resize { cols: u16, rows: u16 },
}

/// One terminal.
pub struct Terminal {
    inner: Arc<Inner>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Terminal {
    /// Spawn a shell on a fresh PTY.
    pub fn spawn(config: &TerminalConfig) -> Result<Self, Error> {
        // **A cwd the PTY cannot enter is refused, not ignored.** The child's
        // chdir failure is silent, which would run the shell in whatever
        // directory forge was launched from.
        let cwd = config.cwd.canonicalize().map_err(|error| {
            Error::Spawn(format!(
                "the terminal's cwd {} cannot be used: {error}",
                config.cwd.display()
            ))
        })?;
        if !cwd.is_dir() {
            return Err(Error::Spawn(format!(
                "the terminal's cwd {} is not a directory",
                cwd.display()
            )));
        }
        let plan = shell::plan(&config.shell, &config.env);
        let options = Options {
            shell: Some(Shell::new(plan.program.clone(), plan.args.clone())),
            working_directory: Some(cwd.clone()),
            drain_on_exit: true,
            env: plan.env.clone(),
        };
        let window = WindowSize {
            num_lines: config.rows,
            num_cols: config.cols,
            cell_width: 0,
            cell_height: 0,
        };
        let pty = tty::new(&options, window, 0).map_err(|error| Error::Spawn(error.to_string()))?;
        let writer = pty
            .file()
            .try_clone()
            .map_err(|error| Error::Spawn(format!("the master fd could not be cloned: {error}")))?;
        let probe = pty
            .file()
            .try_clone()
            .map_err(|error| Error::Spawn(format!("the master fd could not be cloned: {error}")))?;
        let child_pid = i32::try_from(pty.child().id()).unwrap_or(0);

        let events = Arc::new(Mutex::new(Vec::new()));
        let listener = Listener { events: Arc::clone(&events) };
        let term = Term::new(
            Config { scrolling_history: config.scrollback_lines, ..Default::default() },
            &Size { cols: usize::from(config.cols), rows: usize::from(config.rows) },
            listener,
        );
        let initial_lines = u64::try_from(term.grid().total_lines()).unwrap_or(0);
        let (output, _) = broadcast::channel(OUTPUT_BACKLOG);
        let (exit_tx, exit_rx) = watch::channel(false);
        let (state_tx, state_rx) = watch::channel(State {
            running: false,
            input_expected: None,
            exit: None,
            source: state::Source::Heuristic,
        });
        let (resize_tx, resize_rx) = std::sync::mpsc::channel();
        let inner = Arc::new(Inner {
            writer: Mutex::new(writer),
            probe: Mutex::new(probe),
            term: FairMutex::new(term),
            events,
            osc: Mutex::new(Osc133::new()),
            output,
            state_tx,
            state_rx,
            exit_tx,
            exit_rx,
            // Chunks start at 1: a viewer's repaint sequence is `next - 1`,
            // so an unparsed terminal reports 0 and the first chunk still
            // lies above it.
            seq: AtomicU32::new(1),
            lines: AtomicU64::new(initial_lines),
            history: config.scrollback_lines,
            child_pid,
            closed: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            cols: AtomicU16::new(config.cols),
            rows: AtomicU16::new(config.rows),
            shell: config.shell.clone(),
            cwd,
            shim: plan.shim.clone(),
            last_output: Mutex::new(Instant::now()),
            shell_exit: Mutex::new(None),
            title: Mutex::new(String::new()),
            resize_tx,
        });
        tracing::debug!(
            event_name = "terminal_spawned",
            shell = %config.shell,
            cwd = %config.cwd.display(),
            cols = config.cols,
            rows = config.rows,
            pid = child_pid,
            "a terminal was spawned"
        );
        let thread = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("forge-terminal".to_owned())
                .spawn(move || read_loop(&inner, pty, &resize_rx))
                .map_err(|error| {
                    Error::Spawn(format!("the read thread could not start: {error}"))
                })?
        };
        Ok(Self { inner, thread: Mutex::new(Some(thread)) })
    }

    /// Type text as-is.
    pub fn write_text(&self, text: &str) -> Result<(), Error> {
        self.inner.write(text.as_bytes())
    }

    /// Type raw bytes as-is: a client's own keystrokes, which are already the
    /// bytes the PTY wants.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<(), Error> {
        self.inner.write(bytes)
    }

    /// Press named keys, in order.
    pub fn write_keys(&self, keys: &[crate::keys::Key]) -> Result<(), Error> {
        for key in keys {
            self.inner.write(key.bytes())?;
        }
        Ok(())
    }

    /// Resize the PTY and the model.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), Error> {
        if self.inner.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        if !(MIN_COLS..=MAX_COLS).contains(&cols) || !(MIN_ROWS..=MAX_ROWS).contains(&rows) {
            return Err(Error::Resize { cols, rows });
        }
        {
            // **The model resizes here, not on the read loop.** A repaint
            // taken right after this call must already be laid out at the
            // new size, and the read cursor moves with the reflow.
            let mut term = self.inner.term.lock();
            let mut lines = self.inner.lines.load(Ordering::Relaxed);
            screen::resize(&mut term, usize::from(cols), usize::from(rows), &mut lines);
            self.inner.lines.store(lines, Ordering::Relaxed);
        }
        self.inner.cols.store(cols, Ordering::Relaxed);
        self.inner.rows.store(rows, Ordering::Relaxed);
        let _ = self.inner.resize_tx.send((cols, rows));
        Ok(())
    }

    /// End the terminal: kill the process group and let the loop finish.
    pub fn close(&self) {
        self.inner.close();
    }

    /// The visible screen as text.
    pub fn screen(&self) -> Screen {
        let term = self.inner.term.lock();
        screen::extract(&term)
    }

    /// A bounded read of the scrollback and the screen.
    pub fn read(&self, since: Option<u64>, max_lines: usize, max_bytes: usize) -> ReadResult {
        let term = self.inner.term.lock();
        let appended = self.inner.lines.load(Ordering::Relaxed);
        screen::read(&term, appended, since, max_lines, max_bytes)
    }

    /// The visible screen as a VT stream, with the sequence the next chunk
    /// will carry - both read under the term's lock, because the sequence is
    /// bumped under it after each parse. So every chunk below the sequence is
    /// already in the repaint, and no chunk at or above it is.
    pub fn repaint_and_seq(&self) -> (Vec<u8>, u32) {
        let term = self.inner.term.lock();
        (crate::repaint::repaint(&term), self.inner.seq.load(Ordering::Relaxed))
    }

    /// What the terminal is doing.
    pub fn state(&self) -> State {
        self.inner.state()
    }

    /// The output stream, from the next chunk.
    pub fn subscribe(&self) -> broadcast::Receiver<OutputChunk> {
        self.inner.output.subscribe()
    }

    /// The state stream, from the current value.
    pub fn state_stream(&self) -> watch::Receiver<State> {
        self.inner.state_rx.clone()
    }

    /// A stream that fires once, when the shell exits.
    pub fn exit_stream(&self) -> watch::Receiver<bool> {
        self.inner.exit_rx.clone()
    }

    /// Whether the shell process is alive.
    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::Relaxed)
    }

    /// The shell's own exit, once it has one.
    pub fn shell_exit(&self) -> Option<Exit> {
        *self.inner.shell_exit.lock()
    }

    pub fn cols(&self) -> u16 {
        self.inner.cols.load(Ordering::Relaxed)
    }

    pub fn rows(&self) -> u16 {
        self.inner.rows.load(Ordering::Relaxed)
    }

    pub fn shell(&self) -> &str {
        &self.inner.shell
    }

    pub fn cwd(&self) -> &Path {
        &self.inner.cwd
    }

    /// The title the shell last set, when it set one.
    pub fn title(&self) -> String {
        self.inner.title.lock().clone()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.inner.close();
        if let Some(thread) = self.thread.lock().take() {
            let _ = thread.join();
        }
        if let Some(shim) = &self.inner.shim {
            let _ = std::fs::remove_dir_all(shim);
        }
    }
}

/// The listener `Term` reports its own responses through.
#[derive(Clone)]
struct Listener {
    events: Arc<Mutex<Vec<Event>>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        self.events.lock().push(event);
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

struct Inner {
    writer: Mutex<std::fs::File>,
    probe: Mutex<std::fs::File>,
    term: FairMutex<Term<Listener>>,
    events: Arc<Mutex<Vec<Event>>>,
    osc: Mutex<Osc133>,
    output: broadcast::Sender<OutputChunk>,
    state_tx: watch::Sender<State>,
    state_rx: watch::Receiver<State>,
    exit_tx: watch::Sender<bool>,
    exit_rx: watch::Receiver<bool>,
    seq: AtomicU32,
    /// The lifetime count of lines that entered the buffer, read cursors
    /// measured in it. Written under the term lock, so it is consistent with
    /// the grid a read sees.
    lines: AtomicU64,
    history: usize,
    child_pid: i32,
    closed: AtomicBool,
    alive: AtomicBool,
    cols: AtomicU16,
    rows: AtomicU16,
    shell: String,
    cwd: PathBuf,
    shim: Option<PathBuf>,
    last_output: Mutex<Instant>,
    shell_exit: Mutex<Option<Exit>>,
    title: Mutex<String>,
    resize_tx: std::sync::mpsc::Sender<(u16, u16)>,
}

impl Inner {
    fn write(&self, bytes: &[u8]) -> Result<(), Error> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(Error::Closed);
        }
        let mut writer = self.writer.lock();
        // A partial write has already put its prefix on the wire, so the
        // retry resumes at the offset rather than repeating it.
        let mut sent = 0usize;
        for _ in 0..WRITE_RETRIES {
            match writer.write(&bytes[sent..]) {
                Ok(written) => {
                    sent += written;
                    if sent == bytes.len() {
                        *self.last_output.lock() = Instant::now();
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(Error::Write(error.to_string())),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Err(Error::Write("the PTY did not accept the input".to_owned()))
    }

    fn echo(&self) -> bool {
        let probe = self.probe.lock();
        rustix::termios::tcgetattr(&*probe)
            .map_or(true, |termios| termios.local_modes.contains(rustix::termios::LocalModes::ECHO))
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(pid) = Pid::from_raw(self.child_pid) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }

    fn publish_state(&self) {
        let state = self.state();
        // **Only a real transition notifies.** Every read batch republishes,
        // and a watcher that announced each one would push a frame per chunk.
        let _ = self.state_tx.send_if_modified(|current| {
            if *current == state {
                return false;
            }
            *current = state;
            true
        });
    }

    /// **A dead shell is not running anything.** Its marks still hold the last
    /// command, so the classifier alone would report a running session whose
    /// last exit is stale; the shell's own exit is the honest answer.
    fn state(&self) -> State {
        if !self.alive.load(Ordering::Relaxed) {
            let exit =
                self.shell_exit.lock().map(|exit| Exit { code: exit.code, signal: exit.signal });
            return State {
                running: false,
                input_expected: None,
                exit,
                source: state::Source::Exact,
            };
        }
        let marks = self.osc.lock().marks();
        let echo = self.echo();
        let quiet = self.last_output.lock().elapsed() > QUIET;
        let last_line = {
            let term = self.term.lock();
            last_nonempty_line(&term)
        };
        state::classify(ClassifyInput { marks, echo, quiet, last_line: &last_line })
    }
}

/// The read loop: poll, read, tap, parse, respond, and notice the child.
fn read_loop(
    inner: &Arc<Inner>,
    mut pty: tty::Pty,
    resize_rx: &std::sync::mpsc::Receiver<(u16, u16)>,
) {
    let mut parser: Processor = Processor::new();
    let mut buf = vec![0u8; 65536];
    let timeout = Timespec { tv_sec: 0, tv_nsec: 100_000_000 };
    loop {
        if inner.closed.load(Ordering::Relaxed) {
            break;
        }
        while let Ok((cols, rows)) = resize_rx.try_recv() {
            let window =
                WindowSize { num_lines: rows, num_cols: cols, cell_width: 0, cell_height: 0 };
            pty.on_resize(window);
        }

        let mut fds = [PollFd::new(pty.file(), PollFlags::IN)];
        let mut ended = false;
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => {}
            Ok(_) => {
                let flags = fds[0].revents();
                if flags.contains(PollFlags::IN) {
                    read_ready(inner, &mut pty, &mut parser, &mut buf);
                }
                ended = poll_ended(flags);
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => {
                tracing::debug!(
                    event_name = "terminal_poll_failed",
                    pid = inner.child_pid,
                    %error,
                    "the terminal's poll failed; the terminal ends"
                );
                break;
            }
        }

        // **The child's own exit is read before a hangup ends the loop**, or
        // the shell's code would be lost to the same poll that reports it.
        if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
            let exit =
                status.map(|status| Exit { code: status.code(), signal: exit_signal(status) });
            *inner.shell_exit.lock() = exit;
            tracing::debug!(
                event_name = "terminal_exited",
                pid = inner.child_pid,
                exit = ?exit,
                "a terminal's shell exited"
            );
            break;
        }
        if ended {
            // **Nothing is left to read.** The slave side is gone, so polling
            // would spin on the hung-up fd; end the terminal and kill what
            // still ran on it.
            inner.close();
            // **The hangup is how a shell's exit first shows on Linux.** The
            // SIGCHLD can land a moment after the hangup, so the reap is
            // waited for and the shell's own code is kept; a child the kill
            // ended reports its signal instead.
            let deadline = Instant::now() + Duration::from_millis(200);
            loop {
                if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
                    let exit = status
                        .map(|status| Exit { code: status.code(), signal: exit_signal(status) });
                    *inner.shell_exit.lock() = exit;
                    tracing::debug!(
                        event_name = "terminal_exited",
                        pid = inner.child_pid,
                        exit = ?exit,
                        "a terminal's shell exited"
                    );
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            break;
        }
    }
    inner.alive.store(false, Ordering::Relaxed);
    inner.publish_state();
    let _ = inner.exit_tx.send(true);
}

/// Read everything the PTY has, tap it, parse it, and answer the terminal's
/// own queries.
fn read_ready(inner: &Arc<Inner>, pty: &mut tty::Pty, parser: &mut Processor, buf: &mut [u8]) {
    loop {
        match pty.reader().read(buf) {
            Ok(0) => break,
            Ok(n) => {
                let bytes = &buf[..n];
                inner.osc.lock().feed(bytes);
                // **Parse and sequence under one lock.** A reader that sees
                // sequence N can repaint and know every chunk below N is in
                // the screen; a sequence bumped before the parse would let a
                // chunk be skipped as "already drawn" while it was not.
                let seq = {
                    let mut term = inner.term.lock();
                    let mut appended = inner.lines.load(Ordering::Relaxed);
                    parser::advance(parser, &mut term, bytes, inner.history, &mut appended);
                    inner.lines.store(appended, Ordering::Relaxed);
                    inner.seq.fetch_add(1, Ordering::Relaxed)
                };
                let _ = inner.output.send(OutputChunk { seq, bytes: bytes.to_vec() });
                *inner.last_output.lock() = Instant::now();
                drain_responses(inner, pty);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => {
                tracing::debug!(
                    event_name = "terminal_read_failed",
                    pid = inner.child_pid,
                    %error,
                    "the terminal's read failed; the terminal ends"
                );
                break;
            }
        }
    }
    inner.publish_state();
}

/// Write the terminal's own responses (a device-attributes answer, a colour
/// report) back to the PTY, and keep the title the shell set.
fn drain_responses(inner: &Arc<Inner>, pty: &mut tty::Pty) {
    let events: Vec<Event> = {
        let mut events = inner.events.lock();
        std::mem::take(&mut *events)
    };
    for event in events {
        match event {
            Event::PtyWrite(text) => {
                let _ = pty.writer().write_all(text.as_bytes());
            }
            Event::Title(title) => *inner.title.lock() = title,
            Event::ResetTitle => inner.title.lock().clear(),
            _ => {}
        }
    }
}

/// Whether a poll's flags say the terminal is over: the slave side is gone or
/// the fd errored, so no byte can arrive again.
fn poll_ended(flags: PollFlags) -> bool {
    flags.intersects(PollFlags::HUP | PollFlags::ERR)
}

/// The last non-empty line on the visible screen.
fn last_nonempty_line<T: EventListener>(term: &Term<T>) -> String {
    let grid = term.grid();
    for row in (0..grid.screen_lines()).rev() {
        let line = alacritty_terminal::index::Line(i32::try_from(row).unwrap_or(i32::MAX));
        let text: String = (&grid[line]).into_iter().map(|cell| cell.c).collect();
        let trimmed = text.trim_end();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    String::new()
}

/// The signal a process died from, when it did.
fn exit_signal(status: std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(shell: &str) -> TerminalConfig {
        let mut env = HashMap::new();
        env.insert("HOME".to_owned(), std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned()));
        env.insert("PATH".to_owned(), std::env::var("PATH").unwrap_or_default());
        TerminalConfig {
            shell: shell.to_owned(),
            cwd: std::env::temp_dir(),
            env,
            cols: 80,
            rows: 24,
            scrollback_lines: 2000,
        }
    }

    /// **The whole loop, end to end.** A shell is spawned, a command typed,
    /// its output read off the screen, and the terminal closed cleanly.
    #[test]
    fn a_shell_runs_a_command_and_the_screen_carries_its_output() {
        let terminal = Terminal::spawn(&config("/bin/sh")).expect("a terminal");
        // Wait for the shell to be ready, then type a command.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !terminal.screen().text.trim().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        terminal.write_text("echo FORGE_TERMINAL_MARKER_$((6*7))\r").expect("a write");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut found = false;
        while Instant::now() < deadline {
            if terminal.screen().text.contains("FORGE_TERMINAL_MARKER_42") {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(found, "the command's output reached the screen: {:?}", terminal.screen().text);
        terminal.close();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && terminal.is_alive() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!terminal.is_alive(), "the shell ended");
    }

    /// The output stream carries the bytes the screen was built from, with a
    /// monotonic sequence.
    #[test]
    fn the_output_stream_carries_the_bytes_with_a_sequence() {
        let terminal = Terminal::spawn(&config("/bin/sh")).expect("a terminal");
        let mut output = terminal.subscribe();
        terminal.write_text("echo STREAM_MARKER\r").expect("a write");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match output.try_recv() {
                Ok(chunk) => {
                    seen.push(chunk.seq);
                    if String::from_utf8_lossy(&chunk.bytes).contains("STREAM_MARKER") {
                        break;
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        assert!(!seen.is_empty(), "the stream carried the shell's output");
        assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "the sequence is monotonic");
        terminal.close();
    }

    /// A resize reaches the PTY and the model, and the model reflows.
    #[test]
    fn a_resize_reaches_the_model() {
        let terminal = Terminal::spawn(&config("/bin/sh")).expect("a terminal");
        terminal.resize(100, 30).expect("a resize");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && terminal.cols() != 100 {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(terminal.cols(), 100);
        assert_eq!(terminal.rows(), 30);
        terminal.close();
    }

    /// Closing kills the terminal's process group, not just the PTY: a child
    /// that never reads the terminal is still gone.
    #[test]
    fn closing_a_terminal_kills_its_process_group() {
        let terminal = Terminal::spawn(&config("/bin/sh")).expect("a terminal");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && terminal.screen().text.trim().is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        terminal.write_text("echo PID=$$\r").expect("a write");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut pid = None;
        while Instant::now() < deadline && pid.is_none() {
            let screen = terminal.screen().text;
            pid = screen
                .split_whitespace()
                .filter_map(|word| word.strip_prefix("PID="))
                .find_map(|pid| pid.parse::<i32>().ok());
            if pid.is_none() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let Some(pid) = pid else {
            panic!("the shell printed its pid: {:?}", terminal.screen().text);
        };
        // `exec` keeps the pid and the process group, so the sleep stands in
        // for the shell without touching the terminal; the ignored HUP
        // outlives exec, so closing the PTY alone does not end it.
        terminal.write_text("trap \"\" HUP; exec sleep 300\r").expect("a write");
        std::thread::sleep(Duration::from_millis(200));
        terminal.close();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut gone = false;
        while Instant::now() < deadline && !gone {
            if let Some(pid) = Pid::from_raw(pid) {
                gone = rustix::process::test_kill_process(pid).is_err();
            }
            if !gone {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if !gone {
            // The PTY's own drop waits for the child, so a child this close
            // failed to reach would hang the assertion instead of failing it.
            if let Some(pid) = Pid::from_raw(pid) {
                let _ = rustix::process::kill_process(pid, Signal::KILL);
            }
        }
        assert!(gone, "the terminal's process group was killed");
    }

    /// **A cwd the PTY cannot enter is refused by name.** alacritty ignores
    /// the child's chdir failure, which would run the shell in forge's own
    /// launch directory.
    #[test]
    fn a_cwd_that_cannot_be_entered_is_refused() {
        let mut bad = config("/bin/sh");
        bad.cwd = PathBuf::from("/definitely/not/a/forge/terminal/cwd");
        let Err(error) = Terminal::spawn(&bad) else {
            panic!("the cwd is refused");
        };
        assert!(
            error.to_string().contains("/definitely/not/a/forge/terminal/cwd"),
            "the error names the path: {error}"
        );
    }

    /// **A dead shell is not running and reports its own exit**, not the last
    /// command's marks.
    #[test]
    fn a_dead_shell_reports_its_own_exit() {
        let terminal = Terminal::spawn(&config("/bin/sh")).expect("a terminal");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && terminal.screen().text.trim().is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        terminal.write_text("exit 3\r").expect("a write");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && terminal.is_alive() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let state = terminal.state();
        assert!(!state.running, "the shell is gone: {state:?}");
        assert!(state.input_expected.is_none(), "nothing is waiting for input: {state:?}");
        assert_eq!(state.exit, Some(Exit { code: Some(3), signal: None }), "{state:?}");
    }

    /// **A hung-up or errored PTY is over.** Polling one again would return
    /// the same flag forever, which was a full-CPU spin reporting the shell
    /// alive.
    #[test]
    fn a_hung_up_or_errored_pty_ends_the_loop() {
        assert!(poll_ended(PollFlags::HUP), "a hangup has nothing left to read");
        assert!(poll_ended(PollFlags::ERR), "an error is not a retry");
        assert!(!poll_ended(PollFlags::IN), "a readable terminal is not over");
    }

    /// A closed terminal refuses input rather than writing into a dead PTY.
    #[test]
    fn a_closed_terminal_refuses_input() {
        let terminal = Terminal::spawn(&config("/bin/sh")).expect("a terminal");
        terminal.close();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && terminal.is_alive() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(terminal.write_text("x"), Err(Error::Closed)));
    }
}

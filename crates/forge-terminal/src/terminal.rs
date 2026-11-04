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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
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
}

/// One terminal.
pub struct Terminal {
    inner: Arc<Inner>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Terminal {
    /// Spawn a shell on a fresh PTY.
    pub fn spawn(config: &TerminalConfig) -> Result<Self, Error> {
        let plan = shell::plan(&config.shell, &config.env);
        let options = Options {
            shell: Some(Shell::new(plan.program.clone(), plan.args.clone())),
            working_directory: Some(config.cwd.clone()),
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
            seq: AtomicU32::new(0),
            child_pid,
            closed: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            cols: AtomicU16::new(config.cols),
            rows: AtomicU16::new(config.rows),
            shell: config.shell.clone(),
            cwd: config.cwd.clone(),
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
        screen::read(&term, since, max_lines, max_bytes)
    }

    /// The visible screen as a VT stream, for a reattaching client.
    pub fn repaint(&self) -> Vec<u8> {
        let term = self.inner.term.lock();
        crate::repaint::repaint(&term)
    }

    /// What the terminal is doing.
    pub fn state(&self) -> State {
        let marks = self.inner.osc.lock().marks();
        let echo = self.inner.echo();
        let quiet = self.inner.last_output.lock().elapsed() > QUIET;
        let last_line = {
            let term = self.inner.term.lock();
            last_nonempty_line(&term)
        };
        state::classify(ClassifyInput { marks, echo, quiet, last_line: &last_line })
    }

    /// The output stream, from the next chunk.
    pub fn subscribe(&self) -> broadcast::Receiver<OutputChunk> {
        self.inner.output.subscribe()
    }

    /// The sequence the next output chunk carries. A repaint taken now
    /// reflects every chunk below it.
    pub fn seq(&self) -> u32 {
        self.inner.seq.load(Ordering::Relaxed)
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
        for _ in 0..WRITE_RETRIES {
            match writer.write(bytes) {
                Ok(written) if written == bytes.len() => {
                    *self.last_output.lock() = Instant::now();
                    return Ok(());
                }
                Ok(_) => {}
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
        let marks = self.osc.lock().marks();
        let echo = self.echo();
        let quiet = self.last_output.lock().elapsed() > QUIET;
        let last_line = {
            let term = self.term.lock();
            last_nonempty_line(&term)
        };
        let state = state::classify(ClassifyInput { marks, echo, quiet, last_line: &last_line });
        let _ = self.state_tx.send(state);
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
            let mut term = inner.term.lock();
            term.resize(Size { cols: usize::from(cols), rows: usize::from(rows) });
        }

        let mut fds = [PollFd::new(pty.file(), PollFlags::IN)];
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) => {}
            Ok(_) => {
                if fds[0].revents().contains(PollFlags::IN) {
                    read_ready(inner, &mut pty, &mut parser, &mut buf);
                }
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => break,
        }

        if let Some(ChildEvent::Exited(status)) = pty.next_child_event() {
            let exit =
                status.map(|status| Exit { code: status.code(), signal: exit_signal(status) });
            *inner.shell_exit.lock() = exit;
            inner.alive.store(false, Ordering::Relaxed);
            inner.publish_state();
            let _ = inner.exit_tx.send(true);
            tracing::debug!(
                event_name = "terminal_exited",
                pid = inner.child_pid,
                exit = ?exit,
                "a terminal's shell exited"
            );
            break;
        }
    }
    inner.alive.store(false, Ordering::Relaxed);
    inner.publish_state();
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
                let seq = inner.seq.fetch_add(1, Ordering::Relaxed);
                let _ = inner.output.send(OutputChunk { seq, bytes: bytes.to_vec() });
                {
                    let mut term = inner.term.lock();
                    parser.advance(&mut *term, bytes);
                }
                *inner.last_output.lock() = Instant::now();
                drain_responses(inner, pty);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
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

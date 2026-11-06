//! The narrow surface the terminal tools call: the seat's terminal, driven
//! by bytes and keys and read as text.
//!
//! Mirrors [`crate::mcp::browser::facade`]: the `Tool` impls hold an
//! `Arc<dyn TerminalFacade>` so their tests drive a `MockTerminalFacade`
//! instead of a live PTY.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_primitives::SessionSlot;
use forge_primitives::browser::{HandOffEnding, HandOffSource};
use forge_primitives::terminal::{TerminalExit, TerminalState};
use forge_terminal::Key;
use tokio::time::{Instant, timeout};

use crate::terminal::{TerminalEntry, wire_state};
use crate::workspace::Workspace;

/// What `terminal__open` asks for.
#[derive(Clone, Debug, Default)]
pub struct OpenRequest {
    /// A command typed after the shell is ready.
    pub command: Option<String>,
    /// The cwd the terminal is first spawned in.
    pub cwd: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

/// What `terminal__open` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenAnswer {
    pub id: u16,
    pub screen: String,
    pub cols: u16,
    pub rows: u16,
    pub shell: String,
    pub cwd: String,
}

/// What `terminal__read` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadAnswer {
    pub text: String,
    pub cursor: u64,
    pub cols: u16,
    pub rows: u16,
    pub state: TerminalState,
    pub truncated: bool,
}

/// What `terminal__wait` waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitUntil {
    /// The running command finished. Exact through OSC 133; the heuristic
    /// falls back to a quiet prompt and says so in `state.source`.
    Exit,
    /// The screen has been quiet for the quiet window.
    Quiet,
    /// The screen contains this text.
    Pattern(String),
}

/// What `terminal__wait` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitAnswer {
    pub matched: bool,
    pub text: String,
    pub cursor: u64,
    pub state: TerminalState,
}

/// What `terminal__run` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunAnswer {
    pub exit: Option<TerminalExit>,
    pub text: String,
    pub truncated: bool,
}

/// The agent's terminal, as one tool call sees it.
#[async_trait]
pub trait TerminalFacade: Send + Sync {
    async fn open(&self, seat: &SessionSlot, request: OpenRequest) -> Result<OpenAnswer, String>;

    async fn write(
        &self,
        seat: &SessionSlot,
        id: u16,
        text: Option<String>,
        keys: Vec<Key>,
    ) -> Result<TerminalState, String>;

    async fn read(
        &self,
        seat: &SessionSlot,
        id: u16,
        since: Option<u64>,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<ReadAnswer, String>;

    async fn wait(
        &self,
        seat: &SessionSlot,
        id: u16,
        until: WaitUntil,
        timeout_ms: u64,
    ) -> Result<WaitAnswer, String>;

    async fn resize(&self, seat: &SessionSlot, id: u16, cols: u16, rows: u16)
    -> Result<(), String>;

    async fn close(&self, seat: &SessionSlot, id: u16) -> Result<Option<TerminalExit>, String>;

    async fn hand_off(
        &self,
        seat: &SessionSlot,
        id: u16,
        reason: &str,
    ) -> Result<HandOffEnding, String>;

    async fn run(
        &self,
        seat: &SessionSlot,
        command: &str,
        cwd: Option<String>,
        timeout_ms: u64,
    ) -> Result<RunAnswer, String>;
}

/// Production impl: the workspace's registry.
pub struct ProdTerminalFacade {
    workspace: std::sync::Weak<Workspace>,
}

impl ProdTerminalFacade {
    pub fn from_workspace(workspace: &Arc<Workspace>) -> Arc<dyn TerminalFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }

    fn workspace(&self) -> Result<Arc<Workspace>, String> {
        self.workspace
            .upgrade()
            .ok_or_else(|| "the workspace went away before the call was staged".to_owned())
    }
}

#[async_trait]
impl TerminalFacade for ProdTerminalFacade {
    async fn open(&self, seat: &SessionSlot, request: OpenRequest) -> Result<OpenAnswer, String> {
        let workspace = self.workspace()?;
        // **The screen and the PTY are the person's while a hand-off is
        // parked.** A parallel call or a subagent must not read their typing
        // or type into their prompt.
        refuse_when_held(&workspace, seat)?;
        let entry = match request.cwd.as_deref() {
            Some(cwd) if !cwd.trim().is_empty() => {
                // A cwd names where the terminal is spawned, and a terminal is
                // per seat: a different cwd on an open terminal would need a
                // second one, and a seat has one.
                if workspace.terminal_if_any(seat).is_some() {
                    workspace.terminal_for(seat)?
                } else {
                    workspace.spawn_terminal_at(seat, Some(PathBuf::from(cwd)))?
                }
            }
            _ => workspace.terminal_for(seat)?,
        };
        if let (Some(cols), Some(rows)) = (request.cols, request.rows) {
            entry.terminal.resize(cols, rows).map_err(|error| error.to_string())?;
        }
        if let Some(command) = request.command.filter(|command| !command.trim().is_empty()) {
            wait_ready(&entry).await;
            entry.terminal.write_text(&command).map_err(|error| error.to_string())?;
            entry.terminal.write_keys(&[Key::Enter]).map_err(|error| error.to_string())?;
        }
        let screen = entry.terminal.screen();
        Ok(OpenAnswer {
            id: entry.id,
            screen: screen.text,
            cols: screen.cols,
            rows: screen.rows,
            shell: entry.terminal.shell().to_owned(),
            cwd: entry.terminal.cwd().display().to_string(),
        })
    }

    async fn write(
        &self,
        seat: &SessionSlot,
        id: u16,
        text: Option<String>,
        keys: Vec<Key>,
    ) -> Result<TerminalState, String> {
        let workspace = self.workspace()?;
        refuse_when_held(&workspace, seat)?;
        let entry = workspace.terminal_by_id(seat, id)?;
        // **An agent's write takes the keyboard from the person.** The holder
        // hears it through the control stream, and the write lands rather than
        // being refused: a tool call that cannot act is a dead end for the
        // session.
        entry.clear_control();
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            entry.terminal.write_text(&text).map_err(|error| error.to_string())?;
        }
        if !keys.is_empty() {
            entry.terminal.write_keys(&keys).map_err(|error| error.to_string())?;
        }
        Ok(wire_state(entry.terminal.state()))
    }

    async fn read(
        &self,
        seat: &SessionSlot,
        id: u16,
        since: Option<u64>,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<ReadAnswer, String> {
        let workspace = self.workspace()?;
        refuse_when_held(&workspace, seat)?;
        let entry = workspace.terminal_by_id(seat, id)?;
        let read = entry.terminal.read(since, max_lines, max_bytes);
        Ok(ReadAnswer {
            text: read.text,
            cursor: read.cursor,
            cols: read.cols,
            rows: read.rows,
            state: wire_state(entry.terminal.state()),
            truncated: read.truncated,
        })
    }

    async fn wait(
        &self,
        seat: &SessionSlot,
        id: u16,
        until: WaitUntil,
        timeout_ms: u64,
    ) -> Result<WaitAnswer, String> {
        let workspace = self.workspace()?;
        refuse_when_held(&workspace, seat)?;
        let entry = workspace.terminal_by_id(seat, id)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        // **The entry check is not enough**: the person can take the screen
        // while this polls it, and a wait that kept reading would return
        // what they type.
        let blind = Some((workspace.as_ref(), seat));
        let matched = match until {
            WaitUntil::Exit => wait_exit(&entry, deadline, blind).await?,
            WaitUntil::Quiet => wait_quiet(&entry, deadline, blind).await?,
            WaitUntil::Pattern(pattern) => wait_pattern(&entry, &pattern, deadline, blind).await?,
        };
        // **The last gap is a gap too**: a park that lands after the loop's
        // final check must not get this read.
        refuse_when_held(&workspace, seat)?;
        let read = entry.terminal.read(None, 200, 32 * 1024);
        Ok(WaitAnswer {
            matched,
            text: read.text,
            cursor: read.cursor,
            state: wire_state(entry.terminal.state()),
        })
    }

    async fn resize(
        &self,
        seat: &SessionSlot,
        id: u16,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        let workspace = self.workspace()?;
        refuse_when_held(&workspace, seat)?;
        let entry = workspace.terminal_by_id(seat, id)?;
        entry.terminal.resize(cols, rows).map_err(|error| error.to_string())
    }

    async fn close(&self, seat: &SessionSlot, id: u16) -> Result<Option<TerminalExit>, String> {
        let workspace = self.workspace()?;
        refuse_when_held(&workspace, seat)?;
        let entry = workspace.terminal_by_id(seat, id)?;
        entry.terminal.close();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && entry.terminal.is_alive() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if entry.terminal.is_alive() {
            // **A close that did not take is said, not reported as ok.** The
            // entry stays so the next call can try again or read the screen.
            return Err("the terminal's shell did not exit within 2 s; it is still open".to_owned());
        }
        let exit = entry
            .terminal
            .shell_exit()
            .map(|exit| TerminalExit { code: exit.code, signal: exit.signal });
        workspace.close_terminal(seat);
        Ok(exit)
    }

    async fn hand_off(
        &self,
        seat: &SessionSlot,
        id: u16,
        reason: &str,
    ) -> Result<HandOffEnding, String> {
        let workspace = self.workspace()?;
        if workspace.terminal_held(seat) {
            return Err("a hand-off is already parked on this terminal".to_owned());
        }
        // **The park takes the keyboard** (under its own lock) from a person
        // who holds it; the ending hands the agent the keyboard back.
        let (parked, answer) = workspace.register_terminal_hand_off(seat, id, reason)?;
        let guard =
            ResolveOnDrop { workspace: Arc::clone(&workspace), id: parked, caller: seat.clone() };
        let ending = answer
            .await
            .map_err(|_| "no attached client can show the terminal hand-off".to_owned())?;
        drop(guard);
        Ok(ending)
    }

    async fn run(
        &self,
        seat: &SessionSlot,
        command: &str,
        cwd: Option<String>,
        timeout_ms: u64,
    ) -> Result<RunAnswer, String> {
        let workspace = self.workspace()?;
        refuse_when_held(&workspace, seat)?;
        let entry = workspace.spawn_terminal_for_run(
            seat,
            cwd.filter(|cwd| !cwd.trim().is_empty()).map(PathBuf::from),
        )?;
        // **Guaranteed close on every path**: the guard ends the process group
        // even when a write or a read fails, so no terminal leaks.
        let guard = CloseOnDrop { terminal: Arc::clone(&entry.terminal) };
        wait_ready(&entry).await;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        entry.terminal.write_text(command).map_err(|error| error.to_string())?;
        entry.terminal.write_keys(&[Key::Enter]).map_err(|error| error.to_string())?;
        let matched = wait_exit(&entry, deadline, None).await?;
        let read = entry.terminal.read(None, 500, 64 * 1024);
        let exit = if matched {
            entry
                .terminal
                .state()
                .exit
                .map(|exit| TerminalExit { code: exit.code, signal: exit.signal })
        } else {
            // The timeout killed the process group; the signal is the record.
            entry.terminal.close();
            Some(TerminalExit { code: None, signal: Some(9) })
        };
        drop(guard);
        Ok(RunAnswer { exit, text: read.text, truncated: read.truncated })
    }
}

/// **The screen and the PTY belong to the person while a hand-off is
/// parked.** Every tool that touches either is refused, so a parallel call or
/// a subagent cannot read their typing or write into their prompt.
///
/// **A person's own take-control does not refuse anything**: it is theirs to
/// hold, and a tool that acts takes it back (see `write` and `hand_off`).
fn refuse_when_held(workspace: &Workspace, seat: &SessionSlot) -> Result<(), String> {
    if workspace.terminal_held(seat) {
        return Err(held_error());
    }
    Ok(())
}

fn held_error() -> String {
    "a hand-off is parked on this terminal: the person owns its screen and input until they answer"
        .to_owned()
}

/// The seat a wait is polling, while it may be taken by the person - through
/// a parked hand-off or through a client's take-control. A wait that sees it
/// owned stops rather than reading what they type.
type Blind<'a> = Option<(&'a Workspace, &'a SessionSlot)>;

fn refusal_now(blind: Blind<'_>) -> Option<String> {
    let (workspace, seat) = blind?;
    if workspace.terminal_held(seat) {
        return Some(held_error());
    }
    None
}

/// Resolves a parked hand-off when the awaiting handler goes - a session that
/// died mid-wait, never a decision.
struct ResolveOnDrop {
    workspace: Arc<Workspace>,
    id: uuid::Uuid,
    caller: SessionSlot,
}

impl Drop for ResolveOnDrop {
    fn drop(&mut self) {
        self.workspace.resolve_terminal_hand_off(
            self.id,
            &self.caller,
            forge_primitives::terminal::TerminalHandOffAnswer {
                ending: HandOffEnding::Abandoned,
                source: HandOffSource::Abandoned,
                client: None,
            },
        );
    }
}

/// Closes a terminal when the run's work is over, whatever path it took.
struct CloseOnDrop {
    terminal: Arc<forge_terminal::Terminal>,
}

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.terminal.close();
    }
}

/// Wait until the shell has drawn something, so a typed command reaches a
/// ready line editor rather than being buffered before its prompt exists.
async fn wait_ready(entry: &TerminalEntry) {
    let mut output = entry.terminal.subscribe();
    let deadline = Instant::now() + Duration::from_millis(1200);
    loop {
        if !entry.terminal.screen().text.trim().is_empty() {
            return;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let _ = timeout(left.min(Duration::from_millis(50)), output.recv()).await;
    }
}

/// Wait for a command to finish: exact through the marks, the heuristic's
/// quiet prompt otherwise, and the shell's own exit for either.
async fn wait_exit(
    entry: &TerminalEntry,
    deadline: Instant,
    blind: Blind<'_>,
) -> Result<bool, String> {
    let mut states = entry.terminal.state_stream();
    loop {
        if let Some(why) = refusal_now(blind) {
            return Err(why);
        }
        if !entry.terminal.is_alive() {
            return Ok(true);
        }
        let state = entry.terminal.state();
        let finished = match state.source {
            forge_terminal::Source::Exact => !state.running && state.exit.is_some(),
            forge_terminal::Source::Heuristic => {
                !state.running
                    && state
                        .input_expected
                        .as_ref()
                        .is_some_and(|expected| expected.kind == forge_terminal::InputKind::Prompt)
            }
        };
        if finished {
            return Ok(true);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let _ = timeout(left.min(Duration::from_millis(50)), states.changed()).await;
    }
}

/// Wait until no output has arrived for the quiet window.
async fn wait_quiet(
    entry: &TerminalEntry,
    deadline: Instant,
    blind: Blind<'_>,
) -> Result<bool, String> {
    let mut output = entry.terminal.subscribe();
    loop {
        if let Some(why) = refusal_now(blind) {
            return Err(why);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let window = Duration::from_millis(300);
        match timeout(window.min(left), output.recv()).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return Ok(true),
            Err(_) if left >= window => return Ok(true),
            Err(_) => return Ok(false),
        }
    }
}

/// Wait until the screen contains `pattern`.
async fn wait_pattern(
    entry: &TerminalEntry,
    pattern: &str,
    deadline: Instant,
    blind: Blind<'_>,
) -> Result<bool, String> {
    let mut output = entry.terminal.subscribe();
    loop {
        if let Some(why) = refusal_now(blind) {
            return Err(why);
        }
        if entry.terminal.screen().text.contains(pattern) {
            return Ok(true);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let _ = timeout(left.min(Duration::from_millis(100)), output.recv()).await;
    }
}

/// One captured write: the terminal, the text and the keys.
#[cfg(test)]
type WriteCall = (u16, Option<String>, Vec<Key>);

/// One captured read: the terminal, the cursor and the two bounds.
#[cfg(test)]
type ReadCall = (u16, Option<u64>, usize, usize);

/// Mock for the tools' unit tests: captures what a call carried and answers
/// with a preloaded outcome.
#[cfg(test)]
pub struct MockTerminalFacade {
    pub opens: parking_lot::Mutex<Vec<OpenRequest>>,
    pub writes: parking_lot::Mutex<Vec<WriteCall>>,
    pub reads: parking_lot::Mutex<Vec<ReadCall>>,
    pub waits: parking_lot::Mutex<Vec<(u16, WaitUntil, u64)>>,
    pub resizes: parking_lot::Mutex<Vec<(u16, u16, u16)>>,
    pub closes: parking_lot::Mutex<Vec<u16>>,
    pub hand_offs: parking_lot::Mutex<Vec<(u16, String)>>,
    pub runs: parking_lot::Mutex<Vec<(String, Option<String>, u64)>>,
    pub seats: parking_lot::Mutex<Vec<SessionSlot>>,
    pub answer: parking_lot::Mutex<Result<OpenAnswer, String>>,
    pub state: parking_lot::Mutex<Result<TerminalState, String>>,
    pub read_answer: parking_lot::Mutex<Result<ReadAnswer, String>>,
    pub wait_answer: parking_lot::Mutex<Result<WaitAnswer, String>>,
    pub close_answer: parking_lot::Mutex<Result<Option<TerminalExit>, String>>,
    pub hand_off_answer: parking_lot::Mutex<Result<HandOffEnding, String>>,
    pub run_answer: parking_lot::Mutex<Result<RunAnswer, String>>,
}

#[cfg(test)]
impl MockTerminalFacade {
    pub fn new() -> Self {
        Self {
            opens: parking_lot::Mutex::new(Vec::new()),
            writes: parking_lot::Mutex::new(Vec::new()),
            reads: parking_lot::Mutex::new(Vec::new()),
            waits: parking_lot::Mutex::new(Vec::new()),
            resizes: parking_lot::Mutex::new(Vec::new()),
            closes: parking_lot::Mutex::new(Vec::new()),
            hand_offs: parking_lot::Mutex::new(Vec::new()),
            runs: parking_lot::Mutex::new(Vec::new()),
            seats: parking_lot::Mutex::new(Vec::new()),
            answer: parking_lot::Mutex::new(Ok(OpenAnswer {
                id: 1,
                screen: "$ ".to_owned(),
                cols: 120,
                rows: 30,
                shell: "/bin/zsh".to_owned(),
                cwd: "/tmp".to_owned(),
            })),
            state: parking_lot::Mutex::new(Ok(empty_state())),
            read_answer: parking_lot::Mutex::new(Ok(ReadAnswer {
                text: "hello".to_owned(),
                cursor: 1,
                cols: 120,
                rows: 30,
                state: empty_state(),
                truncated: false,
            })),
            wait_answer: parking_lot::Mutex::new(Ok(WaitAnswer {
                matched: true,
                text: "done".to_owned(),
                cursor: 2,
                state: empty_state(),
            })),
            close_answer: parking_lot::Mutex::new(Ok(Some(TerminalExit {
                code: Some(0),
                signal: None,
            }))),
            hand_off_answer: parking_lot::Mutex::new(Ok(HandOffEnding::Done)),
            run_answer: parking_lot::Mutex::new(Ok(RunAnswer {
                exit: Some(TerminalExit { code: Some(0), signal: None }),
                text: "ran".to_owned(),
                truncated: false,
            })),
        }
    }

    pub fn into_arc(self) -> Arc<dyn TerminalFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
pub(crate) fn empty_state() -> TerminalState {
    TerminalState {
        running: false,
        input_expected: None,
        exit: None,
        source: forge_primitives::terminal::TerminalSource::Heuristic,
    }
}

#[cfg(test)]
#[async_trait]
impl TerminalFacade for MockTerminalFacade {
    async fn open(&self, seat: &SessionSlot, request: OpenRequest) -> Result<OpenAnswer, String> {
        self.seats.lock().push(seat.clone());
        self.opens.lock().push(request);
        self.answer.lock().clone()
    }

    async fn write(
        &self,
        seat: &SessionSlot,
        id: u16,
        text: Option<String>,
        keys: Vec<Key>,
    ) -> Result<TerminalState, String> {
        self.seats.lock().push(seat.clone());
        self.writes.lock().push((id, text, keys));
        self.state.lock().clone()
    }

    async fn read(
        &self,
        seat: &SessionSlot,
        id: u16,
        since: Option<u64>,
        max_lines: usize,
        max_bytes: usize,
    ) -> Result<ReadAnswer, String> {
        self.seats.lock().push(seat.clone());
        self.reads.lock().push((id, since, max_lines, max_bytes));
        self.read_answer.lock().clone()
    }

    async fn wait(
        &self,
        seat: &SessionSlot,
        id: u16,
        until: WaitUntil,
        timeout_ms: u64,
    ) -> Result<WaitAnswer, String> {
        self.seats.lock().push(seat.clone());
        self.waits.lock().push((id, until, timeout_ms));
        self.wait_answer.lock().clone()
    }

    async fn resize(
        &self,
        seat: &SessionSlot,
        id: u16,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        self.seats.lock().push(seat.clone());
        self.resizes.lock().push((id, cols, rows));
        Ok(())
    }

    async fn close(&self, seat: &SessionSlot, id: u16) -> Result<Option<TerminalExit>, String> {
        self.seats.lock().push(seat.clone());
        self.closes.lock().push(id);
        self.close_answer.lock().clone()
    }

    async fn hand_off(
        &self,
        seat: &SessionSlot,
        id: u16,
        reason: &str,
    ) -> Result<HandOffEnding, String> {
        self.seats.lock().push(seat.clone());
        self.hand_offs.lock().push((id, reason.to_owned()));
        self.hand_off_answer.lock().clone()
    }

    async fn run(
        &self,
        seat: &SessionSlot,
        command: &str,
        cwd: Option<String>,
        timeout_ms: u64,
    ) -> Result<RunAnswer, String> {
        self.seats.lock().push(seat.clone());
        self.runs.lock().push((command.to_owned(), cwd, timeout_ms));
        self.run_answer.lock().clone()
    }
}

//! The per-seat terminals: the registry, the hand-off that parks on the
//! person, and the idle clock.
//!
//! **The terminal belongs to the seat, not the occupant.** It is keyed by the
//! slot and created lazily on first use; `/new`, `/resume` and a handoff
//! leave the PTY, its shell and its scrollback exactly where they were. It
//! lives as long as the seat: a release ends the seat and its terminal, and
//! an explicit close, the shell exiting or forge exiting also ends it.
//!
//! **No approval gate.** Bash through the CLI already reaches the shell here,
//! so the terminal adds no capability a session does not have. The one thing
//! that is gated is the person's own input, through the hand-off or through
//! their own take-control of the raised surface.
//!
//! **Lock order: `terminal_handoffs` before `terminals`.** Every path that
//! needs both takes them in that order, so a claim, an answer, a typed byte
//! and the idle sweep can never deadlock against each other. A path that
//! needed the other order has to drop one guard before taking the other.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use forge_primitives::SessionSlot;
use forge_primitives::browser::HandOffEnding;
use forge_primitives::terminal::{
    TerminalExit, TerminalHandOff, TerminalHandOffAnswer, TerminalInputExpected, TerminalInputKind,
    TerminalSource, TerminalState, TerminalSummary,
};
use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;

/// One chunk of a terminal's output, re-exported so a server half can name
/// the type its stream carries without depending on the crate directly.
pub use forge_terminal::OutputChunk;

/// The size a terminal opens at before a client's attach resizes it.
pub(crate) const DEFAULT_COLS: u16 = 120;
pub(crate) const DEFAULT_ROWS: u16 = 30;

/// How often the idle clock looks at the registry.
const CLOCK_TICK: Duration = Duration::from_secs(30);

/// One seat's terminal, as the registry holds it.
pub struct TerminalEntry {
    /// The id the binary frames carry.
    pub id: u16,
    pub terminal: Arc<forge_terminal::Terminal>,
    /// When an agent call or a viewer last touched it, for the idle clock.
    pub last_used: Mutex<Instant>,
    /// How many clients are attached.
    pub attached: AtomicUsize,
    /// The connection holding the person's control, or 0 for the agent.
    /// **One at a time**, like the browser's controller: another client can
    /// take it over, and the holder always hears it lost the role.
    controller: AtomicU64,
    /// The holder every attached viewer follows, so a bar can say who has it.
    control: tokio::sync::watch::Sender<Option<u64>>,
}

impl TerminalEntry {
    fn touch(&self) {
        *self.last_used.lock() = Instant::now();
    }

    /// The person's connection holding the terminal, when one does.
    pub fn controller(&self) -> Option<u64> {
        match self.controller.load(Ordering::Relaxed) {
            0 => None,
            id => Some(id),
        }
    }

    /// Give one connection the person's control, telling every attached
    /// viewer who holds it. A take always lands: like the browser's role, a
    /// second client taking over is how the first one loses it.
    pub fn take_control(&self, viewer: u64) {
        if viewer == 0 {
            return;
        }
        self.controller.store(viewer, Ordering::Relaxed);
        self.touch();
        let _ = self.control.send(Some(viewer));
    }

    /// Claim the control only where nobody holds it, telling every attached
    /// viewer when the claim lands. **The hand-off's door**: the first client
    /// to type during a parked hand-off owns the keyboard, and a client that
    /// loses the race is a viewer rather than a second hand on the secret.
    pub fn claim_control(&self, viewer: u64) -> bool {
        if viewer == 0 {
            return false;
        }
        if self
            .controller
            .compare_exchange(0, viewer, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        self.touch();
        let _ = self.control.send(Some(viewer));
        true
    }

    /// Give the person's control back to the agent whatever connection holds
    /// it, which is what the end of a hand-off owes: the ask parked the
    /// keyboard, and the ending takes it back.
    pub fn clear_control(&self) {
        if self.controller.swap(0, Ordering::Relaxed) != 0 {
            self.touch();
            let _ = self.control.send(None);
        }
    }

    /// Give control back when its holder releases it; another connection's
    /// release is not this one's to take.
    pub fn release_control(&self, viewer: u64) -> bool {
        if self.controller() != Some(viewer) {
            return false;
        }
        self.controller.store(0, Ordering::Relaxed);
        self.touch();
        let _ = self.control.send(None);
        true
    }

    /// Follow the controller: the latest holder, and every change.
    pub fn control_stream(&self) -> tokio::sync::watch::Receiver<Option<u64>> {
        self.control.subscribe()
    }

    fn idle_for(&self) -> Duration {
        self.last_used.lock().elapsed()
    }

    /// The summary the strip and the session record draw.
    pub fn summary(&self, slot: &SessionSlot, held: bool) -> TerminalSummary {
        TerminalSummary {
            id: self.id,
            seat: slot.clone(),
            shell: self.terminal.shell().to_owned(),
            cwd: self.terminal.cwd().display().to_string(),
            cols: self.terminal.cols(),
            rows: self.terminal.rows(),
            alive: self.terminal.is_alive(),
            state: wire_state(self.terminal.state()),
            title: self.terminal.title(),
            held,
        }
    }
}

/// The wire state, from the crate's own.
pub(crate) fn wire_state(state: forge_terminal::State) -> TerminalState {
    TerminalState {
        running: state.running,
        input_expected: state.input_expected.map(|expected| TerminalInputExpected {
            kind: match expected.kind {
                forge_terminal::InputKind::Password => TerminalInputKind::Password,
                forge_terminal::InputKind::Confirm => TerminalInputKind::Confirm,
                forge_terminal::InputKind::Prompt => TerminalInputKind::Prompt,
            },
            evidence: expected.evidence,
        }),
        exit: state.exit.map(|exit| TerminalExit { code: exit.code, signal: exit.signal }),
        source: match state.source {
            forge_terminal::Source::Exact => TerminalSource::Exact,
            forge_terminal::Source::Heuristic => TerminalSource::Heuristic,
        },
    }
}

impl Workspace {
    /// The seat's terminal, spawned on first use.
    ///
    /// **One per seat, and the slot is the key.** A `/new` or `/resume`
    /// swapping the occupant never reaches here, which is what makes the PTY
    /// outlive it.
    pub(crate) fn terminal_for(
        self: &Arc<Self>,
        slot: &SessionSlot,
    ) -> Result<Arc<TerminalEntry>, String> {
        self.spawn_terminal_at(slot, None)
    }

    /// The seat's terminal, spawned in `cwd` the first time; an existing
    /// terminal is returned as it is.
    ///
    /// **A shell that exited without a close is replaced**, so the seat's
    /// next open starts a fresh shell rather than handing back a dead PTY.
    pub(crate) fn spawn_terminal_at(
        self: &Arc<Self>,
        slot: &SessionSlot,
        cwd: Option<PathBuf>,
    ) -> Result<Arc<TerminalEntry>, String> {
        let mut terminals = self.terminals.lock();
        if let Some(entry) = terminals.get(slot).cloned() {
            if entry.terminal.is_alive() {
                entry.touch();
                return Ok(entry);
            }
            terminals.remove(slot);
        }
        let entry = self.spawn_terminal(slot, cwd)?;
        terminals.insert(slot.clone(), Arc::clone(&entry));
        drop(terminals);
        self.ensure_terminal_clock();
        self.watch_terminal_exit(slot, &entry);
        self.watch_terminal_state(slot, &entry);
        self.announce_terminal(slot, Some(entry.summary(slot, false)));
        Ok(entry)
    }

    /// The seat's terminal, spawned on first use. **The person's door**: a
    /// view can open one on a seat no agent has touched.
    ///
    /// **The door follows the seat read.** A declared seat answers a read
    /// before anything starts in it (its directory resolves from the
    /// declaration), so its door opens and the terminal it starts appears in
    /// the record. A seat with no working tree - a worker never spawned, or a
    /// slot naming no project - has its read refused, and the door refuses
    /// with the same sentence rather than starting a shell nobody can draw.
    pub fn open_terminal(
        self: &Arc<Self>,
        slot: &SessionSlot,
    ) -> Result<Arc<TerminalEntry>, String> {
        if self.cwd_for_session(slot).is_none() {
            // **The seat read's own sentence**: a door that refused with
            // different words would be a second answer to one fact.
            return Err(format!("forge holds no session for {}", slot.display()));
        }
        self.terminal_for(slot)
    }

    /// A fresh terminal for `terminal__run`, not registered: the command's
    /// own terminal, closed when the call returns.
    pub(crate) fn spawn_terminal_for_run(
        &self,
        slot: &SessionSlot,
        cwd: Option<PathBuf>,
    ) -> Result<Arc<TerminalEntry>, String> {
        self.spawn_terminal(slot, cwd)
    }

    /// The seat's terminal, when it has one.
    pub fn terminal_if_any(&self, slot: &SessionSlot) -> Option<Arc<TerminalEntry>> {
        self.terminals.lock().get(slot).cloned()
    }

    /// The seat's terminal, refused unless `id` names it.
    pub fn terminal_by_id(
        &self,
        slot: &SessionSlot,
        id: u16,
    ) -> Result<Arc<TerminalEntry>, String> {
        let Some(entry) = self.terminal_if_any(slot) else {
            return Err(
                "this seat has no terminal open; run terminal__open first (the id it returns is \
                 what this call takes)"
                    .to_owned(),
            );
        };
        if entry.id != id {
            return Err(format!("terminal {id} is not this seat's; its terminal is {}", entry.id));
        }
        entry.touch();
        Ok(entry)
    }

    /// Whether a terminal hand-off is parked on the seat.
    pub fn terminal_held(&self, slot: &SessionSlot) -> bool {
        self.terminal_handoffs.lock().values().any(|(owner, _, _)| owner == slot)
    }

    /// Whether the person holds the seat's terminal through a client's
    /// take-control, rather than through an agent's hand-off.
    pub fn terminal_controlled(&self, slot: &SessionSlot) -> bool {
        self.terminal_if_any(slot).is_some_and(|entry| entry.controller().is_some())
    }

    /// The hand-off door's keystrokes: the first client to type claims the
    /// keyboard, and the claim and the write are one step under the park's
    /// own lock.
    ///
    /// **A byte typed as the ask ended is refused**, so nothing typed during
    /// the park lands after the agent has its keyboard back, and a client
    /// that loses the claim is a viewer rather than a second hand on the
    /// secret.
    pub fn write_handoff_keyboard(
        &self,
        slot: &SessionSlot,
        id: u16,
        viewer: u64,
        bytes: &[u8],
    ) -> Result<(), String> {
        let parked = self.terminal_handoffs.lock();
        if !parked.values().any(|(owner, _, _)| owner == slot) {
            return Err("the terminal hand-off has ended".to_owned());
        }
        let entry = self.terminal_by_id(slot, id)?;
        if entry.controller() != Some(viewer) && !entry.claim_control(viewer) {
            return Err("another client holds the keyboard".to_owned());
        }
        entry.terminal.write_bytes(bytes).map_err(|error| error.to_string())
    }

    /// The person's own take, refused while a hand-off holds the keyboard.
    ///
    /// **Checked under the hand-off lock**: a take racing a park either lands
    /// before the park exists or is refused, never half of each.
    pub fn take_terminal_control(&self, slot: &SessionSlot, viewer: u64) -> Result<(), String> {
        let parked = self.terminal_handoffs.lock();
        if parked.values().any(|(owner, _, _)| owner == slot) {
            return Err("a terminal hand-off holds the keyboard; use the hand-off card".to_owned());
        }
        if let Some(entry) = self.terminal_if_any(slot) {
            entry.take_control(viewer);
        }
        Ok(())
    }

    /// Close the seat's terminal, if it has one.
    pub fn close_terminal(&self, slot: &SessionSlot) {
        let removed = self.terminals.lock().remove(slot);
        if let Some(entry) = removed {
            entry.terminal.close();
            self.announce_terminal(slot, None);
        }
        self.abandon_terminal_hand_offs(slot);
    }

    /// Close every terminal an idle clock may close: untouched by any agent
    /// call, unattached, and unheld by a hand-off.
    pub(crate) fn sweep_idle_terminals(&self) {
        let idle_minutes = self.config_arc().terminal.idle_minutes;
        if idle_minutes == 0 {
            return;
        }
        let idle = Duration::from_secs(idle_minutes.saturating_mul(60));
        // **The idle set is taken under `terminals` alone.** `terminal_held`
        // takes `terminal_handoffs`, and the module's order is hand-offs
        // before terminals; holding `terminals` across the held check would
        // be the one path taking them the other way, which deadlocks against
        // a take, an answer or a typed byte.
        let idle_slots: Vec<SessionSlot> = self
            .terminals
            .lock()
            .iter()
            .filter(|(_, entry)| {
                entry.attached.load(Ordering::Relaxed) == 0 && entry.idle_for() >= idle
            })
            .map(|(slot, _)| slot.clone())
            .collect();
        let victims: Vec<SessionSlot> =
            idle_slots.into_iter().filter(|slot| !self.terminal_held(slot)).collect();
        for slot in victims {
            tracing::debug!(
                event_name = "terminal_idle_closed",
                slot = %slot.display(),
                idle_minutes,
                "an idle terminal was closed"
            );
            self.close_terminal(&slot);
        }
    }

    /// Tell the views a terminal opened, changed or closed.
    pub(crate) fn announce_terminal(&self, slot: &SessionSlot, terminal: Option<TerminalSummary>) {
        let _ = self
            .update_sender()
            .send(SessionUpdate::TerminalChanged { key: slot.clone(), terminal });
    }

    /// Hold a terminal hand-off for the person at a client, and hand back its
    /// id plus the receiver the caller awaits.
    ///
    /// **No timeout rides the wait.** The ask sits until an answer or the
    /// caller's own drop. The one case answered up front is a stream nobody
    /// can answer on, which fails closed rather than parking the session on a
    /// prompt no view can draw.
    pub(crate) fn register_terminal_hand_off(
        &self,
        caller: &SessionSlot,
        terminal: u16,
        reason: &str,
    ) -> Result<(uuid::Uuid, oneshot::Receiver<HandOffEnding>), String> {
        let entry = self.terminal_by_id(caller, terminal)?;
        let state = wire_state(entry.terminal.state());
        // The card describes the terminal when it asked; a program that turns
        // ECHO back on later does not rewrite the reason the person was shown.
        let secret = state
            .input_expected
            .as_ref()
            .is_some_and(|expected| expected.kind == TerminalInputKind::Password);
        let (sender, receiver) = oneshot::channel();
        let id = uuid::Uuid::new_v4();
        let handoff = TerminalHandOff { id, reason: reason.to_owned(), terminal, secret };
        self.terminal_handoffs.lock().insert(id, (caller.clone(), handoff.clone(), sender));
        // **The strip says the input is the person's** while the ask waits,
        // so a row drawn beside the dock does not read as idle.
        self.announce_terminal(caller, Some(entry.summary(caller, true)));
        let answerable = self
            .update_sender()
            .send_answering(SessionUpdate::TerminalHandOffPending { key: caller.clone(), handoff });
        if !answerable {
            self.terminal_handoffs.lock().remove(&id);
            // **The strip must not keep saying the input is the person's**
            // for a park that was withdrawn before it ever showed.
            self.announce_terminal(caller, Some(entry.summary(caller, false)));
            let (_ignored_sender, dead_receiver) = oneshot::channel();
            return Ok((id, dead_receiver));
        }
        self.note_ask(caller);
        Ok((id, receiver))
    }

    /// Whether a hand-off with this id is still registered to `caller`, without
    /// removing it.
    pub(crate) fn terminal_handoff_waiting(&self, id: uuid::Uuid, caller: &SessionSlot) -> bool {
        self.terminal_handoffs.lock().get(&id).is_some_and(|(owner, _, _)| owner == caller)
    }

    /// Remove a parked hand-off, answer its waiter, and tell every view it is
    /// gone. `false` when the id is not this caller's to resolve.
    pub(crate) fn resolve_terminal_hand_off(
        &self,
        id: uuid::Uuid,
        caller: &SessionSlot,
        answer: TerminalHandOffAnswer,
    ) -> bool {
        let mut parked = self.terminal_handoffs.lock();
        if !parked.get(&id).is_some_and(|(owner, _, _)| owner == caller) {
            return false;
        }
        let Some((owner, handoff, sender)) = parked.remove(&id) else { return false };
        // **The ending takes the keyboard back while the park is still under
        // the lock**, so a claim that saw the park as live cannot land after
        // it and leave the agent blind behind a hand-off that is gone.
        if let Some(entry) = self.terminal_if_any(&owner) {
            entry.clear_control();
        }
        drop(parked);
        tracing::debug!(
            event_name = "terminal_hand_off_resolved",
            id = %id,
            ending = ?answer.ending,
            source = ?answer.source,
            client = answer.client,
            seat = %owner.display(),
            terminal = handoff.terminal,
            "a parked terminal hand-off left the registry"
        );
        let _ = sender.send(answer.ending);
        let _ = self.update_sender().send(SessionUpdate::TerminalHandOffResolved {
            key: owner.clone(),
            id,
            answer,
        });
        // The strip stops saying the input waits.
        if let Some(entry) = self.terminal_if_any(&owner) {
            self.announce_terminal(&owner, Some(entry.summary(&owner, false)));
        }
        true
    }

    /// A seat ended - or `/new`/`/resume` replaced the occupant that was
    /// waiting - with a hand-off still parked: resolve it as abandoned so no
    /// dock outlives its asker.
    pub(crate) fn abandon_terminal_hand_offs(&self, slot: &SessionSlot) {
        let ids: Vec<uuid::Uuid> = self
            .terminal_handoffs
            .lock()
            .iter()
            .filter(|(_, (owner, _, _))| owner == slot)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let _ = self.resolve_terminal_hand_off(
                id,
                slot,
                TerminalHandOffAnswer {
                    ending: HandOffEnding::Abandoned,
                    source: forge_primitives::browser::HandOffSource::Abandoned,
                    client: None,
                },
            );
        }
    }

    /// Spawn a terminal for `slot`, with its cwd and the environment its
    /// `claude` child gets.
    fn spawn_terminal(
        &self,
        slot: &SessionSlot,
        cwd: Option<PathBuf>,
    ) -> Result<Arc<TerminalEntry>, String> {
        let Some(project) = self.project_for_slot(slot) else {
            return Err(format!(
                "{slot} names no configured project, so it has no cwd or environment to give a \
                 terminal",
                slot = slot.display()
            ));
        };
        let account_env =
            self.pool
                .lock()
                .get(slot)
                .and_then(|pooled| self.accounts.env(&pooled.account))
                .or_else(|| {
                    project.accounts.first().and_then(|name| {
                        self.accounts.env(&forge_gateway::AccountKey(name.clone()))
                    })
                })
                .unwrap_or_default();
        let env = crate::config::session_env(&project, &account_env);
        let cwd = cwd
            .or_else(|| self.cwd_for_session(slot).map(PathBuf::from))
            .unwrap_or_else(|| project.path.clone());
        let config = self.config_arc();
        let terminal = forge_terminal::Terminal::spawn(&forge_terminal::TerminalConfig {
            shell: config.terminal.shell.clone(),
            cwd,
            env,
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            scrollback_lines: config.terminal.scrollback_lines,
        })
        .map_err(|error| format!("the terminal could not start: {error}"))?;
        let id = self.next_terminal_id.fetch_add(1, Ordering::Relaxed);
        let (control, _) = tokio::sync::watch::channel(None);
        Ok(Arc::new(TerminalEntry {
            id,
            terminal: Arc::new(terminal),
            last_used: Mutex::new(Instant::now()),
            attached: AtomicUsize::new(0),
            controller: AtomicU64::new(0),
            control,
        }))
    }

    /// Tell the strip when the shell exits, so "the shell exited" is drawn
    /// without waiting for a record read.
    fn watch_terminal_exit(self: &Arc<Self>, slot: &SessionSlot, entry: &Arc<TerminalEntry>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let weak = Arc::downgrade(self);
        let slot = slot.clone();
        let entry = Arc::clone(entry);
        let mut exit = entry.terminal.exit_stream();
        runtime.spawn(async move {
            if exit.changed().await.is_err() {
                return;
            }
            let Some(workspace) = weak.upgrade() else { return };
            let held = workspace.terminal_held(&slot);
            workspace.announce_terminal(&slot, Some(entry.summary(&slot, held)));
        });
    }

    /// **A state change is a push, not a poll.** The strip's words - a
    /// command running, a password wanted - come from the record, and a
    /// record that only changed on open, close and exit would say "idle"
    /// through a whole command.
    fn watch_terminal_state(self: &Arc<Self>, slot: &SessionSlot, entry: &Arc<TerminalEntry>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let weak = Arc::downgrade(self);
        let slot = slot.clone();
        let entry = Arc::clone(entry);
        let mut states = entry.terminal.state_stream();
        runtime.spawn(async move {
            while states.changed().await.is_ok() {
                let Some(workspace) = weak.upgrade() else { return };
                // Only while this is still the seat's terminal: a replaced
                // shell's watcher must not announce the old entry.
                if workspace.terminal_if_any(&slot).map(|held| held.id) != Some(entry.id) {
                    return;
                }
                let held = workspace.terminal_held(&slot);
                workspace.announce_terminal(&slot, Some(entry.summary(&slot, held)));
            }
        });
    }

    /// Start the idle clock, once.
    fn ensure_terminal_clock(self: &Arc<Self>) {
        if self.terminal_clock_started.swap(true, Ordering::SeqCst) {
            return;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CLOCK_TICK).await;
                let Some(workspace) = weak.upgrade() else { return };
                workspace.sweep_idle_terminals();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ECHO off is the password signal, and it makes the hand-off's card say
    /// the typing is hidden.
    #[test]
    fn a_password_terminal_hand_off_is_secret() {
        let state = TerminalState {
            running: false,
            input_expected: Some(TerminalInputExpected {
                kind: TerminalInputKind::Password,
                evidence: "ECHO off".to_owned(),
            }),
            exit: None,
            source: TerminalSource::Exact,
        };
        let secret = state
            .input_expected
            .as_ref()
            .is_some_and(|expected| expected.kind == TerminalInputKind::Password);
        assert!(secret);
    }

    /// **The terminal belongs to the slot, not the occupant.** `/new` and
    /// `/resume` swap the session id and leave the slot alone, so a second
    /// `terminal_for` on the same slot finds the same PTY - which is what
    /// makes the shell and its scrollback outlive both.
    #[tokio::test]
    async fn the_terminal_belongs_to_the_slot_across_occupants() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let first = workspace.terminal_for(&slot).expect("a terminal");
        // A `/new` or `/resume` is a new occupant on the same slot; the
        // registry is keyed by the slot, so the next read is the same value.
        let again = workspace.terminal_for(&slot).expect("the same terminal");
        assert_eq!(again.id, first.id, "the terminal id is the slot's terminal");
        assert!(
            Arc::ptr_eq(&first.terminal, &again.terminal),
            "the same PTY, shell and scrollback are what a new occupant finds",
        );
        workspace.close_terminal(&slot);
    }

    /// **A hand-off parks on the terminal and blinds the agent**: while one is
    /// held, the write and read tools refuse before touching the screen.
    #[tokio::test]
    async fn a_parked_hand_off_blinds_the_agent() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        let (_id, _answer) = workspace
            .register_terminal_hand_off(&slot, entry.id, "the sudo password")
            .expect("a hand-off parks");
        assert!(workspace.terminal_held(&slot), "the person owns the input");
        let refused = crate::mcp::terminal::facade::ProdTerminalFacade::from_workspace(&workspace);
        assert!(
            refused.read(&slot, entry.id, None, 200, 32 * 1024).await.is_err(),
            "a read while parked is refused",
        );
        assert!(
            refused.write(&slot, entry.id, Some("x".to_owned()), Vec::new()).await.is_err(),
            "a write while parked is refused",
        );
        workspace.close_terminal(&slot);
    }

    /// **The person can take the terminal without an agent asking.** While a
    /// client holds the role the tools are refused, another client's take
    /// overrides the holder, and a release hands the agent the input back.
    #[tokio::test]
    async fn a_client_take_control_blinds_the_agent_until_release() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        entry.take_control(7);
        assert!(workspace.terminal_controlled(&slot), "the person holds the terminal");
        assert!(!workspace.terminal_held(&slot), "which is not a hand-off");
        let facade = crate::mcp::terminal::facade::ProdTerminalFacade::from_workspace(&workspace);
        assert!(
            facade.read(&slot, entry.id, None, 10, 1024).await.is_err(),
            "a read while the person controls is refused",
        );
        assert!(
            facade.write(&slot, entry.id, Some("x".to_owned()), Vec::new()).await.is_err(),
            "a write while the person controls is refused",
        );
        entry.take_control(8);
        assert_eq!(entry.controller(), Some(8), "another client's take overrides the holder");
        assert!(!entry.release_control(7), "the old holder's release does not land");
        assert!(entry.release_control(8), "the holder releases it");
        assert!(!workspace.terminal_controlled(&slot), "the agent owns the input again");
        assert!(facade.read(&slot, entry.id, None, 10, 1024).await.is_ok(), "and reads again");
        workspace.close_terminal(&slot);
    }

    /// **A wait already in flight stops when a hand-off parks.** The entry
    /// check is not enough: the person can take the screen while the agent
    /// is polling it, and a wait that keeps reading would return what they
    /// type.
    #[tokio::test]
    async fn a_wait_in_flight_stops_when_a_hand_off_parks() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        let facade = crate::mcp::terminal::facade::ProdTerminalFacade::from_workspace(&workspace);
        let waiter = {
            let facade = Arc::clone(&facade);
            let slot = slot.clone();
            let id = entry.id;
            tokio::spawn(async move {
                facade
                    .wait(
                        &slot,
                        id,
                        crate::mcp::terminal::facade::WaitUntil::Pattern("NEVER_SHOWN".to_owned()),
                        5_000,
                    )
                    .await
            })
        };
        // Let the wait reach its poll loop before the person takes the screen.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let (_id, _answer) = workspace
            .register_terminal_hand_off(&slot, entry.id, "the sudo password")
            .expect("a hand-off parks");
        let joined = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("the wait returns once the person holds the screen")
            .expect("the wait task joins");
        assert!(joined.is_err(), "a wait in flight must refuse once a hand-off parks: {joined:?}");
        workspace.close_terminal(&slot);
    }

    /// **`terminal__run` is a terminal call too.** A hand-off owns the seat's
    /// terminal and its input; a run started behind it is the same call the
    /// other tools refuse.
    #[tokio::test]
    async fn run_is_refused_while_a_hand_off_is_parked() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        let (_id, _answer) = workspace
            .register_terminal_hand_off(&slot, entry.id, "the sudo password")
            .expect("a hand-off parks");
        let facade = crate::mcp::terminal::facade::ProdTerminalFacade::from_workspace(&workspace);
        let result = facade.run(&slot, "echo x", None, 1_000).await;
        assert!(result.is_err(), "run is refused while the person owns the screen");
        workspace.close_terminal(&slot);
    }

    /// **An answer settles the park**: the waiter gets the ending, the seat
    /// stops being held, and the views are told the ask resolved.
    #[tokio::test]
    async fn a_hand_off_answer_settles_the_park() {
        let (workspace, mut rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        let (id, answer) = workspace
            .register_terminal_hand_off(&slot, entry.id, "the sudo password")
            .expect("a hand-off parks");
        assert!(workspace.terminal_handoff_waiting(id, &slot));
        let resolved = workspace.resolve_terminal_hand_off(
            id,
            &slot,
            TerminalHandOffAnswer {
                ending: HandOffEnding::Done,
                source: forge_primitives::browser::HandOffSource::Unstated,
                client: None,
            },
        );
        assert!(resolved, "the registered hand-off resolves");
        assert_eq!(answer.await.expect("an ending"), HandOffEnding::Done);
        assert!(!workspace.terminal_held(&slot), "the agent owns the input again");
        assert!(!workspace.terminal_handoff_waiting(id, &slot));
        let mut told = false;
        while let Ok(frame) = rx.try_recv() {
            if matches!(
                &*frame.update,
                SessionUpdate::TerminalHandOffResolved { id: resolved, .. } if *resolved == id
            ) {
                told = true;
            }
        }
        assert!(told, "the views were told the ask resolved");
        workspace.close_terminal(&slot);
    }

    /// **A seat that ends with a hand-off parked abandons it**, so the waiter
    /// stops and no dock outlives its asker.
    #[tokio::test]
    async fn ending_the_seat_abandons_a_parked_hand_off() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        let (id, answer) = workspace
            .register_terminal_hand_off(&slot, entry.id, "the sudo password")
            .expect("a hand-off parks");
        workspace.release_session(&slot);
        assert_eq!(
            answer.await.expect("an ending"),
            HandOffEnding::Abandoned,
            "the seat ended, so the ask cannot be answered"
        );
        assert!(!workspace.terminal_handoff_waiting(id, &slot));
    }

    /// A hand-off id another seat names resolves nothing: the park is the
    /// owner's alone.
    #[tokio::test]
    async fn another_seat_cannot_resolve_a_hand_off() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let other = SessionSlot::lead("OtherOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        let (id, _answer) = workspace
            .register_terminal_hand_off(&slot, entry.id, "the sudo password")
            .expect("a hand-off parks");
        let resolved = workspace.resolve_terminal_hand_off(
            id,
            &other,
            TerminalHandOffAnswer {
                ending: HandOffEnding::Done,
                source: forge_primitives::browser::HandOffSource::Unstated,
                client: None,
            },
        );
        assert!(!resolved, "another seat's answer is refused");
        assert!(workspace.terminal_held(&slot), "the ask still parks on its owner");
        workspace.close_terminal(&slot);
    }

    /// **A command starting is pushed to the views**, so the strip says it is
    /// running without waiting for a record re-read.
    #[tokio::test]
    async fn a_command_start_is_pushed_to_the_views() {
        let (workspace, mut rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline
            && entry.terminal.screen().text.trim().is_empty()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The open's own announcements are not this test's.
        while rx.try_recv().is_ok() {}
        entry.terminal.write_text("sleep 1\r").expect("a write");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut running = false;
        while tokio::time::Instant::now() < deadline && !running {
            match rx.try_recv() {
                Ok(frame) => {
                    if let SessionUpdate::TerminalChanged { terminal: Some(summary), .. } =
                        &*frame.update
                    {
                        running |= summary.state.running;
                    }
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
        assert!(running, "a TerminalChanged said the command is running");
        workspace.close_terminal(&slot);
    }

    /// **The seat ending ends its terminal.** The release path is the one
    /// place teardown happens; a `/new` or `/resume` never reaches it.
    #[tokio::test]
    async fn ending_the_seat_closes_its_terminal() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        workspace.release_session(&slot);
        assert!(
            workspace.terminal_if_any(&slot).is_none(),
            "the seat ended, so its terminal left the registry with it",
        );
        entry.terminal.close();
    }

    /// **The door follows the seat read.** A declared seat answers its read
    /// before anything starts in it, so its door opens and the terminal it
    /// starts appears in the record. A worker never spawned has no working
    /// tree, its read is refused, and its door refuses with the same words.
    #[tokio::test]
    async fn opening_a_terminal_follows_the_seat_read() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");

        let declared = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.open_terminal(&declared).expect("a declared seat answers its read");
        drop(entry);
        workspace.close_terminal(&declared);

        let unstarted = SessionSlot::worker("TestOrg", "proj", "nobody");
        assert!(
            workspace.open_terminal(&unstarted).is_err(),
            "a seat with no working tree opened a terminal",
        );
        assert!(workspace.terminal_if_any(&unstarted).is_none(), "and none was registered");
    }

    /// **The park's ending takes the keyboard even from a claim that landed.**
    /// A claim read under the same lock as the ending cannot outlive it.
    #[tokio::test]
    async fn the_hand_off_ending_clears_a_claimed_keyboard() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        let (parked, _rx) = workspace
            .register_terminal_hand_off(&slot, entry.id, "the sudo password")
            .expect("a hand-off parks");

        assert!(
            workspace.write_handoff_keyboard(&slot, entry.id, 7, b"echo door\r").is_ok(),
            "the door claims the keyboard with its first byte",
        );
        assert!(workspace.terminal_controlled(&slot), "and the person holds it");

        assert!(workspace.resolve_terminal_hand_off(
            parked,
            &slot,
            TerminalHandOffAnswer {
                ending: HandOffEnding::Done,
                source: forge_primitives::browser::HandOffSource::Takeover,
                client: Some(7),
            },
        ));
        assert!(!workspace.terminal_controlled(&slot), "the ending gave the keyboard back");
        assert!(
            workspace.write_handoff_keyboard(&slot, entry.id, 8, b"echo late\r").is_err(),
            "a byte after the ending cannot land",
        );
        entry.terminal.close();
    }

    /// **A take cannot land while a hand-off is parked**, checked under the
    /// park's own lock; the ending opens the door again.
    #[tokio::test]
    async fn a_take_is_refused_while_a_hand_off_is_parked() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj", "/tmp");
        let slot = SessionSlot::lead("TestOrg", "proj");
        let entry = workspace.terminal_for(&slot).expect("a terminal");
        let (parked, _rx) = workspace
            .register_terminal_hand_off(&slot, entry.id, "the sudo password")
            .expect("a hand-off parks");

        assert!(
            workspace.take_terminal_control(&slot, 7).is_err(),
            "a take landed while a hand-off held the keyboard",
        );
        assert!(!workspace.terminal_controlled(&slot), "and no role was handed out");

        assert!(workspace.resolve_terminal_hand_off(
            parked,
            &slot,
            TerminalHandOffAnswer {
                ending: HandOffEnding::Done,
                source: forge_primitives::browser::HandOffSource::Takeover,
                client: None,
            },
        ));
        assert!(
            workspace.take_terminal_control(&slot, 7).is_ok(),
            "the person's own take is theirs again once the ask is answered",
        );
        assert!(workspace.terminal_controlled(&slot), "and they hold it");
        entry.terminal.close();
    }

    /// The crate's state crosses to the wire unchanged, so a model reading the
    /// tool result sees the same evidence the classifier wrote.
    #[test]
    fn the_crates_state_crosses_verbatim() {
        let state = forge_terminal::State {
            running: true,
            input_expected: Some(forge_terminal::InputExpected {
                kind: forge_terminal::InputKind::Prompt,
                evidence: "quiet at a prompt".to_owned(),
            }),
            exit: Some(forge_terminal::Exit { code: Some(3), signal: None }),
            source: forge_terminal::Source::Heuristic,
        };
        let wire = wire_state(state);
        assert!(wire.running);
        assert_eq!(wire.exit, Some(TerminalExit { code: Some(3), signal: None }));
        assert_eq!(wire.source, TerminalSource::Heuristic);
        assert_eq!(wire.input_expected.expect("an input").kind, TerminalInputKind::Prompt);
    }
}

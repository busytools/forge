//! The cron subsystem: the durable schedule set, its mutation policy, the
//! due-fire pass, the parked-prompt fate of a fire, and the MCP tools'
//! caller-scoped view.
//!
//! The `forge.toml` project catalogue, the session pool, the Command bus and
//! the redb store stay with the workspace; the schedule state and the
//! policy over it live here. What this module needs from its host arrives
//! through three seams:
//!
//! - [`CronStore`] is every read and write that must survive a restart: the
//!   rows and the fired-prompt/schedule pairing. A run with no store
//!   answers the no-store defaults.
//! - [`CronEmit`] is the frame a write announces and the service-status
//!   notice a deferred fire raises.
//! - [`CronHost`] is the delivery of one due fire into its owner's session
//!   and the shutdown flag - the two decisions that need the spine.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use forge_primitives::cron::{CronEntry, CronId, CronKind, CronName};
use parking_lot::Mutex;

use crate::cron::schedule;

/// How often the scheduler wakes to fire due crons. Minute granularity
/// matches the cron-expression resolution. The host's tick loop reads it.
pub const CRON_TICK_INTERVAL: Duration = Duration::from_secs(60);

/// The gap between warnings while one cooling episode lasts; the ticks in
/// between repeat at `debug`.
const COOLING_REWARN_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// India Standard Time's offset from UTC, the clock a reader of the
/// notice is on.
pub const IST_OFFSET_MINUTES: i64 = 5 * 60 + 30;

/// Why one fire did not land, as the delivery host reports it. The
/// subsystem decides the entry's fate from it, so a hand-off is never taken
/// for a success it was not.
pub enum CronFireOutcome {
    /// Dispatched into a running session, or buffered + a spawn kicked off.
    Delivered,
    /// The cron's project is no longer in forge.toml: the entry is removed
    /// rather than advanced forever.
    TargetGone,
    /// The owner still has a row, but the boot wave would skip it, so
    /// nothing can drain a buffered prompt. `directory` is where its
    /// session would have started, for the warning. The row is kept: a
    /// restored worktree brings the owner back.
    TargetCannotBeWoken { directory: std::path::PathBuf },
    /// Every account serving the project's model is cooling until the
    /// carried reset: the entry stays due.
    DeferredCooling { cooling: CronCooling },
    /// The Command channel is closed (shutting down), or the host was not
    /// bound yet: the entry stays due so the next tick or boot catch-up
    /// re-fires it.
    DispatchFailed,
    /// The owner is asleep and this cron may not wake it: the entry stays
    /// due without advancing.
    SkippedAsleep,
}

/// The cooling answer a deferred fire carries, mapped by the host from its
/// own account-pool shape so this crate stays pool-free.
pub struct CronCooling {
    pub reset_in: Duration,
    pub org: String,
    pub model: String,
    pub tried: usize,
}

/// The durable half: the cron rows and the fired-prompt/schedule pairing.
pub trait CronStore: Send + Sync {
    fn upsert(&self, cron: &CronEntry) -> Result<(), String>;
    fn delete(&self, id: &CronId) -> Result<(), String>;
    fn put_name(&self, prompt: &str, name: &CronName, now: SystemTime) -> Result<(), String>;
    fn names_for(
        &self,
        prompts: &[&str],
    ) -> Result<std::collections::BTreeMap<String, CronName>, String>;
}

/// The frames and notices a cron write or a deferred fire produces.
pub trait CronEmit: Send + Sync {
    /// A project's schedule set moved; the host composes the lead-slot
    /// frame from the set it now holds. A project no seat carries is the
    /// host's to drop, with its own debug line.
    fn schedules_changed(&self, project: &str, crons: &[CronEntry]);
    /// A warning a reader acts on (a deferred cooling episode, a dispatch
    /// that could not land).
    fn service_status(&self, message: String);
}

/// What firing one due entry needs from the spine.
pub trait CronHost: Send + Sync {
    /// Deliver `cron`'s prompt into its owner's session, echoing the block
    /// as it goes. The outcome decides whether the entry advances, is
    /// removed, or stays due.
    fn deliver(&self, cron: &CronEntry, missed: bool) -> CronFireOutcome;
    /// A tick landing inside a shutdown's drain must not wake a session
    /// behind it.
    fn is_shutting_down(&self) -> bool;
}

/// Everything the workspace hands the subsystem when it is built.
pub struct CronSeams {
    pub store: Arc<dyn CronStore>,
    pub emit: Arc<dyn CronEmit>,
    pub host: Arc<dyn CronHost>,
}

/// The durable schedule set and the policy over it.
pub struct Cron {
    entries: Mutex<Vec<CronEntry>>,
    /// Set at boot when the durable rows could not be read: the store could
    /// not open, or its read failed. The in-memory set is empty while rows
    /// may still be on disk, so it is not the registry. `cron__list` refuses
    /// on it rather than answering an empty array a caller would read as
    /// "you have no crons".
    ///
    /// It gates `cron__list` only: under the same failed load `delete_cron`
    /// still answers `NoSuchCron` from the empty set, and `create_cron` still
    /// promises durability while `persist_cron` returns quietly. Both predate
    /// this cut and the charter's list-then-delete flow is gated by the list
    /// refusal; the siblings are #2943.
    load_failed: std::sync::atomic::AtomicBool,
    /// The ids the LAST pass found unwakeable, so the first warning about
    /// one is a `WARN` and the repeats are not. Replaced wholesale each
    /// pass, so an episode that clears warns again.
    unwakeable: Mutex<HashSet<CronId>>,
    /// The ids the last pass found cooling, with the time each last warned.
    cooling: Mutex<HashMap<CronId, SystemTime>>,
    store: Arc<dyn CronStore>,
    emit: Arc<dyn CronEmit>,
    host: Arc<dyn CronHost>,
}

impl Cron {
    pub fn new(entries: Vec<CronEntry>, load_failed: bool, seams: CronSeams) -> Self {
        Self {
            entries: Mutex::new(entries),
            load_failed: std::sync::atomic::AtomicBool::new(load_failed),
            unwakeable: Mutex::new(HashSet::new()),
            cooling: Mutex::new(HashMap::new()),
            store: seams.store,
            emit: seams.emit,
            host: seams.host,
        }
    }

    /// Lock the durable cron list and apply `f`. Every cron-list mutation
    /// routes through here to take the lock, and the mutation site persists
    /// what it changed from inside its own closure - a row at a time, never
    /// the whole in-memory set. A boot read that failed leaves that set
    /// EMPTY while the store handle stays live, so a whole-set write would
    /// erase every durable row the read never loaded.
    fn with_entries_mut<R>(&self, f: impl FnOnce(&mut Vec<CronEntry>) -> R) -> R {
        let mut crons = self.entries.lock();
        f(&mut crons)
    }

    /// The host's test wrapper (`Workspace::with_crons_mut`, itself
    /// `cfg(test)`) drives a test's schedule set through here; every
    /// production caller is in this module.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_crons_mut<R>(&self, f: impl FnOnce(&mut Vec<CronEntry>) -> R) -> R {
        self.with_entries_mut(f)
    }

    /// Persist one cron row, replacing any prior record for its id. Called
    /// from inside the mutation's lock.
    fn persist_cron(&self, cron: &CronEntry) {
        if let Err(error) = self.store.upsert(cron) {
            tracing::error!(
                target: "forge_tools::cron",
                cron_id = %cron.id,
                %error,
                "persisting a cron to the store failed; it may be lost on restart",
            );
        }
    }

    /// Delete one cron row by id. Called from inside the mutation's lock.
    fn delete_stored_cron(&self, id: &CronId) {
        if let Err(error) = self.store.delete(id) {
            tracing::error!(
                target: "forge_tools::cron",
                cron_id = %id,
                %error,
                "deleting a cron from the store failed; it may come back on restart",
            );
        }
    }

    /// Append a cron and persist it. Backs `cron__create`.
    pub fn push_cron(&self, entry: CronEntry) {
        let project_name = entry.project_name.clone();
        self.with_entries_mut(|crons| {
            self.persist_cron(&entry);
            crons.push(entry);
        });
        self.announce_cron_schedules_changed(&project_name);
    }

    /// Remove the cron `id` in `project_name` regardless of who owns it,
    /// persist, and report whether an entry was removed. Backs the
    /// fire-router's dead-slot removal; the owner-scoped `cron__delete`
    /// uses [`Self::remove_cron_owned_by`].
    pub fn remove_cron(&self, project_name: &str, id: &CronId) -> bool {
        let removed = self.with_entries_mut(|crons| {
            let before = crons.len();
            crons.retain(|c| !(c.id == *id && c.project_name == project_name));
            let removed = crons.len() != before;
            if removed {
                self.delete_stored_cron(id);
            }
            removed
        });
        if removed {
            self.announce_cron_schedules_changed(project_name);
        }
        removed
    }

    /// Remove the cron `id` in `project_name` only when the slot that
    /// owns it matches `label` (`None` = a lead cron, `Some(label)` =
    /// that worker's), persist, and return the entry removed (`None` when
    /// nothing matched). Backs `cron__delete` so a caller deletes only its
    /// own crons.
    pub fn remove_cron_owned_by(
        &self,
        project_name: &str,
        id: &CronId,
        label: Option<&str>,
    ) -> Option<CronEntry> {
        let removed = self.with_entries_mut(|crons| {
            let pos = crons.iter().position(|c| {
                c.id == *id && c.project_name == project_name && c.team_role.as_deref() == label
            });
            let removed = pos.map(|pos| crons.remove(pos));
            if removed.is_some() {
                self.delete_stored_cron(id);
            }
            removed
        });
        if removed.is_some() {
            self.announce_cron_schedules_changed(project_name);
        }
        removed
    }

    /// Remove worker `label`'s crons in `project_name` from the in-memory
    /// set and the store, scoped to `(project name, team_role ==
    /// Some(label))`. Backs the host's worker teardown, so a despawned
    /// dynamic worker's crons go with its durable row.
    pub fn remove_crons_for_worker(&self, project_name: &str, label: &str) {
        let removed = self.with_entries_mut(|crons| {
            let mut removed: Vec<CronId> = Vec::new();
            crons.retain(|c| {
                let taking =
                    c.project_name == project_name && c.team_role.as_deref() == Some(label);
                if taking {
                    removed.push(c.id.clone());
                }
                !taking
            });
            for id in &removed {
                self.delete_stored_cron(id);
            }
            !removed.is_empty()
        });
        if removed {
            self.announce_cron_schedules_changed(project_name);
        }
    }

    /// Tell the host the project's schedule set moved, as the set the write
    /// just left. The host routes it on the project's lead seat; a project
    /// no seat carries is the host's to drop.
    fn announce_cron_schedules_changed(&self, project_name: &str) {
        let crons = self.crons_for_project(project_name);
        self.emit.schedules_changed(project_name, &crons);
    }

    /// Whether this run's boot could not load the durable crons.
    /// `cron__list` refuses on it.
    pub fn durable_crons_unavailable(&self) -> bool {
        self.load_failed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The crons registered for `project_name`, whatever their owner. The
    /// two scoping consumers narrow it - `cron__list` to the caller's own
    /// crons, the Inspector SCHEDULES snapshot to the active tab's - while
    /// the refused-delete determination reads it whole, because telling
    /// "not in this project" from "not yours" needs the other owners' rows.
    pub fn crons_for_project(&self, project_name: &str) -> Vec<CronEntry> {
        self.entries.lock().iter().filter(|c| c.project_name == project_name).cloned().collect()
    }

    /// A snapshot of every cron across all projects. Backs the per-tick
    /// due-check.
    fn all_crons_snapshot(&self) -> Vec<CronEntry> {
        self.entries.lock().clone()
    }

    /// Remember the schedule a fired prompt came from, so its row can name
    /// the schedule after a reload the delivery frame does not survive.
    ///
    /// Best-effort: a store that will not write costs a reloaded row its
    /// title, which is exactly what it had before the pairing existed.
    pub fn remember_cron_name(
        &self,
        slot: &forge_primitives::SessionSlot,
        uuid: &str,
        cron_id: &str,
        description: Option<&str>,
    ) {
        let name =
            CronName { cron_id: cron_id.to_owned(), description: description.map(str::to_owned) };
        if let Err(error) = self.store.put_name(uuid, &name, SystemTime::now()) {
            tracing::warn!(
                event_name = "cron_name_store_failed",
                org = slot.org(),
                project = slot.project(),
                label = slot.label(),
                cron = %cron_id,
                prompt = %uuid,
                %error,
                "the fired prompt's schedule was not recorded; a reload falls back to the prompt"
            );
        }
    }

    /// The schedules the given fired prompts came from, keyed by the prompt's
    /// own id: the read half of the pairing the delivery frame carries.
    pub fn cron_names(
        &self,
        slot: &forge_primitives::SessionSlot,
        uuids: &[&str],
    ) -> std::collections::BTreeMap<String, CronName> {
        self.store.names_for(uuids).unwrap_or_else(|error| {
            tracing::warn!(
                event_name = "cron_names_read_failed",
                org = slot.org(),
                project = slot.project(),
                label = slot.label(),
                prompts = uuids.len(),
                %error,
                "the fired prompts' schedules could not be read; their rows fall back to the prompt"
            );
            std::collections::BTreeMap::new()
        })
    }

    /// Advance a fired cron and persist: a recurring cron records
    /// `last_fire` and moves `next_fire` to the next future slot (removed
    /// if it somehow has none); a run-once is removed. A direct state
    /// mutation - the fire's prompt delivery goes through the Command bus
    /// separately.
    pub fn advance_or_remove_cron(&self, id: &CronId, fired_at: SystemTime) {
        // The entry's project comes out of the fold, since a run-once the
        // advance removes is the one thing that can no longer answer it.
        let mut project_name = None;
        self.with_entries_mut(|crons| {
            let Some(pos) = crons.iter().position(|c| &c.id == id) else { return };
            project_name = Some(crons[pos].project_name.clone());
            match &crons[pos].kind {
                CronKind::Once(_) => {
                    crons.remove(pos);
                    self.delete_stored_cron(id);
                }
                CronKind::Recurring(_) => {
                    if let Some(next) = schedule::next_fire_after(&crons[pos].kind, fired_at) {
                        crons[pos].last_fire = Some(fired_at);
                        crons[pos].next_fire = next;
                        self.persist_cron(&crons[pos]);
                    } else {
                        // A recurring expr that parses but never matches
                        // (e.g. "0 0 30 2 *"). Don't drop it silently; the
                        // warning below is the only trace.
                        let removed = crons.remove(pos);
                        let expr = if let CronKind::Recurring(e) = &removed.kind {
                            e.as_str()
                        } else {
                            ""
                        };
                        tracing::warn!(
                            target: "forge_tools::cron",
                            cron_id = %removed.id,
                            project = %removed.project_name,
                            expr = %expr,
                            "recurring cron has no upcoming occurrence; removed it",
                        );
                        self.delete_stored_cron(id);
                    }
                }
            }
        });
        if let Some(project_name) = project_name {
            self.announce_cron_schedules_changed(&project_name);
        }
    }

    /// Fire every cron due at `now`: deliver each prompt into its project
    /// session (spawning it if asleep, unless the entry carries
    /// `wake: false` for a job that must not wake a sleeping
    /// owner, in which case a sleeping owner leaves the fire due and
    /// unadvanced) and advance/remove the entry.
    /// Delivery routes through the host; the advance is a direct state
    /// write - kept separate per the cron state-vs-delivery split. `now` is
    /// injected so tests are deterministic. Also the boot catch-up: calling
    /// this once at startup fires every cron that came due while forge was
    /// down, advancing each past its missed slots (catch-up-once).
    pub fn fire_due(&self, now: SystemTime) {
        // A tick can land inside a shutdown's drain; the session it would
        // wake would register behind it and outlive forge. The schedule is
        // untouched, so the next boot catches up.
        if self.host.is_shutting_down() {
            return;
        }
        let snapshot = self.all_crons_snapshot();
        let due = schedule::due_crons(&snapshot, now);
        // What THIS pass finds unwakeable, committed at the end so the set
        // holds only the latest pass's answer.
        let mut still_unwakeable = std::collections::HashSet::new();
        // What THIS pass still found cooling, and when each last warned.
        let mut still_cooling = std::collections::HashMap::new();
        for id in &due {
            let Some(cron) = snapshot.iter().find(|c| &c.id == id) else { continue };
            // Overdue by more than two ticks: forge or the session was down
            // through the scheduled minute. Two ticks (not one) absorbs the
            // scheduler's Skip-behaviour jitter so a same-window fire under
            // load is never mislabelled missed.
            let missed = now > cron.next_fire + CRON_TICK_INTERVAL * 2;
            match self.host.deliver(cron, missed) {
                // Delivered (or spawn kicked off): advance a recurring to
                // its next slot, remove a fired run-once.
                CronFireOutcome::Delivered => self.advance_or_remove_cron(id, now),
                // The slot has no session (project removed from
                // forge.toml, or a worker label with no durable row):
                // remove the cron rather than advance a dead entry
                // forever.
                CronFireOutcome::TargetGone => {
                    tracing::warn!(
                        target: "forge_tools::cron",
                        event_name = "cron_owner_gone",
                        project = %cron.project_name,
                        cron_id = %id,
                        label = cron.team_role.as_deref().unwrap_or("lead"),
                        "cron slot has no session; removing the cron",
                    );
                    self.remove_cron(&cron.project_name, id);
                }
                // The owner's row is kept but the wave skips it, so the
                // prompt has nowhere to land right now. A recurring cron
                // has another slot coming, so this fire is dropped and the
                // schedule advances; parking it would grow a bucket
                // nothing drains. A one-shot has no next slot, and
                // advancing it removes it, so it stays due and fires once
                // the owner can be woken.
                CronFireOutcome::TargetCannotBeWoken { directory } => {
                    let recurring = matches!(cron.kind, CronKind::Recurring(_));
                    // A stuck one-shot is re-evaluated every tick, so the
                    // repeats carry the same event_name at debug rather
                    // than writing the same warning once a minute for as
                    // long as the directory is missing.
                    if self.unwakeable_is_new(id) {
                        tracing::warn!(
                            target: "forge_tools::cron",
                            event_name = "cron_owner_cannot_be_woken",
                            project = %cron.project_name,
                            cron_id = %id,
                            label = cron.team_role.as_deref().unwrap_or("lead"),
                            directory = %directory.display(),
                            recurring,
                            "cron owner cannot be woken, so this fire did not land; the owner's \
                             row is kept",
                        );
                    } else {
                        tracing::debug!(
                            target: "forge_tools::cron",
                            event_name = "cron_owner_cannot_be_woken",
                            project = %cron.project_name,
                            cron_id = %id,
                            "cron owner still cannot be woken; the first warning for this \
                             cron already names it",
                        );
                    }
                    still_unwakeable.insert(id.clone());
                    if recurring {
                        self.advance_or_remove_cron(id, now);
                    }
                }
                // The owner is asleep and this cron may not wake it: leave
                // the fire due, unadvanced, so the next session start (or a
                // tick while the owner is awake) runs it. Nothing to warn
                // about - the cron asked for exactly this.
                CronFireOutcome::SkippedAsleep => {
                    tracing::debug!(
                        target: "forge_tools::cron",
                        event_name = "cron_skipped_asleep",
                        project = %cron.project_name,
                        cron_id = %id,
                        "the cron's owner is asleep and this cron does not wake it; the fire \
                         stays due",
                    );
                }
                // Every account serving the project's model is cooling:
                // the fire stays due, and the reason is said once per
                // episode rather than on every tick of it.
                CronFireOutcome::DeferredCooling { cooling } => {
                    let reset = cooling_reset_text(cooling.reset_in, now);
                    let warned_before = self.cooling_warned_at(id);
                    let warn_now = warned_before.is_none_or(|at| {
                        now.duration_since(at).unwrap_or(Duration::ZERO) >= COOLING_REWARN_INTERVAL
                    });
                    if warn_now {
                        tracing::warn!(
                            target: "forge_tools::cron",
                            event_name = "cron_deferred_cooling",
                            project = %cron.project_name,
                            cron_id = %id,
                            label = cron.team_role.as_deref().unwrap_or("lead"),
                            org = %cooling.org,
                            model = %cooling.model,
                            cooling_accounts = cooling.tried,
                            reset_in_secs = cooling.reset_in.as_secs(),
                            resets_at = %reset,
                            "cron fire deferred: every account serving this project's model is \
                             cooling, so the wake would be refused; it stays due and retries",
                        );
                        self.emit.service_status(format!(
                            "Cron in '{}' deferred: no account serving {} in {} is free \
                             ({} account(s) cooling in the org); resets at {reset}",
                            cron.project_name, cooling.model, cooling.org, cooling.tried,
                        ));
                    } else {
                        tracing::debug!(
                            target: "forge_tools::cron",
                            event_name = "cron_deferred_cooling",
                            project = %cron.project_name,
                            cron_id = %id,
                            label = cron.team_role.as_deref().unwrap_or("lead"),
                            org = %cooling.org,
                            model = %cooling.model,
                            cooling_accounts = cooling.tried,
                            reset_in_secs = cooling.reset_in.as_secs(),
                            resets_at = %reset,
                            "cron fire still deferred by the same cooling; the warning for this \
                             episode already names it",
                        );
                    }
                    // A repeat carries the earlier warning's time forward,
                    // so the floor counts from that warning, not this tick.
                    let warned_at = warned_before.filter(|_| !warn_now).unwrap_or(now);
                    still_cooling.insert(id.clone(), warned_at);
                }
                // Command channel closed (shutting down): leave the cron
                // due so the next boot catch-up re-fires it - don't consume
                // a fire that never handed off.
                CronFireOutcome::DispatchFailed => {
                    tracing::warn!(
                        target: "forge_tools::cron",
                        project = %cron.project_name,
                        cron_id = %id,
                        "cron fire deferred; leaving it due to retry",
                    );
                    self.emit.service_status(format!(
                        "Cron in '{}' could not fire yet (its session is shutting down, the \
                         host was not bound yet, or a spawn would be refused right now); it \
                         stays due and retries",
                        cron.project_name
                    ));
                }
            }
        }
        // Commit this pass. Anything not in it - fired, advanced, deleted
        // or simply not due - keeps no marker, so the next time it is
        // unwakeable the warning is a WARN again, and the next cooling
        // episode is a new one.
        *self.unwakeable.lock() = still_unwakeable;
        *self.cooling.lock() = still_cooling;
    }

    /// Whether this fire pass is the FIRST to find `id` unwakeable,
    /// recording it as seen if so. True is the pass that warns; false is
    /// every later pass while the condition holds.
    fn unwakeable_is_new(&self, id: &CronId) -> bool {
        self.unwakeable.lock().insert(id.clone())
    }

    /// When the previous fire pass last warned about this cron's cooling,
    /// or `None` when that pass did not find it cooling.
    fn cooling_warned_at(&self, id: &CronId) -> Option<SystemTime> {
        self.cooling.lock().get(id).copied()
    }

    /// Seed one durable cron directly, bypassing the tool create path - a
    /// host fixture's entry point to the otherwise private mutation.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed(&self, entry: CronEntry) {
        self.with_entries_mut(|crons| {
            self.persist_cron(&entry);
            crons.push(entry);
        });
    }
}

/// The reset a deferred cron names: the wall clock in IST plus the wait
/// ahead, so a reader needs no arithmetic on a duration. A reset no
/// clock can spell falls back to the wait alone rather than panicking
/// the scheduler.
fn cooling_reset_text(reset_in: Duration, now: SystemTime) -> String {
    let ist = now
        .checked_add(reset_in)
        .and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|since_epoch| {
            chrono::DateTime::from_timestamp(
                i64::try_from(since_epoch.as_secs()).ok()?,
                since_epoch.subsec_nanos(),
            )
        })
        .and_then(|at| at.checked_add_signed(chrono::TimeDelta::minutes(IST_OFFSET_MINUTES)));
    match ist {
        Some(at) => format!("{} IST (in {})", at.format("%H:%M"), wait_text(reset_in)),
        None => format!("in {}", wait_text(reset_in)),
    }
}

/// A wait as the two largest units that describe it ("1h 5m", "3m 20s").
fn wait_text(wait: Duration) -> String {
    let (hours, minutes, seconds) =
        (wait.as_secs() / 3600, wait.as_secs() % 3600 / 60, wait.as_secs() % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_unformattable_reset_falls_back_to_the_wait() {
        let last_second = chrono::DateTime::<chrono::Utc>::MAX_UTC.timestamp();
        let near_the_end = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(
                u64::try_from(last_second - 60).expect("a positive second count"),
            );
        for (reset_in, now) in [
            (std::time::Duration::MAX, std::time::SystemTime::now()),
            (std::time::Duration::from_secs(60), near_the_end),
        ] {
            let text = super::cooling_reset_text(reset_in, now);
            assert!(text.starts_with("in "), "the wait survives without a clock: {text}");
            assert!(!text.contains("IST"), "and no wall clock is claimed: {text}");
        }
    }
}

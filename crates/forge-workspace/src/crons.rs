//! The cron cluster on [`Workspace`]: the durable-cron store accessor
//! and its mutation helpers, the asleep-slot prompt buffers, the fire
//! router, the scheduler task, and the cross-crate seed helper.
//!
//! Everything here stays on `Workspace` as a second `impl` block, so
//! every caller (the boot path in [`crate::workspace`], the
//! `mcp::cron` facade, `spawn::deliver_cron_prompt`, forge-tui's boot)
//! keeps its path. The `crons`, `cron_scheduler_started` and `update_tx`
//! fields these methods own are `pub(crate)` for the same reason `db` is:
//! so this sibling module can reach them without a wrapper. A fired cron
//! with no live session parks in [`crate::parked`]. Schedule math lives in
//! [`crate::mcp::cron::schedule`]; the MCP tool surface in
//! [`crate::mcp::cron`]; delivery in [`crate::spawn`].

use std::sync::Arc;
use std::time::Duration;

use tracing::Instrument;

use crate::protocol::SessionUpdate;
use crate::target::ProjectKey;
use crate::workspace::Workspace;

/// How often the cron scheduler wakes to fire due crons. Minute
/// granularity matches the cron-expression resolution.
const CRON_TICK_INTERVAL: Duration = Duration::from_secs(60);

/// A cron prompt buffered for a sleeping slot: the raw prompt plus whether
/// this fire is overdue (delivered with a missed marker on drain).
#[derive(Debug)]
pub(crate) struct PendingCron {
    pub text: String,
    pub missed: bool,
}

impl Workspace {
    /// Lock the durable cron list, apply `f`, and persist to the
    /// machine-local store. Every cron-list mutation routes through here -
    /// `cron__create` / `cron__delete`, the scheduler's fire-advance, and
    /// boot catch-up - so the in-memory set and the store never diverge.
    pub(crate) fn with_crons_mut<R>(
        &self,
        f: impl FnOnce(&mut Vec<forge_primitives::CronEntry>) -> R,
    ) -> R {
        let mut crons = self.crons.lock();
        let result = f(&mut crons);
        if let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::cron::replace_all(db, &crons)
        {
            tracing::error!(
                target: "forge_workspace::crons",
                %error,
                "persisting crons to the store failed; a scheduled cron may be lost on restart",
            );
        }
        result
    }

    /// Append a cron and persist. Backs `cron__create`.
    pub(crate) fn push_cron(&self, entry: forge_primitives::CronEntry) {
        self.with_crons_mut(|crons| crons.push(entry));
    }

    /// Remove the cron `id` in `project_name` regardless of who owns it,
    /// persist, and report whether an entry was removed. Backs the
    /// fire-router's dead-slot removal; the owner-scoped `cron__delete`
    /// uses [`Self::remove_cron_owned_by`].
    pub(crate) fn remove_cron(&self, project_name: &str, id: &forge_primitives::CronId) -> bool {
        self.with_crons_mut(|crons| {
            let before = crons.len();
            crons.retain(|c| !(c.id == *id && c.project_name == project_name));
            crons.len() != before
        })
    }

    /// Remove the cron `id` in `project_name` only when the slot that
    /// owns it matches `label` (`None` = a lead cron, `Some(label)` =
    /// that worker's), persist, and report whether an entry was removed.
    /// Backs `cron__delete` so a caller deletes only its own crons.
    pub(crate) fn remove_cron_owned_by(
        &self,
        project_name: &str,
        id: &forge_primitives::CronId,
        label: Option<&str>,
    ) -> bool {
        self.with_crons_mut(|crons| {
            let before = crons.len();
            crons.retain(|c| {
                !(c.id == *id && c.project_name == project_name && c.team_role.as_deref() == label)
            });
            crons.len() != before
        })
    }

    /// Remove worker `label`'s crons in `project_key` from the in-memory
    /// set and the store, scoped to `(project name, team_role == Some(label))`.
    /// Backs `spawn::teardown_worker` so a despawned dynamic worker's crons
    /// go with its durable row. The key resolves to a name via the same view
    /// lookup `remove_gotify_subscriptions_for_worker` uses.
    pub(crate) fn delete_crons_for_worker(&self, project_key: &ProjectKey, label: &str) {
        let Some(project_name) =
            self.list_projects().into_iter().find(|v| v.key == *project_key).map(|v| v.name)
        else {
            tracing::warn!(
                target: "forge_workspace::crons",
                project = %project_key.as_str(),
                label = %label,
                "could not resolve a project name at worker teardown; its crons may be stranded",
            );
            return;
        };
        self.with_crons_mut(|crons| {
            crons.retain(|c| {
                !(c.project_name == project_name && c.team_role.as_deref() == Some(label))
            });
        });
    }

    /// The crons registered for `project_name`. Backs `cron__list` and
    /// the Inspector SCHEDULES snapshot, which scopes by the active tab's
    /// stamped project name.
    pub fn crons_for_project(&self, project_name: &str) -> Vec<forge_primitives::CronEntry> {
        self.crons.lock().iter().filter(|c| c.project_name == project_name).cloned().collect()
    }

    /// A snapshot of every cron across all projects. Backs the
    /// scheduler's per-tick due-check.
    pub(crate) fn all_crons_snapshot(&self) -> Vec<forge_primitives::CronEntry> {
        self.crons.lock().clone()
    }

    /// Advance a fired cron and persist: a recurring cron records
    /// `last_fire` and moves `next_fire` to the next future slot (removed
    /// if it somehow has none); a run-once is removed. A direct state
    /// mutation - the fire's prompt delivery goes through the Command bus
    /// separately.
    pub(crate) fn advance_or_remove_cron(
        &self,
        id: &forge_primitives::CronId,
        fired_at: std::time::SystemTime,
    ) {
        self.with_crons_mut(|crons| {
            let Some(pos) = crons.iter().position(|c| &c.id == id) else { return };
            match &crons[pos].kind {
                forge_primitives::CronKind::Once(_) => {
                    crons.remove(pos);
                }
                forge_primitives::CronKind::Recurring(_) => {
                    if let Some(next) =
                        crate::mcp::cron::schedule::next_fire_after(&crons[pos].kind, fired_at)
                    {
                        crons[pos].last_fire = Some(fired_at);
                        crons[pos].next_fire = next;
                    } else {
                        // A recurring expr that parses but never matches
                        // (e.g. "0 0 30 2 *") - reachable via a hand-edited
                        // cron.toml. Don't drop it silently; the warning
                        // below is the only trace.
                        let removed = crons.remove(pos);
                        let expr = if let forge_primitives::CronKind::Recurring(e) = &removed.kind {
                            e.as_str()
                        } else {
                            ""
                        };
                        tracing::warn!(
                            target: "forge_workspace::crons",
                            cron_id = %removed.id,
                            project = %removed.project_name,
                            expr = %expr,
                            "recurring cron has no upcoming occurrence; removed it",
                        );
                    }
                }
            }
        });
    }

    /// Fire every cron due at `now`: deliver each prompt into its project
    /// session (spawning it if asleep) and advance/remove the entry.
    /// Delivery routes through the Command bus (a session action); the
    /// advance is a direct state write - kept separate per the cron
    /// state-vs-delivery split. `now` is injected so tests are
    /// deterministic. Also the boot catch-up: calling this once at
    /// startup fires every cron that came due while forge was down,
    /// advancing each past its missed slots (catch-up-once).
    pub fn fire_due_crons(self: &Arc<Self>, now: std::time::SystemTime) {
        use crate::spawn::CronFireOutcome;
        let snapshot = self.all_crons_snapshot();
        let due = crate::mcp::cron::schedule::due_crons(&snapshot, now);
        // What THIS pass finds unwakeable, committed at the end so the set
        // holds only the latest pass's answer.
        let mut still_unwakeable = std::collections::HashSet::new();
        for id in &due {
            let Some(cron) = snapshot.iter().find(|c| &c.id == id) else { continue };
            // Overdue by more than two ticks: forge or the session was down
            // through the scheduled minute. Two ticks (not one) absorbs the
            // scheduler's Skip-behaviour jitter so a same-window fire under
            // load is never mislabelled missed.
            let missed = now > cron.next_fire + CRON_TICK_INTERVAL * 2;
            match crate::spawn::deliver_cron_prompt(
                self,
                &cron.project_name,
                cron.team_role.as_deref(),
                cron.prompt.clone(),
                missed,
            ) {
                // Delivered (or spawn kicked off): advance a recurring to
                // its next slot, remove a fired run-once.
                CronFireOutcome::Delivered => self.advance_or_remove_cron(id, now),
                // The slot has no session (project removed from
                // forge.toml, or a worker label with no durable row):
                // remove the cron rather than advance a dead entry
                // forever.
                CronFireOutcome::TargetGone => {
                    tracing::warn!(
                        target: "forge_workspace::crons",
                        project = %cron.project_name,
                        cron_id = %id,
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
                    let recurring = matches!(cron.kind, forge_primitives::CronKind::Recurring(_));
                    // A stuck one-shot is re-evaluated every tick, so the
                    // repeats carry the same event_name at debug rather
                    // than writing the same warning once a minute for as
                    // long as the directory is missing.
                    if self.cron_unwakeable_is_new(id) {
                        tracing::warn!(
                            target: "forge_workspace::crons",
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
                            target: "forge_workspace::crons",
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
                // Command channel closed (shutting down): leave the cron
                // due so the next boot catch-up re-fires it - don't consume
                // a fire that never handed off.
                CronFireOutcome::DispatchFailed => {
                    tracing::warn!(
                        target: "forge_workspace::crons",
                        project = %cron.project_name,
                        cron_id = %id,
                        "cron fire deferred; leaving it due to retry",
                    );
                    let _ = self.update_tx.send(SessionUpdate::ServiceStatus {
                        severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
                        message: format!(
                            "Cron in '{}' could not fire yet (its session is shutting down, or a \
                             spawn would be refused right now); it stays due and retries",
                            cron.project_name
                        ),
                    });
                }
            }
        }
        // Commit this pass. Anything not in it - fired, advanced, deleted
        // or simply not due - keeps no marker, so the next time it is
        // unwakeable the warning is a WARN again.
        self.cron_unwakeable_commit(still_unwakeable);
    }

    /// Spawn the cron scheduler: a background task that wakes every
    /// `CRON_TICK_INTERVAL` (~60s), fires every due cron, and exits when
    /// the workspace drops. Idempotent (a second call no-ops). Mirrors
    /// [`Workspace::start_usage_poller`]; started once at boot.
    pub fn start_cron_scheduler(self: &Arc<Self>) {
        if self.cron_scheduler_started.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let weak = Arc::downgrade(self);
        let span = tracing::info_span!("cron_scheduler");
        tokio::spawn(
            async move {
                let mut interval = tokio::time::interval_at(
                    tokio::time::Instant::now() + CRON_TICK_INTERVAL,
                    CRON_TICK_INTERVAL,
                );
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(workspace) = weak.upgrade() else {
                        return;
                    };
                    workspace.fire_due_crons(std::time::SystemTime::now());
                }
            }
            .instrument(span),
        );
    }
}

#[cfg(any(test, feature = "testing"))]
impl Workspace {
    /// Register a durable cron directly, bypassing the MCP create path.
    /// Cross-crate test access to the otherwise `pub(crate)` cron store
    /// so forge-tui can exercise the Inspector's `refresh_forge_crons`
    /// resolution against a seeded cron.
    pub fn seed_test_cron(&self, entry: forge_primitives::CronEntry) {
        self.push_cron(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use crate::SessionSlot;
    use crate::protocol::Command;
    use crate::workspace::PooledAgent;
    use forge_gateway::AccountKey;

    /// The cron prompts parked for `(project, label)`, under whichever org
    /// the seeded project belongs to.
    fn parked_crons(ws: &crate::Workspace, project: &str, label: Option<&str>) -> Vec<String> {
        let org =
            ws.list_projects().into_iter().find(|v| v.name == project).expect("seeded project").org;
        ws.parked_by_slot
            .lock()
            .get(&crate::SessionSlot::for_label(&org, project, label))
            .map(|parked| parked.cron.iter().map(|p| p.text.clone()).collect())
            .unwrap_or_default()
    }

    /// Persist a worker row for `label` in `project_key`, failing loudly
    /// when it did not land. The key must resolve to a configured
    /// project, since the row lands in the `(org, project name, label)`
    /// keyed `sessions` table.
    fn seed_worker_row(ws: &crate::Workspace, project_key: &ProjectKey, label: &str) {
        ws.seed_test_worker_row(project_key, label);
        assert!(
            ws.stored_worker_row(project_key, label).expect("read the seeded row").is_some(),
            "the worker row for {label} did not land; does {project_key:?} resolve to a project?",
        );
    }

    /// Seed the `proj` fixture with a project path that exists, and
    /// return its key. A non-git worker runs in that directory, so the
    /// wave checks it is still there before re-spawning the worker - a
    /// fixture path that cannot exist is not a shape production has.
    fn seed_project_with_a_real_root(ws: &crate::Workspace, dir: &tempfile::TempDir) -> ProjectKey {
        let root = dir.path().join("proj-root");
        std::fs::create_dir_all(&root).expect("create the project dir");
        ws.seed_test_project("proj", &root.to_string_lossy());
        ws.project_key_for_name("proj").expect("seeded project")
    }

    fn live_worker_entry(project: &str, label: &str) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.to_owned(),
            charter: "c".to_owned(),
            slot: SessionSlot::worker("TestOrg", project, label),
            session_id: None,
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", project),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// Drain every currently-queued `SessionUpdate` from the test rx.
    fn drain_updates(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::protocol::SessionUpdate>,
    ) -> Vec<crate::protocol::SessionUpdate> {
        let mut out = Vec::new();
        while let Ok(u) = rx.try_recv() {
            out.push(u);
        }
        out
    }

    fn project_expanded_path(workspace: &Workspace, name: &str) -> String {
        workspace.list_projects().into_iter().find(|p| p.name == name).map_or_else(
            || panic!("project '{name}' missing from workspace"),
            |p| p.path.to_string_lossy().into_owned(),
        )
    }

    #[test]
    fn cron_methods_push_list_remove_and_persist() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        let persisted = || {
            crate::store::cron::list(ws.db.lock().as_ref().expect("db installed")).expect("list")
        };

        let entry = CronEntry {
            id: CronId::from("c1"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        };

        ws.push_cron(entry.clone());
        assert_eq!(ws.crons_for_project("forge"), vec![entry.clone()], "listed for its project");
        assert!(ws.crons_for_project("other").is_empty(), "scoped by project name");
        assert_eq!(persisted(), vec![entry.clone()], "push persisted to the store");

        // A different project cannot delete another project's cron.
        assert!(!ws.remove_cron("other", &entry.id), "delete is scoped to the owning project");
        assert_eq!(ws.crons_for_project("forge").len(), 1);

        assert!(ws.remove_cron("forge", &entry.id), "the owning project removes it");
        assert!(ws.crons_for_project("forge").is_empty());
        assert!(persisted().is_empty(), "removal persisted");

        assert!(!ws.remove_cron("forge", &CronId::from("c1")), "removing a gone id reports false");
    }

    #[test]
    fn advance_or_remove_cron_recurring_advances_run_once_removes() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let fired = std::time::SystemTime::now();

        ws.push_cron(CronEntry {
            id: CronId::from("r"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "p".to_owned(),
            created_at: fired,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        });
        ws.advance_or_remove_cron(&CronId::from("r"), fired);
        let after = ws.crons_for_project("forge");
        assert_eq!(after.len(), 1, "a recurring cron stays after firing");
        assert!(after[0].next_fire > fired, "next_fire advanced to a future slot");
        assert_eq!(after[0].last_fire, Some(fired), "last_fire recorded");

        ws.push_cron(CronEntry {
            id: CronId::from("o"),
            project_name: "forge".to_owned(),
            kind: CronKind::Once(std::time::SystemTime::UNIX_EPOCH),
            prompt: "p".to_owned(),
            created_at: fired,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        });
        ws.advance_or_remove_cron(&CronId::from("o"), fired);
        assert!(
            ws.crons_for_project("forge").iter().all(|c| c.id != CronId::from("o")),
            "a run-once is removed after firing",
        );

        // A missing id is a no-op, not a panic.
        ws.advance_or_remove_cron(&CronId::from("ghost"), fired);
    }

    #[test]
    fn fire_due_crons_spawns_asleep_project_buffers_prompt_and_skips_future() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("forge", "/tmp/forge");

        let now = std::time::SystemTime::now();
        let past = std::time::SystemTime::UNIX_EPOCH;
        let far_future = now + std::time::Duration::from_secs(86_400);

        ws.push_cron(CronEntry {
            id: CronId::from("due"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "morning".to_owned(),
            created_at: past,
            description: None,
            last_fire: None,
            next_fire: past,
            team_role: None,
        });
        ws.push_cron(CronEntry {
            id: CronId::from("later"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "later".to_owned(),
            created_at: past,
            description: None,
            last_fire: None,
            next_fire: far_future,
            team_role: None,
        });

        ws.enable_test_dispatch_intercept();
        ws.fire_due_crons(now);
        let dispatched = ws.drain_test_dispatch_buffer();

        // The asleep project got exactly one SpawnProject - for the due
        // cron, not the future one.
        let spawns = dispatched
            .iter()
            .filter(|c| {
                matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
            })
            .count();
        assert_eq!(spawns, 1, "one spawn for the single due cron");

        // The due cron's prompt is buffered for its slot (the lead) for
        // delivery once the session reaches Connected.
        let buffered = parked_crons(&ws, "forge", None);
        assert_eq!(buffered, vec!["morning".to_owned()], "the due cron's prompt was buffered");

        // The due cron advanced past now; the future cron is untouched.
        let crons = ws.crons_for_project("forge");
        let due = crons.iter().find(|c| c.id == CronId::from("due")).expect("due present");
        assert!(due.next_fire > now, "the fired cron advanced past now");
        let later = crons.iter().find(|c| c.id == CronId::from("later")).expect("later present");
        assert_eq!(later.next_fire, far_future, "the not-yet-due cron is untouched");
    }

    #[test]
    fn boot_catch_up_fires_overdue_once_advances_persists_and_does_not_refire() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        let db = crate::store::Db::open(&dir.path().join("db.redb")).expect("open db");
        ws.install_db_for_test(db);
        ws.seed_test_project("forge", "/tmp/forge-catchup");

        let now = std::time::SystemTime::now();
        let past = std::time::SystemTime::UNIX_EPOCH;
        let far_future = now + std::time::Duration::from_secs(86_400);

        // An overdue recurring + overdue run-once + a not-yet-due one, all
        // for the seeded "forge" project. Boot catch-up is the single
        // fire_due_crons call main.rs makes at startup.
        for entry in [
            CronEntry {
                id: CronId::from("rec"),
                project_name: "forge".to_owned(),
                kind: CronKind::Recurring("*/5 * * * *".to_owned()),
                prompt: "morning".to_owned(),
                created_at: past,
                description: None,
                last_fire: None,
                next_fire: past,
                team_role: None,
            },
            CronEntry {
                id: CronId::from("once"),
                project_name: "forge".to_owned(),
                kind: CronKind::Once(past),
                prompt: "deploy".to_owned(),
                created_at: past,
                description: None,
                last_fire: None,
                next_fire: past,
                team_role: None,
            },
            CronEntry {
                id: CronId::from("future"),
                project_name: "forge".to_owned(),
                kind: CronKind::Recurring("*/5 * * * *".to_owned()),
                prompt: "later".to_owned(),
                created_at: past,
                description: None,
                last_fire: None,
                next_fire: far_future,
                team_role: None,
            },
        ] {
            ws.push_cron(entry);
        }
        assert_eq!(ws.crons_for_project("forge").len(), 3, "all three crons loaded");

        // Boot catch-up: the one call main.rs makes at startup.
        ws.enable_test_dispatch_intercept();
        ws.fire_due_crons(now);
        let first = ws.drain_test_dispatch_buffer();
        let spawns = first
            .iter()
            .filter(|c| {
                matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "forge")
            })
            .count();
        assert_eq!(spawns, 2, "boot fires the two overdue crons, not the future one");

        // Overdue recurring advanced past now + recorded last_fire; overdue
        // run-once removed; future untouched.
        let crons = ws.crons_for_project("forge");
        assert!(crons.iter().all(|c| c.id != CronId::from("once")), "overdue run-once removed");
        let rec = crons.iter().find(|c| c.id == CronId::from("rec")).expect("recurring present");
        assert!(rec.next_fire > now, "overdue recurring advanced past now");
        assert_eq!(rec.last_fire, Some(now), "the fired recurring recorded last_fire");
        let fut = crons.iter().find(|c| c.id == CronId::from("future")).expect("future present");
        assert_eq!(fut.next_fire, far_future, "the future cron is untouched");

        // The advance persisted to the store - this is what stops a double-fire.
        let persisted =
            crate::store::cron::list(ws.db.lock().as_ref().expect("db installed")).expect("list");
        assert!(persisted.iter().all(|c| c.id != CronId::from("once")), "removal persisted");
        assert!(
            persisted
                .iter()
                .find(|c| c.id == CronId::from("rec"))
                .expect("rec persisted")
                .next_fire
                > now,
            "advance persisted",
        );

        // The next tick does NOT re-fire: the advanced crons aren't due again.
        ws.fire_due_crons(now);
        let refires = ws
            .drain_test_dispatch_buffer()
            .iter()
            .filter(|c| matches!(c, crate::protocol::Command::SpawnProject { .. }))
            .count();
        assert_eq!(refires, 0, "advanced crons are not due again; no double-fire");
    }

    /// A cron fired into a running lead delivers the raw prompt as a plain
    /// user turn AND echoes a `CronPromptAppended` so the chat shows a cron
    /// block (mirrors the gotify running-target echo). Reproduce-first: the
    /// echo is absent until `deliver_cron_prompt` calls
    /// `push_cron_prompt_into_chat`.
    #[test]
    fn deliver_cron_prompt_to_running_lead_emits_cron_prompt_appended() {
        let dir = tempdir().expect("tempdir");
        let (ws, mut rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("cronlead", "/tmp/cron-lead");
        // Seed the catalog + pool so the project has a running (open) lead:
        // list_projects derives `is_open` from pool membership.
        let cwd = project_expanded_path(&ws, "cronlead");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionSlot::lead("TestOrg", "cronlead");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );

        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
        ws.enable_test_dispatch_intercept();
        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, "cronlead", None, "morning".to_owned(), false);
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));

        // The running lead receives the raw cron prompt as a plain user turn.
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, Command::Prompt { key, text, .. } if key == &lead_key && text == "morning"
            )),
            "the running lead receives the fired cron prompt verbatim",
        );

        // AND the delivery echoes a CronPromptAppended so the chat shows a block.
        let echoed = drain_updates(&mut rx).into_iter().any(|u| {
            matches!(
                u,
                SessionUpdate::CronPromptAppended { key, text }
                    if key == lead_key && text == "morning"
            )
        });
        assert!(echoed, "a running-lead cron fire emits a CronPromptAppended echo");
    }

    #[test]
    fn deliver_worker_cron_to_a_live_worker_prompts_the_worker() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("proj", "/tmp/wc-live");
        let key = ws.list_projects().into_iter().find(|v| v.name == "proj").expect("view").key;
        ws.insert_live_worker(&key, live_worker_entry("proj", "reviewer"));
        let worker_key = SessionSlot::worker("TestOrg", "proj", "reviewer");
        ws.mark_session_connected_for_test(&worker_key, "worker-uuid");

        ws.enable_test_dispatch_intercept();
        let outcome = crate::spawn::deliver_cron_prompt(
            &ws,
            "proj",
            Some("reviewer"),
            "review the diff".to_owned(),
            false,
        );
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, Command::Prompt { key, text, .. }
                    if key == &worker_key && text == "review the diff"
            )),
            "a live worker's cron fires straight into the worker",
        );
    }

    #[test]
    fn deliver_asleep_worker_cron_buffers_and_wakes_the_project() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        // The row is what gives the slot a session while it is asleep; without it
        // the fire router collects the cron instead.
        let key = seed_project_with_a_real_root(&ws, &dir);
        seed_worker_row(&ws, &key, "reviewer");

        ws.enable_test_dispatch_intercept();
        let outcome = crate::spawn::deliver_cron_prompt(
            &ws,
            "proj",
            Some("reviewer"),
            "nightly".to_owned(),
            false,
        );
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, Command::SpawnProject { project_name, .. } if project_name == "proj"
            )),
            "an asleep worker cron wakes the whole project via SpawnProject",
        );
        let buffered = parked_crons(&ws, "proj", Some("reviewer"));
        assert_eq!(buffered, vec!["nightly".to_owned()], "buffered for the worker owner");
    }

    /// A cron for a worker that's a live entry but still Spawning
    /// (session_id None) must NOT dispatch a bare Command::Prompt
    /// (dropped) - the gate in `live_cron_slot` routes it to the
    /// owner-keyed buffer, drained on the worker's own Connected.
    #[test]
    fn deliver_cron_to_spawning_worker_buffers_via_owner() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let key = seed_project_with_a_real_root(&ws, &dir);
        seed_worker_row(&ws, &key, "reviewer");
        let worker_key = SessionSlot::worker("TestOrg", "proj", "reviewer");
        ws.insert_live_worker(&key, live_worker_entry("proj", "reviewer"));
        // Registered but not connected: session_id stays None.
        ws.register_domain_session(worker_key.clone(), None);

        ws.enable_test_dispatch_intercept();
        let outcome = crate::spawn::deliver_cron_prompt(
            &ws,
            "proj",
            Some("reviewer"),
            "nightly".to_owned(),
            false,
        );
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            !dispatched.iter().any(|c| matches!(
                c, Command::Prompt { key, .. } if key == &worker_key
            )),
            "no bare Prompt to the still-spawning worker (would be dropped)",
        );
        let buffered = parked_crons(&ws, "proj", Some("reviewer"));
        assert_eq!(
            buffered,
            vec!["nightly".to_owned()],
            "buffered for the worker's Connected drain"
        );
    }

    /// A worker spawned this second is live before it connects, and a git
    /// worker's worktree is created during that same window. Judged by its
    /// row alone it looks unwakeable, and the fire would be dropped; it is
    /// only not there YET, so the prompt parks for its own Connected drain.
    #[test]
    fn deliver_worker_cron_parks_for_an_owner_that_is_still_spawning() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let key = seed_project_with_a_real_root(&ws, &dir);
        // A git worker whose worktree claude has not created yet: the row
        // alone says this one cannot start.
        ws.record_worker_row(&key, "reviewer", "reviewer-uuid", "c", None, None, false, true)
            .expect("seed the row");
        ws.insert_live_worker(&key, live_worker_entry("proj", "reviewer"));

        ws.enable_test_dispatch_intercept();
        let outcome = crate::spawn::deliver_cron_prompt(
            &ws,
            "proj",
            Some("reviewer"),
            "nightly".to_owned(),
            false,
        );
        assert!(
            matches!(outcome, crate::spawn::CronFireOutcome::Delivered),
            "a worker still spawning is not unwakeable - its worktree is being created \
             along with it; got {}",
            outcome_name(&outcome),
        );
        assert_eq!(
            parked_crons(&ws, "proj", Some("reviewer")),
            vec!["nightly".to_owned()],
            "the prompt parks for the spawning worker's Connected drain",
        );
    }

    #[tokio::test]
    async fn deliver_asleep_dynamic_worker_cron_buffers_and_wakes_the_project() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let key = seed_project_with_a_real_root(&ws, &dir);
        // "scratch" exists only via its persisted worker row.
        seed_worker_row(&ws, &key, "scratch");

        ws.enable_test_dispatch_intercept();
        let outcome = crate::spawn::deliver_cron_prompt(
            &ws,
            "proj",
            Some("scratch"),
            "hourly".to_owned(),
            false,
        );
        assert!(matches!(outcome, crate::spawn::CronFireOutcome::Delivered));
        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, Command::SpawnProject { project_name, .. } if project_name == "proj"
            )),
            "an asleep dynamic worker cron wakes the project too",
        );
        assert_eq!(
            parked_crons(&ws, "proj", Some("scratch")).len(),
            1,
            "buffered for the dynamic worker owner"
        );
    }

    #[test]
    fn deliver_worker_cron_with_owner_conclusively_gone_is_target_gone() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        // Db open, empty dynamic_workers: the row is the only thing that
        // could bring this owner back, so "ghost" is conclusively absent
        // and its cron must be collected rather than buffered into a
        // bucket nothing will ever drain.
        ws.seed_test_project("proj", "/tmp/wc-gone");
        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, "proj", Some("ghost"), "x".to_owned(), false);
        assert!(
            matches!(outcome, crate::spawn::CronFireOutcome::TargetGone),
            "a label with no dynamic_workers row is conclusively gone",
        );
    }

    /// A cron's owner can have a row and still be unwakeable: the row
    /// says it runs in a worktree, and the worktree is gone, so the wave
    /// skips it and nothing is left to drain a parked prompt. Buffering
    /// would grow the bucket once per fire until process exit, and
    /// reporting Delivered would advance the watermark past a prompt that
    /// never reached anyone.
    ///
    /// The control half is the same fire once the worktree stands: it is
    /// delivered and parked, so the refusal above is the missing
    /// directory and not the fixture.
    #[test]
    fn deliver_worker_cron_with_an_unwakeable_owner_is_neither_parked_nor_delivered() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let project_dir = tempdir().expect("project dir");
        ws.seed_test_project("proj", &project_dir.path().to_string_lossy());
        let key = ws.project_key_for_name("proj").expect("seeded project");
        ws.record_worker_row(&key, "steward", "steward-uuid", "c", None, None, false, true)
            .expect("seed the worker's row");

        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, "proj", Some("steward"), "x".to_owned(), false);
        assert!(
            matches!(outcome, crate::spawn::CronFireOutcome::TargetCannotBeWoken { .. }),
            "an owner whose worktree is gone cannot be woken, so its fire is not delivered",
        );
        assert_eq!(
            parked_crons(&ws, "proj", Some("steward")).len(),
            0,
            "nothing parks for an owner that can never drain the bucket",
        );

        std::fs::create_dir_all(
            project_dir.path().join(".claude").join("worktrees").join("steward"),
        )
        .expect("restore the worktree");
        let delivered =
            crate::spawn::deliver_cron_prompt(&ws, "proj", Some("steward"), "x".to_owned(), false);
        assert!(
            matches!(delivered, crate::spawn::CronFireOutcome::Delivered),
            "the same fire is delivered once the worktree is back, so the refusal above \
             was the missing directory and not the fixture",
        );
        assert_eq!(
            parked_crons(&ws, "proj", Some("steward")).len(),
            1,
            "and its prompt is parked for the owner to drain on connect",
        );
    }

    #[test]
    fn deliver_worker_cron_leaves_it_when_the_owner_check_cannot_read() {
        // No db installed, so the durable-worker lookup fails: absence can't
        // be confirmed, so the cron must be left (retried next tick), not
        // deleted as owner-gone.
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("proj", "/tmp/wc-unknown");
        let outcome =
            crate::spawn::deliver_cron_prompt(&ws, "proj", Some("scratch"), "x".to_owned(), false);
        assert!(
            matches!(outcome, crate::spawn::CronFireOutcome::DispatchFailed),
            "a failed owner check leaves the cron for the next tick, not TargetGone",
        );
    }

    /// A spawn needs the account map settled, and one half of that is the
    /// gateway listener, which the boot catch-up can outrun. That refusal
    /// is transient, so the fire stays unconsumed for the next tick
    /// rather than advancing past a prompt that never landed.
    #[test]
    fn deliver_cron_with_the_listener_unbound_leaves_the_fire_for_the_next_tick() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("proj", "/tmp/wc-gate");
        ws.enable_test_dispatch_intercept();

        // Control: with the listener bound this same fire is delivered, so
        // the refusal below is the map and not the owner check.
        let bound = crate::spawn::deliver_cron_prompt(&ws, "proj", None, "x".to_owned(), false);
        assert!(
            matches!(bound, crate::spawn::CronFireOutcome::Delivered),
            "the same fire with the listener bound is delivered",
        );
        ws.drain_test_dispatch_buffer();

        ws.seed_test_gateway_ready(false);
        let unbound = crate::spawn::deliver_cron_prompt(&ws, "proj", None, "x".to_owned(), false);
        assert!(
            matches!(unbound, crate::spawn::CronFireOutcome::DispatchFailed),
            "an unbound listener is a transient refusal, not a delivered fire",
        );
        assert_eq!(
            parked_crons(&ws, "proj", None).len(),
            1,
            "the deferred fire parked nothing, so the retry parks it once rather than twice",
        );
    }

    /// The other half of an unsettled map is the accounts themselves: the
    /// walk skips every account still `Loading`, so a project whose only
    /// account has not settled is refused there. Also pins the boundary -
    /// `Bailed` is terminal, so it settles the map and the walk falls back
    /// to it rather than refusing.
    #[test]
    fn deliver_cron_with_an_unsettled_account_map_leaves_the_fire_for_the_next_tick() {
        let (ws, _dir) = workspace_with_one_unsettled_account();
        ws.enable_test_dispatch_intercept();

        // Nothing runs the account loader here, so the one account starts
        // `Loading`: the fire is left rather than parked.
        let unsettled = crate::spawn::deliver_cron_prompt(&ws, "proj", None, "x".to_owned(), false);
        assert_eq!(
            outcome_name(&unsettled),
            "DispatchFailed",
            "an account still loading is a transient refusal, not a delivered fire",
        );
        assert!(
            parked_crons(&ws, "proj", None).is_empty(),
            "and nothing is parked, so the retry parks it once rather than twice",
        );

        // Controls: the same fire is delivered once the map settles, so
        // the refusal above is the account map and not the owner check.
        ws.seed_test_ready_account("acct-a");
        let ready = crate::spawn::deliver_cron_prompt(&ws, "proj", None, "x".to_owned(), false);
        assert_eq!(
            outcome_name(&ready),
            "Delivered",
            "the same fire with the account settled is delivered",
        );

        // `Bailed` is terminal too, so it settles the map as well, and the
        // walk falls back to it as the last resort rather than refusing.
        ws.seed_test_account_state("acct-a", forge_gateway::LoadingState::Bailed);
        let bailed = crate::spawn::deliver_cron_prompt(&ws, "proj", None, "x".to_owned(), false);
        assert_eq!(
            outcome_name(&bailed),
            "Delivered",
            "a Bailed account settles the map and the walk falls back to it",
        );
        assert_eq!(
            parked_crons(&ws, "proj", None).len(),
            2,
            "both settled fires parked their prompt",
        );
    }

    /// A cooldown empties the project's walk until its reset, which is
    /// transient the same way an unsettled map is, so the fire stays due
    /// rather than parking a prompt the refusal would expire.
    #[test]
    fn deliver_cron_during_a_cooldown_leaves_the_fire_for_the_next_tick() {
        let (ws, _dir) = workspace_with_one_unsettled_account();
        ws.seed_test_ready_account("acct-a");
        ws.enable_test_dispatch_intercept();

        // Control: with the one account serving and not cooling, the same
        // fire is delivered.
        let served = crate::spawn::deliver_cron_prompt(&ws, "proj", None, "x".to_owned(), false);
        assert_eq!(
            outcome_name(&served),
            "Delivered",
            "the same fire with the account serving is delivered",
        );

        // The usage probe's own verdict is the public way to cool an
        // account, and it takes epoch seconds.
        let reset_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs()
            + 60;
        ws.gateway
            .report_probe_limit(&forge_gateway::AccountKey("acct-a".to_owned()), Some(reset_at));
        let cooling = crate::spawn::deliver_cron_prompt(&ws, "proj", None, "x".to_owned(), false);
        assert_eq!(
            outcome_name(&cooling),
            "DispatchFailed",
            "a cooling walk is a transient refusal, not a delivered fire",
        );
        assert_eq!(
            parked_crons(&ws, "proj", None).len(),
            1,
            "the deferred fire parked nothing, so the retry parks it once rather than twice",
        );
    }

    /// The outcome's own name, so a failure says which variant it got
    /// rather than only that it was not the expected one.
    fn outcome_name(outcome: &crate::spawn::CronFireOutcome) -> &'static str {
        match outcome {
            crate::spawn::CronFireOutcome::Delivered => "Delivered",
            crate::spawn::CronFireOutcome::TargetGone => "TargetGone",
            crate::spawn::CronFireOutcome::TargetCannotBeWoken { .. } => "TargetCannotBeWoken",
            crate::spawn::CronFireOutcome::DispatchFailed => "DispatchFailed",
        }
    }

    /// A workspace over a forge.toml declaring one account and one project
    /// with the model that account serves, so the account walk is the
    /// thing a spawn would reach. Nothing runs the account loader in a
    /// test, so the account map starts unsettled. The tempdir must
    /// outlive the caller.
    ///
    /// `new_for_test` rather than a stubbed workspace on purpose: the
    /// stubs carry an EMPTY account pool, where `all_loaded` is vacuously
    /// true and `set_loading` is a no-op, so they cannot tell the two
    /// halves of the predicate apart.
    fn workspace_with_one_unsettled_account()
    -> (std::sync::Arc<crate::Workspace>, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let forge_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge dir");
        std::fs::write(
            forge_dir.join("forge.toml"),
            r#"
[[orgs]]
name = "TestOrg"
accounts = ["acct-a"]

[[orgs.projects]]
name = "proj"
path = "/tmp/wc-unsettled"
model = "claude-sonnet-5"

[[accounts]]
display_name = "acct-a"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let ws = std::sync::Arc::new(
            crate::Workspace::new_for_test(dir.path().to_owned()).expect("boot from the fixture"),
        );
        (ws, dir)
    }

    #[test]
    fn deliver_cron_marks_an_overdue_fire_as_missed() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/wc-missed");
        let cwd = project_expanded_path(&ws, "proj");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionSlot::lead("TestOrg", "proj");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );

        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");
        ws.enable_test_dispatch_intercept();
        crate::spawn::deliver_cron_prompt(&ws, "proj", None, "standup".to_owned(), true);
        crate::spawn::deliver_cron_prompt(&ws, "proj", None, "standup".to_owned(), false);
        let dispatched = ws.drain_test_dispatch_buffer();
        let texts: Vec<String> = dispatched
            .iter()
            .filter_map(|c| match c {
                Command::Prompt { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts.contains(&"[missed cron] standup".to_owned()),
            "an overdue fire is delivered with the missed marker",
        );
        assert!(texts.contains(&"standup".to_owned()), "an on-time fire has no marker");
    }

    #[test]
    fn fire_due_crons_missed_threshold_is_two_ticks() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/wc-thresh");
        let cwd = project_expanded_path(&ws, "proj");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionSlot::lead("TestOrg", "proj");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent {
                handle: Arc::new(handle),
                account: AccountKey("test".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-session".to_owned(),
            },
        );
        ws.mark_session_connected_for_test(&lead_key, "lead-uuid");

        let now = std::time::SystemTime::now();
        let cron = |id: &str, prompt: &str, next_fire| CronEntry {
            id: CronId::from(id),
            project_name: "proj".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: prompt.to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire,
            team_role: None,
        };
        // Due one tick ago: within the jitter window, on-time. Due three
        // ticks ago: a genuine catch-up, missed.
        ws.push_cron(cron("recent", "recent", now - std::time::Duration::from_secs(60)));
        ws.push_cron(cron("stale", "stale", now - std::time::Duration::from_secs(180)));

        ws.enable_test_dispatch_intercept();
        ws.fire_due_crons(now);
        let texts: Vec<String> = ws
            .drain_test_dispatch_buffer()
            .iter()
            .filter_map(|c| match c {
                Command::Prompt { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"recent".to_owned()), "a one-tick-late fire is on-time");
        assert!(
            texts.contains(&"[missed cron] stale".to_owned()),
            "a three-tick-late fire is marked missed",
        );
    }

    #[test]
    fn fire_due_crons_removes_a_cron_whose_project_is_gone() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        // No db installed, so push_cron stays in memory - no config dir needed.
        let (ws, _rx) = Workspace::testing_stub();
        let now = std::time::SystemTime::now();

        ws.push_cron(CronEntry {
            id: CronId::from("orphan"),
            project_name: "deleted-project".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "p".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH, // overdue -> due now
            team_role: None,
        });

        ws.enable_test_dispatch_intercept();
        ws.fire_due_crons(now);
        let dispatched = ws.drain_test_dispatch_buffer();

        assert!(
            dispatched.iter().all(|c| !matches!(c, crate::protocol::Command::SpawnProject { .. })),
            "a cron whose project is gone gets no spawn",
        );
        assert!(
            ws.crons_for_project("deleted-project").is_empty(),
            "an overdue cron whose project left forge.toml is removed, not advanced forever",
        );
    }

    /// The router's fate for a fire whose owner cannot be woken. A
    /// recurring cron has another slot coming, so this fire is dropped and
    /// the schedule advances. A one-shot has no later slot and advancing
    /// removes it, so it stays due: dropping it would throw away the only
    /// prompt that cron will ever carry.
    ///
    /// Neither may park - that bucket has nothing to drain it.
    #[test]
    fn fire_due_crons_advances_recurring_and_keeps_a_one_shot_when_the_owner_cannot_be_woken() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let project_dir = tempdir().expect("project dir");
        ws.seed_test_project("proj", &project_dir.path().to_string_lossy());
        let key = ws.project_key_for_name("proj").expect("seeded project");
        // The row says the worker runs in a worktree that is not there, so
        // the wave would skip it.
        ws.record_worker_row(&key, "steward", "steward-uuid", "c", None, None, false, true)
            .expect("seed the stranded worker's row");

        let now = std::time::SystemTime::now();
        let past = std::time::SystemTime::UNIX_EPOCH;
        for (id, kind) in [
            ("recurring", CronKind::Recurring("*/5 * * * *".to_owned())),
            ("one-shot", CronKind::Once(past)),
        ] {
            ws.push_cron(CronEntry {
                id: CronId::from(id),
                project_name: "proj".to_owned(),
                kind,
                prompt: "p".to_owned(),
                created_at: past,
                description: None,
                last_fire: None,
                next_fire: past,
                team_role: Some("steward".to_owned()),
            });
        }

        ws.enable_test_dispatch_intercept();
        ws.fire_due_crons(now);

        let after = ws.crons_for_project("proj");
        let ids: Vec<String> = after.iter().map(|c| c.id.0.clone()).collect();
        let recurring = after.iter().find(|c| c.id == CronId::from("recurring"));
        assert!(
            recurring.is_some_and(|c| c.last_fire.is_some()),
            "a recurring cron's fire is dropped and its schedule advances, rather than \
             the entry being removed; entries left {ids:?}",
        );
        assert!(
            after.iter().any(|c| c.id == CronId::from("one-shot")),
            "a one-shot has no later slot, so it stays due rather than being removed \
             along with its prompt; entries left {ids:?}",
        );
        assert_eq!(
            parked_crons(&ws, "proj", Some("steward")).len(),
            0,
            "nothing parks for an owner that can never drain the bucket",
        );
    }

    #[test]
    fn advance_or_remove_cron_removes_never_occurring_recurring() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        // "0 0 30 2 *" (Feb 30) has no upcoming occurrence, so
        // next_fire_after returns None and the entry is removed (with a
        // warn) rather than left stuck at a stale next_fire.
        ws.push_cron(CronEntry {
            id: CronId::from("impossible"),
            project_name: "forge".to_owned(),
            kind: CronKind::Recurring("0 0 30 2 *".to_owned()),
            prompt: "p".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        });
        ws.advance_or_remove_cron(&CronId::from("impossible"), std::time::SystemTime::now());
        assert!(
            ws.crons_for_project("forge").is_empty(),
            "a recurring cron with no upcoming occurrence is removed, not left stuck",
        );
    }

    /// The swap-once guard: the first `start_cron_scheduler` flips
    /// `cron_scheduler_started` and runs exactly one tick loop, and a
    /// second call is a no-op. The due cron is the observable that the
    /// guard opens - a guard that never opens fires nothing.
    #[tokio::test(start_paused = true)]
    async fn start_cron_scheduler_fires_a_due_cron_once_despite_a_second_start() {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/wc-scheduler-once");

        ws.push_cron(CronEntry {
            id: CronId::from("due"),
            project_name: "proj".to_owned(),
            kind: CronKind::Recurring("*/5 * * * *".to_owned()),
            prompt: "tick".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH, // overdue -> due at the first tick
            team_role: None,
        });

        ws.enable_test_dispatch_intercept();
        ws.start_cron_scheduler();
        assert!(
            ws.cron_scheduler_started.load(std::sync::atomic::Ordering::Acquire),
            "the first start flips the swap-once guard",
        );

        // Let the spawned task poll once so its interval is registered
        // at t=0 (first tick at +60s) before the clock moves.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::time::advance(CRON_TICK_INTERVAL).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let first_tick = count_proj_spawns(&ws);
        assert_eq!(
            first_tick, 1,
            "the first start runs the scheduler; a guard that never opens fires nothing",
        );

        // A second start must not spawn a second tick loop. Fires
        // cannot tell two loops apart - ticks run sequentially here and
        // the first loop's advance hides every due cron from the second
        // (on the multi-thread runtime their ticks race, which is what
        // the guard prevents) - so its only trace is another task-held
        // Weak.
        let weak_before = Arc::downgrade(&ws).weak_count();
        ws.start_cron_scheduler();
        let weak_after = Arc::downgrade(&ws).weak_count();
        assert_eq!(weak_after, weak_before, "a second start must not spawn a second tick loop");
    }

    fn count_proj_spawns(ws: &Workspace) -> usize {
        ws.drain_test_dispatch_buffer()
            .iter()
            .filter(|c| {
                matches!(c, crate::protocol::Command::SpawnProject { project_name, .. }
                    if project_name == "proj")
            })
            .count()
    }

    /// The tick loop holds the workspace only weakly: once the caller's
    /// `Arc` is gone the workspace drops even though the task is parked
    /// on its next tick. Advancing past a tick drives the
    /// upgrade-failure exit; a task that moved a strong `Arc` in would
    /// keep the workspace alive and fail the upgrade assert.
    #[tokio::test(start_paused = true)]
    async fn the_scheduler_task_does_not_hold_the_workspace_alive() {
        let dir = tempdir().expect("tempdir");
        let weak = {
            let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
            ws.start_cron_scheduler();
            // Poll the task once so its interval is registered and it
            // sits parked on the next tick, holding only the Weak.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            Arc::downgrade(&ws)
        };
        tokio::time::advance(CRON_TICK_INTERVAL * 2).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(weak.upgrade().is_none(), "the scheduler task must hold only a Weak");
    }
}

//! The cron cluster on [`Workspace`]: the durable-cron store accessor
//! and its mutation helpers, the asleep-owner prompt buffers, the fire
//! router, the scheduler task, and the cross-crate seed helper.
//!
//! Everything here stays on `Workspace` as a second `impl` block, so
//! every caller (the boot path in [`crate::workspace`], the
//! `mcp::cron` facade, `spawn::deliver_cron_prompt`, forge-tui's boot)
//! keeps its path. The `crons`, `pending_cron_by_owner`,
//! `cron_scheduler_started` and `update_tx` fields these methods own
//! are `pub(crate)` for the same reason `db` is: so this sibling
//! module can reach them without a wrapper. Schedule math lives in
//! [`crate::mcp::cron::schedule`]; the MCP tool surface in
//! [`crate::mcp::cron`]; delivery in [`crate::spawn`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tracing::Instrument;

use crate::protocol::SessionUpdate;
use crate::target::{ProjectKey, SessionKey};
use crate::workspace::Workspace;

/// How often the cron scheduler wakes to fire due crons. Minute
/// granularity matches the cron-expression resolution.
const CRON_TICK_INTERVAL: Duration = Duration::from_secs(60);

/// A cron prompt buffered for an asleep owner: the raw prompt plus whether
/// this fire is overdue (delivered with a missed marker on drain).
#[derive(Debug)]
pub(crate) struct PendingCron {
    pub text: String,
    pub missed: bool,
}

/// Cron prompts buffered for asleep owners, keyed by `(project, team_role)`.
pub(crate) type PendingCronMap = HashMap<(String, Option<String>), Vec<PendingCron>>;

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

    /// Remove the cron `id` in `project_name` regardless of owner, persist,
    /// and report whether an entry was removed. Backs the fire-router's
    /// owner-gone removal; the owner-scoped `cron__delete` uses
    /// [`Self::remove_cron_owned_by`].
    pub(crate) fn remove_cron(&self, project_name: &str, id: &forge_primitives::CronId) -> bool {
        self.with_crons_mut(|crons| {
            let before = crons.len();
            crons.retain(|c| !(c.id == *id && c.project_name == project_name));
            crons.len() != before
        })
    }

    /// Remove the cron `id` in `project_name` only when its owner matches
    /// `owner` (`None` = a lead cron, `Some(label)` = that worker's),
    /// persist, and report whether an entry was removed. Backs the
    /// owner-scoped `cron__delete` so a caller deletes only its own crons.
    pub(crate) fn remove_cron_owned_by(
        &self,
        project_name: &str,
        id: &forge_primitives::CronId,
        owner: Option<&str>,
    ) -> bool {
        self.with_crons_mut(|crons| {
            let before = crons.len();
            crons.retain(|c| {
                !(c.id == *id && c.project_name == project_name && c.team_role.as_deref() == owner)
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

    /// Buffer a cron prompt for an asleep owner, keyed by
    /// `(project, team_role)`; drained on the owner's first `Connected`.
    pub(crate) fn buffer_cron_for_owner(
        &self,
        project: &str,
        team_role: Option<&str>,
        text: String,
        missed: bool,
    ) {
        self.pending_cron_by_owner
            .lock()
            .entry((project.to_owned(), team_role.map(str::to_owned)))
            .or_default()
            .push(PendingCron { text, missed });
    }

    /// Take (and clear) the cron prompts buffered for the connecting
    /// session's owner - `(project of cwd, the session's worker label, or
    /// None for a lead)`. Empty when nothing was buffered or the cwd is
    /// under no project.
    pub(crate) fn take_pending_crons_for_session(
        &self,
        session_key: &SessionKey,
        cwd: &str,
    ) -> Vec<PendingCron> {
        let Some(project) = self.project_name_for_path(cwd) else { return Vec::new() };
        let team_role = self.worker_label_for_session(session_key);
        self.pending_cron_by_owner.lock().remove(&(project, team_role)).unwrap_or_default()
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
                        // cron.toml. Don't silently drop it (hard
                        // rule #13).
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
        for id in &due {
            let Some(cron) = snapshot.iter().find(|c| &c.id == id) else { continue };
            // Overdue by more than two ticks: forge or the owner was down
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
                // Owner gone (project removed from forge.toml, or a worker
                // label that is no longer static or a durable dynamic row):
                // remove the cron rather than advance a dead entry forever.
                CronFireOutcome::TargetGone => {
                    tracing::warn!(
                        target: "forge_workspace::crons",
                        project = %cron.project_name,
                        cron_id = %id,
                        "cron owner gone; removing the cron",
                    );
                    self.remove_cron(&cron.project_name, id);
                }
                // Command channel closed (shutting down): leave the cron
                // due so the next boot catch-up re-fires it - don't consume
                // a fire that never handed off.
                CronFireOutcome::DispatchFailed => {
                    tracing::warn!(
                        target: "forge_workspace::crons",
                        project = %cron.project_name,
                        cron_id = %id,
                        "cron fire dispatch failed; leaving it due for the next boot",
                    );
                    let _ = self.update_tx.send(SessionUpdate::ServiceStatus {
                        severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
                        message: format!(
                            "Cron in '{}' could not fire (its session is shutting down); it stays due for the next boot",
                            cron.project_name
                        ),
                    });
                }
            }
        }
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

    use crate::account::AccountKey;
    use crate::protocol::Command;
    use crate::workspace::PooledAgent;

    fn dynamic_worker_row(
        project: &str,
        label: &str,
    ) -> crate::store::dynamic_workers::DynamicWorker {
        crate::store::dynamic_workers::DynamicWorker {
            project_key: project.to_owned(),
            label: label.to_owned(),
            charter: format!("charter for {label}"),
            kick: Some(format!("kick for {label}")),
            resume_kick: None,
            interactive: false,
        }
    }

    fn live_worker_entry(label: &str, key: &str) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.to_owned(),
            charter: "c".to_owned(),
            session_key: SessionKey::from_session_id(key),
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".to_owned(),
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

        // The due cron's prompt is buffered for its owner (the lead) for
        // delivery once the session reaches Connected.
        let buffered: Vec<String> = ws
            .pending_cron_by_owner
            .lock()
            .get(&("forge".to_owned(), None))
            .map(|v| v.iter().map(|p| p.text.clone()).collect())
            .unwrap_or_default();
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
        let lead_key = SessionKey::from_session_id("lead-uuid");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent { handle: Arc::new(handle), account: AccountKey("test".to_owned()) },
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
                SessionUpdate::CronPromptAppended { session_id, text }
                    if session_id == lead_key.as_str() && text == "morning"
            )
        });
        assert!(echoed, "a running-lead cron fire emits a CronPromptAppended echo");
    }

    #[test]
    fn deliver_worker_cron_to_a_live_worker_prompts_the_worker() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("proj", "/tmp/wc-live");
        let key = ws.list_projects().into_iter().find(|v| v.name == "proj").expect("view").key;
        ws.insert_live_worker(&key, live_worker_entry("reviewer", "worker-uuid"));
        let worker_key = SessionKey::from_session_id("worker-uuid");
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
        ws.seed_test_project("proj", "/tmp/wc-static");
        // The row is what makes the owner exist while asleep; without it
        // the fire router collects the cron instead.
        let key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "proj")
            .map(|v| v.key)
            .expect("seeded project");
        let _ = ws.persist_dynamic_worker(&crate::store::dynamic_workers::DynamicWorker {
            project_key: key.as_str().to_owned(),
            label: "reviewer".to_owned(),
            charter: "review".to_owned(),
            kick: None,
            resume_kick: None,
            interactive: false,
        });

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
        let buffered: Vec<String> = ws
            .pending_cron_by_owner
            .lock()
            .get(&("proj".to_owned(), Some("reviewer".to_owned())))
            .map(|v| v.iter().map(|p| p.text.clone()).collect())
            .unwrap_or_default();
        assert_eq!(buffered, vec!["nightly".to_owned()], "buffered for the worker owner");
    }

    /// A cron for a worker that's a live entry but still Spawning
    /// (session_id None) must NOT dispatch a bare Command::Prompt
    /// (dropped) - the session_id gate in live_cron_owner routes it to
    /// the owner-keyed buffer, drained on the worker's own Connected.
    #[test]
    fn deliver_cron_to_spawning_worker_buffers_via_owner() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("proj", "/tmp/wc-spawning");
        let key = ws.list_projects().into_iter().find(|v| v.name == "proj").expect("view").key;
        let _ = ws.persist_dynamic_worker(&crate::store::dynamic_workers::DynamicWorker {
            project_key: key.as_str().to_owned(),
            label: "reviewer".to_owned(),
            charter: "review".to_owned(),
            kick: None,
            resume_kick: None,
            interactive: false,
        });
        let worker_key = SessionKey::from_session_id("worker-spawning-cron");
        ws.insert_live_worker(&key, live_worker_entry("reviewer", "worker-spawning-cron"));
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
        let buffered: Vec<String> = ws
            .pending_cron_by_owner
            .lock()
            .get(&("proj".to_owned(), Some("reviewer".to_owned())))
            .map(|v| v.iter().map(|p| p.text.clone()).collect())
            .unwrap_or_default();
        assert_eq!(
            buffered,
            vec!["nightly".to_owned()],
            "buffered for the worker's Connected drain"
        );
    }

    #[tokio::test]
    async fn deliver_asleep_dynamic_worker_cron_buffers_and_wakes_the_project() {
        let (ws, _rx) = Workspace::testing_stub();
        let dir = tempdir().expect("tempdir");
        ws.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        ws.seed_test_project("proj", "/tmp/wc-dyn");
        let key = ws.list_projects().into_iter().find(|v| v.name == "proj").expect("view").key;
        // "scratch" exists only via its dynamic_workers row.
        let _ = ws.persist_dynamic_worker(&dynamic_worker_row(key.as_str(), "scratch"));

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
        let count = ws
            .pending_cron_by_owner
            .lock()
            .get(&("proj".to_owned(), Some("scratch".to_owned())))
            .map_or(0, Vec::len);
        assert_eq!(count, 1, "buffered for the dynamic worker owner");
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

    #[test]
    fn deliver_cron_marks_an_overdue_fire_as_missed() {
        let dir = tempdir().expect("tempdir");
        let (ws, _rx) = Workspace::testing_stub_with_config_dir(dir.path().to_owned());
        ws.seed_test_project("proj", "/tmp/wc-missed");
        let cwd = project_expanded_path(&ws, "proj");
        ws.record_connected_session(&cwd, "lead-uuid", None);
        let lead_key = SessionKey::from_session_id("lead-uuid");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent { handle: Arc::new(handle), account: AccountKey("test".to_owned()) },
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
        let lead_key = SessionKey::from_session_id("lead-uuid");
        let (handle, _agent_rx) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            lead_key.clone(),
            PooledAgent { handle: Arc::new(handle), account: AccountKey("test".to_owned()) },
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
}

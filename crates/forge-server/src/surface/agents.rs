//! `agents()`: every project's lead and workers as one row type.

use std::collections::HashMap;
use std::ops::Range;
use std::time::SystemTime;

use forge_primitives::{SessionLifecycleState, SessionSlot, WorkerLiveness};
use forge_workspace::{ProjectKey, Workspace};

use crate::surface::ViewSurface;

/// One agent as a view reads it. A lead and a worker are the same row:
/// which project spawned it, and what nests under what, are the view's
/// business rather than the row's.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentRow {
    pub slot: SessionSlot,
    /// The worker's own label, or `"lead"` for a project's own agent.
    pub label: String,
    pub lifecycle: SessionLifecycleState,
    pub has_background_work: bool,
    pub pending: Option<PendingKind>,
    /// How many prompts the session is holding. Zero when it holds none,
    /// and more than one when a prompt waits behind another.
    pub pending_depth: usize,
    pub last_activity: Option<SystemTime>,
    /// Why this session failed, from the core's own record: a worker's
    /// spawn diagnostic, or the failure the slot's last connection left
    /// behind. `None` for a session that has not failed.
    pub reason: Option<String>,
    /// When this session's newest turn ended in failure, which the rail
    /// marks until the reader opens the seat. `None` when nothing failed,
    /// when the reader cancelled that turn, or when a newer turn has since
    /// been committed.
    pub failed_turn: Option<SystemTime>,
    /// Whether a session on this seat ended and no reader has been shown it
    /// since. The row's failure is the durable trace of a crash nobody
    /// watched, and this is what tells a seat that never came up (which
    /// stays parked with its reason) from one that should be started again.
    pub died: bool,
}

/// What a session is waiting on a person for. The core's own kind rather
/// than a second enum of the same two arms, so the row and the read cannot
/// drift apart.
pub use forge_workspace::PendingInteractionKind as PendingKind;

/// Every project's agents, in the order `list_projects` returned the
/// projects.
pub struct Agents {
    rows: Vec<AgentRow>,
    /// Where each project's rows sit in `rows`, so `for_project` answers
    /// with a slice rather than a walk of the fleet.
    spans: HashMap<ProjectKey, Range<usize>>,
}

impl Agents {
    pub(super) fn collect(workspace: &Workspace) -> Self {
        let labels = workspace.worker_labels_by_project();
        let live = workspace.live_worker_states_by_project();
        let mut rows = Vec::new();
        let mut spans = HashMap::new();

        for project in workspace.list_projects() {
            let lead = SessionSlot::lead(&project.org, &project.name);
            // A project with no session behind it is the home's Start row
            // rather than an agent, so it contributes no rows at all - but a
            // row is still owed while a failure stands, and while a death no
            // reader has been shown stands. **The death is its own gate**:
            // the recovering connect clears the failure by design, so a seat
            // revived under a standing death would otherwise lose its row
            // between the revival and the next show, and the show after that
            // would draw the old death on a session that never died.
            if workspace.domain_session_for(&lead).is_none()
                && workspace.spawn_failure(&lead).is_none()
                && !workspace.death_stands(&lead)
            {
                continue;
            }
            let start = rows.len();
            rows.push(row_for(workspace, lead, "lead".to_owned(), None, None));
            for label in labels.get(&project.key).into_iter().flatten() {
                let slot = SessionSlot::worker(&project.org, &project.name, label);
                let state = live
                    .get(&project.key)
                    .and_then(|states| states.iter().find(|s| &s.label == label));
                rows.push(row_for(
                    workspace,
                    slot,
                    label.clone(),
                    state.map(|state| state.status),
                    state.and_then(|state| state.diagnostic.clone()),
                ));
            }
            spans.insert(project.key.clone(), start..rows.len());
        }

        Self { rows, spans }
    }

    /// The rows for `key`, lead first, empty for a project that
    /// contributed none.
    pub fn for_project(&self, key: &ProjectKey) -> &[AgentRow] {
        // `rows` is contiguous per project because `collect` builds it one
        // project at a time, so the span is the whole answer.
        self.spans.get(key).map_or(&[], |span| &self.rows[span.clone()])
    }

    /// Every row, across every project.
    pub fn all(&self) -> &[AgentRow] {
        &self.rows
    }
}

/// One row, with the two liveness states the slot derivation cannot see:
/// a worker that has not connected yet, and one whose spawn failed.
fn row_for(
    workspace: &Workspace,
    slot: SessionSlot,
    label: String,
    status: Option<WorkerLiveness>,
    reason: Option<String>,
) -> AgentRow {
    let lifecycle = match status {
        Some(WorkerLiveness::Spawning) => SessionLifecycleState::Spawning,
        Some(WorkerLiveness::Failed) => SessionLifecycleState::Failed,
        Some(WorkerLiveness::Running) | None => workspace.session_activity(&slot),
    };
    AgentRow {
        has_background_work: workspace.has_background_work(&slot),
        last_activity: workspace.session_last_activity(&slot),
        failed_turn: workspace.session_failed_turn(&slot),
        died: workspace.death_stands(&slot),
        pending: workspace.pending_interaction(&slot),
        pending_depth: workspace.pending_interaction_depth(&slot),
        // A worker's own spawn diagnostic is the more specific record, so
        // it wins; a lead has only the slot's, which is why the fallback
        // is here rather than at the call sites. **A standing death is the
        // last of the three**: the recovering connect clears the failure
        // (the lifecycle has to tell the truth about now), and the reason
        // the seat died still has to reach the row that carries `died`.
        reason: reason
            .or_else(|| workspace.spawn_failure(&slot))
            .or_else(|| workspace.death_reason(&slot)),
        lifecycle,
        label,
        slot,
    }
}

impl ViewSurface {
    /// Every project's agents: its lead, then its workers.
    pub fn agents(&self) -> Agents {
        Agents::collect(&self.workspace)
    }

    /// When `slot`'s newest turn ended in failure, or `None` when nothing
    /// failed or a newer turn has since been committed.
    ///
    /// The same fact a row carries, read for one seat: showing the seat
    /// spends that mark, and whether there was one to spend decides whether
    /// the home needs re-encoding for the connection that attached.
    pub fn failed_turn(&self, slot: &SessionSlot) -> Option<SystemTime> {
        self.workspace.session_failed_turn(slot)
    }

    /// A subscribe has served `slot`, which is this view saying the seat was
    /// shown: a death the reader had not been shown is spent with it - the
    /// mark and its reason leave the row - so a crash nobody watched is the
    /// durable trace until someone looks. Answers whether anything was
    /// spent, which is the connection's cue to re-encode the home it holds.
    ///
    /// **"Shown" is the SUBJECT's granularity, not the page's**: any
    /// subscriber to the seat - the chat, the review page, a document
    /// hand-off - serves it and spends the trace, whether or not that page
    /// ever draws the failure. Narrowing this would need the server to know
    /// which page is on the other end, which is the client's business and
    /// not something the wire carries.
    ///
    /// Called after the seat's own snapshot was encoded, deliberately: the
    /// read that spends the mark is the read that shows it.
    pub fn seat_shown(&self, slot: &SessionSlot) -> bool {
        self.workspace.spend_death(slot)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;

    use forge_primitives::{SessionLifecycleState, SessionSlot, WorkerLiveness};

    use crate::surface::ViewSurface;
    use crate::surface::agents::PendingKind;

    /// The row set the home draws: a project's lead first, then its
    /// workers in the persisted-label order. Catches a collect that heads
    /// the list with the workers, that reads labels from the live registry
    /// rather than the persisted rows, or that drops the lead row because
    /// no `WorkerEntry` describes it.
    #[test]
    fn agents_rows_a_projects_lead_first_then_its_workers() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();

        let lead = SessionSlot::lead("TestOrg", "forge");
        workspace.register_domain_session(lead.clone(), None);
        workspace.seed_test_worker_row(&project, "probe-a");
        workspace.register_domain_session(SessionSlot::worker("TestOrg", "forge", "probe-a"), None);

        let agents = surface.agents();
        let rows = agents.for_project(&project);

        assert_eq!(rows.len(), 2, "the lead and the one worker label the registry holds");
        assert_eq!(rows[0].slot, lead, "the lead's row comes first");
        assert_eq!(rows[0].label, "lead", "the lead's row is labelled for what it is");
        assert_eq!(rows[1].label, "probe-a", "then each worker under its own label");
        assert_eq!(rows[1].slot, SessionSlot::worker("TestOrg", "forge", "probe-a"));
        assert_eq!(agents.all().len(), 2, "all() carries every row for_project answers");
    }

    /// A project nobody has started is drawn by the home as a dormant row
    /// with a Start rather than as an agent, so it contributes no rows
    /// here. Catches a collect that emits a lead row for a slot with no
    /// session behind it.
    #[test]
    fn a_project_with_no_live_session_contributes_no_rows() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();

        workspace.seed_test_worker_row(&project, "probe-a");

        let agents = surface.agents();

        assert!(
            agents.for_project(&project).is_empty(),
            "a dormant project is the home's Start row, not an agent row",
        );
        assert!(agents.all().is_empty(), "and it contributes nothing to the fleet count");
    }

    /// A worker row carries the worker's own live state rather than the
    /// slot derivation's: a spawn that has not connected reads `Spawning`
    /// and a dead one reads `Failed`, where `session_activity` alone would
    /// call both `Idle` or `Sleeping` and hide them behind a calm mark.
    #[test]
    fn a_worker_row_carries_its_liveness_over_the_slot_derivation() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        workspace.register_domain_session(SessionSlot::lead("TestOrg", "forge"), None);

        let spawning = SessionSlot::worker("TestOrg", "forge", "probe-spawn");
        let failed = SessionSlot::worker("TestOrg", "forge", "probe-failed");
        workspace.seed_test_worker_row(&project, "probe-spawn");
        workspace.seed_test_worker_row(&project, "probe-failed");
        workspace.insert_live_worker(&project, worker_row("probe-spawn", WorkerLiveness::Spawning));
        workspace.insert_live_worker(&project, worker_row("probe-failed", WorkerLiveness::Failed));

        let agents = surface.agents();
        let rows = agents.for_project(&project);
        let lifecycle_of = |slot: &SessionSlot| {
            rows.iter()
                .find(|row| &row.slot == slot)
                .map(|row| row.lifecycle)
                .expect("the worker has a row")
        };

        assert_eq!(
            lifecycle_of(&spawning),
            SessionLifecycleState::Spawning,
            "a worker that has not connected is starting, not idle",
        );
        assert_eq!(
            lifecycle_of(&failed),
            SessionLifecycleState::Failed,
            "a dead spawn is failed, and is kept only so the reason can be read",
        );
    }

    /// A worker label with no live registry entry still gets a row, and
    /// the row reads asleep: the despawned worker's row is shown until it
    /// is despawned, so it must not be silently absent.
    #[test]
    fn a_persisted_worker_label_with_no_live_entry_reads_asleep() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        workspace.register_domain_session(SessionSlot::lead("TestOrg", "forge"), None);
        workspace.seed_test_worker_row(&project, "probe-gone");

        let agents = surface.agents();
        let rows = agents.for_project(&project);

        let row = rows
            .iter()
            .find(|row| row.label == "probe-gone")
            .expect("the persisted label still has a row");
        assert_eq!(
            row.lifecycle,
            SessionLifecycleState::Sleeping,
            "no session behind it is asleep, whichever liveness the registry last held",
        );
    }

    /// The ask reaches the row, which is what lets a needs-you row name
    /// what it is waiting on instead of only marking it. Catches a row
    /// wired to `None`, and one answering another session's pending set.
    #[test]
    fn a_row_carries_what_its_session_is_waiting_on() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        workspace.register_domain_session(SessionSlot::lead("TestOrg", "forge"), None);

        let asked = SessionSlot::worker("TestOrg", "forge", "probe-asked");
        let quiet = SessionSlot::worker("TestOrg", "forge", "probe-quiet");
        workspace.seed_test_worker_row(&project, "probe-asked");
        workspace.seed_test_worker_row(&project, "probe-quiet");
        workspace.seed_test_pending_interaction(&asked, PendingKind::Question);

        let agents = surface.agents();
        let rows = agents.for_project(&project);
        let row_of = |slot: &SessionSlot| {
            rows.iter().find(|row| &row.slot == slot).expect("the session has a row")
        };

        assert_eq!(
            row_of(&asked).pending,
            Some(PendingKind::Question),
            "the row names the question its session is held on",
        );
        assert_eq!(
            row_of(&quiet).pending,
            None,
            "a session holding nothing says so rather than borrowing its neighbour's ask",
        );
    }

    /// A failed row says why. The reason is the core's own record of the
    /// failure, carried through the projection a render path reads rather
    /// than re-fetched by the view. Catches a row that drops it, and one
    /// that answers a live worker with another session's reason.
    #[test]
    fn a_row_carries_the_reason_the_core_recorded() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        workspace.register_domain_session(SessionSlot::lead("TestOrg", "forge"), None);
        workspace.seed_test_worker_row(&project, "probe-dead");
        workspace.seed_test_worker_row(&project, "probe-live");

        let mut dead = worker_row("probe-dead", WorkerLiveness::Failed);
        dead.diagnostic = Some("OAuth token expired; run /login to retry".to_owned());
        workspace.insert_live_worker(&project, dead);
        workspace.insert_live_worker(&project, worker_row("probe-live", WorkerLiveness::Running));

        let agents = surface.agents();
        let rows = agents.for_project(&project);
        let reason_of = |label: &str| {
            rows.iter().find(|row| row.label == label).expect("the row exists").reason.clone()
        };

        assert_eq!(
            reason_of("probe-dead").as_deref(),
            Some("OAuth token expired; run /login to retry"),
            "a failed row carries the reason the core recorded",
        );
        assert_eq!(reason_of("probe-live"), None, "a live worker has no failure to explain");
        assert_eq!(
            reason_of("lead"),
            None,
            "a lead that has not failed carries none, the same as a live worker",
        );
    }

    /// The failed-turn fact reaches the row, which is what the rail's
    /// failure mark draws from. Catches a row that drops it, and one that
    /// stamps a seat whose newest turn never failed.
    #[test]
    fn a_row_carries_the_failed_turn_the_core_recorded() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        workspace.register_domain_session(SessionSlot::lead("TestOrg", "forge"), None);

        let failed = SessionSlot::worker("TestOrg", "forge", "probe-failed-turn");
        let quiet = SessionSlot::worker("TestOrg", "forge", "probe-quiet-turn");
        workspace.seed_test_worker_row(&project, "probe-failed-turn");
        workspace.seed_test_worker_row(&project, "probe-quiet-turn");
        workspace.register_domain_session(failed.clone(), None);
        let at = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000);
        workspace.record_failed_turn_for_test(&failed, at);

        let agents = surface.agents();
        let rows = agents.for_project(&project);
        let failed_of = |slot: &SessionSlot| {
            rows.iter().find(|row| &row.slot == slot).expect("the row exists").failed_turn
        };

        assert_eq!(failed_of(&failed), Some(at), "the row carries the failed turn's instant");
        assert_eq!(failed_of(&quiet), None, "a seat that never failed carries none");
    }

    /// A session whose subprocess died keeps its row after the failure
    /// released the session: this is what the fleet's left panel draws the
    /// failed mark from when an agent dies. Catches a collect that drops the
    /// row because no domain session is left behind - the shape the fleet
    /// showed nothing for.
    #[test]
    fn a_lead_whose_session_died_keeps_a_failed_row() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        let lead = SessionSlot::lead("TestOrg", "forge");
        workspace
            .record_spawn_failure_for_test(&lead, "the claude subprocess closed its output stream");

        let agents = surface.agents();
        let row = agents
            .for_project(&project)
            .first()
            .expect("a crashed seat still has a row, with no domain session behind it");

        assert_eq!(row.slot, lead, "the row is the seat that died");
        assert_eq!(
            row.lifecycle,
            SessionLifecycleState::Failed,
            "a dead session reads failed rather than absent",
        );
        assert_eq!(
            row.reason.as_deref(),
            Some("the claude subprocess closed its output stream"),
            "and the row carries why",
        );
    }

    /// **A recovered seat reads the truth about now, with the death riding
    /// as its own trace.** The recovering connect clears the failure - the
    /// lifecycle is about the seat as it stands, and a live, working
    /// project drawn failed is the same lie the issue was filed against,
    /// mirrored - so what carries the death until a reader is shown the
    /// seat is `died` plus the reason it left. Showing the seat takes both.
    #[test]
    fn a_recovered_seat_reads_live_and_still_carries_its_death() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        let lead = SessionSlot::lead("TestOrg", "forge");
        workspace.register_domain_session(lead.clone(), None);
        workspace.record_death_for_test(
            &lead,
            "dead-occupant",
            "the claude subprocess closed its output stream",
        );

        let agents = surface.agents();
        let row = agents.for_project(&project).first().expect("the recovered seat has its row");

        assert_ne!(
            row.lifecycle,
            SessionLifecycleState::Failed,
            "a recovered seat reading Failed is the lie, mirrored",
        );
        assert!(row.died, "and the death nobody was shown still rides");
        assert_eq!(
            row.reason.as_deref(),
            Some("the claude subprocess closed its output stream"),
            "with the reason it left, which the cleared failure no longer carries",
        );

        // Shown: the trace goes, the row stays.
        assert!(surface.seat_shown(&lead), "the show takes the trace");
        let agents = surface.agents();
        let row = agents.for_project(&project).first().expect("the row is still there");
        assert!(!row.died, "the spent death is off the row");
        assert_eq!(row.reason, None, "and its reason with it");
    }

    /// And a seat that crashed and is still DOWN reads Failed with its
    /// reason: that is the truth about a seat nothing brought back. Catches
    /// a row that reports a live state for a seat no session is behind.
    #[test]
    fn a_seat_that_crashed_and_is_still_down_reads_failed() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        let lead = SessionSlot::lead("TestOrg", "forge");
        workspace
            .record_spawn_failure_for_test(&lead, "the claude subprocess closed its output stream");
        workspace.record_death_for_test(
            &lead,
            "dead-occupant",
            "the claude subprocess closed its output stream",
        );

        let agents = surface.agents();
        let row = agents.for_project(&project).first().expect("the crashed seat has its row");

        assert_eq!(
            row.lifecycle,
            SessionLifecycleState::Failed,
            "a seat nothing brought back reads failed",
        );
        assert!(row.died, "and the death it has not been shown rides");
        assert_eq!(
            row.reason.as_deref(),
            Some("the claude subprocess closed its output stream"),
            "with the reason it died with",
        );
    }

    /// **A death keeps the row on its own.** With no session behind it and
    /// no failure standing - the recovering connect clears the failure by
    /// design, and a release takes the domain - the row is owed for the
    /// death itself; skipped, the mark would vanish between the revival and
    /// the next show and come back stale on a session that never died.
    #[test]
    fn a_seat_with_no_session_and_a_standing_death_keeps_its_row() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        let lead = SessionSlot::lead("TestOrg", "forge");
        // No domain session (the release took it) and no failure (the
        // recovering connect cleared it): the death alone.
        workspace.record_death_for_test(
            &lead,
            "dead-occupant",
            "the claude subprocess closed its output stream",
        );

        let agents = surface.agents();
        let row = agents
            .for_project(&project)
            .first()
            .expect("a standing death keeps the row with nothing else behind it");

        assert!(row.died, "the death is on the row");
        assert_eq!(
            row.reason.as_deref(),
            Some("the claude subprocess closed its output stream"),
            "with the reason it left",
        );
        assert_eq!(
            row.lifecycle,
            SessionLifecycleState::Sleeping,
            "and the lifecycle reads the seat that is not there",
        );
    }

    /// A lead's failed spawn reaches its row, which is what the slot's own
    /// failure record is for: the failure releases the session, so a row
    /// reading only the lifecycle would call the slot dormant.
    #[test]
    fn a_lead_carries_the_reason_its_spawn_failed() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project =
            surface.roster().project_named("forge").expect("configured project").key.clone();
        let lead = SessionSlot::lead("TestOrg", "forge");
        workspace.register_domain_session(lead.clone(), None);
        workspace.record_spawn_failure_for_test(&lead, "the subprocess exited");

        let agents = surface.agents();
        let row = agents.for_project(&project).first().expect("the lead's row");

        assert_eq!(
            row.lifecycle,
            SessionLifecycleState::Failed,
            "a spawn that failed is failed rather than idle",
        );
        assert_eq!(row.reason.as_deref(), Some("the subprocess exited"), "and the row carries why");
    }

    fn worker_row(label: &str, status: WorkerLiveness) -> forge_workspace::WorkerEntry {
        forge_workspace::WorkerEntry {
            label: label.to_owned(),
            charter: format!("charter for {label}"),
            slot: SessionSlot::worker("TestOrg", "forge", label),
            session_id: None,
            status,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", "forge"),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            cwd: None,
            diagnostic: None,
            kick: None,
        }
    }
}

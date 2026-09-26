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
pub struct AgentRow {
    pub slot: SessionSlot,
    /// The worker's own label, or `"lead"` for a project's own agent.
    pub label: String,
    pub lifecycle: SessionLifecycleState,
    pub has_background_work: bool,
    pub pending: Option<PendingKind>,
    pub last_activity: Option<SystemTime>,
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
            // rather than an agent, so it contributes no rows at all.
            if workspace.domain_session_for(&lead).is_none() {
                continue;
            }
            let start = rows.len();
            rows.push(row_for(workspace, lead, "lead".to_owned(), None));
            for label in labels.get(&project.key).into_iter().flatten() {
                let slot = SessionSlot::worker(&project.org, &project.name, label);
                let status = live
                    .get(&project.key)
                    .and_then(|states| states.iter().find(|state| &state.label == label))
                    .map(|state| state.status);
                rows.push(row_for(workspace, slot, label.clone(), status));
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
) -> AgentRow {
    let lifecycle = match status {
        Some(WorkerLiveness::Spawning) => SessionLifecycleState::Spawning,
        Some(WorkerLiveness::Failed) => SessionLifecycleState::Failed,
        Some(WorkerLiveness::Running) | None => workspace.session_activity(&slot),
    };
    AgentRow {
        has_background_work: workspace.has_background_work(&slot),
        last_activity: workspace.session_last_activity(&slot),
        pending: workspace.pending_interaction(&slot),
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
            diagnostic: None,
            kick: None,
        }
    }
}

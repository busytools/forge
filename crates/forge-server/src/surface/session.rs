//! `session()`: one session's operational state.

use std::path::{Path, PathBuf};

use forge_primitives::SessionSlot;
use forge_workspace::Workspace;

use super::ViewSurface;

/// The machine turn the record carries, from the two states the core and the
/// CLI each already know.
///
/// A handoff outranks a compaction, because it is the one that replaces the
/// seat; `None` is a seat taking input.
pub fn machine_turn(
    handoff: bool,
    compacting: bool,
) -> Option<forge_primitives::runtime::MachineTurn> {
    use forge_primitives::runtime::MachineTurn;
    if handoff {
        Some(MachineTurn::Handoff)
    } else if compacting {
        Some(MachineTurn::Compacting)
    } else {
        None
    }
}

/// What a view reads about one session.
pub struct SessionState {
    /// The seat this state belongs to.
    pub slot: SessionSlot,
    /// Where forge scans git for this session: a git worker's worktree
    /// path, else the caller's `cwd_raw` unchanged.
    pub scan_cwd: PathBuf,
    /// The prompts still waiting in the CLI's queue, oldest first - the pile a
    /// client draws.
    ///
    /// The same read path: `prompt_lifecycle` frames speak only on a change,
    /// so a client attaching mid-queue would otherwise draw nothing until the
    /// next prompt moved. Held by the core, so two clients agree.
    pub queue: Vec<forge_workspace::protocol::QueuedPrompt>,
    /// Clients with voice on for this seat, this one included. A late
    /// attach reads the room here rather than waiting for the next toggle.
    pub voice_listeners: u32,
}

impl ViewSurface {
    /// Whether a handoff owns this seat: its judgment turn is running, or the
    /// swap it opened is still building or spawning.
    ///
    /// From the moment the judgment prompt is sent until the successor
    /// connects, the seat has no composer to take input for - which is what
    /// tells the page to stand down. A handoff merely *parked* on a running
    /// turn is not this: the person still has the seat until the boundary,
    /// when it begins for real.
    pub fn handoff_in_flight(&self, slot: &SessionSlot) -> bool {
        self.workspace.handoff_in_flight(slot)
    }
}

impl SessionState {
    pub(super) fn collect(workspace: &Workspace, slot: &SessionSlot, cwd_raw: &Path) -> Self {
        let queue = workspace
            .domain_session_for(slot)
            .map(|domain| {
                let guard = domain.lock();
                // **The wire fields only.** The held entry carries the
                // prompt's image payloads for the row a start will draw, and
                // a seat read has no use for them: cloning the whole entry
                // per read copies every base64 payload only for serde to
                // skip it.
                guard
                    .prompt_queue
                    .iter()
                    .map(|prompt| forge_workspace::protocol::QueuedPrompt {
                        uuid: prompt.uuid.clone(),
                        source: prompt.source,
                        text: prompt.text.clone(),
                        attachments: Vec::new(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            slot: slot.clone(),
            scan_cwd: workspace.git_scan_cwd_for_session(slot, cwd_raw),
            queue,
            voice_listeners: u32::try_from(workspace.voice_listeners(slot)).unwrap_or(u32::MAX),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use forge_primitives::{SessionSlot, WorkerLiveness};
    use forge_workspace::{WorkerEntry, Workspace};

    use forge_primitives::runtime::MachineTurn;

    use super::machine_turn;
    use crate::surface::ViewSurface;

    fn stub_workspace() -> Arc<Workspace> {
        let (workspace, _updates) = Workspace::testing_stub();
        workspace
    }

    fn status_frame(slot: &SessionSlot, status: &serde_json::Value) -> crate::SessionUpdate {
        use forge_primitives::Message;
        let msg: Message = serde_json::from_value(serde_json::json!({
            "type": "system",
            "session_id": "s1",
            "subtype": "status",
            "status": status,
        }))
        .expect("a status frame");
        crate::SessionUpdate::ChatAppended { key: slot.clone(), msg, source: None }
    }

    /// **A seat read's queue carries no attachment payloads.** The held entry
    /// keeps the images for the row a start draws; cloning it whole per read
    /// copies base64 only for serde to skip it.
    #[test]
    fn the_read_queue_carries_no_attachment_payloads() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let seat = SessionSlot::lead("TestOrg", "forge");
        workspace.register_domain_session(seat.clone(), None);
        workspace.domain_session_for(&seat).expect("the domain").lock().prompt_queue.push(
            forge_workspace::protocol::QueuedPrompt {
                uuid: "p1".to_owned(),
                source: forge_workspace::protocol::PromptSource::You,
                text: "queued words".to_owned(),
                attachments: vec![forge_primitives::ImageAttachment {
                    data: "iVBORw0KGgo=".to_owned(),
                    mime_type: "image/png".to_owned(),
                }],
            },
        );

        let state = super::SessionState::collect(&workspace, &seat, Path::new("/tmp"));

        assert_eq!(state.queue.len(), 1, "the waiting prompt is still served");
        assert!(state.queue[0].attachments.is_empty(), "and its payloads stay core-side",);
    }

    /// **A handoff owns the seat until the swap replaces it, and a
    /// compaction until the CLI settles it.** The composer is taken away for
    /// exactly that span, and a handoff outranks a compaction because it is
    /// the one that replaces the seat.
    #[test]
    fn the_machine_turn_is_a_handoff_until_the_swap_and_a_compaction_until_it_settles() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(std::sync::Arc::clone(&workspace));
        let seat = SessionSlot::lead("TestOrg", "forge");
        workspace.register_domain_session(seat.clone(), None);
        let mut composer = crate::composer::Composer::default();

        let turn = |surface: &ViewSurface, composer: &crate::composer::Composer| {
            machine_turn(surface.handoff_in_flight(&seat), composer.compacting(&seat))
        };

        assert_eq!(turn(&surface, &composer), None, "an idle seat takes input");

        // **A parked handoff is not a running one.** It waits for the turn
        // boundary to begin, and the person still has the seat until then -
        // standing the composer down here would lock a box on a request that
        // has not started.
        workspace.domain_session_for(&seat).expect("the domain").lock().handoff.parked = true;
        assert_eq!(turn(&surface, &composer), None, "a parked handoff leaves the seat live");
        workspace.domain_session_for(&seat).expect("the domain").lock().handoff.parked = false;

        composer.apply(&status_frame(&seat, &serde_json::json!("compacting")));
        assert_eq!(
            turn(&surface, &composer),
            Some(MachineTurn::Compacting),
            "the CLI's status frame names the compaction",
        );
        composer.apply(&status_frame(&seat, &serde_json::Value::Null));
        assert_eq!(turn(&surface, &composer), None, "and its null settles it");

        workspace.domain_session_for(&seat).expect("the domain").lock().handoff.awaiting = true;
        assert_eq!(
            turn(&surface, &composer),
            Some(MachineTurn::Handoff),
            "the judgment turn owns the seat",
        );
        composer.apply(&status_frame(&seat, &serde_json::json!("compacting")));
        assert_eq!(
            turn(&surface, &composer),
            Some(MachineTurn::Handoff),
            "a handoff outranks a compaction begun beside it",
        );
        workspace.domain_session_for(&seat).expect("the domain").lock().handoff.awaiting = false;
        composer.apply(&status_frame(&seat, &serde_json::Value::Null));
        assert_eq!(turn(&surface, &composer), None, "the swap and the settle clear both");
    }

    fn git_worker(slot: SessionSlot) -> WorkerEntry {
        WorkerEntry {
            label: slot.label().to_owned(),
            charter: "charter".to_owned(),
            slot,
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("TestOrg", "forge"),
            needs_tag: false,
            is_git_repo_at_spawn: true,
            cwd: None,
            diagnostic: None,
            kick: None,
        }
    }

    /// Catches a session verb that hands back the caller's own cwd for a
    /// git worker, and one that names a seat other than the one it was
    /// asked about.
    #[test]
    fn session_scan_cwd_agrees_with_the_workspace() {
        let workspace = stub_workspace();
        workspace.seed_test_project("forge", "/tmp/forge-session");
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let project = surface.roster().project_named("forge").expect("seeded project").key.clone();
        let worker = SessionSlot::worker("TestOrg", "forge", "probe-a");
        workspace.insert_live_worker(&project, git_worker(worker.clone()));

        // Both lifecycle cwds - the project root a fresh spawn reports,
        // and the worktree path a resumed one reports - must converge on
        // the worker's worktree.
        for cwd_raw in ["/tmp/forge-session", "/tmp/forge-session/.claude/worktrees/probe-a"] {
            let cwd = Path::new(cwd_raw);
            assert_eq!(
                surface.session(&worker, cwd).scan_cwd,
                workspace.git_scan_cwd_for_session(&worker, cwd),
                "a git worker's scan cwd must be the workspace's own answer for {cwd_raw}"
            );
            assert_eq!(
                surface.session(&worker, cwd).scan_cwd,
                PathBuf::from("/tmp/forge-session/.claude/worktrees/probe-a"),
                "a git worker must scan its worktree, not the cwd it was handed"
            );
        }

        let lead = SessionSlot::lead("TestOrg", "forge");
        let cwd = Path::new("/tmp/elsewhere");
        let state = surface.session(&lead, cwd);
        assert_eq!(
            state.scan_cwd,
            PathBuf::from("/tmp/elsewhere"),
            "a session that is not a git worker must scan the cwd it was handed"
        );
        assert_eq!(state.slot, lead, "the state must name the seat it was asked about");
    }
}

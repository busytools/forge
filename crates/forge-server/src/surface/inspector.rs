//! The session's own facts: its header and the sections an inspector
//! draws from it.
//!
//! Each is held by the core from the same event the matching
//! `SessionUpdate` is built from, so this hands over what the core has
//! rather than deriving anything: a view reads the session instead of
//! folding the update stream a second time.

use forge_primitives::git_diff::PrRefresh;
use forge_primitives::runtime::AvailableModel;
use forge_primitives::{
    CurrentModel, EffortLevel, MonitorRecord, PermissionMode, SessionId, SessionSlot,
};

use crate::surface::ViewSurface;

// A view reads the values these verbs hand it, so it needs their names
// too - re-exported here rather than reached for in the crates below,
// which a view does not name.
pub use forge_workspace::env::processes::{
    ProcessEntry, ProcessSnapshot, basename_exe, extract_inner_command,
};
pub use forge_workspace::work::WorkSnapshot;
pub use forge_workspace::{ContextUsage, McpServers};

/// What a session header states about the session.
pub struct SessionHeader {
    /// The occupant's id, as the CLI named it, or `None` when there is no
    /// occupant to name: a seat nothing has started, one whose CLI has not
    /// answered yet, and one whose id was dropped after a background sign-in
    /// or connection failure all read the same.
    ///
    /// Carried here rather than read off the conversation's frames, because a
    /// client attaching to a running seat hears no `Connected` - a
    /// subscription carries no backlog - and a frame's id is the turn's fact
    /// rather than the seat's. The worker registry carries the id for a live
    /// dynamic worker, and nothing carries it for any other seat.
    pub session_id: Option<SessionId>,
    /// The model the CLI resolved for this session, from its connect and
    /// from every later frame that names a different one.
    pub model: Option<CurrentModel>,
    /// The effort the session runs at: the hook-observed level when the
    /// session has reported one, else the level its launch stamped, else
    /// forge's default. Always a value rather than an `Option`, because
    /// a session that has not used a tool yet still runs at some level
    /// and a header that showed nothing there would be wrong.
    pub effort: EffortLevel,
    /// The permission mode the session runs in: the hook-observed mode
    /// when one has fired, else the level its launch stamped, which forge
    /// always writes from its own effective default. `None` only when the
    /// launch pinned nothing and no hook has reported, which is a session
    /// nothing has started.
    pub permission_mode: Option<PermissionMode>,
    /// How full the context window is, from the bridge's last answer.
    pub context: ContextUsage,
    /// The model catalogue the session's connect resolved against, which a
    /// picker draws its rows from. Empty for a session nothing has started.
    pub available_models: Vec<AvailableModel>,
    /// Whether a turn is in flight, from the core's own OR of the stamped
    /// signal and the wire-lagged one: the two disagree in the window
    /// between a prompt being routed and its echo arriving, so a read of
    /// either alone is wrong for one of them.
    pub turn_in_flight: bool,
}

impl ViewSurface {
    /// The session's header facts: model, effort, permission mode and
    /// context usage.
    ///
    /// One call rather than four, because a header renders them together
    /// and each read takes the session's lock.
    pub fn header(&self, slot: &SessionSlot) -> SessionHeader {
        let Some(domain) = self.workspace.domain_session_for(slot) else {
            return SessionHeader {
                session_id: None,
                model: None,
                effort: EffortLevel::Max,
                permission_mode: None,
                context: ContextUsage::default(),
                available_models: Vec::new(),
                turn_in_flight: false,
            };
        };
        let held = domain.lock();
        SessionHeader {
            session_id: held.session_id.clone(),
            model: held.current_model.clone(),
            effort: held.observed_effort.unwrap_or(held.configured_effort),
            permission_mode: held.observed_permission_mode.or(held.configured_permission_mode),
            context: held.context_usage.unwrap_or_default(),
            available_models: held.available_models.clone(),
            turn_in_flight: held.turn_in_flight(),
        }
    }

    /// The MCP servers the session's bridge last reported, and the
    /// failure standing beside them when the read did not complete.
    ///
    /// MCP is configured per session rather than per account, so this is
    /// the session's own set. `None` until the first snapshot lands, as
    /// opposed to a snapshot that carries none.
    pub fn mcp_servers(&self, slot: &SessionSlot) -> Option<McpServers> {
        self.workspace.domain_session_for(slot).and_then(|domain| domain.lock().mcp_servers.clone())
    }

    /// The last OS walk of the session's process tree.
    ///
    /// The walk is driven by whoever owns the tick that performs it and
    /// the core holds its answer, so this neither performs one nor waits
    /// for the next: a slot nothing has walked reads as `None`.
    ///
    /// The snapshot carries the instant it was taken, and a view draws it:
    /// a walk is only ever taken for the session a view is looking at, so
    /// the answer here can be arbitrarily old and must not be drawn as
    /// though it were taken now.
    pub fn processes(&self, slot: &SessionSlot) -> Option<ProcessSnapshot> {
        self.workspace.process_snapshot(slot)
    }

    /// The seat's working tree, read on the loop's own rule: the stored scan
    /// while it is inside the snapshot window, a new one past it.
    ///
    /// **The record's read is the seat's first read, and the loop's too.** A
    /// held seat's snapshot takes the tree under the same gate the watch's
    /// first pass uses, so the two are one scan: whichever gets there first
    /// stores, and the other answers that store rather than reading the tree
    /// again. A seat nobody holds has no loop to race and reads unguarded.
    ///
    /// **The window is what keeps the record self-consistent.** The record
    /// carries the scan's hunks beside its branch and count; serving an
    /// arbitrarily old scan would put its hunks beside a row the first pass
    /// has since moved, so a store past the window is re-read here rather
    /// than carried.
    pub async fn work(
        &self,
        slot: &SessionSlot,
        cwd: &std::path::Path,
        // Whether this is the review page's own read, which fetches the base
        // itself a moment later.
        review: bool,
    ) -> WorkSnapshot {
        // **The record's read never waits on the PR lookup.** A
        // lookup past its window is a TLS + API round trip (~0.9 s here) and
        // it sat inside the open a client waits for; the seat's loop scans
        // with `Fetch` and pushes the refreshed row instead.
        if let Ok(held) =
            self.workspace.scan_work_if_stale(slot, false, PrRefresh::ServeCached).await
        {
            return held;
        }
        // A seat with no session has no store to hold a read, and its record
        // still answers the tree it stands in - so there is nothing cached to
        // serve and no loop to refresh it either: this read fetches. **The
        // review read fetches the base itself**, so its scan takes none of the
        // background kick: one read, one fetch, and nothing of forge's queued
        // ahead of that fetch on the shared repository gate.
        if review {
            return self.workspace.scan_work_at_without_base_fetch(slot, cwd).await;
        }
        self.workspace.scan_work_at(slot, cwd, PrRefresh::Fetch).await
    }

    /// Refresh the base the seat's drawn branch diff is measured against,
    /// waiting for the fetch with no cap.
    ///
    /// `None` when no branch layer is drawn: the default branch, a detached
    /// HEAD, or no resolved default. Only the read that draws the diff calls
    /// this, so a state it reports belongs to the moment that diff was read
    /// rather than to an earlier scan.
    pub async fn refresh_diff_base(
        &self,
        held: &WorkSnapshot,
    ) -> Option<forge_workspace::env::git_diff::BaseRefresh> {
        if !held.diff.branch_ahead.is_populated() {
            return None;
        }
        let default = held.diff.default_branch.as_deref()?;
        Some(forge_workspace::env::git_diff::refresh_base_for_display(&held.cwd, default).await)
    }

    /// The scan a drawing read must take: pinned to the base revision the
    /// refresh resolved, and gate-awaited so it cannot serve a store written
    /// before the base moved. Local only.
    ///
    /// `None` when the scan could not be taken - no session behind the seat,
    /// or its gate held past the read's bound (which `scan_work_pinned` logs
    /// with the slot and the wait). **The caller must then re-read locally**
    /// ([`Self::work_pinned_local`]) at the same sha: the store's count
    /// predates the base the refresh moved, and a scan against the ref's name
    /// cannot be paired with content read at the sha this read resolved.
    ///
    /// The refresh's state rides the stored row, so the `base` beside a pinned
    /// `ahead` is that base's own measurement rather than the last watch's.
    pub async fn diff_scan(
        &self,
        slot: &SessionSlot,
        cwd: &std::path::Path,
        refresh: &forge_workspace::env::git_diff::BaseRefresh,
    ) -> Option<WorkSnapshot> {
        match self
            .workspace
            .scan_work_pinned(slot, cwd, refresh.base_ref.as_deref(), Some(refresh.state))
            .await
        {
            Ok(held) => Some(held),
            Err(why) => {
                // DEBUG: the read answers a correct scan either way, and the
                // no-session case is this door's own reason - the only one
                // that would otherwise reach the caller as a bare `None`.
                tracing::debug!(
                    target: "forge_workspace::work",
                    event_name = "review_diff_scan_refused",
                    slot = %slot.display(),
                    outcome = "no_data",
                    error = %why,
                    "the pinned scan could not be taken; the read re-reads locally",
                );
                None
            }
        }
    }

    /// The local re-read a drawing read takes when its pin could not be: the
    /// count and the chain must describe the ref the content is read at, and
    /// that ref is the one the refresh just left. **Pinned to the same sha the
    /// content read takes**, so a fetch that moves the ref between the two
    /// cannot straddle two bases; the refresh's own state rides it, so the
    /// row's `base` and the diff's agree. Never fetches, and takes no gate.
    pub async fn work_pinned_local(
        &self,
        slot: &SessionSlot,
        cwd: &std::path::Path,
        base: Option<&str>,
        measured: Option<forge_primitives::git_diff::BaseState>,
    ) -> WorkSnapshot {
        self.workspace.scan_work_at_pinned_local(slot, cwd, base, measured).await
    }

    /// Store a scan's answer, which is the write half of [`Self::work`].
    ///
    /// A fixture's door, and the same store the seat's own loop writes: one
    /// store means a record read and a pushed row cannot disagree.
    pub fn store_work_snapshot(&self, slot: &SessionSlot, snapshot: WorkSnapshot) {
        self.workspace.store_work_snapshot(slot, snapshot);
    }

    /// Show a seat: this view is looking at it, which is what keeps its
    /// working tree scanned while the view holds it. A sessionless seat is
    /// held too - its watch waits for the session - so every call takes a
    /// hold, and a caller must release it exactly once.
    ///
    /// **The caller owes [`Self::note_record_read`] once its record is built.**
    /// That mark is what starts the watch's first pass; a view that holds
    /// without it has a watch that waits and scans nothing.
    pub fn hold_seat(&self, slot: &SessionSlot) {
        self.workspace.hold_seat(slot);
    }

    /// Mark the seat's record read, so its watch's first pass starts from the
    /// stores the snapshot carried.
    pub fn note_record_read(&self, slot: &SessionSlot) {
        self.workspace.note_record_read(slot);
    }

    /// Stop showing a seat, which is what lets its scan go when nobody else
    /// is holding it.
    pub fn release_seat(&self, slot: &SessionSlot) {
        self.workspace.release_seat(slot);
    }

    /// Ask the core for a fresh context reading on `slot`, which its bridge
    /// answers with [`SessionUpdate::ContextUsageSnapshot`](crate::SessionUpdate::ContextUsageSnapshot).
    ///
    /// The ask rather than the read, because the reading only exists once the
    /// CLI has computed it: a view asks for the seat it is addressing, and
    /// the socket asks for the seat a client reads, so both reach the one
    /// probe rather than each holding a way to reach the CLI.
    ///
    /// # Errors
    ///
    /// [`DispatchError::UnknownSession`](forge_workspace::DispatchError::UnknownSession)
    /// when the seat has no agent to ask or has not stamped a session id yet,
    /// and [`DispatchError::SessionClosed`](forge_workspace::DispatchError::SessionClosed)
    /// when the ask could not be handed to that agent.
    pub fn refresh_context_usage(
        &self,
        slot: &SessionSlot,
    ) -> Result<(), forge_workspace::DispatchError> {
        self.workspace.refresh_context_usage(slot)
    }

    /// Ask the session's bridge for a fresh MCP snapshot, which it answers
    /// with [`SessionUpdate::McpSnapshot`](crate::SessionUpdate::McpSnapshot).
    ///
    /// The ask rather than the read, for the same reason as
    /// [`Self::refresh_context_usage`]: the snapshot only exists once the
    /// bridge has answered one, and the socket asks for the seat a client
    /// reads the way a view asks for the seat it is addressing.
    ///
    /// # Errors
    ///
    /// See [`Self::refresh_context_usage`].
    pub fn refresh_mcp_snapshot(
        &self,
        slot: &SessionSlot,
    ) -> Result<(), forge_workspace::DispatchError> {
        self.workspace.refresh_mcp_snapshot(slot)
    }

    /// The monitors the session has running, and the ones that settled
    /// while it did, folded from the wire.
    ///
    /// Empty when nothing is running, however many monitors have been and
    /// gone: the set drains once every entry in it is terminal, which is
    /// the rule a view applies to its own copy.
    pub fn monitors(&self, slot: &SessionSlot) -> Vec<MonitorRecord> {
        self.workspace
            .domain_session_for(slot)
            .map_or_else(Vec::new, |domain| domain.lock().monitors.clone())
    }

    /// The seat's live tasks, each entry with the command its own card
    /// carried: the CLI's background registry plus the long foreground calls
    /// it announced, which never enter that registry.
    ///
    /// Empty when nothing is running. A view draws the processes feed's
    /// leading rows from this: the tasks are held on the session so a view
    /// that attached after the snapshot arrived still reads them, and the
    /// command is what tells it whether the OS walk has already adopted the
    /// process behind one.
    pub fn background_tasks(&self, slot: &SessionSlot) -> Vec<forge_workspace::BackgroundTask> {
        self.workspace
            .domain_session_for(slot)
            .map_or_else(Vec::new, |domain| domain.lock().task_rows())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::runtime::{AvailableModel, RuntimeSessionState};
    use forge_primitives::{
        CurrentModel, EffortLevel, MonitorRecord, MonitorStatus, PermissionMode, SessionSlot,
    };
    use forge_workspace::{ContextUsage, McpServers};

    use crate::surface::ViewSurface;

    fn seat(label: &str) -> SessionSlot {
        SessionSlot::lead("TestOrg", label)
    }

    /// **The inspector's hold is served at once** (#2506): opening a seat must
    /// not wait for the tree's scan, the process walk or the file index - the
    /// watch the hold starts answers those as updates.
    #[tokio::test]
    async fn the_hold_is_served_before_the_seats_scans_complete() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = Arc::new(ViewSurface::new(Arc::clone(&workspace)));
        let seat = seat("forge");
        workspace.register_domain_session(seat.clone(), None);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        workspace.seed_test_scan_gate(Arc::clone(&gate));

        surface.hold_seat(&seat);

        assert!(workspace.work_snapshot(&seat).is_none(), "the tree is not read on the hold path");
    }

    /// **The surface reads the holds the registry does not name.** A long
    /// foreground call never enters `background_tasks_changed`, so a read
    /// that served only that registry would draw nothing for it.
    #[test]
    fn the_read_includes_the_announced_holds_the_registry_does_not_name() {
        let fleet = crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("a fleet");
        fleet.start("TestOrg", "proj").expect("the project starts");
        let seat = seat("forge");
        fleet.seed_view_facts(
            &seat,
            crate::testing::ViewFacts {
                live_tasks: vec![forge_workspace::BackgroundTask {
                    task_id: "t-live".to_owned(),
                    is_backgrounded: false,
                    task_type: "local_bash".to_owned(),
                    description: "the gate".to_owned(),
                    command: Some("cargo nextest run".to_owned()),
                    tool_use_id: Some("tu-live".to_owned()),
                }],
                ..crate::testing::ViewFacts::default()
            },
        );

        let rows = fleet.surface().background_tasks(&seat);
        assert_eq!(rows.len(), 1, "the announced hold is read: {rows:?}");
        assert_eq!(rows[0].task_id, "t-live", "under its own id");
        assert!(!rows[0].is_backgrounded, "and its own flag crosses with it");
    }

    /// The header reports the four facts the core was handed, rather than
    /// re-deriving any of them: each one is set to a value no derivation
    /// from an empty session could produce.
    #[test]
    fn the_header_reports_the_facts_the_core_holds() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        let domain = workspace.register_domain_session(lead.clone(), None);
        {
            let mut held = domain.lock();
            held.observed_permission_mode = Some(PermissionMode::Plan);
            held.observed_effort = Some(EffortLevel::Xhigh);
            held.current_model = Some(CurrentModel::new("claude-opus-5", "Opus", "Claude Opus 5"));
            held.context_usage =
                Some(ContextUsage { percent: Some(62), max_tokens: Some(1_000_000) });
        }

        let header = surface.header(&lead);

        assert_eq!(
            header.permission_mode,
            Some(PermissionMode::Plan),
            "the mode the hook observed is the mode the header states",
        );
        assert_eq!(
            header.effort,
            EffortLevel::Xhigh,
            "and the level it observed stands over the configured one",
        );
        assert_eq!(
            header.model.as_ref().map(|model| model.resolved_id.as_str()),
            Some("claude-opus-5"),
            "the model the session resolved is reported as it resolved",
        );
        assert_eq!(header.context.percent, Some(62), "and the context reading comes through");
        assert_eq!(header.context.max_tokens, Some(1_000_000), "with the window it is a share of");
    }

    /// A picker has no rows and a chat cannot tell whether the turn it is
    /// watching has finished, because neither fact is reachable: the
    /// catalogue and the turn's presence are held by the core and read by
    /// nothing. Both are carried here.
    #[test]
    fn the_header_carries_the_catalogue_and_the_turn() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        let domain = workspace.register_domain_session(lead.clone(), None);
        {
            let mut held = domain.lock();
            held.available_models = vec![
                AvailableModel::new("claude-opus-5", "Opus"),
                AvailableModel::new("sonnet", "S"),
            ];
            held.turn_pending = true;
        }

        let header = surface.header(&lead);

        assert_eq!(
            header.available_models.len(),
            2,
            "the catalogue the connect resolved against is readable, or a picker draws no rows",
        );
        assert!(header.turn_in_flight, "and a turn the core stamped as pending reads as in flight");
    }

    /// The other input of the OR, and the reason it is an OR: `turn_pending`
    /// is stamped synchronously and `runtime_state` is wire-lagged, so a
    /// read that trusted either alone would be wrong for one of the two.
    #[test]
    fn a_wire_lagged_running_state_alone_still_reads_as_in_flight() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        let domain = workspace.register_domain_session(lead.clone(), None);
        {
            let mut held = domain.lock();
            held.runtime_state = Some(RuntimeSessionState::Running);
            held.turn_pending = false;
        }

        assert!(
            surface.header(&lead).turn_in_flight,
            "the wire-lagged state alone still says a turn is in flight",
        );
    }

    /// Effort has two sources and a session that has not used a tool yet
    /// has only the second. A read that answered with nothing there would
    /// leave the header blank on exactly the sessions a user just opened.
    #[test]
    fn the_effort_the_header_reports_falls_back_to_the_launch_level() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        let domain = workspace.register_domain_session(lead.clone(), None);
        domain.lock().configured_effort = EffortLevel::High;

        let header = surface.header(&lead);

        assert_eq!(
            header.effort,
            EffortLevel::High,
            "a session whose hook has not fired reports the level it was launched at",
        );
    }

    /// The mode has the same shape as the effort: a session that has not
    /// used a tool yet was still launched in a mode, and a header that
    /// went blank there would be wrong about every session between its
    /// spawn and its first tool call.
    #[test]
    fn the_mode_the_header_reports_falls_back_to_the_launch_mode() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        let domain = workspace.register_domain_session(lead.clone(), None);
        domain.lock().configured_permission_mode = Some(PermissionMode::Plan);

        let header = surface.header(&lead);

        assert_eq!(
            header.permission_mode,
            Some(PermissionMode::Plan),
            "a session whose hook has not fired reports the mode it was launched in",
        );
    }

    /// The hook is the higher-fidelity source, so it stands over the
    /// launch's own answer once it has something to say.
    #[test]
    fn an_observed_mode_stands_over_the_launch_mode() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        let domain = workspace.register_domain_session(lead.clone(), None);
        {
            let mut held = domain.lock();
            held.configured_permission_mode = Some(PermissionMode::Plan);
            held.observed_permission_mode = Some(PermissionMode::BypassPermissions);
        }

        let header = surface.header(&lead);

        assert_eq!(
            header.permission_mode,
            Some(PermissionMode::BypassPermissions),
            "the mode a hook saw is the mode the session is in",
        );
    }

    /// The occupant's own id, which a view has no other way to read: a client
    /// attaching to a running seat hears no `Connected` (a subscription
    /// carries no backlog) and the conversation's frames are not the seat's
    /// fact.
    #[test]
    fn the_header_carries_the_occupants_session_id() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        let domain = workspace.register_domain_session(lead.clone(), None);
        domain.lock().session_id = Some(forge_primitives::SessionId::new("d4f70669-1f2a"));

        assert_eq!(
            surface.header(&lead).session_id,
            Some(forge_primitives::SessionId::new("d4f70669-1f2a")),
            "the id the core holds for the occupant is the id the header states",
        );
    }

    /// A seat nobody has started reads as blank rather than borrowing
    /// another session's header.
    #[test]
    fn the_header_of_an_unstarted_seat_reads_blank() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));

        let header = surface.header(&seat("never-started"));

        assert!(header.model.is_none(), "no session, no model");
        assert_eq!(header.permission_mode, None, "and no mode observed");
        assert_eq!(header.context, ContextUsage::default(), "and no context to report");
        assert_eq!(header.effort, EffortLevel::Max, "and forge's default level, not a blank");
        assert_eq!(header.session_id, None, "and no occupant to name");
    }

    /// The MCP set is the session's own, and a failure replaces the set
    /// it invalidates rather than sitting beside it.
    #[test]
    fn the_mcp_read_answers_with_the_snapshot_the_core_holds() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        let domain = workspace.register_domain_session(lead.clone(), None);

        assert!(
            surface.mcp_servers(&lead).is_none(),
            "nothing read yet is not the same as nothing configured",
        );

        domain.lock().mcp_servers =
            Some(McpServers { servers: Vec::new(), error: Some("the CLI refused".to_owned()) });
        let held = surface.mcp_servers(&lead).expect("the held snapshot comes back");
        assert_eq!(held.error.as_deref(), Some("the CLI refused"), "and carries why it is empty");
        assert!(held.servers.is_empty(), "with no server standing beside the failure");
    }

    /// The process walk is held rather than performed: a slot nothing has
    /// walked answers `None` rather than paying for a `sysinfo` refresh
    /// on the calling thread.
    #[test]
    fn the_process_read_answers_with_the_walk_the_core_holds() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        workspace.register_domain_session(lead.clone(), None);

        assert!(surface.processes(&lead).is_none(), "a slot nothing walked reads as none");

        let walked_at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(60);
        workspace.store_process_snapshot(
            &lead,
            Some(forge_workspace::env::processes::ProcessSnapshot {
                processes: Vec::new(),
                scanned_at: walked_at,
            }),
        );

        assert_eq!(
            surface.processes(&lead).map(|snapshot| snapshot.scanned_at),
            Some(walked_at),
            "the walk the core holds is the walk the read answers with",
        );
    }

    /// The monitor set comes back whole, settled entries included: which
    /// of them a surface draws is the view's business, not the read's.
    #[test]
    fn the_monitor_read_answers_with_the_set_the_core_holds() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let lead = seat("forge");
        let domain = workspace.register_domain_session(lead.clone(), None);
        domain.lock().monitors = vec![
            MonitorRecord {
                tool_use_id: "tu-live".to_owned(),
                task_id: Some("t-live".to_owned()),
                description: "ci-watch".to_owned(),
                command: "gh run watch 1".to_owned(),
                persistent: true,
                timeout_ms: 0,
                status: MonitorStatus::Running,
                output_file: None,
                ended_at: None,
            },
            MonitorRecord {
                tool_use_id: "tu-done".to_owned(),
                task_id: Some("t-done".to_owned()),
                description: "deploy-gate".to_owned(),
                command: "gh run watch 2".to_owned(),
                persistent: false,
                timeout_ms: 0,
                status: MonitorStatus::Completed,
                output_file: Some("/tmp/monitor-out".to_owned()),
                ended_at: None,
            },
        ];

        let held = surface.monitors(&lead);

        assert_eq!(held.len(), 2, "the whole set comes back, settled entries included");
        assert_eq!(held[0].status, MonitorStatus::Running, "a live monitor reads as running");
        assert_eq!(
            held[1].status,
            MonitorStatus::Completed,
            "and a settled one keeps the way it settled",
        );
        assert_eq!(
            held[1].output_file.as_deref(),
            Some("/tmp/monitor-out"),
            "with the file its output went to",
        );
        assert!(
            surface.monitors(&seat("never-started")).is_empty(),
            "and a seat with no session has no monitors",
        );
    }
}

//! The session's own facts: its header and the sections an inspector
//! draws from it.
//!
//! Each is held by the core from the same event the matching
//! `SessionUpdate` is built from, so this hands over what the core has
//! rather than deriving anything: a view reads the session instead of
//! folding the update stream a second time.

use std::sync::Arc;

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

    /// Store a walk's answer, which is the write half of [`Self::processes`].
    ///
    /// One store for both walkers: the terminal walks the seat it is addressing
    /// and the socket walks the seat a client reads. A second store would let
    /// the two answers drift, and two writers through one store of one shape
    /// is the whole of the sharing.
    pub fn store_process_snapshot(&self, slot: &SessionSlot, snapshot: Option<ProcessSnapshot>) {
        self.workspace.store_process_snapshot(slot, snapshot);
    }

    /// The seat's working tree, as the scan that owns it last answered, or as
    /// a read taken here when nothing has scanned it yet.
    ///
    /// **The read is the exception rather than the path.** A seat a view is
    /// showing is scanned by its own loop, so this answers a stored value and
    /// shells out to nothing; reading here covers a seat nobody has held,
    /// which is a lagging edge rather than a steady state.
    pub async fn work(&self, slot: &SessionSlot, cwd: &std::path::Path) -> WorkSnapshot {
        if let Some(held) = self.workspace.work_snapshot(slot) {
            return held;
        }
        self.workspace.scan_work_at(slot, cwd).await
    }

    /// Store a scan's answer, which is the write half of [`Self::work`].
    ///
    /// A fixture's door, and the same store the seat's own loop writes: one
    /// store means a record read and a pushed row cannot disagree.
    pub fn store_work_snapshot(&self, slot: &SessionSlot, snapshot: WorkSnapshot) {
        self.workspace.store_work_snapshot(slot, snapshot);
    }

    /// Show a seat: this view is looking at it, which is what keeps its
    /// working tree scanned while the view holds it.
    ///
    /// The answer is whether the hold was TAKEN - a seat with no session
    /// refuses one - and a caller must release only the holds it took.
    pub async fn hold_seat(self: &Arc<Self>, slot: &SessionSlot) -> bool {
        self.workspace.hold_seat(slot).await
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
    /// CLI has computed it: the terminal asks for the seat it is addressing,
    /// and the socket asks for the seat a client reads, so both reach the one
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

    /// The monitors the session has running, and the ones that settled
    /// while it did, folded from the wire.
    ///
    /// Empty when nothing is running, however many monitors have been and
    /// gone: the set drains once every entry in it is terminal, which is
    /// the rule the terminal applies to its own copy.
    pub fn monitors(&self, slot: &SessionSlot) -> Vec<MonitorRecord> {
        self.workspace
            .domain_session_for(slot)
            .map_or_else(Vec::new, |domain| domain.lock().monitors.clone())
    }

    /// The CLI's background-task registry for this seat, each entry with the
    /// command its own card carried.
    ///
    /// Empty when nothing is running. A view draws the processes feed's
    /// leading rows from this: the tasks are the CLI's own registry, held on
    /// the session so a view that attached after the snapshot arrived still
    /// reads them, and the command is what tells it whether the OS walk has
    /// already adopted the process behind one.
    pub fn background_tasks(&self, slot: &SessionSlot) -> Vec<forge_workspace::BackgroundTask> {
        self.workspace
            .domain_session_for(slot)
            .map_or_else(Vec::new, |domain| domain.lock().background_tasks.clone())
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

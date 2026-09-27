//! The session's own facts: its header and the sections an inspector
//! draws from it.
//!
//! Each is held by the core from the same event the matching
//! `SessionUpdate` is built from, so this hands over what the core has
//! rather than deriving anything: a view reads the session instead of
//! folding the update stream a second time.

use std::collections::HashMap;

use forge_primitives::{CurrentModel, EffortLevel, MonitorRecord, PermissionMode, SessionSlot};
use forge_workspace::env::processes::ProcessSnapshot;

use crate::surface::ViewSurface;

pub use forge_workspace::{ContextUsage, McpServers};

/// What a session header states about the session.
pub struct SessionHeader {
    /// The model the CLI resolved for this session, from its connect and
    /// from every later frame that names a different one.
    pub model: Option<CurrentModel>,
    /// The effort the session runs at: the hook-observed level when the
    /// session has reported one, else the level its launch stamped, else
    /// forge's default. Always a value rather than an `Option`, because
    /// a session that has not used a tool yet still runs at some level
    /// and a header that showed nothing there would be wrong.
    pub effort: EffortLevel,
    /// The permission mode a hook last observed. `None` until one fires:
    /// unlike effort there is no configured level to fall back to, since
    /// what the CLI runs at is its own default until something changes
    /// it, and that default is not a forge setting.
    pub permission_mode: Option<PermissionMode>,
    /// How full the context window is, from the bridge's last answer.
    pub context: ContextUsage,
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
                model: None,
                effort: EffortLevel::Max,
                permission_mode: None,
                context: ContextUsage::default(),
            };
        };
        let held = domain.lock();
        SessionHeader {
            model: held.current_model.clone(),
            effort: held.observed_effort.unwrap_or(held.configured_effort),
            permission_mode: held.observed_permission_mode,
            context: held.context_usage.unwrap_or_default(),
        }
    }

    /// Which sub-agent ran which tool call, keyed by the `tool_use_id`
    /// the sub-agent fired. This is not [`Self::subagents`], which is the
    /// CLI's catalogue of the agent types that exist: this is the
    /// per-call attribution of one session's own work.
    pub fn subagent_attribution(&self, slot: &SessionSlot) -> HashMap<String, String> {
        self.workspace
            .domain_session_for(slot)
            .map_or_else(HashMap::new, |domain| domain.lock().subagent_attribution.clone())
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
    pub fn processes(&self, slot: &SessionSlot) -> Option<ProcessSnapshot> {
        self.workspace.process_snapshot(slot)
    }

    /// The monitors the session has running or has finished, folded from
    /// the wire.
    pub fn monitors(&self, slot: &SessionSlot) -> Vec<MonitorRecord> {
        self.workspace
            .domain_session_for(slot)
            .map_or_else(Vec::new, |domain| domain.lock().monitors.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

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
        assert_eq!(
            header.permission_mode, None,
            "and the mode, which has no configured fallback, stays unstated",
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
    }

    /// The attribution is per session: a read that answered another
    /// seat's would label one session's tool calls with another's
    /// sub-agents.
    #[test]
    fn the_subagent_attribution_reads_the_seat_it_was_asked_about() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));
        let asked = seat("forge");
        let neighbour = seat("other");
        workspace
            .register_domain_session(asked.clone(), None)
            .lock()
            .subagent_attribution
            .insert("tu-asked".to_owned(), "Explore".to_owned());
        workspace
            .register_domain_session(neighbour.clone(), None)
            .lock()
            .subagent_attribution
            .insert("tu-neighbour".to_owned(), "code-reviewer".to_owned());

        let held = surface.subagent_attribution(&asked);

        assert_eq!(
            held.get("tu-asked").map(String::as_str),
            Some("Explore"),
            "the seat's own attribution is what comes back",
        );
        assert_eq!(held.len(), 1, "and only its own, rather than the fleet's");
        assert!(
            surface.subagent_attribution(&seat("never-started")).is_empty(),
            "a seat with no session attributes nothing",
        );
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

//! View fixtures for a sibling crate's tests.
//!
//! A `ViewSurface` only ever comes off an `Arc<Workspace>`, and the
//! constructors that build one without a real machine are behind
//! `forge-workspace`'s own test features. A crate testing a view would
//! otherwise have to name that crate in its manifest to get one; this
//! module is the way round, kept behind its own `testing` feature so a
//! production build never sees it.
//!
//! Every constructor here hands back its failure rather than panicking, so
//! a caller that wants a panic is the one that asks for it.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use forge_primitives::SessionSlot;
use forge_primitives::tasks::{Task, TaskStatus};
use forge_primitives::{CronEntry, CronId, CronKind, WebConfig};
use forge_workspace::{ProjectKey, Workspace};

use crate::SessionUpdate;
use crate::live::Live;
use crate::surface::{PendingKind, ViewSurface};
use crate::transport::TransportState;
use crate::work::WorkCache;

/// What a fixture hands back when it cannot build what was asked for.
pub type FixtureError = Box<dyn std::error::Error + Send + Sync>;

/// The facts a session holds that no update stream carries, for a view's
/// fixture to seed. Every field is what the session reports: leaving one
/// out is a session that has not reported it.
#[derive(Default)]
pub struct ViewFacts {
    pub model: Option<forge_primitives::CurrentModel>,
    /// The effort a hook observed, which a session reports only once it
    /// has used a tool.
    pub observed_effort: Option<forge_primitives::EffortLevel>,
    /// The effort the launch stamped, which stands until a hook reports.
    pub configured_effort: Option<forge_primitives::EffortLevel>,
    pub permission_mode: Option<forge_primitives::PermissionMode>,
    pub context: Option<forge_workspace::ContextUsage>,
    pub mcp: Option<forge_workspace::McpServers>,
    pub process_snapshot: Option<forge_workspace::env::processes::ProcessSnapshot>,
    pub monitors: Vec<forge_primitives::MonitorRecord>,
}

/// A view surface over a stub workspace, plus the seeding a test needs to
/// put a roster in front of it.
pub struct Fleet {
    surface: Arc<ViewSurface>,
    workspace: Arc<Workspace>,
    /// Where the workspace's store and the CLI's transcripts live, which a
    /// transcript fixture has to write into.
    config_dir: std::path::PathBuf,
}

/// The session id a seeded transcript belongs to. One per fleet is enough:
/// a test that needs two reads two slots against their own files.
const SEEDED_SESSION: &str = "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45";

impl TransportState {
    /// A server over a fixture fleet, for a test that opens a socket.
    pub fn for_test() -> Result<Self, FixtureError> {
        let fleet = Fleet::in_dir(&scratch_dir(), &[("TestOrg", &["proj"])])?;
        Ok(Self {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            live: Mutex::new(Live::new()),
            config: WebConfig::default(),
        })
    }
}

/// A directory under the system temp dir, one per call.
///
/// `Fleet` writes a store under the directory it is given and needs it to
/// outlive the fixture, so the directory is left in place rather than
/// cleaned up. This module ships with the library, so it cannot reach for a
/// temp-file crate: a dev-dependency is not in scope here.
fn scratch_dir() -> std::path::PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    std::env::temp_dir().join(format!(
        "forge-server-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

impl Fleet {
    /// A fleet whose `forge.toml` declares one project per name under each
    /// `(org, projects)` entry, in the order given. `config_dir` has to
    /// outlive the fleet - the workspace's store lives under it, and each
    /// project's path is a directory of its own under it.
    pub fn in_dir(config_dir: &Path, orgs: &[(&str, &[&str])]) -> Result<Self, FixtureError> {
        let forge = config_dir.join("forge");
        std::fs::create_dir_all(&forge)?;
        std::fs::write(forge.join("forge.toml"), config(config_dir, orgs))?;
        // `new_for_test` opens the store under `config_dir`'s own
        // app-support base, so a fixture gets a durable layer without
        // opening a second handle on the same file.
        let workspace = Arc::new(Workspace::new_for_test(config_dir.to_owned())?);
        // An empty document, so no fixture reads the machine's own
        // `$HOME/.claude.json` for a preference.
        workspace.seed_test_user_preferences(serde_json::json!({}));
        Ok(Self {
            surface: Arc::new(ViewSurface::new(Arc::clone(&workspace))),
            workspace,
            config_dir: config_dir.to_owned(),
        })
    }

    /// The surface, which keeps the workspace alive on its own.
    pub fn surface(&self) -> Arc<ViewSurface> {
        Arc::clone(&self.surface)
    }

    /// Push one update onto the core's stream, so a test can watch a view
    /// react to it without driving a whole session.
    pub fn emit(&self, update: SessionUpdate) {
        self.workspace.emit_for_test(update);
    }

    /// Hold a claude version snapshot, so a view test renders a version
    /// line without a real `claude --version` and npm probe.
    pub fn set_cli_version(&self, installed: Option<&str>, latest: Option<&str>) {
        self.workspace.seed_test_cli_version(installed, latest);
    }

    /// Hold the CLI's per-user preferences document, so a view test reads a
    /// preference of its own rather than the machine's.
    pub fn set_user_preferences(&self, preferences: serde_json::Value) {
        self.workspace.seed_test_user_preferences(preferences);
    }

    /// Give `project` a live lead session, registering the domain a spawn
    /// would.
    pub fn start(&self, org: &str, project: &str) -> Result<(), FixtureError> {
        self.workspace.register_domain_session(SessionSlot::lead(org, project), None);
        Ok(())
    }

    /// Put `label` under `project` as a worker that has run before: a
    /// persisted row, so it is offered, plus a live entry, so it is not
    /// asleep.
    pub fn add_worker(&self, org: &str, project: &str, label: &str) -> Result<(), FixtureError> {
        let key = self.project_key(project)?;
        self.workspace.seed_test_worker_row(&key, label);
        self.workspace.insert_live_worker(
            &key,
            forge_workspace::WorkerEntry {
                label: label.to_owned(),
                charter: format!("charter for {label}"),
                slot: SessionSlot::worker(org, project, label),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead(org, project),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
        self.workspace.register_domain_session(SessionSlot::worker(org, project, label), None);
        Ok(())
    }

    /// Write a session's transcript, where the CLI would have left it, and
    /// register `slot` as the session that owns it. `rows` are the JSONL
    /// lines themselves, so a fixture writes the wire shapes it means.
    pub fn seed_transcript(
        &self,
        org: &str,
        project: &str,
        label: &str,
        rows: &[&str],
    ) -> Result<(), FixtureError> {
        let slot = if label == "lead" {
            SessionSlot::lead(org, project)
        } else {
            SessionSlot::worker(org, project, label)
        };
        let cwd = self
            .workspace
            .cwd_for_session(&slot)
            .ok_or_else(|| format!("{project} holds no session for {label}"))?;
        let key = forge_workspace::userdata::catalog::scan::project_key_for_directory(Some(&cwd));
        let dir = self.config_dir.join("projects").join(key);
        std::fs::create_dir_all(&dir)?;
        self.workspace.seed_test_running_session_id(&slot, SEEDED_SESSION);
        std::fs::write(dir.join(format!("{SEEDED_SESSION}.jsonl")), rows.join("\n"))?;
        Ok(())
    }

    /// Give `slot` a live agent, which is what makes a view treat the seat as
    /// running rather than as one nothing is behind. A stub: the commands a
    /// view sends it are read by nobody.
    pub fn install_agent(&self, org: &str, project: &str, label: &str) {
        let slot = if label == "lead" {
            SessionSlot::lead(org, project)
        } else {
            SessionSlot::worker(org, project, label)
        };
        let _commands = self.workspace.install_testing_stub(&slot);
    }

    /// Catch every command the core is dispatched, so a test can assert
    /// what a view asked for without driving a session.
    pub fn intercept_dispatch(&self) {
        self.workspace.enable_test_dispatch_intercept();
    }

    /// The commands caught since the last call.
    pub fn dispatched(&self) -> Vec<forge_workspace::Command> {
        self.workspace.drain_test_dispatch_buffer()
    }

    /// Give `slot` the core's own record of a spawn that failed, as the
    /// connection-failure path leaves one.
    pub fn fail_spawn(&self, org: &str, project: &str, label: &str, reason: &str) {
        let slot = if label == "lead" {
            SessionSlot::lead(org, project)
        } else {
            SessionSlot::worker(org, project, label)
        };
        self.workspace.record_spawn_failure_for_test(&slot, reason);
    }

    /// Hold `slot` on a pending interaction, the way a session that has
    /// asked a person for something reads.
    pub fn seed_test_pending_interaction(&self, slot: &SessionSlot, kind: PendingKind) {
        self.workspace.seed_test_pending_interaction(slot, kind);
    }

    /// Let go of everything parked on `slot`, which is the half of answering
    /// that is the core's.
    pub fn clear_test_pending(&self, slot: &SessionSlot) {
        self.workspace.clear_test_pending(slot);
    }

    /// Hold `slot` waiting to be let in, the way a session that cannot
    /// authenticate reads.
    pub fn await_login(&self, slot: &SessionSlot) {
        self.workspace.seed_test_awaiting_login(slot);
    }

    /// Put `label` under `project` as a worker whose spawn has not
    /// connected yet, so its row reads as starting rather than asleep.
    pub fn add_starting_worker(
        &self,
        org: &str,
        project: &str,
        label: &str,
    ) -> Result<(), FixtureError> {
        let key = self.project_key(project)?;
        self.workspace.seed_test_worker_row(&key, label);
        self.workspace.insert_live_worker(
            &key,
            forge_workspace::WorkerEntry {
                label: label.to_owned(),
                charter: format!("charter for {label}"),
                slot: SessionSlot::worker(org, project, label),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Spawning,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead(org, project),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
        self.workspace.register_domain_session(SessionSlot::worker(org, project, label), None);
        Ok(())
    }

    /// Hold `slot` on a queue of `count` unanswered prompts, so a view
    /// test can render a depth a session holding several reads.
    pub fn seed_test_prompt_queue(&self, slot: &SessionSlot, kind: PendingKind, count: usize) {
        self.workspace.seed_test_prompt_queue(slot, kind, count);
    }

    /// Advertise `commands` and `agents` for `slot`, the way the CLI's
    /// init frame leaves them: what the composer's `/` and `&` triggers
    /// read.
    pub fn advertise(
        &self,
        slot: &SessionSlot,
        commands: Vec<forge_primitives::AvailableCommand>,
        agents: Vec<forge_primitives::AvailableAgent>,
    ) {
        self.workspace.seed_test_advertised_catalogues(slot, commands, agents);
    }

    /// Declare a task under `project`, held by the session labelled
    /// `owner`, with `artifact` as what it produced.
    pub fn add_task(
        &self,
        org: &str,
        project: &str,
        subject: &str,
        owner: &str,
        artifact: Option<&str>,
    ) -> Result<(), FixtureError> {
        self.workspace.seed_test_task(Task {
            id: subject.into(),
            project_name: project.to_owned(),
            subject: subject.to_owned(),
            active_form: None,
            detail: None,
            status: TaskStatus::InProgress,
            owner: Some(SessionSlot::worker(org, project, owner)),
            parent: None,
            artifact: artifact.map(str::to_owned),
            estimate: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        });
        Ok(())
    }

    /// Declare a durable cron under `project`, firing a day out so a view
    /// renders it as a schedule rather than as an overdue one.
    pub fn add_cron(&self, project: &str, prompt: &str) -> Result<(), FixtureError> {
        self.workspace.seed_test_cron(CronEntry {
            id: CronId::from(prompt),
            project_name: project.to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: prompt.to_owned(),
            description: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            last_fire: None,
            next_fire: std::time::SystemTime::now() + std::time::Duration::from_secs(86_400),
            team_role: None,
        });
        Ok(())
    }

    /// Record a transcript row for `project`, as a session that ran and
    /// ended would leave behind: a project whose session is gone but whose
    /// history is not.
    pub fn record_session(&self, project: &str, session_id: &str) -> Result<(), FixtureError> {
        let path = self.project_view(project)?.path.to_string_lossy().into_owned();
        self.workspace.record_connected_session(&path, session_id, None);
        Ok(())
    }

    /// Hold `facts` on `slot`'s session, as the events that carry them
    /// would: the header's four, the MCP snapshot, the process walk, the
    /// monitor set and the sub-agent attribution.
    ///
    /// A field left out is a fact the session has not reported, which is
    /// its own state rather than an empty one.
    pub fn seed_view_facts(&self, slot: &SessionSlot, facts: ViewFacts) {
        let domain = self
            .workspace
            .domain_session_for(slot)
            .unwrap_or_else(|| self.workspace.register_domain_session(slot.clone(), None));
        let mut held = domain.lock();
        held.current_model = facts.model;
        held.observed_effort = facts.observed_effort;
        if let Some(effort) = facts.configured_effort {
            held.configured_effort = effort;
        }
        held.observed_permission_mode = facts.permission_mode;
        held.context_usage = facts.context;
        held.mcp_servers = facts.mcp;
        held.process_snapshot = facts.process_snapshot;
        held.monitors = facts.monitors;
    }

    fn project_view(&self, project: &str) -> Result<forge_workspace::ProjectView, FixtureError> {
        self.workspace
            .list_projects()
            .into_iter()
            .find(|view| view.name == project)
            .ok_or_else(|| format!("{project} is not a project this fleet declared").into())
    }

    fn project_key(&self, project: &str) -> Result<ProjectKey, FixtureError> {
        self.project_view(project).map(|view| view.key)
    }
}

/// The `forge.toml` a fleet is built from: one `[[orgs]]` per entry, each
/// project under its own directory so two projects never share a path.
fn config(config_dir: &Path, orgs: &[(&str, &[&str])]) -> String {
    let mut sections: Vec<String> = Vec::new();
    for (org, projects) in orgs {
        sections.push(format!("[[orgs]]\nname = \"{org}\"\naccounts = [\"Acct\"]\n\n"));
        for project in *projects {
            let path = config_dir.join(project);
            sections.push(format!(
                "[[orgs.projects]]\nname = \"{project}\"\npath = \"{}\"\n\n",
                path.display()
            ));
        }
    }
    sections.push(
        "[[accounts]]\ndisplay_name = \"Acct\"\ntoken = \"t\"\nmodels = [\"claude-sonnet-5\"]\nprovider = \"anthropic\"\n"
            .to_owned(),
    );
    // Dictation on, so the composer draws the control that starts a take:
    // an install with it off has no way in at all.
    sections.push(format!(
        "[dictate]\nenabled = true\nmodels_dir = \"{}\"\n",
        config_dir.join("models").display()
    ));
    sections.concat()
}

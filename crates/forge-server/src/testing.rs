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
use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_primitives::tasks::{Task, TaskStatus};
use forge_primitives::{CronEntry, CronId, CronKind};
use forge_workspace::{ProjectKey, Workspace};
use tokio::sync::mpsc;

use crate::SessionUpdate;
use crate::surface::{PendingKind, ViewSurface};
use crate::transport::TransportState;

/// What a fixture hands back when it cannot build what was asked for.
pub type FixtureError = Box<dyn std::error::Error + Send + Sync>;

/// The facts a session holds that no update stream carries, for a view's
/// fixture to seed. Every field is what the session reports: leaving one
/// out is a session that has not reported it.
#[derive(Default)]
pub struct ViewFacts {
    pub session_id: Option<forge_primitives::SessionId>,
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
    /// The CLI's background-task registry, as `background_tasks_changed`
    /// would have left it.
    pub background_tasks: Vec<forge_workspace::BackgroundTask>,
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
    /// Declared last, so the directory goes after the store this fixture opened
    /// under it has closed - unless a surface is cloned into something that
    /// outlives the fleet, which unlinks the file while it is still open.
    owned_dir: Option<tempfile::TempDir>,
}

/// The session id a seeded transcript belongs to. One per fleet is enough:
/// a test that needs two reads two slots against their own files.
const SEEDED_SESSION: &str = "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45";

impl Fleet {
    /// A fleet over a config directory of its own, removed with the fleet.
    pub fn new(orgs: &[(&str, &[&str])]) -> Result<Self, FixtureError> {
        let dir = tempfile::tempdir()?;
        let mut fleet = Self::in_dir(dir.path(), orgs)?;
        fleet.owned_dir = Some(dir);
        Ok(fleet)
    }

    /// A fleet whose `forge.toml` declares one project per name under each
    /// `(org, projects)` entry, in the order given. `config_dir` has to
    /// outlive the fleet - the workspace's store lives under it, and each
    /// project's path is a directory of its own under it.
    ///
    /// **The project directories are not created.** A fixture that makes one
    /// itself moves the key `project_key` answers, because that derivation
    /// canonicalises a path that now resolves - and everything the fixture
    /// registered before it appeared is registered under a key nothing looks
    /// up again. Read `project_key` before creating one.
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
            owned_dir: None,
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

    /// Give a project a task, so a fixture can read one back rather than
    /// answering with an empty list a reader cannot tell from a missing field.
    pub fn seed_task(&self, task: forge_primitives::tasks::Task) {
        self.workspace.seed_test_task(task);
    }

    /// [`Self::emit`], reporting whether a subscriber was there to take it.
    ///
    /// `false` once nothing is listening, which is how a test tells a
    /// subscription still attached from one that went with its socket.
    pub fn emit_and_report(&self, update: SessionUpdate) -> bool {
        self.workspace.emit_for_test_reported(update)
    }

    /// How many subscribers are attached right now, the transport's own fold
    /// included. A test that watches a socket's subscription arrive and leave
    /// reads this rather than asking whether an emit landed: the fold makes
    /// every emit land, whether or not a client is there.
    pub fn subscriber_count(&self) -> usize {
        self.workspace.test_subscriber_count()
    }

    /// How many subscribers could answer a prompt right now. The role decides
    /// whether the core parks a turn on a reply rather than resolving it
    /// `Cancelled`, and nothing on the wire reports it - so a test that watches
    /// a client declare `answering` reads this.
    pub fn answering_count(&self) -> usize {
        self.workspace.test_answering_count()
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

    /// A worker label with no live registry entry: the row a despawned worker
    /// leaves behind, which is drawn until the label is despawned.
    ///
    /// The project's directory has to exist for the label to be offered at
    /// all - `worker_row_can_start` asks whether the tree it would start in
    /// is there.
    pub fn add_despawned_worker(&self, project: &str, label: &str) -> Result<(), FixtureError> {
        let key = self.project_key(project)?;
        self.workspace.seed_test_worker_row(&key, label);
        Ok(())
    }

    /// [`Self::add_worker`] for a worker spawned in a git repo, whose tree
    /// is the worktree under the project rather than the project root.
    ///
    /// The two reads differ, which is the whole point: a fixture that puts
    /// every worker in the project's own tree cannot tell a per-seat read
    /// from a per-project one.
    pub fn add_git_worker(
        &self,
        org: &str,
        project: &str,
        label: &str,
    ) -> Result<(), FixtureError> {
        let key = self.project_key(project)?;
        self.workspace.seed_test_git_worker_row(&key, label);
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
                is_git_repo_at_spawn: true,
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

    /// Hold the seat's conversation on `state`, read from the transcript a
    /// fixture just seeded.
    ///
    /// **The transport no longer reads a transcript**, so a fixture that wants
    /// a seat to answer with one has to put it there: it reads the transcript
    /// RAW - the shape a `Connected` carries and a replay answers with - and
    /// lets the conversation convert it, which is the state a `Connected`
    /// would have left. Not through `ViewSurface::conversation`, which
    /// converts the task notices on the way out: a fixture built on the
    /// converted shape would exercise something production never seeds from,
    /// so a defect in the seed's own conversion could not be seen here.
    ///
    /// `has_dispatches` is left to the conversation's own rule rather than
    /// passed in: a fixture that set it by hand would pin a value the fold
    /// computes, and the two could disagree without a test saying so.
    pub fn hold_conversation(
        &self,
        state: &TransportState,
        org: &str,
        project: &str,
        label: &str,
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
        let read = forge_workspace::session_history(
            self.workspace.config_dir(),
            &self
                .workspace
                .running_session_id_for(&slot)
                .ok_or_else(|| format!("{project} holds no running session for {label}"))?,
            &cwd,
        );
        state.conversations.insert(
            &slot,
            crate::transport::conversation::Conversation::new(read.messages, read.compaction_count),
        );
        Ok(())
    }

    /// Give `slot` a live agent, which is what makes a view treat the seat as
    /// running rather than as one nothing is behind. A stub: nothing answers
    /// the commands it is sent, and a caller that holds the receiver it hands
    /// back reads what was asked.
    pub fn install_agent(
        &self,
        org: &str,
        project: &str,
        label: &str,
    ) -> mpsc::UnboundedReceiver<forge_primitives::AgentCommand> {
        let slot = if label == "lead" {
            SessionSlot::lead(org, project)
        } else {
            SessionSlot::worker(org, project, label)
        };
        self.workspace.install_testing_stub(&slot)
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
        held.session_id = facts.session_id;
        held.current_model = facts.model;
        held.observed_effort = facts.observed_effort;
        if let Some(effort) = facts.configured_effort {
            held.configured_effort = effort;
        }
        held.observed_permission_mode = facts.permission_mode;
        held.context_usage = facts.context;
        held.mcp_servers = facts.mcp;
        held.process_snapshot = facts.process_snapshot;
        held.background_tasks = facts.background_tasks;
        held.monitors = facts.monitors;
    }

    fn project_view(&self, project: &str) -> Result<forge_workspace::ProjectView, FixtureError> {
        self.workspace
            .list_projects()
            .into_iter()
            .find(|view| view.name == project)
            .ok_or_else(|| format!("{project} is not a project this fleet declared").into())
    }

    /// The registry key for `project`.
    ///
    /// **The key is derived from the project's PATH, and the derivation
    /// canonicalises it**, so the same project answers a different key before
    /// and after its directory exists. `in_dir` writes the config without
    /// creating the project directories, so a fixture that creates one itself
    /// gets a key that moves under it: anything registered before it appeared
    /// is registered under a key nothing looks up again, and the failure is
    /// silent - an empty worker list rather than an error.
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

#[cfg(test)]
mod tests {
    use super::Fleet;

    /// A fixture's directory goes with the fixture.
    ///
    /// Asserted across three, because one that abandons only its first
    /// directory would pass with one.
    #[test]
    fn a_fixtures_directory_is_removed_with_the_fixture() {
        let paths: Vec<std::path::PathBuf> = (0..3)
            .map(|_| {
                let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
                let path = fleet.config_dir.clone();
                assert!(
                    path.exists(),
                    "the directory is there while the fixture is: {}",
                    path.display(),
                );
                path
            })
            .collect();

        for path in paths {
            assert!(
                !path.exists(),
                "and is removed with it, not abandoned under the temp dir: {}",
                path.display(),
            );
        }
    }
}

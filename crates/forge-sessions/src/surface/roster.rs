//! `roster()`: the projects and their sessions, plus the per-project
//! lists the Projects pane draws.

use std::path::PathBuf;
use std::sync::Arc;

use forge_primitives::CronEntry;
use forge_primitives::SessionSlot;
use forge_primitives::tasks::Task;
use forge_workspace::{ProjectKey, ProjectView, SessionChipInfo, Workspace};

/// The projects and the per-project lists a view draws them from.
pub struct Roster {
    workspace: Arc<Workspace>,
    /// Every project in `forge.toml`, each carrying its catalog
    /// sessions; the same list [`Workspace::list_projects`] returns.
    pub projects: Vec<ProjectView>,
}

impl Roster {
    pub(super) fn collect(workspace: Arc<Workspace>) -> Self {
        let projects = workspace.list_projects();
        Self { workspace, projects }
    }

    /// The project `name` declares, or `None` for a name no project
    /// declares.
    pub fn project_named(&self, name: &str) -> Option<&ProjectView> {
        self.projects.iter().find(|project| project.name == name)
    }

    /// The declared project NAME that owns `cwd`, matched by the
    /// longest configured ancestor path.
    pub fn project_name_for_path(&self, cwd: &str) -> Option<String> {
        self.workspace.project_name_for_path(cwd)
    }

    /// The names of the projects pinned to spawn at launch.
    pub fn auto_start(&self) -> Vec<String> {
        self.workspace.auto_start_project_names()
    }

    /// The account chip `project` renders beside its row, or `None`
    /// when no account would serve it.
    pub fn chip_for(&self, project: &ProjectKey) -> Option<SessionChipInfo> {
        self.workspace.session_chip_for(project)
    }

    /// Whether a spawn in `project` would find an account.
    pub fn would_bind(&self, project: &ProjectKey) -> bool {
        self.workspace.project_would_bind(project)
    }

    /// Whether `slot` has a workspace-side domain registered.
    pub fn has_domain(&self, slot: &SessionSlot) -> bool {
        self.workspace.domain_session_for(slot).is_some()
    }

    /// Whether `slot` has a live agent handle.
    pub fn has_agent(&self, slot: &SessionSlot) -> bool {
        self.workspace.has_agent_for(slot)
    }

    /// OS pid of `slot`'s `claude` subprocess, or `None` when the
    /// session has no live client.
    pub fn claude_pid(&self, slot: &SessionSlot) -> Option<u32> {
        self.workspace.claude_pid(slot)
    }

    /// The config dir `slot`'s agent reads, or `None` when no agent is
    /// registered for it.
    pub fn config_dir(&self, slot: &SessionSlot) -> Option<PathBuf> {
        self.workspace.config_dir_for(slot)
    }

    /// The task rows declared under the project named `project`.
    pub fn tasks_for_project(&self, project: &str) -> Vec<Task> {
        self.workspace.tasks_for_project(project)
    }

    /// The durable cron rows declared under the project named
    /// `project`.
    pub fn crons_for_project(&self, project: &str) -> Vec<CronEntry> {
        self.workspace.crons_for_project(project)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_primitives::tasks::{Task, TaskStatus};
    use forge_primitives::{CronEntry, CronId, CronKind, SessionSlot, TaskId};
    use forge_workspace::Workspace;

    use crate::surface::ViewSurface;

    fn stub_workspace() -> Arc<Workspace> {
        let (workspace, _updates) = Workspace::testing_stub();
        workspace
    }

    fn sample_task(id: &str, project: &str) -> Task {
        Task {
            id: TaskId::from(id),
            project_name: project.to_owned(),
            subject: format!("subject {id}"),
            active_form: None,
            detail: None,
            status: TaskStatus::Pending,
            owner: None,
            parent: None,
            artifact: None,
            estimate: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    fn workspace_with_a_bound_project() -> (tempfile::TempDir, Arc<Workspace>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge_dir = dir.path().join("forge");
        std::fs::create_dir_all(&forge_dir).expect("forge dir");
        std::fs::write(
            forge_dir.join("forge.toml"),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        (dir, workspace)
    }

    fn sample_cron(id: &str, project: &str) -> CronEntry {
        CronEntry {
            id: CronId::from(id),
            project_name: project.to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            description: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        }
    }

    /// Catches a roster that stops taking its project list from
    /// `list_projects` (filtering it, reordering it, or serving a
    /// stale walk), and a `project_name_for_path` that reimplements the
    /// ancestor match instead of forwarding it.
    #[test]
    fn roster_carries_the_projects_and_resolves_cwds_as_the_workspace_does() {
        let workspace = stub_workspace();
        workspace.seed_test_project("forge", "/tmp/forge-roster-a");
        workspace.seed_test_project("other", "/tmp/forge-roster-a/other");

        let roster = ViewSurface::new(Arc::clone(&workspace)).roster();
        let direct = workspace.list_projects();

        let verb: Vec<&str> = roster.projects.iter().map(|p| p.name.as_str()).collect();
        let expected: Vec<&str> = direct.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(verb, expected, "the roster must carry the projects list_projects returns");
        for project in &roster.projects {
            let same =
                direct.iter().find(|p| p.name == project.name).expect("every roster project");
            assert_eq!(project.path, same.path, "the roster must carry the project's own path");
            assert_eq!(
                project.sessions.len(),
                same.sessions.len(),
                "the roster must carry every catalog session row"
            );
        }

        assert_eq!(
            roster.project_named("other").map(|p| p.name.clone()),
            Some("other".to_owned()),
            "project_named must answer for a declared name"
        );
        assert!(
            roster.project_named("absent").is_none(),
            "project_named must answer None for an undeclared name"
        );

        // The nested cwd is owned by the longer of the two seeded paths,
        // so a first-match or a shortest-prefix implementation disagrees.
        let nested = "/tmp/forge-roster-a/other/nested";
        assert_eq!(
            roster.project_name_for_path(nested),
            workspace.project_name_for_path(nested),
            "the roster must resolve a cwd to the project the workspace names"
        );
        assert_eq!(
            roster.project_name_for_path(nested),
            Some("other".to_owned()),
            "the roster must resolve a cwd to its longest owning path"
        );
        assert_eq!(
            roster.project_name_for_path("/tmp/nowhere"),
            workspace.project_name_for_path("/tmp/nowhere"),
            "a cwd under no project must resolve to None on both paths"
        );
    }

    /// Catches a roster read of a session's domain, agent, pid or config
    /// dir that answers from its own bookkeeping rather than the
    /// workspace's.
    #[test]
    fn roster_reads_a_sessions_domain_and_agent_as_the_workspace_does() {
        let workspace = stub_workspace();
        let connected = SessionSlot::lead("TestOrg", "forge");
        let idle = SessionSlot::lead("TestOrg", "other");
        let _stub = workspace.install_testing_stub(&connected);

        let roster = ViewSurface::new(Arc::clone(&workspace)).roster();

        assert!(
            roster.has_domain(&connected),
            "a session with an agent behind it must read as registered"
        );
        assert_eq!(
            roster.has_domain(&idle),
            workspace.domain_session_for(&idle).is_some(),
            "an unregistered session must read as unregistered"
        );
        assert!(
            roster.has_agent(&connected),
            "a session with a live handle must read as having one"
        );
        assert_eq!(
            roster.has_agent(&idle),
            workspace.has_agent_for(&idle),
            "a session with no handle must read as having none"
        );
        assert_eq!(
            roster.claude_pid(&connected),
            workspace.claude_pid(&connected),
            "the roster's pid must be the workspace's own answer"
        );
        assert_eq!(
            roster.config_dir(&connected),
            workspace.config_dir_for(&connected),
            "the roster's config dir must be the workspace's own answer"
        );
        assert!(
            roster.config_dir(&connected).is_some(),
            "an installed agent must resolve a config dir"
        );
    }

    /// Catches a `chip_for` or `would_bind` that stops forwarding to the
    /// walk, or that answers from a snapshot taken when the roster was
    /// collected rather than from the core as it is now.
    ///
    /// The account is seeded after the roster is collected, so an
    /// eagerly-filled roster fails the first assertion.
    #[test]
    fn roster_chip_and_binding_forward_the_walk() {
        let (_dir, workspace) = workspace_with_a_bound_project();
        let roster = ViewSurface::new(Arc::clone(&workspace)).roster();
        let project = roster.project_named("forge").expect("configured project").key.clone();

        workspace.seed_test_ready_account("Stargate");

        assert!(
            roster.would_bind(&project),
            "a ready account declaring the project's model is what a spawn lands on"
        );
        assert_eq!(
            roster.would_bind(&project),
            workspace.project_would_bind(&project),
            "the binding answer must be the workspace's own"
        );
        assert_eq!(
            roster.chip_for(&project).map(|chip| chip.account_name),
            workspace.session_chip_for(&project).map(|chip| chip.account_name),
            "the chip must be the workspace's own answer"
        );
        assert_eq!(
            roster.chip_for(&project).map(|chip| chip.account_name),
            Some("Stargate".to_owned()),
            "the chip must name the account the walk would pick"
        );
    }

    /// Catches task or cron rows that come from anywhere but the
    /// project's own store slice.
    #[test]
    fn roster_rows_come_from_the_project_stores_as_the_workspace_reports_them() {
        let workspace = stub_workspace();
        workspace.seed_test_project("forge", "/tmp/forge-roster-b");
        workspace.seed_test_task(sample_task("t-1", "forge"));
        workspace.seed_test_cron(sample_cron("c-1", "forge"));

        let roster = ViewSurface::new(Arc::clone(&workspace)).roster();

        assert_eq!(
            roster.tasks_for_project("forge"),
            workspace.tasks_for_project("forge"),
            "the roster's task rows must be the project's own"
        );
        assert_eq!(roster.tasks_for_project("forge").len(), 1, "the seeded task must be listed");
        assert!(
            roster.tasks_for_project("other").is_empty(),
            "another project must see none of its tasks"
        );
        assert_eq!(
            roster.crons_for_project("forge"),
            workspace.crons_for_project("forge"),
            "the roster's cron rows must be the project's own"
        );
        assert_eq!(roster.crons_for_project("forge").len(), 1, "the seeded cron must be listed");

        let project = roster.project_named("forge").expect("seeded project").key.clone();
        assert_eq!(
            roster.chip_for(&project),
            workspace.session_chip_for(&project),
            "the chip must be the workspace's own answer"
        );
        assert_eq!(
            roster.would_bind(&project),
            workspace.project_would_bind(&project),
            "the binding answer must be the workspace's own"
        );
    }
}

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
use forge_workspace::{ProjectKey, Workspace};

use crate::surface::ViewSurface;

/// What a fixture hands back when it cannot build what was asked for.
pub type FixtureError = Box<dyn std::error::Error + Send + Sync>;

/// A view surface over a stub workspace, plus the seeding a test needs to
/// put a roster in front of it.
pub struct Fleet {
    surface: Arc<ViewSurface>,
    workspace: Arc<Workspace>,
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
        Ok(Self { surface: Arc::new(ViewSurface::new(Arc::clone(&workspace))), workspace })
    }

    /// The surface, which keeps the workspace alive on its own.
    pub fn surface(&self) -> Arc<ViewSurface> {
        Arc::clone(&self.surface)
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

    /// Declare a task under `project`, held by the session labelled
    /// `owner`.
    pub fn add_task(
        &self,
        org: &str,
        project: &str,
        subject: &str,
        owner: &str,
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
            artifact: None,
            estimate: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            updated_at: std::time::SystemTime::UNIX_EPOCH,
        });
        Ok(())
    }

    fn project_key(&self, project: &str) -> Result<ProjectKey, FixtureError> {
        self.workspace
            .list_projects()
            .into_iter()
            .find(|view| view.name == project)
            .map(|view| view.key)
            .ok_or_else(|| format!("{project} is not a project this fleet declared").into())
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
    sections.concat()
}

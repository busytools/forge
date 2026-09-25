//! The view surface: what a view may read from the core.
//!
//! One verb per subject rather than the workspace's own methods, so a
//! second view attaches to a contract instead of rediscovering the first
//! view's call sites. A verb returns a snapshot built from the same
//! internals the direct calls used, and every value in it is
//! self-contained - no terminal type crosses.

pub mod accounts;
pub mod connectors;
pub mod dictate;
pub mod plugins;
pub mod reviews;

use std::sync::Arc;

use forge_workspace::Workspace;

/// A view's handle on the core.
pub struct ViewSurface {
    workspace: Arc<Workspace>,
}

impl ViewSurface {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }

    /// The workspace behind the surface. Reads go through the verbs;
    /// this is for the dispatch and subscribe plumbing a view drives
    /// itself.
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use forge_workspace::Workspace;

    const CONFIG: &str = "\
[[orgs]]
name = \"TestOrg\"
accounts = [\"Granite\", \"OpenRouter-TM\"]

[[orgs.projects]]
name = \"forge\"
path = \"/tmp\"

[[accounts]]
display_name = \"Granite\"
token = \"t\"
models = [\"claude-sonnet-5\"]
provider = \"anthropic\"

[[accounts]]
display_name = \"OpenRouter-TM\"
token = \"t\"
models = [\"deepseek-v4.1-flash\"]
provider = \"openrouter\"
base_url = \"https://openrouter.ai/api\"

[dictate]
enabled = true
models_dir = \"/tmp/forge-dictate-models\"
";

    /// A workspace over a tempdir config with two accounts and one
    /// project, with its store open, plus the tempdir that must outlive
    /// it.
    pub(crate) fn workspace() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(forge.join("forge.toml"), CONFIG).expect("write forge.toml");
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("workspace");
        workspace.install_db_for_test(
            forge_workspace::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        (Arc::new(workspace), dir)
    }

    /// A workspace whose durable Gotify and Slack subscriptions were
    /// written before it opened its store, so the boot load puts them in
    /// memory and the reads see real rows.
    pub(crate) fn workspace_with_connector_subs() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(forge.join("forge.toml"), CONFIG).expect("write forge.toml");
        let app_support = dir.path().join("app-support");
        std::fs::create_dir_all(&app_support).expect("app-support/");
        {
            // `new_for_test` opens this exact path and loads both sets at
            // construction, so the rows have to be there first.
            let db =
                forge_workspace::store::Db::open(&app_support.join("db.redb")).expect("open db");
            forge_workspace::store::gotify::insert(&db, &gotify_sub("forge"))
                .expect("insert gotify sub");
            forge_workspace::store::slack::insert(&db, &slack_sub("forge"))
                .expect("insert slack sub");
        }
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("workspace");
        (Arc::new(workspace), dir)
    }

    pub(crate) fn gotify_sub(project: &str) -> forge_primitives::GotifySubscription {
        forge_primitives::GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: project.to_owned(),
            team_role: None,
            applications: vec!["forge".to_owned()],
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    pub(crate) fn slack_sub(project: &str) -> forge_primitives::slack::SlackSubscription {
        forge_primitives::slack::SlackSubscription {
            id: uuid::Uuid::new_v4(),
            workspace: "acme".to_owned(),
            project: project.to_owned(),
            team_role: None,
            target: forge_primitives::slack::SlackSubscriptionTarget::DirectMessages,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }
}

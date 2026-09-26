//! The read surface a view uses: named verbs by subject, returning
//! values that carry no terminal type.
//!
//! Writes stay on `Workspace::dispatch` and changes on
//! `Workspace::subscribe`; this is the read half, so a second view
//! attaches to the core without reading it.

pub mod accounts;
pub mod agents;
pub mod connectors;
pub mod dictate;
pub mod plugins;
pub mod reviews;
pub mod roster;
pub mod session;
pub mod workers;

use std::path::Path;
use std::sync::Arc;

use forge_primitives::SessionSlot;
use forge_workspace::Workspace;

pub use accounts::{AccountsView, GatewayView};
pub use agents::{AgentRow, Agents, PendingKind};
pub use dictate::DictateView;
// A view compares the values the surface hands it, so it needs their names
// too - re-exported here rather than reached for in the crate below, which
// a view does not name.
pub use forge_workspace::{DictateFailure, DictateModelState, LoadingState};
pub use roster::Roster;
pub use session::SessionState;
pub use workers::{WorkerRef, Workers};

/// A view's read handle on the core.
pub struct ViewSurface {
    workspace: Arc<Workspace>,
}

/// The update the core emits, re-exported: it is the workspace-to-view
/// protocol already, so a view subscribes to it rather than to a second
/// vocabulary this crate would have to keep in step.
pub use forge_workspace::SessionUpdate;

/// The wire shape a turn that finished arrives as. A view marking a
/// completion reads the `Result` through this rather than deciding for
/// itself which results count.
pub fn is_success_result(is_error: bool, subtype: &str) -> bool {
    !is_error && subtype == "success"
}

impl ViewSurface {
    pub fn new(workspace: Arc<Workspace>) -> Self {
        Self { workspace }
    }

    /// The core's own update stream, as a mirror of it. Every caller gets
    /// a receiver of its own, so a second view attaches beside the first
    /// rather than stealing its events.
    ///
    /// This is the form for a view that renders no prompt: the observer
    /// role, and no replay of what was emitted before the caller attached,
    /// because it reads what it needs from the read verbs when it is
    /// asked. Both halves are deliberate and neither suits the TUI, which
    /// is the view that DOES render prompts and answer them - moving it
    /// onto this verb would make it an observer, and every permission and
    /// question request would resolve `Cancelled` instead of reaching a
    /// person. It stays on `Workspace::subscribe`.
    pub fn subscribe(&self) -> tokio::sync::mpsc::UnboundedReceiver<SessionUpdate> {
        self.workspace.subscribe_mirror()
    }

    /// The projects, their sessions, and the per-project lists the
    /// Projects pane renders.
    pub fn roster(&self) -> Roster {
        Roster::collect(Arc::clone(&self.workspace))
    }

    /// One session's operational state. `cwd_raw` is the caller's own
    /// cwd for the session, which a git worker's worktree overrides.
    pub fn session(&self, slot: &SessionSlot, cwd_raw: &Path) -> SessionState {
        SessionState::collect(&self.workspace, slot, cwd_raw)
    }

    /// The live workers, per project.
    pub fn workers(&self) -> Workers {
        Workers::collect(Arc::clone(&self.workspace))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{SessionUpdate, ViewSurface};

    /// The mirror is the call site, not the fanout. Both verbs sit on one
    /// `UpdateFanout`, whose own test pins its two halves; what this pins
    /// is which half each one calls, because repointing the surface's verb
    /// at `Workspace::subscribe` steals the boot notice from the view that
    /// renders prompts and every other test in the tree stays green.
    #[tokio::test]
    async fn a_surface_mirror_leaves_the_backlog_for_a_workspace_subscriber() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = ViewSurface::new(Arc::clone(&workspace));

        // Emitted with nothing attached, so the fanout holds it for
        // whichever caller attaches first.
        workspace.emit_for_test(SessionUpdate::CatalogLoaded);

        let mut mirror = surface.subscribe();
        assert!(
            mirror.try_recv().is_err(),
            "a view attaching through the surface takes no backlog, since the held notice \
             belongs to the view that renders prompts",
        );

        let mut answering = workspace.subscribe();
        assert!(
            matches!(answering.try_recv(), Ok(SessionUpdate::CatalogLoaded)),
            "the notice held before any caller attached is still there for the workspace \
             subscriber, which is what the mirror left it for",
        );
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

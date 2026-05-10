//! Sleeping-project spawn flow (Phase 2b-α).
//!
//! When the user clicks a project header in the Projects pane whose
//! lead session isn't currently in `app.sessions`, this module
//! synthesizes a `__spawn_<project_name>__` Session bucket
//! immediately, switches the active session to it (so chat shows a
//! "Waking …" placeholder), and kicks off the background work to
//! ask `Workspace` for the project's lead AgentHandle. When the
//! agent emits its first `Connected` event, the synthetic-key
//! migration in `events::session::handle_connected_client_event`
//! recognizes the `__spawn_<project>__` sentinel and migrates the
//! bucket onto the real claude-issued session UUID — so the
//! placeholder welcome, the cwd, and the spawning lifecycle state
//! survive the migration.
//!
//! Reuses the same `CONN_SLOT` plumbing the startup flow uses:
//! the spawned task writes the freshly-built `Arc<AgentHandle>`
//! into the slot, and the Connected event handler picks it up via
//! `take_connection_slot`.
//!
//! Single-flight assumption inherited from the startup flow: each
//! spawn overwrites the previous slot pointer in `CONN_SLOT`, so
//! two clicks in rapid succession (before the first Connected
//! event lands) can race. Acceptable for Phase 2b-α; Phase 2b
//! tightens this.

use std::rc::Rc;

use crate::agent::client::SessionLaunchSettings;
use crate::app::App;

use super::bridge_lifecycle;
use super::{CONN_SLOT, ConnectionSlot, StartConnectionParams};

/// Public entry point invoked by the Projects-pane click handler
/// when the user clicks a sleeping project's header.
///
/// Synthesizes a `__spawn_<project_key>__` Session bucket
/// (lifecycle = Spawning, placeholder welcome message, cwd seeded
/// from the project's display path), switches the active session
/// to it, and spawns a background task that calls
/// [`forge_workspace::Workspace::get_agent_handle`] with
/// [`forge_workspace::SessionTarget::Named`].
///
/// `project_key` is the canonicalised on-disk project key string
/// (matches `ProjectView::key.as_str()`) — that's what the Projects
/// pane stamps on its hit targets. The toml `name` used for
/// [`forge_workspace::SessionTarget::Named`] is resolved from the
/// matched [`forge_workspace::ProjectView`].
///
/// For the test path that wants to drive the helper directly
/// without a real Workspace, `project_key` and the toml `name` can
/// be the same value (the test's `forge.toml` simply names the
/// project after its key).
///
/// No-op when:
/// - the App has no workspace (test scaffolds, broken-invariant paths)
/// - no project in `forge.toml` matches `project_key`
///
/// In both cases the call returns silently and the previously-active
/// session stays active.
pub fn spawn_for_sleeping_project(app: &mut App, project_key: &str) {
    let Some(workspace) = app.workspace.as_ref().map(Rc::clone) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "spawn_for_sleeping_project_no_workspace",
            project = %project_key,
            "spawn requested without a workspace; ignoring"
        );
        return;
    };

    let Some(project) = workspace
        .list_projects()
        .into_iter()
        .find(|p| p.key.as_str() == project_key || p.name == project_key)
    else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "spawn_for_sleeping_project_unknown",
            project = %project_key,
            "spawn requested for an unknown project; ignoring"
        );
        return;
    };
    // The toml `name` field is what `SessionTarget::Named` resolves
    // against — distinct from `key.as_str()` (the canonicalised
    // on-disk project key). Click flow stamps the key; we resolve
    // the name here.
    let project_name = project.name.clone();

    // Synthesize the spawning bucket. The synthetic-key sentinel
    // pattern `__<name>__` is what the Connected handler matches
    // when it migrates the bucket onto the real session UUID.
    let spawn_key =
        forge_workspace::SessionKey::from_session_id(format!("__spawn_{project_name}__"));

    // If the bucket already exists (user clicked the same sleeping
    // project twice in rapid succession before the first Connected
    // landed), don't re-synthesize — just switch to the existing
    // bucket so the user sees the in-flight spawning state.
    if app.sessions.contains_key(&spawn_key) {
        app.switch_active_session(spawn_key);
        return;
    }

    let mut bucket = crate::app::session::Session::new(spawn_key.clone());
    bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Spawning;
    bucket.cwd_raw.clone_from(&project.display_path);
    bucket.cwd.clone_from(&project.display_path);
    // Placeholder welcome — `sync_welcome_snapshot` after Connected
    // refreshes this with the real account / subscription details.
    bucket.messages.push(crate::app::ChatMessage::welcome(
        env!("CARGO_PKG_VERSION"),
        "...",
        &project.display_path,
        "...",
    ));
    bucket.message_retained_bytes.push(0);
    app.sessions.insert(spawn_key.clone(), bucket);
    app.switch_active_session(spawn_key.clone());

    // Build the connection task params. Use the same plumbing as
    // the startup flow (`run_connection_task`) but route it for a
    // Named target. Failures before the first Connected event get
    // tagged with the spawn synthetic key so the visible spawning
    // bucket gets the connection-failed message.
    let params = StartConnectionParams {
        event_tx: app.event_tx.clone(),
        workspace,
        session_launch_settings: SessionLaunchSettings::default(),
        target: forge_workspace::SessionTarget::Named(project_name.clone()),
        pre_connect_key: spawn_key.clone(),
    };

    // Allocate a fresh slot pointer, install it as the latest
    // CONN_SLOT writer, and hand the writer to the connection task.
    // Single-flight: subsequent spawns overwrite this; see module
    // doc-comment for the trade-off.
    let conn_slot: Rc<std::cell::RefCell<Option<ConnectionSlot>>> =
        Rc::new(std::cell::RefCell::new(None));
    let conn_slot_writer = Rc::clone(&conn_slot);
    CONN_SLOT.with(|slot| {
        *slot.borrow_mut() = Some(conn_slot);
    });

    let project_for_log = project_name.clone();
    tokio::task::spawn_local(async move {
        bridge_lifecycle::run_connection_task(params, conn_slot_writer).await;
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "spawn_for_sleeping_project_task_exited",
            project = %project_for_log,
            "sleeping-project spawn connection task exited",
        );
    });

    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "spawn_for_sleeping_project_started",
        project = %project_name,
        "sleeping-project spawn task started",
    );
}

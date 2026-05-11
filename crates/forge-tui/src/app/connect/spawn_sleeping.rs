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
//! The AgentHandle for the freshly-spawned session is routed
//! through the `ClientEvent::Connected` envelope itself, so no
//! thread-local sidecar is needed and rapid spawn-sleeping clicks
//! no longer race over a shared CONN_SLOT pointer.

use std::panic::AssertUnwindSafe;
use std::rc::Rc;

use futures::FutureExt as _;

use crate::agent::client::SessionLaunchSettings;
use crate::app::App;

use super::StartConnectionParams;
use super::bridge_lifecycle;

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
    // `cwd_raw` is the absolute filesystem path consumed by
    // `file_index::restart`, `trust::store::normalize_project_key`,
    // and the git-context watcher. `display_path` keeps the un-
    // expanded `~/...` form for the visible cwd label.
    bucket.cwd_raw = project.path.to_string_lossy().to_string();
    bucket.cwd.clone_from(&project.display_path);
    // "Waking <project>…" placeholder system message — the only
    // chat content the user sees during the spawn window. The
    // Connected handler clears `bucket.messages` wholesale and
    // replays history into the migrated bucket, so this placeholder
    // is naturally replaced when the spawn completes. No upfront
    // welcome card here — the real welcome lands via
    // `sync_welcome_snapshot` after Connected, with proper
    // account / subscription / session-id values rather than the
    // literal `"..."` stand-ins.
    bucket.messages.push(crate::app::ChatMessage::new(
        crate::app::MessageRole::System(Some(crate::app::SystemSeverity::Info)),
        vec![crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(&format!(
            "Waking {project_name}…"
        )))],
        None,
    ));
    bucket.message_retained_bytes.push(0);
    app.sessions.insert(spawn_key.clone(), bucket);
    app.switch_active_session(spawn_key.clone());

    // Build the connection task params. Use the same plumbing as
    // the startup flow (`run_connection_task`) but route it for a
    // Named target. Failures before the first Connected event get
    // tagged with the spawn synthetic key so the visible spawning
    // bucket gets the connection-failed message.
    //
    // Clone `update_tx` + `spawn_key` for the panic-recovery path
    // before moving them into `params`: if `run_connection_task`
    // panics the bucket would otherwise stay stuck in `Spawning`
    // forever, with no user-visible diagnostic.
    let panic_update_tx = workspace.update_sender();
    let panic_session_key = spawn_key.clone();
    let params = StartConnectionParams {
        event_tx: app.event_tx.clone(),
        workspace,
        session_launch_settings: SessionLaunchSettings::default(),
        target: forge_workspace::SessionTarget::Named(project_name.clone()),
        pre_connect_key: spawn_key.clone(),
        // Sleeping-project spawn failure must NOT kill forge-tui:
        // the user has an active session; a fresh spawn's failure
        // surfaces inline in the spawn bucket and lets the app keep
        // running. Only the startup connection task sets this true.
        is_fatal_on_failure: false,
    };

    let project_for_log = project_name.clone();
    spawn_connection_task(params, panic_update_tx, panic_session_key, project_for_log);

    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "spawn_for_sleeping_project_started",
        project = %project_name,
        "sleeping-project spawn task started",
    );
}

/// Public entry point invoked by the Projects-pane click handler
/// when the user clicks a non-lead session row whose session isn't
/// currently in `app.sessions`. The drilldown lists every session
/// from the on-disk catalog (lead + non-lead), so any non-lead
/// click lands here.
///
/// Synthesizes a `__resume_<session_id>__` Session bucket
/// (lifecycle = Spawning, placeholder welcome message, cwd seeded
/// from the parent project's display path), switches the active
/// session to it, and spawns a background task that calls
/// [`forge_workspace::Workspace::get_agent_handle`] with
/// [`forge_workspace::SessionTarget::Session`].
///
/// The synthetic-key migration in
/// `crate::app::events::session::handle_connected_client_event`
/// recognises the `__resume_<id>__` sentinel via the same
/// `__<name>__` rule it applies to `__spawn_<project>__`, so the
/// placeholder bucket migrates onto the real session UUID once the
/// bridge emits its first `Connected` event.
///
/// No-op when the App has no workspace, when the session id can't
/// be located in the workspace catalog, or when the parent project
/// of the session is missing.
pub fn spawn_for_sleeping_session(app: &mut App, session_key: &forge_workspace::SessionKey) {
    let Some(workspace) = app.workspace.as_ref().map(Rc::clone) else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "spawn_for_sleeping_session_no_workspace",
            session_id = %session_key.as_str(),
            "session resume requested without a workspace; ignoring"
        );
        return;
    };

    // Locate the parent project so we can seed `cwd` / `cwd_raw`
    // sensibly for the spawning window. Without this the bucket
    // would render an empty cwd label and any user interaction
    // during the Spawning phase (e.g. typing `@` to mention a file)
    // would hit the empty-path branch in file_index.
    let Some(project) = workspace
        .list_projects()
        .into_iter()
        .find(|p| p.sessions.iter().any(|s| &s.session == session_key))
    else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "spawn_for_sleeping_session_unknown",
            session_id = %session_key.as_str(),
            "session resume requested for an unknown session id; ignoring"
        );
        return;
    };

    // Synthesize the resume bucket. The synthetic-key sentinel
    // pattern `__<name>__` is what the Connected handler matches
    // when it migrates the bucket onto the real session UUID. Use
    // `__resume_<id>__` to mirror the `__spawn_<project>__` flow
    // for sleeping projects; both follow the same migration rule.
    let synthetic_key = forge_workspace::SessionKey::from_session_id(format!(
        "__resume_{}__",
        session_key.as_str()
    ));

    // Idempotency: a second click on the same session row before
    // the first Connected event lands just switches to the existing
    // bucket — no second connection task spawned, no duplicate
    // bucket inserted.
    if app.sessions.contains_key(&synthetic_key) {
        app.switch_active_session(synthetic_key);
        return;
    }

    let mut bucket = crate::app::session::Session::new(synthetic_key.clone());
    bucket.lifecycle_state = crate::app::session::SessionLifecycleState::Spawning;
    bucket.cwd_raw = project.path.to_string_lossy().to_string();
    bucket.cwd.clone_from(&project.display_path);
    // "Waking <project>…" placeholder system message — replaced
    // by the real welcome card via `sync_welcome_snapshot` once
    // Connected lands and the bucket migrates to its real key.
    let display_label = project.name.clone();
    bucket.messages.push(crate::app::ChatMessage::new(
        crate::app::MessageRole::System(Some(crate::app::SystemSeverity::Info)),
        vec![crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(&format!(
            "Waking {display_label}…"
        )))],
        None,
    ));
    bucket.message_retained_bytes.push(0);
    app.sessions.insert(synthetic_key.clone(), bucket);
    app.switch_active_session(synthetic_key.clone());

    let panic_update_tx = workspace.update_sender();
    let panic_session_key = synthetic_key.clone();
    let params = StartConnectionParams {
        event_tx: app.event_tx.clone(),
        workspace,
        session_launch_settings: SessionLaunchSettings::default(),
        target: forge_workspace::SessionTarget::Session(session_key.clone()),
        pre_connect_key: synthetic_key.clone(),
        is_fatal_on_failure: false,
    };

    let session_id_for_log = session_key.as_str().to_owned();
    spawn_connection_task(params, panic_update_tx, panic_session_key, session_id_for_log.clone());

    tracing::info!(
        target: crate::logging::targets::APP_SESSION,
        event_name = "spawn_for_sleeping_session_started",
        session_id = %session_id_for_log,
        "session resume task started",
    );
}

/// Wrap the connection task in `catch_unwind` and dispatch a
/// `ConnectionFailed` event if it panics, so the spawning bucket
/// doesn't sit at `Spawning` forever with no diagnostic. Shared
/// between the project-spawn and session-resume helpers — both
/// route through the same `bridge_lifecycle::run_connection_task`
/// and share the panic-recovery contract.
fn spawn_connection_task(
    params: StartConnectionParams,
    panic_update_tx: tokio::sync::mpsc::UnboundedSender<forge_workspace::SessionUpdate>,
    panic_session_key: forge_workspace::SessionKey,
    label_for_log: String,
) {
    tokio::task::spawn_local(async move {
        let result =
            AssertUnwindSafe(bridge_lifecycle::run_connection_task(params)).catch_unwind().await;
        if let Err(panic) = result {
            tracing::error!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "spawn_task_panic",
                label = %label_for_log,
                session_key = %panic_session_key.as_str(),
                "spawn task panicked; emitting ConnectionFailed",
            );
            let message = format!("internal error during spawn: {panic:?}");
            // Phase 3a: ClientEvent emit removed; the SessionUpdate
            // path drives `handle_connection_failed_event` via the
            // `WorkspaceUpdate` dispatcher. Eliminates the
            // double-execution that would otherwise fire both the
            // direct ClientEvent dispatcher AND the new SessionUpdate
            // reducer on the same panic.
            let _ = panic_update_tx.send(forge_workspace::SessionUpdate::ConnectionFailed {
                key: panic_session_key,
                message,
                fatal: false,
            });
        }
        tracing::debug!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "spawn_connection_task_exited",
            label = %label_for_log,
            "spawn connection task exited",
        );
    });
}

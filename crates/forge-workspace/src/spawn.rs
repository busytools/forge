//! App-level spawn command handlers. Called from
//! [`crate::Workspace::dispatch`] when an App-level
//! `Command::SpawnProject` / `SpawnSession` / `StartDefault` arrives.
//! Each handler synthesizes a spawning key, emits
//! `SessionUpdate::Spawning`, kicks off the agent spawn, then
//! emits `SessionUpdate::KeyRenamed` + `Connected` when the agent
//! reaches its first `system/init` with the real session UUID.
//!
//! The lifecycle from synthetic-key → real-key migration runs
//! inside the spawned `SessionTask`'s `translate_event` (the
//! `AgentEvent::Connected` arm). This module just kicks off the
//! spawn and seeds the synthetic key.

use std::sync::Arc;

use forge_agent::client::SessionLaunchSettings;

use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;
use crate::{SessionKey, SessionTarget};

/// Emit a `SessionUpdate` and log at debug when the receiver is gone
/// (TUI is shutting down or has crashed). The send is logically
/// best-effort — no caller can act on the failure — but visibility
/// in the log distinguishes "TUI dropped the channel" from "the
/// emit never happened" during diagnosis.
fn try_emit(workspace: &Workspace, label: &'static str, update: SessionUpdate) {
    if let Err(err) = workspace.update_tx().send(update) {
        tracing::debug!(
            target: "forge_workspace::spawn",
            label,
            error = %err,
            "SessionUpdate dropped — receiver is gone (likely TUI shutdown)"
        );
    }
}

/// Synthesize a `__spawn_<project_name>__` placeholder bucket key
/// (matches the legacy TUI spawn flow), emit `SessionUpdate::Spawning`,
/// then spawn the agent. The first `Connected` event from the
/// resulting `SessionTask` emits `KeyRenamed` + `Connected` to
/// migrate the synthetic bucket onto the real claude session UUID.
// `async` is required: workspace.rs dispatches this via `tokio::spawn`,
// which needs a future. The body is currently synchronous after the
// `get_agent_handle` simplification but the future shape is part of
// the dispatch contract.
#[allow(clippy::unused_async)]
pub(crate) async fn handle_spawn_project(
    workspace: Arc<Workspace>,
    project_name: String,
    launch_settings: SessionLaunchSettings,
) {
    let Some(project) = workspace.find_project_view_by_name(&project_name) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project_name,
            "Command::SpawnProject for unknown project; ignoring"
        );
        return;
    };

    let synth_key = SessionKey::from_session_id(format!("__spawn_{project_name}__"));
    try_emit(
        &workspace,
        "spawn_project::Spawning",
        SessionUpdate::Spawning {
            key: synth_key.clone(),
            project_name: project_name.clone(),
            cwd: project.path.to_string_lossy().to_string(),
            display_name: project.display_path.clone(),
        },
    );

    match workspace.get_agent_handle_with_spawn_key(
        SessionTarget::Named(project_name.clone()),
        launch_settings,
        Some(synth_key.clone()),
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                project = %project_name,
                spawn_key = %synth_key.as_str(),
                "spawn task started for project"
            );
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                project = %project_name,
                error = %err,
                "spawn_project: get_agent_handle failed"
            );
            try_emit(
                &workspace,
                "spawn_project::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key: synth_key,
                    message: format!("agent spawn failed: {err}"),
                    fatal: false,
                },
            );
        }
    }
}

/// Spawn for a non-lead session row. Synthesizes
/// `__resume_<session_id>__` and resumes via
/// `SessionTarget::Session`.
// `async` required for `tokio::spawn` dispatch — see comment on
// `handle_spawn_project`.
#[allow(clippy::unused_async)]
pub(crate) async fn handle_spawn_session(
    workspace: Arc<Workspace>,
    session_id: String,
    launch_settings: SessionLaunchSettings,
) {
    let synth_key = SessionKey::from_session_id(format!("__resume_{session_id}__"));

    // Locate parent project so we can seed Spawning with the cwd.
    let session_key = SessionKey::from_session_id(session_id.clone());
    let Some(parent) = workspace.find_project_for_session(&session_key) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            session_id = %session_id,
            "Command::SpawnSession for unknown session; ignoring"
        );
        return;
    };
    let cwd = parent.path.to_string_lossy().to_string();
    let display_name = parent.display_path.clone();

    try_emit(
        &workspace,
        "spawn_session::Spawning",
        SessionUpdate::Spawning {
            key: synth_key.clone(),
            project_name: display_name.clone(),
            cwd,
            display_name,
        },
    );

    match workspace.get_agent_handle_with_spawn_key(
        SessionTarget::Session(session_key),
        launch_settings,
        Some(synth_key.clone()),
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                session_id = %session_id,
                spawn_key = %synth_key.as_str(),
                "spawn task started for session resume"
            );
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                session_id = %session_id,
                error = %err,
                "spawn_session: get_agent_handle failed"
            );
            try_emit(
                &workspace,
                "spawn_session::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key: synth_key,
                    message: format!("agent spawn failed: {err}"),
                    fatal: false,
                },
            );
        }
    }
}

/// Startup spawn. Resolves the default project (or the named one
/// passed on argv) and spawns under the `__conn_pending__` synthetic
/// key. Failure before the first Connected emits
/// `SessionUpdate::FatalError` so TUI exits cleanly.
// `async` required for `tokio::spawn` dispatch — see comment on
// `handle_spawn_project`.
#[allow(clippy::unused_async)]
pub(crate) async fn handle_start_default(
    workspace: Arc<Workspace>,
    project_name: Option<String>,
    launch_settings: SessionLaunchSettings,
) {
    let synth_key = SessionKey::from_session_id("__conn_pending__".to_owned());
    let target = match project_name {
        Some(name) => SessionTarget::Named(name),
        None => SessionTarget::Default,
    };

    match workspace
        .get_agent_handle_with_spawn_key(target, launch_settings, Some(synth_key.clone()))
    {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                spawn_key = %synth_key.as_str(),
                "startup spawn task started"
            );
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                error = %err,
                "start_default: get_agent_handle failed"
            );
            try_emit(
                &workspace,
                "start_default::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key: synth_key,
                    message: format!("agent spawn failed: {err}"),
                    fatal: true,
                },
            );
            try_emit(
                &workspace,
                "start_default::FatalError",
                SessionUpdate::FatalError(forge_primitives::error::AppError::ConnectionFailed),
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Workspace;
    use std::fs;
    use tempfile::tempdir;

    fn write_forge_toml(dir: &std::path::Path) {
        fs::write(
            dir.join("forge.toml"),
            r#"
[[orgs]]
name = "Default"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");
    }

    /// `handle_spawn_project` for an unknown project name must not
    /// panic, must not emit a `SessionUpdate::Spawning`, and must
    /// log a warning. Important for the user-input boundary — a
    /// click on a stale row shouldn't crash forge-tui.
    #[tokio::test]
    async fn spawn_project_unknown_project_emits_no_update() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path());
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        handle_spawn_project(
            Arc::clone(&workspace),
            "no-such-project".to_owned(),
            SessionLaunchSettings::default(),
        )
        .await;

        // No SessionUpdate should have been emitted.
        assert!(rx.try_recv().is_err(), "no SessionUpdate emitted for unknown project");
    }

    /// `handle_spawn_project` for a known project must emit a
    /// `SessionUpdate::Spawning` carrying a `__spawn_<name>__`
    /// synthetic key so the TUI can show a Waking placeholder
    /// before the agent reaches Connected.
    #[tokio::test]
    async fn spawn_project_known_project_emits_spawning_with_synth_key() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path());
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        handle_spawn_project(
            Arc::clone(&workspace),
            "forge".to_owned(),
            SessionLaunchSettings::default(),
        )
        .await;

        let update = rx.try_recv().expect("Spawning emit");
        match update {
            SessionUpdate::Spawning { key, project_name, .. } => {
                assert_eq!(key.as_str(), "__spawn_forge__");
                assert_eq!(project_name, "forge");
            }
            other => panic!("expected Spawning update; got {other:?}"),
        }
    }

    /// `handle_start_default` is the startup spawn path; on failure
    /// it must emit `SessionUpdate::ConnectionFailed { fatal: true }`
    /// followed by `SessionUpdate::FatalError`. C1 regression target:
    /// pre-Phase-4 the failure always killed forge-tui regardless of
    /// path; Phase 4 splits the fatal/non-fatal contract — startup
    /// is fatal, sleeping-session-spawn is not.
    ///
    /// Drives the failure path by passing a non-existent project
    /// name (`SessionTarget::Named` resolves via `find_project_by_name`
    /// in `get_agent_handle`, which errors with `ProjectNotFound`).
    #[tokio::test]
    async fn start_default_failure_is_fatal() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path());
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        // Drive a failure by passing a project name that doesn't
        // exist in forge.toml.
        handle_start_default(
            Arc::clone(&workspace),
            Some("nonexistent".to_owned()),
            SessionLaunchSettings::default(),
        )
        .await;

        // Expect ConnectionFailed { fatal: true } then FatalError.
        let first = rx.try_recv().expect("first update");
        match first {
            SessionUpdate::ConnectionFailed { fatal, .. } => {
                assert!(fatal, "startup spawn failure must be fatal");
            }
            other => panic!("expected ConnectionFailed; got {other:?}"),
        }
        let second = rx.try_recv().expect("second update");
        assert!(
            matches!(second, SessionUpdate::FatalError(_)),
            "startup spawn failure must follow with FatalError"
        );
    }

    /// `handle_spawn_session` failure path emits
    /// `ConnectionFailed { fatal: false }` — a sleeping-session
    /// spawn failure must NOT kill the app. C1 regression: pre-fix
    /// the unconditional `FatalError` send in the legacy
    /// bridge_lifecycle path killed the app on any spawn failure;
    /// Phase 4's spawn-route distinguishes the contract.
    ///
    /// Drives the failure by passing a session id that doesn't
    /// appear in any project catalog. `find_project_for_session`
    /// returns None and the handler exits without an emit — this
    /// regression test confirms it does NOT emit a fatal envelope.
    #[tokio::test]
    async fn spawn_session_unknown_session_emits_no_fatal() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path());
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        handle_spawn_session(
            Arc::clone(&workspace),
            "no-such-session-id".to_owned(),
            SessionLaunchSettings::default(),
        )
        .await;

        // The handler should not emit a Fatal envelope. (For the
        // unknown-session path it doesn't emit anything; the
        // important assertion is no FatalError.)
        while let Ok(update) = rx.try_recv() {
            assert!(
                !matches!(update, SessionUpdate::FatalError(_)),
                "spawn_session failure must not emit FatalError"
            );
            if let SessionUpdate::ConnectionFailed { fatal, .. } = update {
                assert!(!fatal, "spawn_session ConnectionFailed must be non-fatal");
            }
        }
    }
}

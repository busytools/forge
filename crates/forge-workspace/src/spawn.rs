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
use parking_lot::Mutex;

use crate::domain_session::DomainSession;
use crate::mcp::peers::facade::PeerStatsDelta;
use crate::mcp::peers::types::WrappedPrompt;
use crate::protocol::{Command, SessionUpdate};
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

/// Synthesize a `__spawn_<project_name>__` placeholder bucket key,
/// emit `SessionUpdate::Spawning`, then spawn the agent. The first
/// `Connected` event from the resulting `SessionTask` emits
/// `KeyRenamed` + `Connected` to migrate the synthetic bucket onto
/// the real claude session UUID.
pub(crate) fn handle_spawn_project(
    workspace: &Arc<Workspace>,
    project_name: &str,
    launch_settings: SessionLaunchSettings,
) {
    let Some(project) = workspace.find_project_view_by_name(project_name) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = project_name,
            "Command::SpawnProject for unknown project; ignoring"
        );
        return;
    };

    let synth_key = SessionKey::from_session_id(format!("__spawn_{project_name}__"));
    try_emit(
        workspace,
        "spawn_project::Spawning",
        SessionUpdate::Spawning {
            key: synth_key.clone(),
            project_name: project_name.to_owned(),
            cwd: project.path.to_string_lossy().to_string(),
            display_name: project.display_path.clone(),
        },
    );

    match workspace.get_agent_handle_with_spawn_key(
        SessionTarget::Named(project_name.to_owned()),
        launch_settings,
        Some(synth_key.clone()),
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                project = project_name,
                spawn_key = %synth_key.as_str(),
                "spawn task started for project"
            );
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                project = project_name,
                error = %err,
                "spawn_project: get_agent_handle failed"
            );
            try_emit(
                workspace,
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

/// Handle a `Command::DeliverPeerPrompt`. Resolves the target
/// project to a running SessionTask (deliver immediately) or a
/// sleeping one (buffer + auto-spawn). Hop stamping on target's
/// DomainSession happens here, before dispatching the wrapped
/// prompt as a regular `Command::Prompt`.
pub(crate) fn handle_deliver_peer_prompt(
    workspace: &Arc<Workspace>,
    _caller: SessionKey,
    target_project: String,
    wrapped: WrappedPrompt,
) {
    // Find the target project's running lead session (if any). The
    // `list_projects()` snapshot has `sessions: Vec<SessionView>` per
    // project; `is_open == true` on a session means an Agent is in
    // the workspace pool — that's "running."
    let target_running_key = workspace
        .list_projects()
        .into_iter()
        .find(|v| v.name == target_project)
        .and_then(|v| v.sessions.into_iter().find(|s| s.is_open).map(|s| s.session));

    if let Some(target_key) = target_running_key {
        // Stamp current_inbound_hop on target's DomainSession before
        // dispatch so any tools the target's LLM fires on the resulting
        // turn read the correct ambient hop via peek_current_inbound_hop.
        stamp_inbound_hop(workspace, &target_key, wrapped.hop);
        // Bump target's incoming counter (sidebar badge) — only for
        // `Question` wrappers (an ask expecting a reply). Tells /
        // Replies / Late-replies / Caller-timeout / Recipient-expired
        // / Delivery-failure all flow through this dispatch path too,
        // but none of them are "awaiting reply" semantically, so they
        // would never decrement and would leave the badge stuck.
        if matches!(wrapped.kind, crate::mcp::peers::types::WrappedKind::Question) {
            let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(workspace);
            facade.bump_inflight_stats(&target_key, PeerStatsDelta::IncomingPlus1);
        }

        // Fire the typed peer-envelope echo BEFORE the LLM-side
        // dispatch so the user-turn block renders in the right
        // order regardless of which event the TUI reducer drains
        // first. The CLI doesn't echo stdin-injected prompts back
        // on stream-json output (only tool_result-bearing user
        // envelopes come back), so the TUI gets no inbound user-turn
        // signal from the SDK side — `PeerEnvelopeAppended` is how
        // the TUI knows to render the peer block.
        push_peer_user_turn_into_chat(workspace, &target_key, &wrapped);
        let text = wrapped.to_prose();
        if let Err(err) = workspace.dispatch(Command::Prompt {
            key: target_key.clone(),
            text,
            attachments: Vec::new(),
        }) {
            tracing::warn!(
                target: "forge_workspace::spawn",
                target_project = %target_project,
                error = ?err,
                "DeliverPeerPrompt dispatch to running target failed"
            );
        }
        return;
    }

    // Target is sleeping (or unknown — defensive). If the project
    // exists in forge.toml, ensure a DomainSession exists at the
    // synthetic spawn key, buffer the wrapped prompt for delivery on
    // Connected, then dispatch SpawnProject.
    if workspace.find_project_view_by_name(&target_project).is_none() {
        tracing::warn!(
            target: "forge_workspace::spawn",
            target_project = %target_project,
            "DeliverPeerPrompt target not in forge.toml; dropping"
        );
        return;
    }

    let synth_key = SessionKey::from_session_id(format!("__spawn_{target_project}__"));

    // Ensure DomainSession at synth_key + buffer wrapped + stamp hop.
    // get_agent_handle_with_spawn_key (called by handle_spawn_project
    // below) will re-use this DomainSession if present; otherwise it
    // creates a new one. We want to buffer BEFORE the spawn so the
    // session_task's Connected handler can drain on first turn.
    {
        let mut handles = workspace.domain_handles.lock();
        let domain = handles
            .entry(synth_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(DomainSession::new(synth_key.clone(), None))))
            .clone();
        drop(handles);
        let mut d = domain.lock();
        let current = d.current_inbound_hop.unwrap_or(0);
        d.current_inbound_hop = Some(current.max(wrapped.hop));
        d.pending_peer_prompts.push(wrapped);
    }

    // Dispatch SpawnProject. handle_spawn_project reuses the
    // DomainSession we just placed at synth_key. Move target_project
    // into the command rather than cloning (it's the last use).
    let project_for_log = target_project.clone();
    if let Err(err) = workspace.dispatch(Command::SpawnProject {
        project_name: target_project,
        launch_settings: SessionLaunchSettings::default(),
    }) {
        tracing::warn!(
            target: "forge_workspace::spawn",
            target_project = %project_for_log,
            error = ?err,
            "DeliverPeerPrompt SpawnProject dispatch failed"
        );
    }
}

/// Stamp `current_inbound_hop = max(current, hop)` on the target's
/// DomainSession. Used by handle_deliver_peer_prompt before
/// dispatching Command::Prompt so the recipient's tools observe the
/// correct ambient hop when they fire outbound asks/tells during
/// the resulting turn.
fn stamp_inbound_hop(workspace: &Workspace, target_key: &SessionKey, hop: u8) {
    let handles = workspace.domain_handles.lock();
    if let Some(domain) = handles.get(target_key).cloned() {
        drop(handles);
        let mut d = domain.lock();
        let current = d.current_inbound_hop.unwrap_or(0);
        d.current_inbound_hop = Some(current.max(hop));
    }
}

/// Emit a typed `PeerEnvelopeAppended` so the target session's TUI
/// chat buffer shows the inbound peer user-turn. The TUI reducer
/// builds the chat-side echo directly from the `WrappedPrompt`'s
/// typed fields — workspace no longer forges an SDK `Message::User`
/// frame (audit I11).
///
/// Note: this only affects the TUI's visible chat echo. The
/// recipient's `claude` subprocess still receives the prose via a
/// separate `Command::Prompt` dispatch — the CLI's input channel
/// is text-shaped and stays that way.
pub(crate) fn push_peer_user_turn_into_chat(
    workspace: &Workspace,
    target_key: &SessionKey,
    wrapped: &WrappedPrompt,
) {
    let _ = workspace.update_sender().send(SessionUpdate::PeerEnvelopeAppended {
        session_id: target_key.as_str().to_owned(),
        wrapped: wrapped.clone(),
    });
}

/// Spawn for a non-lead session row. Synthesizes
/// `__resume_<session_id>__` and resumes via
/// `SessionTarget::Session`.
pub(crate) fn handle_spawn_session(
    workspace: &Arc<Workspace>,
    session_id: &str,
    launch_settings: SessionLaunchSettings,
) {
    let synth_key = SessionKey::from_session_id(format!("__resume_{session_id}__"));

    // Locate parent project so we can seed Spawning with the cwd.
    let session_key = SessionKey::from_session_id(session_id.to_owned());
    let Some(parent) = workspace.find_project_for_session(&session_key) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            session_id,
            "Command::SpawnSession for unknown session; ignoring"
        );
        return;
    };
    let cwd = parent.path.to_string_lossy().to_string();
    let display_name = parent.display_path.clone();

    try_emit(
        workspace,
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
                session_id,
                spawn_key = %synth_key.as_str(),
                "spawn task started for session resume"
            );
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                session_id,
                error = %err,
                "spawn_session: get_agent_handle failed"
            );
            try_emit(
                workspace,
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
pub(crate) fn handle_start_default(
    workspace: &Arc<Workspace>,
    project_name: Option<String>,
    launch_settings: SessionLaunchSettings,
) {
    let synth_key = SessionKey::from_session_id("__conn_pending__".to_owned());
    let target = match project_name {
        Some(name) => SessionTarget::Named(name),
        None => SessionTarget::Default,
    };

    match workspace.get_agent_handle_with_spawn_key(
        target,
        launch_settings,
        Some(synth_key.clone()),
    ) {
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
                workspace,
                "start_default::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key: synth_key,
                    message: format!("agent spawn failed: {err}"),
                    fatal: true,
                },
            );
            try_emit(
                workspace,
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

        handle_spawn_project(&workspace, "no-such-project", SessionLaunchSettings::default());

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

        handle_spawn_project(&workspace, "forge", SessionLaunchSettings::default());

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
    /// followed by `SessionUpdate::FatalError`. Startup failures are
    /// fatal; sleeping-session-spawn failures are not.
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
            &workspace,
            Some("nonexistent".to_owned()),
            SessionLaunchSettings::default(),
        );

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
    /// spawn failure must NOT kill the app. Distinguished from
    /// `handle_start_default`'s fatal contract by route.
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

        handle_spawn_session(&workspace, "no-such-session-id", SessionLaunchSettings::default());

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

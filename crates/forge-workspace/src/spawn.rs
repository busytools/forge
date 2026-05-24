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
use crate::protocol::{Command, SessionUpdate, WorkerSpawnReply, WorkerStatusAction};
use crate::target::ProjectKey;
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

/// Handle a `Command::SpawnWorker`: insert a `Spawning` worker entry
/// in `live_workers[project_key]`, dispatch a fresh-session spawn
/// via `SessionTarget::FreshInProject` with the charter threaded
/// onto `SessionLaunchSettings`, then reply on `return_to` with the
/// synthetic session_id + the tag value. The Connected handler in
/// `session_task::translate_event` writes the actual JSONL tag row
/// and transitions the entry from Spawning to Running (or rolls back
/// on tag-write failure).
///
/// The synth_key reply works because the LLM's `workers__spawn`
/// caller doesn't USE the session_id to address subsequent calls
/// (those go by label); the field exists in `WorkerStatus` for v2
/// describe-by-id workflows and to give the caller a stable
/// identifier to log. The synth -> real rekey is handled inside
/// `Workspace::migrate_session_task`, which also fixes up
/// `live_workers[project_key]`'s `session_key` field in lockstep.
pub(crate) fn handle_spawn_worker(
    workspace: &Arc<Workspace>,
    project_key: ProjectKey,
    label: &str,
    charter: String,
    spawned_by_session_id: String,
    return_to: tokio::sync::oneshot::Sender<Result<WorkerSpawnReply, String>>,
) {
    // Verify the project exists before claiming a synth key.
    let projects = workspace.list_projects();
    let Some(_view) = projects.iter().find(|v| v.key == project_key) else {
        let _ = return_to.send(Err(format!("project not found: {}", project_key.as_str())));
        return;
    };

    // Synthesize a pool key for the not-yet-spawned worker. The
    // SessionTask rekeys this onto the real claude-issued UUID on
    // first Connected; migrate_session_task also rewrites the
    // matching WorkerEntry's session_key in lockstep.
    //
    // A v4 uuid suffix makes the key unique across concurrent spawns
    // of the same label - two `spawn_worker(label="reviewer")` calls
    // would otherwise collide on the pool / command_senders /
    // domain_handles maps and only one would survive.
    let synth_key = SessionKey::from_session_id(format!(
        "__spawn_worker_{}_{}_{}__",
        project_key.as_str(),
        label,
        uuid::Uuid::new_v4().simple()
    ));
    let tag = forge_primitives::worker_tag(label);

    // Insert WorkerEntry as Spawning BEFORE the agent spawn so the
    // Connected handler can find the entry via worker_lookup_for_session
    // when it fires (worker_lookup_for_session reads live_workers).
    let entry = crate::mcp::workers::types::WorkerEntry {
        label: label.to_owned(),
        charter: charter.clone(),
        session_key: synth_key.clone(),
        status: forge_primitives::WorkerLiveness::Spawning,
        spawned_at: std::time::SystemTime::now(),
        spawned_by_session_id,
        needs_tag: true,
    };
    workspace.insert_live_worker(&project_key, entry.clone());
    try_emit(
        workspace,
        "spawn_worker::WorkerStatusChanged::Added",
        SessionUpdate::WorkerStatusChanged {
            project_key: project_key.clone(),
            action: WorkerStatusAction::Added,
            status: entry.to_status(),
        },
    );

    // Spawn the fresh session under the picked account. The charter
    // is threaded onto SessionLaunchSettings.charter; the spawn path
    // (forge_sdk_worker::build_options_with_callback) appends it to
    // the system prompt via --append-system-prompt.
    let settings = SessionLaunchSettings { charter: Some(charter), ..Default::default() };
    let target = SessionTarget::FreshInProject {
        project_key: project_key.clone(),
        synth_key: synth_key.clone(),
    };
    match workspace.get_agent_handle_with_spawn_key(target, settings, Some(synth_key.clone())) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %label,
                spawn_key = %synth_key.as_str(),
                "spawn task started for worker"
            );
            // Reply to the LLM optimistically with the synth key.
            // The LLM addresses subsequent calls by label; the
            // session_id field is informational. Real session UUID
            // lands on Connected via the rekey machinery.
            let _ = return_to
                .send(Ok(WorkerSpawnReply { session_id: synth_key.as_str().to_owned(), tag }));
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %label,
                error = %err,
                "spawn_worker: get_agent_handle failed"
            );
            // Roll back the live_workers entry we just inserted.
            let removed = workspace.remove_latest_worker(&project_key, label);
            if let Some(rolled) = removed {
                try_emit(
                    workspace,
                    "spawn_worker::WorkerStatusChanged::Removed",
                    SessionUpdate::WorkerStatusChanged {
                        project_key,
                        action: WorkerStatusAction::Removed,
                        status: rolled.to_status(),
                    },
                );
            }
            let _ = return_to.send(Err(format!("agent spawn failed: {err}")));
        }
    }
}

/// Handle a `Command::CloseWorker`: remove the latest-spawned worker
/// matching `label` from `live_workers[project_key]`, release its
/// session (terminates the claude subprocess), and emit a `Removed`
/// status event. JSONL on disk is NOT deleted - "close" only means
/// "remove from in-memory live state".
///
/// Worker-bound asks whose `target_project` composite
/// (`<project_key>::<label>`) names the closed worker are expired
/// via `Workspace::expire_inflight_for_closed_worker` so their
/// caller's LLM receives a `DeliveryFailureNotice` instead of
/// waiting forever for a reply.
pub(crate) fn handle_close_worker(
    workspace: &Arc<Workspace>,
    project_key: &ProjectKey,
    label: &str,
) {
    let Some(entry) = workspace.remove_latest_worker(project_key, label) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project_key.as_str(),
            label = %label,
            "handle_close_worker: no matching live worker"
        );
        return;
    };
    let status = entry.to_status();
    // MUST call the non-cascading `release_session` primitive (NOT
    // `release_session_with_cascade`). By the time we get here the
    // worker is already gone from `live_workers` (via
    // `remove_latest_worker` above), so the cascading variant would
    // treat the orphaned session_key as a project-lead under its
    // cascade-detection rule (`in_catalog && !is_worker`) and drain
    // every OTHER worker in the project too. Per-row close MUST only
    // affect the single worker being closed.
    workspace.release_session(&entry.session_key);
    workspace.expire_inflight_for_closed_worker(project_key, label);
    let _ = workspace.update_tx().send(SessionUpdate::WorkerStatusChanged {
        project_key: project_key.clone(),
        action: WorkerStatusAction::Removed,
        status,
    });
}

/// Handle a `Command::DeliverWorkerPrompt`: route a wrapped peer-style
/// envelope to the worker matching `target_label` in the caller's
/// project. Latest-spawned-wins on duplicate labels. Hop stamping on
/// the target's DomainSession + the typed PeerEnvelopeAppended echo
/// follow the same pattern as `handle_deliver_peer_prompt` - workers
/// reuse the peer envelope verbatim so the TUI's chat render is
/// identical between the two paths.
///
/// Unlike `handle_deliver_peer_prompt`, this handler never buffers +
/// auto-spawns: workers are only addressable while live. If the
/// target label vanished between the dispatch and the handler firing
/// (worker closed, lead cascade fired), the prompt is dropped with a
/// warn log.
pub(crate) fn handle_deliver_worker_prompt(
    workspace: &Arc<Workspace>,
    _caller: SessionKey,
    project_key: &ProjectKey,
    target_label: &str,
    wrapped: WrappedPrompt,
) {
    // Latest-spawned matching label wins (mirrors the addressing rule
    // in workers__tell / workers__ask).
    let Some(entry) = workspace
        .list_live_workers(project_key)
        .into_iter()
        .rev()
        .find(|w| w.label == target_label)
    else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project_key.as_str(),
            label = %target_label,
            "deliver_worker_prompt: no matching live worker (target gone since dispatch)"
        );
        return;
    };
    let target_key = entry.session_key.clone();

    // Opportunistic tag-write retry: if this worker was spawned idle
    // (no initial_prompt), the JSONL didn't exist at Connected and
    // the tag-write at that point exhausted into a deferred state.
    // claude is about to process this turn, which means it's about
    // to write the JSONL - kick off a fire-and-forget retry now.
    if entry.needs_tag {
        let cwd = workspace
            .list_projects()
            .into_iter()
            .find(|view| view.key == *project_key)
            .map(|view| view.path.to_string_lossy().into_owned());
        if let Some(cwd) = cwd {
            workspace.retry_worker_tag_opportunistic(project_key, &target_key, target_label, &cwd);
        } else {
            tracing::debug!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %target_label,
                "deliver_worker_prompt: project view missing; skipping tag retry"
            );
        }
    }

    // Stamp current_inbound_hop so any tools the target's LLM fires
    // during the resulting turn observe the correct ambient hop.
    stamp_inbound_hop(workspace, &target_key, wrapped.hop);

    // Bump target's incoming counter for Question kind only (matches
    // peer behavior - the sidebar badge tracks awaiting-reply asks).
    if matches!(wrapped.kind, crate::mcp::peers::types::WrappedKind::Question) {
        let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(workspace);
        facade.bump_inflight_stats(&target_key, PeerStatsDelta::IncomingPlus1);
    }

    // Fire the typed peer-envelope echo BEFORE the LLM-side dispatch
    // so the user-turn block renders in the right order regardless
    // of which event the TUI reducer drains first. Compute the prose
    // body BEFORE the move into PeerEnvelopeAppended so we consume
    // `wrapped` exactly once (the push_peer_user_turn_into_chat helper
    // takes `&WrappedPrompt` and clones internally).
    let text = wrapped.to_prose();
    push_peer_user_turn_into_chat(workspace, &target_key, &wrapped);
    drop(wrapped);
    if let Err(err) =
        workspace.dispatch(Command::Prompt { key: target_key, text, attachments: Vec::new() })
    {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project_key.as_str(),
            label = %target_label,
            error = ?err,
            "DeliverWorkerPrompt dispatch to worker failed"
        );
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

    fn fixture_wrapped() -> WrappedPrompt {
        WrappedPrompt {
            correlation_id: crate::mcp::peers::types::CorrelationId::new_tell(),
            kind: crate::mcp::peers::types::WrappedKind::Message,
            sender_name: "forge".to_owned(),
            sender_org: "Default".to_owned(),
            hop: 1,
            hop_limit: 10,
            body: "fyi".to_owned(),
        }
    }

    /// I3 — `handle_deliver_peer_prompt` for an unknown target must
    /// not panic and must not emit a fatal envelope. The tool itself
    /// rejects unknown targets synchronously via `DeliverError::UnknownTarget`;
    /// the spawn path's defensive branch is the second line of defence
    /// when an LLM races a forge.toml reload.
    #[tokio::test]
    async fn handle_deliver_peer_prompt_unknown_target_is_no_op() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path());
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        let caller = SessionKey::from_str_for_test("caller-1");
        handle_deliver_peer_prompt(
            &workspace,
            caller,
            "no-such-project".to_owned(),
            fixture_wrapped(),
        );

        while let Ok(update) = rx.try_recv() {
            assert!(
                !matches!(update, SessionUpdate::FatalError(_)),
                "unknown target must not emit FatalError"
            );
        }
    }

    /// I3 — `handle_deliver_peer_prompt` against a sleeping known
    /// project buffers the prompt in the target's pending list and
    /// triggers a SpawnProject. The pending list grows by one.
    #[tokio::test]
    async fn handle_deliver_peer_prompt_sleeping_target_buffers_prompt() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join("forge.toml"),
            r#"
[[orgs]]
name = "Default"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[orgs.projects]]
name = "granite-backend"
path = "~/Projects/granite-backend"
auto_start = false

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        )
        .expect("write forge.toml");
        let workspace = Arc::new(Workspace::new(dir.path().to_owned()).await.expect("new"));
        let caller = SessionKey::from_str_for_test("caller-sleep");
        let w = fixture_wrapped();

        handle_deliver_peer_prompt(&workspace, caller, "granite-backend".to_owned(), w.clone());

        // Sleeping branch parks the wrapped at a synthetic
        // `__spawn_granite-backend__` key, then dispatches
        // SpawnProject which (synchronously inside dispatch)
        // migrates the buffered state onto the real resolved
        // session key. Either way, EXACTLY ONE DomainSession in
        // the workspace must carry our wrapped prompt — assert
        // on the typed correlation id rather than the key path.
        let handles = workspace.domain_handles.lock();
        let total: usize = handles
            .values()
            .map(|d| {
                d.lock()
                    .pending_peer_prompts
                    .iter()
                    .filter(|p| p.correlation_id == w.correlation_id)
                    .count()
            })
            .sum();
        assert_eq!(total, 1, "wrapped prompt buffered exactly once across handles");
    }

    fn fake_worker_entry(label: &str, key: &str) -> crate::mcp::workers::types::WorkerEntry {
        crate::mcp::workers::types::WorkerEntry {
            label: label.into(),
            charter: "c".into(),
            session_key: SessionKey::from_session_id(key),
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".into(),
            needs_tag: false,
        }
    }

    /// `handle_close_worker` removes the worker entry, releases the
    /// session, and emits `WorkerStatusChanged { Removed }`. The
    /// label-targeting picks the latest-spawned duplicate.
    #[tokio::test]
    async fn close_worker_removes_entry_and_emits_removed() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        workspace.insert_live_worker(&project, fake_worker_entry("r1", "worker-1"));

        handle_close_worker(&workspace, &project, "r1");

        assert!(workspace.list_live_workers(&project).is_empty());
        let mut saw_removed = false;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::WorkerStatusChanged { action, .. } = update
                && action == WorkerStatusAction::Removed
            {
                saw_removed = true;
            }
        }
        assert!(saw_removed, "Removed event was emitted");
    }

    /// Regression: closing ONE worker must NOT cascade to the others.
    /// Before the fix, `handle_close_worker` called the cascading
    /// `release_session` AFTER `remove_latest_worker`. The cascade-
    /// detection (`in_catalog && !is_worker`) saw the orphaned
    /// session_key as a lead (because it was already gone from
    /// `live_workers` by step 1) and drained every OTHER worker.
    /// The fix calls `release_session_inner` directly.
    #[tokio::test]
    async fn close_one_worker_leaves_other_workers_intact() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        workspace.insert_live_worker(&project, fake_worker_entry("worker-a", "session-a"));
        workspace.insert_live_worker(&project, fake_worker_entry("worker-b", "session-b"));
        workspace.insert_live_worker(&project, fake_worker_entry("worker-c", "session-c"));
        assert_eq!(workspace.list_live_workers(&project).len(), 3);

        handle_close_worker(&workspace, &project, "worker-b");

        let remaining = workspace.list_live_workers(&project);
        assert_eq!(remaining.len(), 2, "only the targeted worker should be removed; got {remaining:?}");
        let labels: Vec<&str> = remaining.iter().map(|w| w.label.as_str()).collect();
        assert!(labels.contains(&"worker-a"), "worker-a must survive");
        assert!(labels.contains(&"worker-c"), "worker-c must survive");

        // Exactly one Removed event for worker-b.
        let mut removed_labels: Vec<String> = Vec::new();
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::WorkerStatusChanged { action, status, .. } = update
                && action == WorkerStatusAction::Removed
            {
                removed_labels.push(status.label);
            }
        }
        assert_eq!(
            removed_labels,
            vec!["worker-b".to_owned()],
            "exactly one Removed event for the closed worker; got {removed_labels:?}"
        );
    }

    /// `handle_close_worker` for an unknown label logs but does not
    /// panic and emits no events. Defensive branch when the TUI's
    /// click races a lead-cascade or a concurrent close.
    #[tokio::test]
    async fn close_worker_unknown_label_is_noop() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");

        handle_close_worker(&workspace, &project, "missing");

        assert!(rx.try_recv().is_err(), "no events emitted for unknown label");
    }

    /// `handle_deliver_worker_prompt` is a no-op when the target
    /// label has no live worker. Mirrors the close_worker_unknown
    /// branch - the upstream Tool gate (workers__tell facade)
    /// rejects synchronously; the spawn handler is defence in depth.
    #[tokio::test]
    async fn deliver_worker_prompt_unknown_label_is_noop() {
        let (workspace, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        let caller = SessionKey::from_str_for_test("caller-1");

        handle_deliver_worker_prompt(&workspace, caller, &project, "missing", fixture_wrapped());
        // No panic, no dispatch attempted. (We can't easily observe
        // "no dispatch" without a stubbed dispatch; the absence of a
        // panic + dropped channels is the test.)
    }

    /// Regression for C2: concurrent spawns of the same label must
    /// produce different synth_keys. The synth_key formula mixes a
    /// v4 uuid suffix into the label so collisions on the
    /// pool / command_senders / domain_handles maps cannot happen.
    /// We probe the formula directly because the full spawn path
    /// requires a real claude subprocess; the formula is the
    /// invariant the rest of the spawn handler relies on.
    #[test]
    fn synth_key_for_duplicate_label_is_unique() {
        let project = ProjectKey::new("forge");
        let mk = |label: &str| -> SessionKey {
            SessionKey::from_session_id(format!(
                "__spawn_worker_{}_{}_{}__",
                project.as_str(),
                label,
                uuid::Uuid::new_v4().simple()
            ))
        };
        let a = mk("reviewer");
        let b = mk("reviewer");
        assert_ne!(
            a.as_str(),
            b.as_str(),
            "two same-label spawns must produce different synth keys"
        );
        assert!(a.as_str().starts_with("__spawn_worker_forge_reviewer_"));
        assert!(b.as_str().starts_with("__spawn_worker_forge_reviewer_"));
    }

    /// `handle_deliver_worker_prompt` for a worker carrying
    /// `needs_tag = true` must kick off an opportunistic tag-write
    /// retry. The worker was spawned idle (no initial_prompt), claude
    /// only writes the JSONL once the first turn arrives, and this
    /// handler runs as that first turn is being routed.
    ///
    /// Verifies the integration end-to-end: pre-seed a Running worker
    /// with `needs_tag = true`, install the testing-stub agent (its
    /// `config_dir` resolves to `/tmp/forge-testing-stub`), seed the
    /// JSONL at the path `tag_session` expects, and assert that after
    /// `handle_deliver_worker_prompt` fires the entry's `needs_tag`
    /// flips to `false` (the retry succeeded). Uses a unique session_id
    /// to avoid collision with other parallel tests that hit the
    /// shared stub config_dir.
    #[tokio::test]
    async fn deliver_prompt_kicks_opportunistic_tag_retry() {
        // Set up a real Workspace::new with a project pointing at a
        // tempdir so list_projects returns the project view we need.
        let toml_dir = tempdir().expect("toml dir");
        let project_root = tempdir().expect("project root");
        let project_path = project_root.path().to_string_lossy().into_owned();
        fs::write(
            toml_dir.path().join("forge.toml"),
            format!(
                r#"
[[orgs]]
name = "Default"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "{project_path}"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
            ),
        )
        .expect("write forge.toml");
        let workspace = Arc::new(Workspace::new(toml_dir.path().to_owned()).await.expect("new"));

        // Resolve the project's key + path from list_projects (matches
        // what the spawn handler will look up).
        let project_view = workspace
            .list_projects()
            .into_iter()
            .find(|view| view.name == "forge")
            .expect("forge project");
        let project_key = project_view.key.clone();

        // Unique session_id so concurrent test runs don't collide on
        // the shared /tmp/forge-testing-stub. Must be a valid UUID
        // since `tag_session` rejects non-UUID with `MessageParse`.
        let session_id = uuid::Uuid::new_v4().hyphenated().to_string();
        let session_key = SessionKey::from_session_id(&session_id);

        // Install the testing stub so config_dir_for resolves (to
        // /tmp/forge-testing-stub via the bridge's default config_dir).
        let _agent_rx = workspace.install_testing_stub(&session_key);

        // Pre-seed the worker entry: Running but needs_tag = true,
        // matching the post-deferred-NotFound state from the
        // `apply_worker_tag_or_rollback` deferred branch.
        workspace.insert_live_worker(
            &project_key,
            crate::mcp::workers::types::WorkerEntry {
                label: "idle".into(),
                charter: "c".into(),
                session_key: session_key.clone(),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: true,
            },
        );

        // Pre-create the JSONL at the path the retry will look for.
        // /tmp/forge-testing-stub/projects/<sanitized>/<session_id>.jsonl
        let stub_config_dir = std::path::PathBuf::from("/tmp/forge-testing-stub");
        let sanitized =
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some(&project_path));
        let projects_dir = forge_sdk::projects_dir_for(&stub_config_dir).join(&sanitized);
        fs::create_dir_all(&projects_dir).expect("project dir");
        let jsonl_path = projects_dir.join(format!("{session_id}.jsonl"));
        fs::write(&jsonl_path, "").expect("seed jsonl");

        // Fire the deliver. The handler observes needs_tag = true,
        // looks up the project view's path, and kicks the retry task.
        // It also dispatches Command::Prompt which the stub agent
        // accepts (the side-effect we don't assert on here).
        let caller = SessionKey::from_str_for_test("caller-1");
        handle_deliver_worker_prompt(&workspace, caller, &project_key, "idle", fixture_wrapped());

        // Wait briefly for the spawned task to finish the retry +
        // status update. The retry should succeed on first attempt
        // because the JSONL exists.
        let needs_tag = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let entries = workspace.list_live_workers(&project_key);
                let entry = entries.iter().find(|e| e.session_key == session_key);
                if let Some(entry) = entry
                    && !entry.needs_tag
                {
                    return false;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("needs_tag must clear within budget");
        assert!(!needs_tag, "needs_tag cleared after opportunistic retry succeeded");

        // Confirm the tag actually landed on disk.
        let body = fs::read_to_string(&jsonl_path).expect("read jsonl");
        assert!(
            body.contains("\"tag\":\"forge:worker:idle\""),
            "tag row appended on opportunistic retry: {body:?}"
        );

        // Clean up the shared stub config_dir entries we created so
        // subsequent parallel runs aren't polluted.
        let _ = fs::remove_file(&jsonl_path);
    }

    /// Regression for C4: closing a worker must expire every
    /// inflight ask whose `target_project` composite names that
    /// worker. Pre-fix `target_project` carried the bare label,
    /// which never matched the project-name path in
    /// `expire_target_inflight` and the asks leaked forever. The
    /// new `expire_inflight_for_closed_worker` keyed on
    /// `<project_key>::<label>` covers worker-bound traffic.
    #[tokio::test]
    async fn close_worker_expires_inflight_asks_addressed_to_it() {
        use crate::mcp::peers::types::{CorrelationId, InflightAsk};
        let (workspace, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        workspace.insert_live_worker(&project, fake_worker_entry("reviewer", "worker-1"));

        // Stamp an inflight ask using the same composite the workers
        // Ask Tool would produce.
        let cid = CorrelationId::new_ask();
        let composite =
            crate::mcp::workers::worker_target_project_key(project.as_str(), "reviewer");
        workspace.inflight_asks.lock().insert(
            cid.clone(),
            InflightAsk {
                correlation_id: cid.clone(),
                caller: SessionKey::from_session_id("lead-uuid"),
                caller_project: project.as_str().to_owned(),
                caller_org: String::new(),
                target_project: composite,
            },
        );
        assert_eq!(workspace.inflight_asks.lock().len(), 1);

        handle_close_worker(&workspace, &project, "reviewer");

        assert!(
            workspace.inflight_asks.lock().is_empty(),
            "ask must be expired when the worker it targets closes"
        );
    }
}

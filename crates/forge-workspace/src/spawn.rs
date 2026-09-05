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
use crate::mcp::gotify::types::GotifyNotification;
use crate::mcp::peers::facade::PeerStatsDelta;
use crate::mcp::peers::types::WrappedPrompt;
use crate::protocol::{
    Command, SessionUpdate, WorkerSpawnReply, WorkerStatusAction, WorktreeDisposition,
};
use crate::target::ProjectKey;
use crate::workspace::Workspace;
use crate::{SessionKey, SessionTarget};

/// A failed prompt dispatch to a running target unwinds the echo's
/// already-opened turn: without this its bar counts forever and the
/// bucket spinner never drops (mirrors the typed-submit
/// compensation). Per-site warns stay at the call sites.
pub(crate) fn send_dispatch_turn_error(
    workspace: &Workspace,
    key: SessionKey,
    err: &crate::protocol::DispatchError,
) {
    let _ = workspace.update_sender().send(SessionUpdate::TurnError {
        key,
        message: err.to_string(),
        class: None,
        terminal_reason: None,
    });
}

/// Build the list of `(flag, value)` extra CLI args specific to a
/// worker spawn. When the project is a git repo, append
/// `("worktree", Some(label))` so the spawned `claude` subprocess
/// creates a worktree at `<repo>/.claude/worktrees/<label>/` and
/// runs the session inside it. In all cases, append a
/// `--disallowedTools EnterWorktree,ExitWorktree` entry: workers are
/// pinned to their spawn-time location (whether a worktree or the
/// project cwd) and must not be able to call claude's built-in
/// worktree-hop tools to escape. Comma-separated value form is
/// empirically accepted by the CLI's variadic `<tools...>` parser.
///
/// Unless `interactive`, `AskUserQuestion` joins that list. A worker's
/// question renders in its own row, which nobody is usually looking
/// at, and an answer that does arrive is indistinguishable from a
/// decision the user made. Denying it at spawn means the capability is
/// never offered rather than refused after the model has already
/// decided to ask.
///
/// The `is_git_repo` boolean is passed in (already-computed by the
/// caller) so the git-repo probe runs at most once per spawn even
/// when the same answer is needed for both the `WorkerEntry.is_git_repo_at_spawn`
/// field and this argument list.
fn build_worker_extra_args(
    is_git_repo: bool,
    label: &str,
    interactive: bool,
) -> Vec<(String, Option<String>)> {
    let mut args = Vec::new();
    if is_git_repo {
        args.push(("worktree".to_owned(), Some(label.to_owned())));
    }
    let mut disallowed = "EnterWorktree,ExitWorktree".to_owned();
    if !interactive {
        disallowed.push_str(",AskUserQuestion");
    }
    args.push(("disallowedTools".to_owned(), Some(disallowed)));
    args
}

/// Charter every lead session is launched with.
const DEFAULT_LEAD_CHARTER: &str = include_str!("spawn/lead_charter.md");

/// Stamp [`DEFAULT_LEAD_CHARTER`] onto the launch settings so every lead
/// session carries one. No-op when a charter is already set - worker
/// spawns supply their own and we never overwrite it.
fn apply_lead_charter(settings: &mut SessionLaunchSettings) {
    if settings.charter.is_some() {
        return;
    }
    settings.charter = Some(DEFAULT_LEAD_CHARTER.to_owned());
}

/// Stamp the account's `[[accounts]] permission_mode` into the launch
/// settings' `permissions.defaultMode`, where the existing
/// `applied_permission_mode` arm in `forge_sdk_worker` picks it up;
/// overrides a launcher-supplied default.
pub(crate) fn stamp_account_permission_mode(
    settings: &mut SessionLaunchSettings,
    mode: forge_primitives::permission::PermissionMode,
) {
    let object =
        settings.settings.get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(map) = object.as_object_mut() else {
        return;
    };
    let perms = map
        .entry(SessionLaunchSettings::PERMISSIONS_KEY.to_owned())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(perms) = perms.as_object_mut() {
        perms.insert(
            SessionLaunchSettings::PERMISSIONS_DEFAULT_MODE_KEY.to_owned(),
            serde_json::Value::String(mode.as_wire().to_owned()),
        );
    }
}

/// Emit a `SessionUpdate` and log at debug when the receiver is gone
/// (TUI is shutting down or has crashed). The send is logically
/// best-effort - no caller can act on the failure - but visibility
/// in the log distinguishes "TUI dropped the channel" from "the
/// emit never happened" during diagnosis.
fn try_emit(workspace: &Workspace, label: &'static str, update: SessionUpdate) {
    if let Err(err) = workspace.update_tx().send(update) {
        tracing::debug!(
            target: "forge_workspace::spawn",
            label,
            error = %err,
            "SessionUpdate dropped - receiver is gone (likely TUI shutdown)"
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
    mut launch_settings: SessionLaunchSettings,
) {
    let Some(project) = workspace.find_project_view_by_name(project_name) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = project_name,
            "Command::SpawnProject for unknown project; ignoring"
        );
        try_emit(
            workspace,
            "spawn_project::unknown_project",
            SessionUpdate::ServiceStatus {
                severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
                message: format!("Unknown project: {project_name}"),
            },
        );
        return;
    };

    apply_lead_charter(&mut launch_settings);

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
        None,
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                project = project_name,
                spawn_key = %synth_key.as_str(),
                // The pool fast path returns a live handle without
                // building a task, so `forge_sdk_options_built` is the
                // per-subprocess signal.
                "spawn dispatched for project"
            );
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                project = project_name,
                error = %err,
                "spawn_project: get_agent_handle failed"
            );
            // No SessionTask exists to run its ConnectionFailed arm, so
            // fail any peer asks buffered against this synth key here -
            // otherwise the caller's LLM waits on a spawn that never
            // happened.
            workspace.expire_buffered_peer_prompts(
                &synth_key,
                crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
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
/// sleeping one (buffer + auto-spawn), then dispatches the wrapped
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
    // the workspace pool - that's "running." MUST skip worker
    // sessions: once a worker connects it lands in `view.sessions`
    // too, and a worker can sit at position 0 / be the first
    // is_open session. Returning a worker key here dispatches the
    // peer envelope to the worker's chat instead of the lead's,
    // which is wrong (peers address project leads, not workers).
    // `live_workers` is the authoritative worker registry; subtract
    // its session keys from the candidate set.
    let target_running_key =
        workspace.list_projects().into_iter().find(|v| v.name == target_project).and_then(|v| {
            let live_worker_keys: std::collections::HashSet<_> =
                workspace.list_live_workers(&v.key).into_iter().map(|w| w.session_key).collect();
            v.sessions
                .into_iter()
                .find(|s| s.is_open && !live_worker_keys.contains(&s.session))
                .map(|s| s.session)
        });

    if let Some(target_key) = target_running_key {
        // Bump the target's incoming badge only for `Question`
        // wrappers. Badges count pending asks awaiting reply; every
        // other kind (tells, replies, delivery-failure notices) has no
        // matching decrement, so bumping would grow the counter
        // without bound.
        if matches!(wrapped.kind, crate::mcp::peers::types::WrappedKind::Question) {
            let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(workspace);
            facade.bump_inflight_stats(&target_key, PeerStatsDelta::IncomingPlus1);
            workspace.stamp_inflight_target(&wrapped.correlation_id, &target_key);
        }

        // Fire the typed peer-envelope echo BEFORE the LLM-side
        // dispatch so the user-turn block renders in the right
        // order regardless of which event the TUI reducer drains
        // first. The CLI doesn't echo stdin-injected prompts back
        // on stream-json output (only tool_result-bearing user
        // envelopes come back), so the TUI gets no inbound user-turn
        // signal from the SDK side - `PeerEnvelopeAppended` is how
        // the TUI knows to render the peer block.
        push_peer_user_turn_into_chat(workspace, &target_key, &wrapped);
        let text = wrapped.to_prose();
        if let Err(err) = workspace.dispatch_workspace_prompt(&target_key, text) {
            tracing::warn!(
                target: "forge_workspace::spawn",
                target_project = %target_project,
                error = ?err,
                "DeliverPeerPrompt dispatch to running target failed"
            );
            send_dispatch_turn_error(workspace, target_key, &err);
        }
        return;
    }

    // Target is sleeping (or unknown - defensive). If the project
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

    // Ensure DomainSession at synth_key + buffer wrapped.
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

/// Outcome of a cron fire's delivery, so `fire_due_crons` can decide the
/// entry's fate rather than treating every hand-off as a success.
pub(crate) enum CronFireOutcome {
    /// Dispatched into a running session, or buffered + a spawn kicked off.
    /// The caller advances/removes the cron as normal.
    Delivered,
    /// The cron's project is no longer in forge.toml. The caller removes
    /// the entry instead of advancing a dead cron forever.
    TargetGone,
    /// The Command channel is closed (workspace shutting down). The caller
    /// leaves the cron due so the next boot catch-up re-fires it.
    DispatchFailed,
}

/// Text delivered for a cron fire: the raw prompt, prefixed with a plain
/// marker when the fire is overdue so the owner can tell a catch-up from
/// an on-time fire.
pub(crate) fn missed_cron_text(prompt: &str, missed: bool) -> String {
    if missed { format!("[missed cron] {prompt}") } else { prompt.to_owned() }
}

/// Deliver a due cron's prompt into its OWNER's session as a plain user
/// turn AND echo it as a cron block. `team_role` `None` routes to the
/// project lead, `Some(label)` to that worker. A live owner gets a
/// `Command::Prompt`; an asleep-but-existing owner is woken by dispatching
/// `Command::SpawnProject` (which resumes the lead and, through the lead's
/// respawn, the worker), the prompt buffered by `(project, team_role)`
/// and drained on the owner's connect. An owner that no longer exists (a
/// gone project, or a worker label with no persisted row) yields
/// `TargetGone`.
pub(crate) fn deliver_cron_prompt(
    workspace: &Arc<Workspace>,
    project_name: &str,
    team_role: Option<&str>,
    prompt: String,
    missed: bool,
) -> CronFireOutcome {
    let Some(view) = workspace.list_projects().into_iter().find(|v| v.name == project_name) else {
        return CronFireOutcome::TargetGone;
    };

    if let Some(target_key) = live_cron_owner(workspace, &view, team_role) {
        // Echo the cron block BEFORE the LLM-side dispatch so it renders in
        // order regardless of which event the TUI reducer drains first.
        let text = missed_cron_text(&prompt, missed);
        push_cron_prompt_into_chat(workspace, &target_key, &text);
        return match workspace.dispatch_workspace_prompt(&target_key, text) {
            Ok(()) => CronFireOutcome::Delivered,
            Err(err) => {
                tracing::warn!(
                    target: "forge_workspace::spawn",
                    project = %project_name,
                    error = ?err,
                    "cron fire dispatch to live owner failed",
                );
                send_dispatch_turn_error(workspace, target_key, &err);
                CronFireOutcome::DispatchFailed
            }
        };
    }

    // Asleep: only wake an owner we can confirm still exists. A conclusive
    // absence removes the cron; an owner check that could not read leaves it
    // for the next tick rather than deleting a real owner's cron on a hiccup.
    match cron_owner_exists(workspace, &view, team_role) {
        CronOwnerCheck::Exists => {}
        CronOwnerCheck::Absent => return CronFireOutcome::TargetGone,
        CronOwnerCheck::Unknown => return CronFireOutcome::DispatchFailed,
    }
    // Buffer by owner, then wake via resume: SpawnProject resumes the lead,
    // whose reconnect re-spawns the persisted workers; each drains its own
    // bucket on connect.
    workspace.buffer_cron_for_owner(project_name, team_role, prompt, missed);
    match workspace.dispatch(Command::SpawnProject {
        project_name: project_name.to_owned(),
        launch_settings: SessionLaunchSettings::default(),
    }) {
        Ok(()) => CronFireOutcome::Delivered,
        Err(err) => {
            tracing::warn!(
                target: "forge_workspace::spawn",
                project = %project_name,
                error = ?err,
                "cron fire SpawnProject dispatch failed",
            );
            CronFireOutcome::DispatchFailed
        }
    }
}

/// The live session that owns a cron in `view`: for a lead cron the
/// running lead (the first open session that is not a live worker); for a
/// worker cron the live worker with that label. `None` when the owner is
/// asleep.
fn live_cron_owner(
    workspace: &Arc<Workspace>,
    view: &crate::views::ProjectView,
    team_role: Option<&str>,
) -> Option<SessionKey> {
    let live = workspace.list_live_workers(&view.key);
    let candidate = if let Some(label) = team_role {
        live.into_iter().find(|w| w.label == label).map(|w| w.session_key)
    } else {
        let live_keys: std::collections::HashSet<_> =
            live.into_iter().map(|w| w.session_key).collect();
        view.sessions
            .iter()
            .find(|s| s.is_open && !live_keys.contains(&s.session))
            .map(|s| s.session.clone())
    };
    // Only treat the owner as a live dispatch target once it has stamped
    // its session_id. A still-spawning owner (session_id None) would drop
    // a bare Command::Prompt, so fall through to the buffer-and-wake path
    // where its own Connected handler drains the owner-keyed buffer.
    let key = candidate?;
    workspace.domain_session_for(&key).filter(|d| d.lock().session_id.is_some()).map(|_| key)
}

/// Outcome of checking whether a cron's owner still exists to be woken.
enum CronOwnerCheck {
    /// The owner exists (the lead, or a worker with a persisted row).
    Exists,
    /// Conclusively gone: the read succeeded and the label has no row in
    /// the `dynamic_workers` table.
    Absent,
    /// The durable-worker lookup could not read, so absence is unconfirmed.
    Unknown,
}

/// Whether a cron's owner still exists to be woken. The lead exists
/// whenever its project does; a worker exists while its label has a row in
/// the `dynamic_workers` table, since that row is what re-spawns it. A read
/// failure yields [`CronOwnerCheck::Unknown`] so the fire router leaves the
/// cron rather than deleting a real owner's cron on a transient hiccup.
fn cron_owner_exists(
    workspace: &Arc<Workspace>,
    view: &crate::views::ProjectView,
    team_role: Option<&str>,
) -> CronOwnerCheck {
    let Some(label) = team_role else {
        return CronOwnerCheck::Exists;
    };
    match workspace.dynamic_worker_exists(&view.key, label) {
        Ok(true) => CronOwnerCheck::Exists,
        Ok(false) => CronOwnerCheck::Absent,
        Err(_) => CronOwnerCheck::Unknown,
    }
}

/// Deliver a matched Gotify `notification` into `project` as a plain user
/// turn AND echo it into the target's chat as a notification block. When
/// `team_role` names a running team worker, deliver straight to it;
/// otherwise deliver to the project lead - echo + dispatch a
/// `Command::Prompt` if it's running, else buffer on the synthetic
/// spawn-key's DomainSession and dispatch `Command::SpawnProject`
/// (`SessionTask` drains + echoes on Connected via
/// `drain_pending_gotify_prompts`). Mirrors [`deliver_cron_prompt`] plus
/// the peer chat-echo. A team-worker subscription with NO live entry
/// falls through to lead delivery (spawning the project brings the team
/// up); a live-but-not-yet-connected worker buffers on its own domain
/// instead. A project no longer in forge.toml is logged and skipped.
pub(crate) fn deliver_gotify_message(
    workspace: &Arc<Workspace>,
    project: &str,
    team_role: Option<&str>,
    notification: GotifyNotification,
) {
    // A team-worker subscription targets that worker. Once it's a live
    // entry the notification belongs to it, never the lead - so handle
    // both the connected case (dispatch now) and the still-spawning case
    // (buffer on the worker's own DomainSession for its Connected drain)
    // here, and never fall through to the lead below.
    if let Some(role) = team_role
        && let Some(worker_key) = team_worker_key(workspace, project, role)
    {
        let connected = workspace
            .domain_session_for(&worker_key)
            .is_some_and(|d| d.lock().session_id.is_some());
        if connected {
            // Echo the notification block BEFORE the LLM-side dispatch so
            // it renders in order regardless of which event the TUI reducer
            // drains first (mirrors handle_deliver_peer_prompt).
            push_gotify_notification_into_chat(workspace, &worker_key, &notification);
            if let Err(err) =
                workspace.dispatch_workspace_prompt(&worker_key, notification.to_prose())
            {
                tracing::warn!(
                    target: "forge_workspace::spawn",
                    project = %project,
                    role = %role,
                    error = ?err,
                    "gotify deliver to running team worker failed",
                );
                send_dispatch_turn_error(workspace, worker_key, &err);
            }
        } else if let Some(domain) = workspace.domain_session_for(&worker_key) {
            domain.lock().pending_gotify_prompts.push(notification);
        } else {
            // Live entry exists but its DomainSession isn't registered yet
            // (the sub-second window between insert_live_worker and the
            // spawn's handle registration). Buffer on the worker's own
            // key - the spawn registers that exact DomainSession (reuse,
            // not overwrite), and its Connected drain re-dispatches the
            // notification.
            let mut handles = workspace.domain_handles.lock();
            let domain = handles
                .entry(worker_key.clone())
                .or_insert_with(|| {
                    Arc::new(Mutex::new(DomainSession::new(worker_key.clone(), None)))
                })
                .clone();
            drop(handles);
            domain.lock().pending_gotify_prompts.push(notification);
        }
        return;
    }

    let running_lead =
        workspace.list_projects().into_iter().find(|v| v.name == project).and_then(|v| {
            let live_worker_keys: std::collections::HashSet<_> =
                workspace.list_live_workers(&v.key).into_iter().map(|w| w.session_key).collect();
            v.sessions
                .into_iter()
                .find(|s| s.is_open && !live_worker_keys.contains(&s.session))
                .map(|s| s.session)
        });

    if let Some(target_key) = running_lead {
        push_gotify_notification_into_chat(workspace, &target_key, &notification);
        if let Err(err) = workspace.dispatch_workspace_prompt(&target_key, notification.to_prose())
        {
            tracing::warn!(
                target: "forge_workspace::spawn",
                project = %project,
                error = ?err,
                "gotify deliver to running project failed",
            );
            send_dispatch_turn_error(workspace, target_key, &err);
        }
        return;
    }

    // Asleep: buffer on the synthetic spawn key and spawn the project
    // (only if it's a real forge.toml project).
    if workspace.find_project_view_by_name(project).is_none() {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project,
            "gotify delivery target gone from forge.toml; skipping",
        );
        return;
    }

    let synth_key = SessionKey::from_session_id(format!("__spawn_{project}__"));
    {
        let mut handles = workspace.domain_handles.lock();
        let domain = handles
            .entry(synth_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(DomainSession::new(synth_key.clone(), None))))
            .clone();
        drop(handles);
        domain.lock().pending_gotify_prompts.push(notification);
    }

    if let Err(err) = workspace.dispatch(Command::SpawnProject {
        project_name: project.to_owned(),
        launch_settings: SessionLaunchSettings::default(),
    }) {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project,
            error = ?err,
            "gotify fire SpawnProject dispatch failed",
        );
    }
}

/// The team worker labelled `label` in `project`, if a live entry
/// exists - regardless of whether it has finished connecting. The
/// caller checks connectedness to decide dispatch-vs-buffer.
fn team_worker_key(workspace: &Arc<Workspace>, project: &str, label: &str) -> Option<SessionKey> {
    let view = workspace.list_projects().into_iter().find(|v| v.name == project)?;
    workspace
        .list_live_workers(&view.key)
        .into_iter()
        .find(|w| w.label == label)
        .map(|w| w.session_key)
}

/// Emit a typed `PeerEnvelopeAppended` so the target session's TUI
/// chat buffer shows the inbound peer user-turn. The TUI reducer
/// builds the chat-side echo directly from the `WrappedPrompt`'s
/// typed fields - workspace no longer forges an SDK `Message::User`
/// frame (audit I11).
///
/// Note: this only affects the TUI's visible chat echo. The
/// recipient's `claude` subprocess still receives the prose via a
/// separate `Command::Prompt` dispatch - the CLI's input channel
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

/// Emit a typed `GotifyNotificationAppended` so the target session's TUI
/// chat buffer shows the inbound notification block. Mirrors
/// [`push_peer_user_turn_into_chat`] - the target's `claude` subprocess
/// still receives the prose via a separate `Command::Prompt` dispatch;
/// this only drives the visible chat echo.
pub(crate) fn push_gotify_notification_into_chat(
    workspace: &Workspace,
    target_key: &SessionKey,
    notification: &GotifyNotification,
) {
    let _ = workspace.update_sender().send(SessionUpdate::GotifyNotificationAppended {
        session_id: target_key.as_str().to_owned(),
        notification: notification.clone(),
    });
}

/// Emit a typed `CronPromptAppended` so the target session's TUI chat
/// buffer shows a cron block for the fired prompt. Mirrors
/// [`push_gotify_notification_into_chat`] - the target's `claude`
/// subprocess still receives the raw prompt via a separate
/// `Command::Prompt`; this only drives the visible chat echo.
pub(crate) fn push_cron_prompt_into_chat(
    workspace: &Workspace,
    target_key: &SessionKey,
    text: &str,
) {
    let _ = workspace.update_sender().send(SessionUpdate::CronPromptAppended {
        session_id: target_key.as_str().to_owned(),
        text: text.to_owned(),
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
        try_emit(
            workspace,
            "spawn_session::unknown_session",
            SessionUpdate::ServiceStatus {
                severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
                message: format!("Session {session_id} is no longer in the project catalog"),
            },
        );
        return;
    };

    // Resume path stamps no lead charter: the drilldown resume that
    // would dispatch SpawnSession lists worker rows too, so wiring it
    // needs lead-vs-worker awareness first or a resumed worker gets
    // branded a lead. Fresh leads get the charter on their spawn paths.

    let cwd = parent.path.to_string_lossy().to_string();
    let display_name = parent.display_path.clone();

    try_emit(
        workspace,
        "spawn_session::Spawning",
        SessionUpdate::Spawning {
            key: synth_key.clone(),
            project_name: parent.name.clone(),
            cwd,
            display_name,
        },
    );

    match workspace.get_agent_handle_with_spawn_key(
        SessionTarget::Session(session_key),
        launch_settings,
        Some(synth_key.clone()),
        None,
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                session_id,
                spawn_key = %synth_key.as_str(),
                "spawn dispatched for session resume"
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

/// Switch the live session `key` to `account_display_name`: tear down
/// its current `claude` subprocess and re-spawn + resume the SAME
/// `session_id` under the picked account's `config_dir`. The account
/// config dirs share `~/.claude/projects` via symlink, so
/// `claude --resume` finds the same conversation - nothing is copied.
/// The forced-account re-spawn seeds `connected_once = true`, so its
/// first `Connected` emits `SessionReplaced`: the chat resets, then the
/// `--resume` backfill re-seeds the same conversation and the new
/// account's `ForgeAccountIdentity` refreshes the label. Refuses with
/// a notice (and no teardown) when a turn is in flight - the
/// authoritative backstop for the TUI idle-gate; a no-op when the
/// account name is unknown.
pub(crate) fn handle_switch_account(
    workspace: &Arc<Workspace>,
    key: SessionKey,
    account_display_name: &str,
    launch_settings: SessionLaunchSettings,
) {
    let Some(forced_account) = workspace.resolve_account_for_switch(account_display_name) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            key = %key.as_str(),
            account = %account_display_name,
            "switch_account: unknown account; ignoring",
        );
        try_emit(
            workspace,
            "switch_account::unknown_account",
            SessionUpdate::SlashCommandError {
                key,
                message: format!("Unknown account: {account_display_name}"),
            },
        );
        return;
    };

    // Authoritative idle backstop: refuse the switch while a turn is in
    // flight, independent of the TUI idle-gate. A delivered peer / cron
    // / gotify prompt can start a turn between picker-open and Enter, and
    // tearing that turn down would silently drop pending interactions and
    // strand inflight peer asks. Surface a notice; do NOT tear down.
    let turn_in_flight =
        workspace.domain_session_for(&key).is_some_and(|domain| domain.lock().turn_in_flight());
    if turn_in_flight {
        try_emit(
            workspace,
            "switch_account::busy",
            SessionUpdate::SlashCommandError {
                key,
                message: "Finish or cancel the current turn before switching accounts.".to_owned(),
            },
        );
        return;
    }

    // Tear down the current account's subprocess so the re-spawn does
    // not hit the pool fast-path and return the existing handle. The
    // old SessionTask exits on its now-closed command channel; its
    // supersession-guarded cleanup no-ops against the new entry.
    workspace.release_session(&key);

    match workspace.get_agent_handle_with_spawn_key(
        SessionTarget::Session(key.clone()),
        launch_settings,
        None,
        Some(forced_account),
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                key = %key.as_str(),
                account = %account_display_name,
                "account switch re-spawn started",
            );
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                key = %key.as_str(),
                account = %account_display_name,
                error = %err,
                "switch_account: re-spawn failed",
            );
            try_emit(
                workspace,
                "switch_account::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key,
                    message: format!("account switch failed: {err}"),
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
    mut launch_settings: SessionLaunchSettings,
) {
    let synth_key = SessionKey::from_session_id("__conn_pending__".to_owned());
    let target = match project_name {
        Some(name) => SessionTarget::Named(name),
        None => SessionTarget::Default,
    };

    apply_lead_charter(&mut launch_settings);

    match workspace.get_agent_handle_with_spawn_key(
        target,
        launch_settings,
        Some(synth_key.clone()),
        None,
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                spawn_key = %synth_key.as_str(),
                "startup spawn dispatched"
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

/// Synthesize a unique pool key for a not-yet-spawned worker. The v4
/// uuid suffix keeps concurrent same-label spawns from colliding on the
/// pool / command_senders / domain_handles maps (only one would
/// survive); the resume path uses a distinct `__resume_worker_` prefix
/// so it's separable from the fresh case in tracing.
fn synth_worker_key(project_key: &ProjectKey, label: &str, is_resume: bool) -> SessionKey {
    let synth_prefix = if is_resume { "__resume_worker_" } else { "__spawn_worker_" };
    SessionKey::from_session_id(format!(
        "{}{}_{}_{}__",
        synth_prefix,
        project_key.as_str(),
        label,
        uuid::Uuid::new_v4().simple()
    ))
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
    resume_existing: Option<String>,
    kick: Option<String>,
    interactive: bool,
    return_to: tokio::sync::oneshot::Sender<Result<WorkerSpawnReply, String>>,
) {
    // Verify the project exists before claiming a synth key. Probe its
    // filesystem path for git-repo-ness exactly once here - a blocking FS
    // call, deliberately BEFORE the live_workers critical section below so
    // it never widens the dedup window - and feed the result into both the
    // WorkerEntry flag and the `--worktree` extra-arg threading below.
    let projects = workspace.list_projects();
    let Some(view) = projects.iter().find(|v| v.key == project_key) else {
        let _ = return_to.send(Err(format!("project not found: {}", project_key.as_str())));
        return;
    };
    let is_git = forge_agent::env::worktree::is_git_repo(&view.path);

    // Synthesize a pool key for the not-yet-spawned worker. The
    // SessionTask rekeys this onto the real claude-issued UUID on
    // first Connected; migrate_session_task also rewrites the
    // matching WorkerEntry's session_key in lockstep.
    let is_resume = resume_existing.is_some();
    let synth_key = synth_worker_key(&project_key, label, is_resume);
    let tag = forge_primitives::worker_tag(label);

    // Insert WorkerEntry as Spawning BEFORE the agent spawn so the
    // Connected handler can find the entry via worker_lookup_for_session
    // when it fires (worker_lookup_for_session reads live_workers).
    //
    // On the resume path `needs_tag` is false - the tag is already on
    // disk in the JSONL (the lead Connected hook only resumes sessions
    // whose tag matches `forge:worker:<label>` so this invariant is
    // guaranteed by the caller).
    //
    // #222: for resume, seed `session_key` with the REAL session_id
    // (from `resume_existing`) rather than the synth key. The fresh
    // spawn path needs the synth-key placeholder because the real
    // session_id isn't known until claude's first `system/init` event;
    // `migrate_session_task` rewrites the WorkerEntry alongside the
    // pool maps when Connected fires (workspace.rs:2620-2633). The
    // resume path knows the real session_id up front, so workspace
    // keys the SessionTask under the real id directly + `rekey_to`
    // becomes a no-op on Connected + `migrate_session_task` never
    // fires + the synth-key WorkerEntry would never get rekeyed.
    // Result: workers stuck visible-as-synth-key in workers__list and
    // TUI bucket lookups failing with "bucket not yet present".
    let entry = crate::mcp::workers::types::WorkerEntry {
        label: label.to_owned(),
        charter: charter.clone(),
        session_key: match &resume_existing {
            Some(real) => SessionKey::from_session_id(real.clone()),
            None => synth_key.clone(),
        },
        status: forge_primitives::WorkerLiveness::Spawning,
        spawned_at: std::time::SystemTime::now(),
        spawned_by_session_id,
        needs_tag: !is_resume,
        is_git_repo_at_spawn: is_git,
        diagnostic: None,
        kick,
    };
    // At-most-one-live-worker-per-label, enforced atomically at this
    // shared core so neither dispatch source - the boot re-spawn or an
    // MCP `workers__spawn` - can double-insert and fork two subprocesses
    // onto one worktree, even on two genuinely-concurrent dispatches for
    // the same label.
    if let Err(existing) = workspace.insert_live_worker_if_label_absent(&project_key, entry.clone())
    {
        let existing_session = existing.as_str().to_owned();
        tracing::debug!(
            target: "forge_workspace::spawn",
            project = %project_key.as_str(),
            label = %label,
            %existing_session,
            "spawn_worker: label already live; skipping duplicate spawn",
        );
        let _ = return_to.send(Err(format!(
            "a worker labeled '{label}' is already live (session {existing_session}); message it with workers__tell / workers__ask or close it first (one live worker per label)"
        )));
        return;
    }
    // Extend the assignment plan so this worker's account comes from the
    // same rotation as the lead's. No-op while the plan is unpopulated
    // (boot still in flight) - `recompute_plan_if_ready` seeds the live
    // workers when it lands, which is the only thing that gives this one
    // an entry.
    let rate_limited_account = workspace.extend_plan_for_adhoc_worker(&project_key, label);
    try_emit(
        workspace,
        "spawn_worker::WorkerStatusChanged::Added",
        SessionUpdate::WorkerStatusChanged {
            project_key: project_key.clone(),
            action: WorkerStatusAction::Added,
            status: entry.to_status(),
            worktree: WorktreeDisposition::untouched(entry.is_git_repo_at_spawn),
        },
    );

    // Spawn the fresh session under the picked account. The charter
    // is threaded onto SessionLaunchSettings.charter; the spawn path
    // (forge_sdk_worker::build_options_with_callback) appends it to
    // the system prompt via --append-system-prompt. `extra_args`
    // carries `("worktree", Some(label))` for git-repo projects so
    // claude forks a worktree at `<repo>/.claude/worktrees/<label>/`.
    //
    // Resume path: `SessionTarget::Session` reads the original cwd
    // from the catalog (Workspace::session_cwd_for) so claude lands
    // back in the worktree it was first spawned in. If the worktree
    // dir was removed out-of-band, claude's spawn fails and the
    // existing ConnectionFailed surface (workspace.rs:570) reports
    // it - we don't silently fall back to fresh-spawn (that would
    // lose state without warning).
    let settings = SessionLaunchSettings {
        charter: Some(charter),
        extra_args: build_worker_extra_args(is_git, label, interactive),
        ..Default::default()
    };
    let target = match resume_existing {
        Some(session_id) => SessionTarget::Session(SessionKey::from_session_id(session_id)),
        None => SessionTarget::FreshInProject {
            project_key: project_key.clone(),
            synth_key: synth_key.clone(),
        },
    };
    match workspace.get_agent_handle_with_spawn_key(target, settings, Some(synth_key.clone()), None)
    {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %label,
                spawn_key = %synth_key.as_str(),
                "spawn dispatched for worker"
            );
            // Reply to the LLM optimistically with the synth key.
            // The LLM addresses subsequent calls by label; the
            // session_id field is informational. Real session UUID
            // lands on Connected via the rekey machinery.
            let _ = return_to.send(Ok(WorkerSpawnReply {
                session_id: synth_key.as_str().to_owned(),
                tag,
                rate_limited_account: rate_limited_account.map(|k| k.0),
                // The MCP facade fills this after its post-reply persist;
                // the re-spawn paths never persist, so it stays None.
                durability_warning: None,
            }));
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
                        // The rollback beat the subprocess, so
                        // `--worktree <label>` never reached one and
                        // there is no worktree to point the user at.
                        worktree: WorktreeDisposition::Absent,
                    },
                );
            }
            let _ = return_to.send(Err(format!("agent spawn failed: {err}")));
        }
    }
}

/// Shared worker teardown used by both `handle_close_worker` (the TUI
/// X-button) and `handle_despawn_worker` (the `workers__despawn` MCP
/// tool): remove the latest-spawned worker matching `label` from
/// `live_workers[project_key]`, release its session (terminates the
/// claude subprocess on drop), and expire its inflight asks. Returns
/// the removed `WorkerEntry`, or `None` when no live worker matched.
/// JSONL on disk is NOT deleted - teardown only removes the in-memory
/// live state.
///
/// The `Removed` event is the caller's to emit via
/// [`emit_worker_removed`]: the despawn path only learns what became
/// of the worktree after this returns.
///
/// Worker-bound asks whose `target_project` composite
/// (`<project_key>::<label>`) names the torn-down worker are expired
/// via `Workspace::expire_inflight_for_closed_worker` so their
/// caller's LLM receives a `DeliveryFailureNotice` instead of
/// waiting forever for a reply.
fn teardown_worker(
    workspace: &Arc<Workspace>,
    project_key: &ProjectKey,
    label: &str,
) -> Option<crate::mcp::workers::types::WorkerEntry> {
    let entry = workspace.remove_latest_worker(project_key, label)?;
    // Both entry points into this routine (the Projects-pane close and
    // the `workers__despawn` MCP tool) delete the persisted worker row so
    // it never re-spawns. Cancel and the lead-close cascade go through
    // other paths and deliberately leave the row intact.
    workspace.delete_dynamic_worker(project_key, label);
    // The row is gone, so nothing re-spawns this label: its durable state
    // has no owner left to wake and goes with it.
    workspace.remove_gotify_subscriptions_for_worker(project_key, label);
    workspace.delete_crons_for_worker(project_key, label);
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
    Some(entry)
}

/// Announce a torn-down worker to the TUI, carrying what became of its
/// worktree for the close toast to state.
fn emit_worker_removed(
    workspace: &Arc<Workspace>,
    project_key: &ProjectKey,
    entry: &crate::mcp::workers::types::WorkerEntry,
    worktree: WorktreeDisposition,
) {
    let _ = workspace.update_tx().send(SessionUpdate::WorkerStatusChanged {
        project_key: project_key.clone(),
        action: WorkerStatusAction::Removed,
        status: entry.to_status(),
        worktree,
    });
}

/// Handle a `Command::CloseWorker` (the TUI per-row X-button): tear
/// the worker down via [`teardown_worker`]. Does NOT touch the git
/// worktree - that's the `workers__despawn` path's job; the X-button's
/// behavior is intentionally unchanged.
pub(crate) fn handle_close_worker(
    workspace: &Arc<Workspace>,
    project_key: &ProjectKey,
    label: &str,
) {
    let Some(entry) = teardown_worker(workspace, project_key, label) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project_key.as_str(),
            label = %label,
            "handle_close_worker: no matching live worker"
        );
        return;
    };
    let worktree = WorktreeDisposition::untouched(entry.is_git_repo_at_spawn);
    emit_worker_removed(workspace, project_key, &entry, worktree);
}

/// Handle a `Command::DespawnWorker` (the `workers__despawn` MCP
/// tool): the lead's clean-close gesture. Unlike `handle_close_worker`
/// it also cleans up the worker's git worktree.
///
/// Order matters: the worktree dirty-check runs BEFORE any teardown,
/// so a dirty worker (uncommitted/untracked or unpushed) is blocked
/// without being killed (unless `force`). Teardown then releases the
/// session, which drops the worker's command sender; the disconnect
/// itself runs asynchronously on the worker's own task and can take
/// up to the 5s close-wait budget to reap the child. The worktree
/// removal runs immediately on the command loop, so it may overlap
/// the claude child's final seconds. A post-teardown worktree-removal
/// failure is surfaced as a warning in the [`DespawnResult`] but never
/// rolls back the kill - teardown and worktree cleanup are independent.
pub(crate) fn handle_despawn_worker(
    workspace: &Arc<Workspace>,
    project_key: &ProjectKey,
    label: &str,
    force: bool,
    respond: tokio::sync::oneshot::Sender<crate::protocol::DespawnResult>,
) {
    use crate::protocol::DespawnResult;

    // Peek the latest-spawned matching worker WITHOUT removing it, so a
    // blocked despawn leaves it live.
    let Some(entry) =
        workspace.list_live_workers(project_key).into_iter().rev().find(|w| w.label == label)
    else {
        let _ = respond.send(DespawnResult::NotFound);
        return;
    };

    // Resolve the worktree path for git-repo workers (claude's
    // `<project_root>/.claude/worktrees/<label>/`). Non-git workers
    // have no worktree to clean.
    let project_view = workspace.list_projects().into_iter().find(|v| v.key == *project_key);
    let worktree_path = if entry.is_git_repo_at_spawn {
        project_view
            .as_ref()
            .map(|v| crate::mcp::workers::types::worker_tag_dir(&v.path, label, true))
    } else {
        None
    };

    // Dirty-check BEFORE teardown: block (nothing torn down) when the
    // worktree is dirty and `force` is not set.
    if !force
        && let Some(path) = worktree_path.as_ref()
        && let Some(reason) = forge_agent::env::worktree::worktree_dirty_reason(path)
    {
        let _ = respond.send(DespawnResult::Blocked { reason });
        return;
    }

    // Teardown (kills the subprocess on drop). The single-threaded
    // command loop means nothing mutated `live_workers` between the
    // peek above and here, but re-checking the removal is defensive.
    let Some(entry) = teardown_worker(workspace, project_key, label) else {
        let _ = respond.send(DespawnResult::NotFound);
        return;
    };

    // Resolve the torn-down worktree's `(project name, branch)` while
    // the path still exists, so its persisted review state can be judged
    // once the worktree is gone. Keyed by the forge.toml project NAME to
    // match what the diff overlay saved under.
    let review_key = worktree_path.as_ref().and_then(|path| {
        let Some(branch) = forge_agent::env::worktree::worktree_branch(path) else {
            tracing::warn!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %label,
                "despawn: could not resolve the worktree branch (detached HEAD or git error); its review threads are not cleaned up and may resurrect on a later worktree reusing the branch",
            );
            return None;
        };
        let Some(name) = project_view.as_ref().map(|v| v.name.clone()) else {
            tracing::warn!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %label,
                branch = %branch,
                "despawn: could not resolve the project name; the branch's review threads are not cleaned up",
            );
            return None;
        };
        Some((name, branch))
    });

    // Worktree cleanup runs AFTER teardown on a verified-clean (or
    // forced) worktree. A failure here is reported but never rolls
    // back the already-completed teardown.
    let mut branch_cleanup_warning = None;
    let worktree_cleanup_warning = match worktree_path.as_ref() {
        Some(path) => match forge_agent::env::worktree::remove_worktree(path, force) {
            Ok(()) => {
                // Only after a successful removal: while the worktree
                // stands it holds the branch checked out, and git refuses
                // to delete a checked-out branch.
                branch_cleanup_warning =
                    project_view.as_ref().and_then(|v| reap_worker_branch(&v.path, label));
                // Threads are only orphaned once their branch is gone, and
                // the reap above can be what removes it - a worker that
                // made no branch of its own sits on `worktree-<label>`.
                if let Some((project, branch)) = review_key.as_ref()
                    && let Some(view) = project_view.as_ref()
                    && !forge_agent::env::worktree::branch_ref_exists(&view.path, branch)
                {
                    workspace.delete_branch_review_state(project, branch);
                }
                None
            }
            Err(err) => {
                tracing::warn!(
                    target: "forge_workspace::spawn",
                    project = %project_key.as_str(),
                    label = %label,
                    error = %err,
                    "despawn: worker torn down but worktree cleanup failed"
                );
                Some(err.to_string())
            }
        },
        None => None,
    };

    // Ground-truthed against the directory rather than git's exit code:
    // git errors for a path it no longer tracks whether or not the
    // directory survives, and the toast's only claim is what is on disk.
    let worktree = match (worktree_path.as_ref(), worktree_cleanup_warning.as_ref()) {
        (None, _) => WorktreeDisposition::untouched(entry.is_git_repo_at_spawn),
        (Some(path), Some(_)) if path.exists() => WorktreeDisposition::RemovalFailed,
        (Some(_), _) => WorktreeDisposition::Removed,
    };
    emit_worker_removed(workspace, project_key, &entry, worktree);

    let _ =
        respond.send(DespawnResult::Despawned { worktree_cleanup_warning, branch_cleanup_warning });
}

/// Reap the `worktree-<label>` branch claude creates for a worker's
/// worktree, which outlives the worktree itself. Returns a warning when
/// the branch was left in place, `None` when it was deleted or never
/// existed.
///
/// The branch is named by convention, never taken from whatever the
/// worktree had checked out: a worker that made its own feature branch
/// has real work on that one, and this must not aim at it.
fn reap_worker_branch(repo: &std::path::Path, label: &str) -> Option<String> {
    use forge_agent::env::worktree::BranchReapOutcome;

    let branch = format!("worktree-{label}");
    let warning = match forge_agent::env::worktree::reap_worktree_branch(repo, &branch) {
        BranchReapOutcome::Reaped => None,
        BranchReapOutcome::NotPresent => {
            tracing::debug!(
                target: "forge_workspace::spawn",
                label = %label,
                branch = %branch,
                "despawn: no worktree branch to reap"
            );
            None
        }
        BranchReapOutcome::Kept { count, tip } => {
            let plural = if count == 1 { "" } else { "s" };
            Some(format!(
                "branch '{branch}' kept: {count} commit{plural} reachable from no other ref \
                 (tip {tip}). Inspect with 'git log -{count} {tip}', then 'git branch -D \
                 {branch}' once it has landed."
            ))
        }
        BranchReapOutcome::KeptOnError { reason } => Some(format!(
            "branch '{branch}' kept: could not verify it holds no unique commits ({reason}). \
             Check 'git log {branch}' and delete it by hand."
        )),
        BranchReapOutcome::DeleteFailed { reason } => Some(format!(
            "branch '{branch}' holds no unique commits, but the delete failed ({reason}). \
             Retry with 'git branch -D {branch}'."
        )),
    };
    // The oneshot reply reaches an LLM's tool result and nothing else, so
    // without this the operator's log has no record of a kept branch.
    if let Some(warning) = &warning {
        tracing::warn!(
            target: "forge_workspace::spawn",
            label = %label,
            branch = %branch,
            warning = %warning,
            "despawn: worktree branch kept"
        );
    }
    warning
}

/// Handle a `Command::DeliverWorkerPrompt`: route a wrapped peer-style
/// envelope to the worker matching `target_label` in the caller's
/// project. Latest-spawned-wins on duplicate labels. The typed
/// PeerEnvelopeAppended echo follows the same pattern as
/// `handle_deliver_peer_prompt` - workers reuse the peer envelope
/// verbatim so the TUI's chat render is identical between the two
/// paths.
///
/// Unlike `handle_deliver_peer_prompt`, this handler never buffers +
/// auto-spawns: workers are only addressable while live. If the
/// target label vanished between the dispatch and the handler firing
/// (worker closed, lead cascade fired), the prompt is dropped with a
/// warn log.
/// Buffer `wrapped` on `target_key`'s `DomainSession` when the target
/// hasn't finished its Connected handshake (no `session_id` yet). Returns
/// `Some(wrapped)` when the target IS connected (caller delivers now), or
/// `None` when it buffered (the target's Connected handler drains
/// `pending_peer_prompts`, doing the bump + render + dispatch). Mirrors
/// the sleeping-peer buffering in `handle_deliver_peer_prompt` so the
/// bump bookkeeping happens exactly once, at real delivery time.
fn buffer_prompt_until_connected(
    workspace: &Arc<Workspace>,
    target_key: &SessionKey,
    wrapped: WrappedPrompt,
) -> Option<WrappedPrompt> {
    let Some(domain) = workspace.domain_handles.lock().get(target_key).cloned() else {
        // No DomainSession to buffer on (target vanished); let the caller
        // proceed - its dispatch warns on the missing target.
        return Some(wrapped);
    };
    let mut d = domain.lock();
    if d.session_id.is_some() {
        return Some(wrapped);
    }
    d.pending_peer_prompts.push(wrapped);
    None
}

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
        // Expire the asks routed at this worker so their callers get
        // the DeliveryFailureNotice instead of waiting the 30-min
        // timeout for a target that no longer exists.
        workspace.expire_inflight_for_closed_worker(project_key, target_label);
        return;
    };
    let target_key = entry.session_key.clone();

    // A worker addressed before it finishes its Connected handshake has
    // no session_id yet, so a bare Command::Prompt would be dropped by
    // execute_command_via_handle. Buffer it on the worker's own
    // DomainSession instead - its Connected handler drains
    // pending_peer_prompts (bump + render + dispatch) exactly like the
    // sleeping-peer path. Skips the tag retry / stamp / dispatch below.
    let Some(wrapped) = buffer_prompt_until_connected(workspace, &target_key, wrapped) else {
        return;
    };

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
            workspace.retry_worker_tag_opportunistic(
                project_key,
                &target_key,
                target_label,
                &cwd,
                entry.is_git_repo_at_spawn,
            );
        } else {
            tracing::debug!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %target_label,
                "deliver_worker_prompt: project view missing; skipping tag retry"
            );
        }
    }

    // Bump target's incoming counter for Question kind only (matches
    // peer behavior - the sidebar badge tracks awaiting-reply asks).
    if matches!(wrapped.kind, crate::mcp::peers::types::WrappedKind::Question) {
        let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(workspace);
        facade.bump_inflight_stats(&target_key, PeerStatsDelta::IncomingPlus1);
        workspace.stamp_inflight_target(&wrapped.correlation_id, &target_key);
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
    if let Err(err) = workspace.dispatch_workspace_prompt(&target_key, text) {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project_key.as_str(),
            label = %target_label,
            error = ?err,
            "DeliverWorkerPrompt dispatch to worker failed"
        );
        send_dispatch_turn_error(workspace, target_key, &err);
    }
}

/// Handle a `Command::DeliverWorkerPromptToLead`: route a wrapped
/// peer-style prompt from a worker back to its lead, addressed by
/// the lead's `SessionKey` (resolved at Tool dispatch from the
/// worker's `spawned_by_session_id`). Same wire shape as
/// `DeliverWorkerPrompt` - PeerEnvelopeAppended echo + Command::Prompt
/// dispatch - so the lead's TUI renders the message identically to
/// a sibling-worker delivery.
///
/// Drops with a warn log when the target lead session is no longer
/// in the pool (lead closed since the worker captured its
/// `spawned_by_session_id`). The lead can't be auto-respawned from
/// this path: that's a project-level decision the worker isn't
/// authorized to make.
pub(crate) fn handle_deliver_worker_prompt_to_lead(
    workspace: &Arc<Workspace>,
    _caller: SessionKey,
    target_lead_key: &SessionKey,
    wrapped: WrappedPrompt,
) {
    // Defensive: confirm the lead session is still in the pool. If it
    // closed since the worker captured its `spawned_by_session_id`,
    // drop the prompt with a warn - same shape as `DeliverWorkerPrompt`
    // does for a worker that vanished between dispatch and handler.
    if !workspace.pool.lock().contains_key(target_lead_key) {
        tracing::warn!(
            target: "forge_workspace::spawn",
            target = %target_lead_key.as_str(),
            "deliver_worker_prompt_to_lead: lead session not in pool (closed since dispatch)"
        );
        return;
    }

    // Same pre-Connect guard as the sibling-worker path: if the lead
    // hasn't stamped its session_id yet, buffer for its Connected drain
    // rather than dispatching a Command::Prompt that would be dropped.
    let Some(wrapped) = buffer_prompt_until_connected(workspace, target_lead_key, wrapped) else {
        return;
    };

    if matches!(wrapped.kind, crate::mcp::peers::types::WrappedKind::Question) {
        let facade = crate::mcp::peers::facade::ProdWorkspaceFacade::from_arc(workspace);
        facade.bump_inflight_stats(target_lead_key, PeerStatsDelta::IncomingPlus1);
        workspace.stamp_inflight_target(&wrapped.correlation_id, target_lead_key);
    }

    let text = wrapped.to_prose();
    push_peer_user_turn_into_chat(workspace, target_lead_key, &wrapped);
    drop(wrapped);
    if let Err(err) = workspace.dispatch_workspace_prompt(target_lead_key, text) {
        tracing::warn!(
            target: "forge_workspace::spawn",
            target = %target_lead_key.as_str(),
            error = ?err,
            "DeliverWorkerPromptToLead dispatch to lead failed"
        );
        send_dispatch_turn_error(workspace, target_lead_key.clone(), &err);
    }
}

/// Open a URL in the user's browser off the dispatch thread, surfacing
/// a failure as a warning notice.
pub(crate) fn handle_open_url(workspace: &Arc<Workspace>, url: String) {
    let ws = Arc::clone(workspace);
    tokio::spawn(async move {
        if let Err(err) = forge_agent::env::open_url::open_url(&url).await {
            // The notice is the user-visible surface; the warn is the
            // triage trail. Never both-swallowed.
            tracing::warn!(
                target: "forge_workspace",
                event_name = "open_url_failed",
                message = "browser open failed",
                outcome = "failure",
                url = %url,
                error = %err,
            );
            let _ = ws.update_sender().send(SessionUpdate::ServiceStatus {
                severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
                message: format!("Failed to open {url}: {err}"),
            });
        }
    });
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Workspace;
    use std::fs;
    use tempfile::tempdir;

    /// Ensure `forge/` exists and return the production `forge/forge.toml`
    /// path, so tests write where forge reads (not the legacy fallback).
    fn forge_toml_path(config_dir: &std::path::Path) -> std::path::PathBuf {
        crate::config::ensure_forge_data_dir(config_dir).expect("forge/ dir").join("forge.toml")
    }

    fn write_forge_toml(dir: &std::path::Path) {
        fs::write(
            forge_toml_path(dir),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
    }

    #[test]
    fn account_permission_mode_stamps_into_absent_settings() {
        let mut settings = SessionLaunchSettings::default();
        stamp_account_permission_mode(
            &mut settings,
            forge_primitives::permission::PermissionMode::BypassPermissions,
        );
        let mode = settings
            .settings
            .as_ref()
            .and_then(|s| s.get("permissions"))
            .and_then(|p| p.get("defaultMode"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(
            mode,
            Some("bypassPermissions"),
            "the account mode reaches the settings JSON the worker's applied_permission_mode arm reads",
        );
    }

    #[test]
    fn account_permission_mode_overrides_the_launcher_default_and_keeps_siblings() {
        let mut settings = SessionLaunchSettings {
            settings: Some(serde_json::json!({
                "permissions": { "defaultMode": "default" },
                "model": "haiku",
            })),
            ..SessionLaunchSettings::default()
        };
        stamp_account_permission_mode(
            &mut settings,
            forge_primitives::permission::PermissionMode::BypassPermissions,
        );
        let record = settings.settings.as_ref().expect("settings kept");
        assert_eq!(
            record
                .get("permissions")
                .and_then(|p| p.get("defaultMode"))
                .and_then(serde_json::Value::as_str),
            Some("bypassPermissions"),
            "the account mode wins over the launcher's session default",
        );
        assert_eq!(
            record.get("model").and_then(serde_json::Value::as_str),
            Some("haiku"),
            "sibling settings keys survive the stamp",
        );
    }

    /// `handle_spawn_project` for an unknown project name must not
    /// panic, must not emit a `SessionUpdate::Spawning`, and must
    /// log a warning. Important for the user-input boundary - a
    /// click on a stale row shouldn't crash forge-tui.
    #[tokio::test]
    async fn spawn_project_unknown_project_emits_no_update() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path());
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        handle_spawn_project(&workspace, "no-such-project", SessionLaunchSettings::default());

        // The user sees a typed notice naming the unknown project.
        assert!(
            matches!(
                rx.try_recv(),
                Ok(SessionUpdate::ServiceStatus { message, .. }) if message.contains("no-such-project")
            ),
            "unknown project surfaces a ServiceStatus notice"
        );
        assert!(rx.try_recv().is_err(), "nothing beyond the notice");
    }

    /// `handle_spawn_project` for a known project must emit a
    /// `SessionUpdate::Spawning` carrying a `__spawn_<name>__`
    /// synthetic key so the TUI can show a Waking placeholder
    /// before the agent reaches Connected.
    #[tokio::test]
    async fn spawn_project_known_project_emits_spawning_with_synth_key() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path());
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
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
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
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
    /// `ConnectionFailed { fatal: false }` - a sleeping-session
    /// spawn failure must NOT kill the app. Distinguished from
    /// `handle_start_default`'s fatal contract by route.
    ///
    /// Drives the failure by passing a session id that doesn't
    /// appear in any project catalog. `find_project_for_session`
    /// returns None and the handler exits without an emit - this
    /// regression test confirms it does NOT emit a fatal envelope.
    #[tokio::test]
    async fn spawn_session_unknown_session_emits_no_fatal() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path());
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
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
            channel: crate::mcp::peers::types::AskChannel::Peers,
            sender_name: "forge".to_owned(),
            sender_org: "Default".to_owned(),
            body: "fyi".to_owned(),
        }
    }

    /// I3 - `handle_deliver_peer_prompt` for an unknown target must
    /// not panic and must not emit a fatal envelope. The tool itself
    /// rejects unknown targets synchronously via `DeliverError::UnknownTarget`;
    /// the spawn path's defensive branch is the second line of defence
    /// when an LLM races a forge.toml reload.
    #[tokio::test]
    async fn handle_deliver_peer_prompt_unknown_target_is_no_op() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path());
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
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

    /// I3 - `handle_deliver_peer_prompt` against a sleeping known
    /// project buffers the prompt in the target's pending list and
    /// triggers a SpawnProject. The pending list grows by one.
    #[tokio::test]
    async fn handle_deliver_peer_prompt_sleeping_target_buffers_prompt() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[orgs.projects]]
name = "gateway-backend"
path = "~/Projects/gateway-backend"
auto_start = false

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let caller = SessionKey::from_str_for_test("caller-sleep");
        let w = fixture_wrapped();

        handle_deliver_peer_prompt(&workspace, caller, "gateway-backend".to_owned(), w.clone());

        // Sleeping branch parks the wrapped at a synthetic
        // `__spawn_gateway-backend__` key, then dispatches
        // SpawnProject which (synchronously inside dispatch)
        // migrates the buffered state onto the real resolved
        // session key. Either way, EXACTLY ONE DomainSession in
        // the workspace must carry our wrapped prompt - assert
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

    /// Closes #308 Fix B: tells (Message kind) are intentionally NOT
    /// bumped through the peer-stats sidebar badge. Badges represent
    /// pending asks awaiting reply, not generic activity. The
    /// `if matches!(wrapped.kind, WrappedKind::Question)` gate at
    /// spawn.rs:209 / :757 / :818 must stay in place; this test
    /// regression-locks the end-state invariant by driving the
    /// tell-dispatch path and asserting `workspace.peer_stats` stays
    /// empty.
    #[tokio::test]
    async fn tell_dispatch_does_not_bump_peer_stats() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            forge_toml_path(dir.path()),
            r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[orgs.projects]]
name = "gateway-backend"
path = "~/Projects/gateway-backend"
auto_start = false

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).await.expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        let caller = SessionKey::from_str_for_test("caller-tell");
        let w = fixture_wrapped(); // WrappedKind::Message (tell)

        handle_deliver_peer_prompt(&workspace, caller, "gateway-backend".to_owned(), w);

        // Drain the update channel - the spawn path may emit other
        // events (ProjectSpawned, ConfigDirsChanged, etc.) but it MUST
        // NOT emit `PeerInflightStatsChanged` for a tell.
        while let Ok(update) = rx.try_recv() {
            assert!(
                !matches!(update, SessionUpdate::PeerInflightStatsChanged { .. }),
                "tells (Message kind) must NOT bump peer_stats; got: {update:?}"
            );
        }
        // End-state invariant: the workspace's per-session peer_stats
        // map carries no entry for any session as a side-effect of a
        // tell.
        assert!(
            workspace.peer_stats.lock().is_empty(),
            "tells must NOT add any per-session peer_stats entry; \
             got: {:?}",
            workspace.peer_stats.lock(),
        );
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
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    /// #1: the at-most-one-live-per-label guard lives in the shared
    /// `handle_spawn_worker` core, so a second dispatch for an
    /// already-live label - a boot re-spawn racing an MCP spawn for the
    /// same label - inserts no duplicate entry and never reaches the
    /// subprocess spawn.
    #[tokio::test]
    async fn handle_spawn_worker_dedups_already_live_label() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("forge", "/tmp/forge");
        let project = workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("seeded project present")
            .key;
        // Simulate the first (e.g. static) reviewer already live.
        workspace.insert_live_worker(&project, fake_worker_entry("reviewer", "reviewer-1"));

        // A second dispatch for the same label (e.g. the dynamic reviewer)
        // hits the guard before any insert or spawn.
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project.clone(),
            "reviewer",
            "charter".to_owned(),
            "lead".to_owned(),
            None,
            None,
            false,
            tx,
        );

        assert_eq!(
            workspace.list_live_workers(&project).len(),
            1,
            "the duplicate spawn must not add a second live worker",
        );
        let reply = rx.await.expect("reply channel");
        let err = reply.expect_err("a duplicate spawn replies an error");
        assert!(err.contains("already live"), "error names the collision: {err}");
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
        assert_eq!(
            remaining.len(),
            2,
            "only the targeted worker should be removed; got {remaining:?}"
        );
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

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn branch_exists(repo: &std::path::Path, branch: &str) -> bool {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .current_dir(repo)
            .output()
            .expect("spawn git");
        match out.status.code() {
            Some(0) => true,
            Some(1) => false,
            other => panic!("git rev-parse in {repo:?} exited {other:?}, so absence is unproven"),
        }
    }

    fn fake_git_worker_entry(label: &str, key: &str) -> crate::mcp::workers::types::WorkerEntry {
        let mut entry = fake_worker_entry(label, key);
        entry.is_git_repo_at_spawn = true;
        entry
    }

    /// A non-git worker despawns cleanly with no worktree step (the
    /// teardown runs, nothing touches a worktree).
    #[tokio::test]
    async fn despawn_non_git_worker_tears_down_without_worktree_step() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        workspace.insert_live_worker(&project, fake_worker_entry("r1", "worker-1"));

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project, "r1", false, tx);
        let result = resp_rx.await.expect("despawn result");

        assert!(
            matches!(
                result,
                crate::protocol::DespawnResult::Despawned {
                    worktree_cleanup_warning: None,
                    branch_cleanup_warning: None,
                }
            ),
            "non-git worker despawns cleanly: {result:?}"
        );
        assert!(workspace.list_live_workers(&project).is_empty(), "worker removed");
        assert_eq!(
            drain_removed_dispositions(&mut rx),
            vec![WorktreeDisposition::Absent],
            "a worker that never had a worktree must not claim one",
        );
    }

    /// Despawning an unknown label reports NotFound and emits nothing.
    #[tokio::test]
    async fn despawn_unknown_label_reports_not_found() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project, "missing", false, tx);
        assert!(matches!(resp_rx.await.expect("result"), crate::protocol::DespawnResult::NotFound));
        assert!(rx.try_recv().is_err(), "no events for unknown label");
    }

    /// Build a workspace whose single project points at a temp git
    /// repo, add a worktree at `<project>/.claude/worktrees/<label>`,
    /// and register a live git-worker under it. The worktree is built
    /// from the project path `list_projects` reports so the handler's
    /// `worker_tag_dir` resolves to the exact on-disk worktree (avoids
    /// the macOS /tmp symlink mismatch). Returns the pieces the test
    /// asserts on plus the tempdir guards (which must outlive the test).
    async fn git_despawn_fixture(
        label: &str,
    ) -> (Arc<Workspace>, ProjectKey, std::path::PathBuf, tempfile::TempDir, tempfile::TempDir)
    {
        let repo = tempdir().expect("repo tempdir");
        run_git(repo.path(), &["init", "-q"]);
        run_git(repo.path(), &["config", "user.email", "t@example.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        std::fs::write(repo.path().join("README.md"), "seed").expect("write seed");
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-q", "-m", "init"]);

        let config = tempdir().expect("config tempdir");
        let repo_path_str = repo.path().to_string_lossy().replace('\\', "/");
        std::fs::write(
            forge_toml_path(config.path()),
            format!(
                "[[orgs]]\nname = \"Default\"\naccounts = [\"Stargate\"]\n\n[[orgs.projects]]\nname = \"forge\"\npath = \"{repo_path_str}\"\n\n[[accounts]]\ndisplay_name = \"Stargate\"\nconfig_dir = \"~/.claude-stargate\"\nprovider = \"anthropic\"\n"
            ),
        )
        .expect("write forge.toml");

        let workspace = Arc::new(
            Workspace::new_for_test(config.path().to_owned()).await.expect("workspace new"),
        );
        let view = workspace.list_projects().into_iter().next().expect("one project");
        let project_key = view.key.clone();
        let project_path = view.path.clone();

        let wt = project_path.join(".claude").join("worktrees").join(label);
        std::fs::create_dir_all(wt.parent().expect("wt parent")).expect("mkdir worktrees");
        // `-b worktree-<label>` is what claude's `--worktree <label>` does;
        // without it git auto-names the branch after the directory and the
        // reap has nothing matching to find.
        run_git(
            &project_path,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("worktree-{label}"),
                wt.to_str().expect("utf8 path"),
            ],
        );

        workspace.insert_live_worker(&project_key, fake_git_worker_entry(label, "worker-1"));
        (workspace, project_key, wt, repo, config)
    }

    /// A git worker with a clean worktree despawns AND removes the
    /// worktree AND reaps the `worktree-<label>` branch behind it.
    #[tokio::test]
    async fn despawn_git_worker_removes_clean_worktree() {
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer").await;
        assert!(wt.exists(), "worktree exists before despawn");
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let result = rx.await.expect("result");
        assert!(
            matches!(
                result,
                crate::protocol::DespawnResult::Despawned {
                    worktree_cleanup_warning: None,
                    branch_cleanup_warning: None,
                }
            ),
            "clean git worktree despawns + removes: {result:?}"
        );
        assert!(workspace.list_live_workers(&project_key).is_empty(), "worker removed");
        assert!(!wt.exists(), "clean worktree removed");
        assert!(
            !branch_exists(repo.path(), "worktree-reviewer"),
            "the worktree branch is reaped, not left behind"
        );
    }

    /// Every `Removed` event's worktree disposition, in order, so a
    /// caller can pin the count as well as the value.
    fn drain_removed_dispositions(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
    ) -> Vec<WorktreeDisposition> {
        let mut seen = Vec::new();
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::WorkerStatusChanged { action, worktree, .. } = update
                && action == WorkerStatusAction::Removed
            {
                seen.push(worktree);
            }
        }
        seen
    }

    /// The `x` button leaves the worktree on disk, so its event still
    /// reports it intact.
    #[tokio::test]
    async fn close_worker_reports_the_worktree_intact() {
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer").await;
        let mut rx = workspace.subscribe().expect("subscribe");

        handle_close_worker(&workspace, &project_key, "reviewer");

        assert_eq!(drain_removed_dispositions(&mut rx), vec![WorktreeDisposition::Intact]);
        assert!(wt.exists(), "the x button does not touch the worktree");
    }

    /// A despawn that removed the worktree says so on the event the
    /// close toast is built from.
    #[tokio::test]
    async fn despawn_reports_the_worktree_removed() {
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer").await;
        let mut rx = workspace.subscribe().expect("subscribe");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let _ = resp_rx.await.expect("result");

        assert_eq!(drain_removed_dispositions(&mut rx), vec![WorktreeDisposition::Removed]);
        assert!(!wt.exists(), "the worktree really is gone");
    }

    /// A despawn whose removal FAILED leaves the worktree behind, and the
    /// event separates that from a successful removal.
    #[tokio::test]
    async fn despawn_reports_a_failed_removal_apart_from_a_successful_one() {
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer").await;
        // Deregister only. `git worktree remove` would also delete the
        // directory, which is the case that reads as Removed.
        std::fs::remove_dir_all(repo.path().join(".git").join("worktrees").join("reviewer"))
            .expect("deregister the worktree");
        let mut rx = workspace.subscribe().expect("subscribe");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", true, tx);
        let result = resp_rx.await.expect("result");

        assert!(
            matches!(
                result,
                crate::protocol::DespawnResult::Despawned { worktree_cleanup_warning: Some(_), .. }
            ),
            "the removal failed: {result:?}"
        );
        assert!(wt.exists(), "the worktree genuinely lingers - that is what the toast claims");
        assert_eq!(drain_removed_dispositions(&mut rx), vec![WorktreeDisposition::RemovalFailed]);
    }

    /// A worktree already gone from disk is Removed, whatever git's exit
    /// code says - the toast's only claim is about the directory.
    #[tokio::test]
    async fn despawn_reports_removed_when_the_worktree_is_already_off_disk() {
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer").await;
        run_git(repo.path(), &["worktree", "remove", wt.to_str().expect("utf8 path")]);
        assert!(!wt.exists(), "nothing on disk before the despawn");
        let mut rx = workspace.subscribe().expect("subscribe");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", true, tx);
        let result = resp_rx.await.expect("result");

        assert!(
            matches!(
                result,
                crate::protocol::DespawnResult::Despawned { worktree_cleanup_warning: Some(_), .. }
            ),
            "git still reports its own failure to the caller: {result:?}"
        );
        assert_eq!(drain_removed_dispositions(&mut rx), vec![WorktreeDisposition::Removed]);
    }

    /// The sync spawn rollback runs before claude was ever launched, so
    /// `--worktree <label>` never reached a process and there is no
    /// worktree for the toast to point at. The `ghost` project is
    /// advertised by the `list_projects` overlay but absent from
    /// `config.projects`, which is what fails the agent-handle lookup
    /// without spawning a subprocess.
    #[tokio::test]
    async fn spawn_rollback_reports_no_worktree_to_preserve() {
        let config = tempdir().expect("config tempdir");
        write_forge_toml(config.path());
        let workspace = Arc::new(
            Workspace::new_for_test(config.path().to_owned()).await.expect("workspace new"),
        );

        let repo = tempdir().expect("repo tempdir");
        run_git(repo.path(), &["init", "-q"]);
        workspace.seed_test_project("ghost", &repo.path().to_string_lossy());
        let project = workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "ghost")
            .expect("seeded project present")
            .key;
        let mut rx = workspace.subscribe().expect("subscribe");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project.clone(),
            "reviewer",
            "charter".to_owned(),
            "lead-uuid".to_owned(),
            None,
            None,
            false,
            tx,
        );
        let err = resp_rx.await.expect("reply channel").expect_err("the agent spawn fails");
        // Failing earlier - at the project guard, before any insert -
        // would satisfy the emptiness check below without the rollback
        // ever running.
        assert!(
            err.contains("agent spawn failed"),
            "the rollback path must be the one taken: {err}"
        );

        assert!(workspace.list_live_workers(&project).is_empty(), "the entry rolled back");
        assert_eq!(
            drain_removed_dispositions(&mut rx),
            vec![WorktreeDisposition::Absent],
            "nothing was created, so the toast must not name a worktree",
        );
    }

    /// The despawn keeps a `worktree-<label>` branch the worker committed
    /// to, and names it in a warning rather than discarding the commits.
    #[tokio::test]
    async fn despawn_keeps_a_worktree_branch_holding_unique_commits() {
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer").await;
        std::fs::write(wt.join("work.txt"), "a worker committed here").expect("write work");
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "real work"]);

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let result = rx.await.expect("result");
        let crate::protocol::DespawnResult::Despawned {
            worktree_cleanup_warning: None,
            branch_cleanup_warning: Some(warning),
        } = result
        else {
            panic!("despawn succeeds with a branch warning, got {result:?}");
        };
        assert!(warning.contains("worktree-reviewer"), "warning names the branch: {warning}");
        assert!(
            warning.contains("1 commit reachable"),
            "warning names the commit count, not just a digit from the sha: {warning}"
        );
        assert!(warning.contains("git log"), "warning is executable: {warning}");
        assert!(!wt.exists(), "the worktree is still removed - the request succeeded");
        assert!(
            branch_exists(repo.path(), "worktree-reviewer"),
            "the branch with unique commits survives"
        );
    }

    /// The two quiet arms: nothing to reap says nothing, an unreadable repo
    /// warns rather than passing for a clean despawn.
    #[test]
    fn reap_worker_branch_warns_only_when_it_cannot_verify() {
        let not_a_repo = tempdir().expect("tempdir");
        let warning =
            reap_worker_branch(not_a_repo.path(), "reviewer").expect("an unreadable repo warns");
        assert!(warning.contains("could not verify"), "names the real cause: {warning}");

        let repo = tempdir().expect("repo tempdir");
        run_git(repo.path(), &["init", "-q"]);
        assert!(
            reap_worker_branch(repo.path(), "reviewer").is_none(),
            "no such branch is silent, not a spurious warning on every despawn",
        );
    }

    /// The reap targets `worktree-<label>` by convention, never whatever the
    /// worktree has checked out - a worker that made its own branch has real
    /// work on that one.
    #[tokio::test]
    async fn despawn_reaps_the_conventional_branch_not_the_checked_out_one() {
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer").await;
        run_git(&wt, &["checkout", "-q", "-b", "feature-x"]);
        std::fs::write(wt.join("work.txt"), "the worker's real work").expect("write work");
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "real work"]);

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let result = rx.await.expect("result");
        assert!(
            matches!(
                result,
                crate::protocol::DespawnResult::Despawned {
                    worktree_cleanup_warning: None,
                    branch_cleanup_warning: None,
                }
            ),
            "the conventional branch is pristine, so nothing is kept: {result:?}"
        );
        assert!(
            !branch_exists(repo.path(), "worktree-reviewer"),
            "the conventional branch is reaped"
        );
        assert!(branch_exists(repo.path(), "feature-x"), "the worker's own branch survives");
    }

    fn review_thread(id: &str) -> forge_primitives::review::ReviewThread {
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };
        ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "src/x.rs".to_owned(),
                side: ReviewSide::New,
                line: 1,
                content_hash: 1,
                context: Vec::new(),
                base_ref: "main".to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: "note".to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: "t".to_owned(),
            updated_at: "t".to_owned(),
            commit: None,
        }
    }

    /// A worker's own feature branch outlives its worktree and is usually
    /// the open PR, so the review state keyed on it must survive the
    /// despawn.
    #[tokio::test]
    async fn despawn_keeps_review_state_for_a_branch_that_still_exists() {
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer").await;
        run_git(&wt, &["checkout", "-q", "-b", "feature-x"]);
        std::fs::write(wt.join("work.txt"), "the worker's real work").expect("write work");
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "real work"]);
        workspace.save_review_threads("forge", "feature-x", &[review_thread("a")]);
        workspace
            .submit_review("forge", "feature-x", None, &[], SessionKey::from_session_id("reviewer"))
            .expect("seal a review on the feature branch");

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let _ = rx.await.expect("result");

        assert!(branch_exists(repo.path(), "feature-x"), "the branch the threads key on is live");
        assert_eq!(
            workspace.load_review_threads("forge", "feature-x").expect("load").len(),
            1,
            "a live branch keeps its review threads",
        );
        assert_eq!(
            workspace.load_reviews("forge", "feature-x").expect("load").len(),
            1,
            "a live branch keeps its submitted reviews",
        );
    }

    /// A pushed branch is reaped locally because the remote holds its
    /// commits, so the review state has to key off the remote-tracking ref
    /// rather than the local head that the reap just removed.
    #[tokio::test]
    async fn despawn_keeps_review_state_for_a_branch_pushed_to_a_remote() {
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer").await;
        let remote = tempdir().expect("remote tempdir");
        run_git(remote.path(), &["init", "-q", "--bare"]);
        run_git(&wt, &["remote", "add", "origin", remote.path().to_str().expect("utf8 path")]);
        std::fs::write(wt.join("work.txt"), "pushed work").expect("write work");
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "real work"]);
        run_git(&wt, &["push", "-q", "-u", "origin", "worktree-reviewer"]);
        workspace.save_review_threads("forge", "worktree-reviewer", &[review_thread("a")]);

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let _ = rx.await.expect("result");

        assert!(
            !branch_exists(repo.path(), "worktree-reviewer"),
            "the local head is reaped - without that this passes for the wrong reason",
        );
        assert_eq!(
            workspace.load_review_threads("forge", "worktree-reviewer").expect("load").len(),
            1,
            "a branch still on the remote keeps its review threads",
        );
    }

    /// Despawning a git worker deletes both the review threads AND the
    /// submitted reviews for the torn-down worktree's branch once that
    /// branch is gone, leaving other branches' rows intact (else a reused
    /// branch inherits phantoms).
    #[tokio::test]
    async fn despawn_deletes_that_branch_reviews_and_threads() {
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer").await;
        let branch = forge_agent::env::worktree::worktree_branch(&wt).expect("worktree branch");
        let thread = review_thread;
        workspace.save_review_threads("forge", &branch, &[thread("a")]);
        workspace.save_review_threads("forge", "survivor", &[thread("b")]);
        // Seal a review on each branch (empty thread set just mints the row).
        let origin = SessionKey::from_session_id("reviewer");
        workspace
            .submit_review("forge", &branch, None, &[], origin.clone())
            .expect("seal torn-down review");
        workspace
            .submit_review("forge", "survivor", None, &[], origin)
            .expect("seal survivor review");

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let _ = rx.await.expect("result");

        assert!(!branch_exists(repo.path(), &branch), "the branch the threads key on is gone");
        assert!(
            workspace.load_review_threads("forge", &branch).expect("load").is_empty(),
            "the torn-down branch's threads are cleaned",
        );
        assert!(
            workspace.load_reviews("forge", &branch).expect("load").is_empty(),
            "the torn-down branch's reviews are cleaned",
        );
        assert_eq!(
            workspace.load_review_threads("forge", "survivor").expect("load").len(),
            1,
            "another branch's threads survive",
        );
        assert_eq!(
            workspace.load_reviews("forge", "survivor").expect("load").len(),
            1,
            "another branch's reviews survive",
        );
    }

    /// A detached-HEAD worktree has no resolvable branch, so despawn skips
    /// review-thread cleanup gracefully (warns, no panic) and leaves
    /// unrelated branches' threads intact.
    #[tokio::test]
    async fn despawn_with_detached_head_skips_cleanup_gracefully() {
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer").await;
        run_git(&wt, &["checkout", "--detach"]);
        workspace.save_review_threads("forge", "survivor", &[review_thread("a")]);

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let _ = rx.await.expect("result");

        assert_eq!(
            workspace.load_review_threads("forge", "survivor").expect("load").len(),
            1,
            "cleanup skipped gracefully, unrelated threads intact",
        );
    }

    /// A dirty worktree without `force` BLOCKS the despawn: nothing is
    /// torn down, the worker stays live, the worktree is intact.
    #[tokio::test]
    async fn despawn_dirty_worktree_without_force_blocks() {
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer").await;
        std::fs::write(wt.join("scratch.txt"), "uncommitted").expect("write scratch");
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let result = rx.await.expect("result");
        assert!(
            matches!(result, crate::protocol::DespawnResult::Blocked { .. }),
            "dirty worktree blocks without force: {result:?}"
        );
        assert_eq!(
            workspace.list_live_workers(&project_key).len(),
            1,
            "worker stays live when blocked"
        );
        assert!(wt.exists(), "worktree intact when blocked");
    }

    /// A dirty worktree WITH `force` tears down + discards the worktree.
    #[tokio::test]
    async fn despawn_dirty_worktree_force_discards() {
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer").await;
        std::fs::write(wt.join("scratch.txt"), "uncommitted").expect("write scratch");
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", true, tx);
        let result = rx.await.expect("result");
        assert!(
            matches!(result, crate::protocol::DespawnResult::Despawned { .. }),
            "force despawns a dirty worktree: {result:?}"
        );
        assert!(workspace.list_live_workers(&project_key).is_empty(), "worker removed under force");
        assert!(!wt.exists(), "worktree discarded under force");
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

    /// A worker addressed before it Connects (session_id still None)
    /// must have the prompt buffered onto its DomainSession, not
    /// dropped - its Connected handler drains pending_peer_prompts.
    #[tokio::test]
    async fn deliver_to_unconnected_worker_buffers_the_prompt() {
        let (workspace, _rx) = Workspace::testing_stub();
        let project = ProjectKey::new("forge");
        let worker_key = SessionKey::from_session_id("__spawn_worker_forge_builder_abc__");
        workspace.insert_live_worker(
            &project,
            crate::mcp::workers::types::WorkerEntry {
                label: "builder".into(),
                charter: "c".into(),
                session_key: worker_key.clone(),
                status: forge_primitives::WorkerLiveness::Spawning,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead".into(),
                needs_tag: false,
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
            },
        );
        // Pre-Connect DomainSession: session_id is None.
        let domain = std::sync::Arc::new(parking_lot::Mutex::new(
            crate::domain_session::DomainSession::new(worker_key.clone(), None),
        ));
        workspace.domain_handles.lock().insert(worker_key.clone(), domain.clone());

        handle_deliver_worker_prompt(
            &workspace,
            SessionKey::from_str_for_test("lead"),
            &project,
            "builder",
            fixture_wrapped(),
        );

        assert_eq!(
            domain.lock().pending_peer_prompts.len(),
            1,
            "prompt buffered for the worker's Connected drain, not dropped"
        );
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
        let a = synth_worker_key(&project, "reviewer", false);
        let b = synth_worker_key(&project, "reviewer", false);
        assert_ne!(
            a.as_str(),
            b.as_str(),
            "two same-label spawns must produce different synth keys"
        );
        assert!(a.as_str().starts_with("__spawn_worker_forge_reviewer_"));
        assert!(b.as_str().starts_with("__spawn_worker_forge_reviewer_"));
        // Resume path uses the distinct prefix.
        let r = synth_worker_key(&project, "reviewer", true);
        assert!(r.as_str().starts_with("__resume_worker_forge_reviewer_"));
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
            forge_toml_path(toml_dir.path()),
            format!(
                r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "{project_path}"
auto_start = true

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
provider = "anthropic"
"#,
            ),
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(Workspace::new_for_test(toml_dir.path().to_owned()).await.expect("new"));

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
        // A Running worker has completed its Connected handshake, so
        // stamp session_id - otherwise the pre-Connect buffer guard in
        // handle_deliver_worker_prompt would park the prompt and the
        // tag retry (asserted below) would never fire.
        if let Some(domain) = workspace.domain_session_for(&session_key) {
            domain.lock().session_id = Some(forge_primitives::SessionId::new(&session_id));
        }

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
                is_git_repo_at_spawn: false,
                diagnostic: None,
                kick: None,
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

    /// `build_worker_extra_args` returns `[("worktree", Some(label))]`
    /// when the project path is a git repo so the worker spawn picks
    /// up `--worktree=<label>` and lands inside an auto-created
    /// `<repo>/.claude/worktrees/<label>/`. The helper takes a
    /// pre-computed `is_git_repo` boolean (caller probes the path
    /// once and reuses the result for both the WorkerEntry flag and
    /// this arg list); the test seeds the bool via the same
    /// `forge_agent::env::worktree::is_git_repo` probe the spawn
    /// path uses so the two layers stay in sync.
    #[test]
    fn worker_in_git_repo_gets_worktree_flag() {
        let dir = tempdir().expect("tempdir");
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        let is_git = forge_agent::env::worktree::is_git_repo(dir.path());
        assert!(is_git, "freshly-initialised tempdir must register as git repo");
        let args = build_worker_extra_args(is_git, "reviewer", false);
        assert!(
            args.iter()
                .any(|(flag, value)| flag == "worktree" && value.as_deref() == Some("reviewer")),
            "expected (\"worktree\", Some(\"reviewer\")) in {args:?}"
        );
    }

    /// `build_worker_extra_args` returns no `worktree` entry when the
    /// project path isn't a git repo - the worker just spawns into
    /// the project's plain cwd and skips the worktree path entirely.
    #[test]
    fn worker_in_non_git_repo_gets_no_worktree_flag() {
        let dir = tempdir().expect("tempdir"); // empty, not a repo
        let is_git = forge_agent::env::worktree::is_git_repo(dir.path());
        assert!(!is_git, "empty tempdir must not register as git repo");
        let args = build_worker_extra_args(is_git, "reviewer", false);
        assert!(
            !args.iter().any(|(flag, _)| flag == "worktree"),
            "expected no worktree entry in {args:?}"
        );
    }

    /// Workers are pinned to their worktree (when they have one) and
    /// must not be able to call claude's built-in `EnterWorktree` /
    /// `ExitWorktree` tools to hop elsewhere. `build_worker_extra_args`
    /// emits a `--disallowedTools` flag carrying both tool names as a
    /// single comma-separated value (empirically confirmed to be
    /// accepted by the claude CLI's variadic `<tools...>` parser).
    /// This test covers the git-repo case: the `worktree` flag is
    /// still present, and the `disallowedTools` flag is added on top.
    #[test]
    fn worker_in_git_repo_blocks_enter_and_exit_worktree() {
        let is_git = true;
        let args = build_worker_extra_args(is_git, "reviewer", false);
        let blocked = args.iter().find(|(flag, _)| flag == "disallowedTools");
        let (_, value) = blocked.expect("expected --disallowedTools entry");
        let value = value.as_deref().expect("expected value for --disallowedTools");
        assert!(value.contains("EnterWorktree"), "EnterWorktree must be blocked, got {value:?}");
        assert!(value.contains("ExitWorktree"), "ExitWorktree must be blocked, got {value:?}");
    }

    /// The non-git-repo case still blocks the worktree-hop tools even
    /// though the worker isn't running inside a worktree - the tool
    /// surface is uniform across project shapes so workers can't be
    /// nudged into surprising behaviour by the project layout.
    #[test]
    fn worker_in_non_git_repo_also_blocks_worktree_tools() {
        let is_git = false;
        let args = build_worker_extra_args(is_git, "reviewer", false);
        let blocked = args.iter().find(|(flag, _)| flag == "disallowedTools");
        let (_, value) = blocked.expect("expected --disallowedTools entry even outside git-repo");
        let value = value.as_deref().expect("expected value for --disallowedTools");
        assert!(value.contains("EnterWorktree"));
        assert!(value.contains("ExitWorktree"));
    }

    /// An answer to a worker's `AskUserQuestion` is indistinguishable
    /// from a decision the user made, and reaches it only if someone
    /// happens to be looking at that worker's row. The default is that
    /// the tool is never offered.
    #[test]
    fn worker_spawned_without_interactive_is_denied_ask_user_question() {
        let args = build_worker_extra_args(true, "reviewer", false);
        let blocked = args.iter().find(|(flag, _)| flag == "disallowedTools");
        let (_, value) = blocked.expect("expected --disallowedTools entry");
        let value = value.as_deref().expect("expected value for --disallowedTools");
        assert!(
            value.contains("AskUserQuestion"),
            "a worker spawned without interactive must not be offered AskUserQuestion, got {value:?}",
        );
    }

    /// The opt-in keeps the tool, and changes nothing else: a worker
    /// spawned to be talked to directly is one whose row the user has
    /// open, but it is still pinned to its spawn location.
    #[test]
    fn worker_spawned_interactive_keeps_ask_user_question() {
        let args = build_worker_extra_args(true, "reviewer", true);
        let blocked = args.iter().find(|(flag, _)| flag == "disallowedTools");
        let (_, value) = blocked.expect("expected --disallowedTools entry");
        let value = value.as_deref().expect("expected value for --disallowedTools");
        assert!(
            !value.contains("AskUserQuestion"),
            "an interactive worker must keep AskUserQuestion, got {value:?}",
        );
        assert!(
            value.contains("EnterWorktree") && value.contains("ExitWorktree"),
            "opting into interactive must not lift the worktree-hop denial, got {value:?}",
        );
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
        use crate::mcp::peers::types::{AskChannel, CorrelationId, InflightAsk};
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
                channel: AskChannel::Workers,
                caller: SessionKey::from_session_id("lead-uuid"),
                target_project: composite,
                target_session: None,
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

#[cfg(test)]
mod lead_charter_tests {
    use super::*;

    /// A lead with no charter set gets the bundled one.
    #[test]
    fn lead_without_a_charter_gets_the_bundled_one() {
        let mut settings = SessionLaunchSettings::default();
        apply_lead_charter(&mut settings);
        assert_eq!(settings.charter.as_deref(), Some(DEFAULT_LEAD_CHARTER));
    }

    /// An already-set charter (a worker spawn's inline persona) is
    /// never overwritten - the guard short-circuits first.
    #[test]
    fn existing_charter_is_preserved_not_overwritten() {
        let mut settings = SessionLaunchSettings {
            charter: Some("pre-existing".into()),
            ..SessionLaunchSettings::default()
        };
        apply_lead_charter(&mut settings);
        assert_eq!(settings.charter.as_deref(), Some("pre-existing"));
    }

    /// The bundled charter ships to every install, so it must not name
    /// tooling or projects that only exist in one author's environment:
    /// a fresh install has no user-scope skills, no plugins and no
    /// justfile, and `team` is not a `forge.toml` key. Most entries got
    /// here by being copied from an on-disk
    /// charter; the two path entries are pre-emptive, since prose about
    /// where a charter lives is the obvious place to write one.
    ///
    /// Every assertion below is a `!contains`, so all of them hold
    /// against an empty string - the first check is what makes the rest
    /// mean anything if the `include_str!` ever resolves somewhere else.
    #[test]
    fn bundled_lead_charter_assumes_no_local_environment() {
        assert!(
            DEFAULT_LEAD_CHARTER.contains("workers__spawn"),
            "the compiled-in charter is the real one, not an empty or wrong file",
        );
        for (token, why) in [
            ("pr-review-loop", "user-scope skill, absent on a fresh install"),
            ("superpowers", "plugin, absent on a fresh install"),
            ("commit-commands", "plugin, absent on a fresh install"),
            ("`just ", "project justfile, not every project has one"),
            ("hub-modules", "one user's project name"),
            ("team = ", "not a forge.toml key"),
            ("static_workers", "no longer a forge.toml key either"),
            ("~/.claude", "the charter must not pin a path in the user's home"),
            ("forge-team", "the charter must not name the deleted role filesystem"),
        ] {
            assert!(
                !DEFAULT_LEAD_CHARTER.contains(token),
                "bundled lead charter names '{token}' ({why})"
            );
        }
    }

    /// The token test above checks absence only; these pin the decided
    /// despawn wording against silent softening in either direction.
    #[test]
    fn lead_charter_carries_the_despawn_trigger() {
        assert!(
            DEFAULT_LEAD_CHARTER.contains("despawn at the chain node"),
            "the chain-node despawn rule stays: {DEFAULT_LEAD_CHARTER}",
        );
        assert!(
            DEFAULT_LEAD_CHARTER.contains("NOT when the last downstream PR closes"),
            "the despawn point is the chain node, not the last PR: {DEFAULT_LEAD_CHARTER}",
        );
        assert!(
            DEFAULT_LEAD_CHARTER.contains("re-armed by the next stage's kick"),
            "idle chain-stage workers re-arm on the next kick: {DEFAULT_LEAD_CHARTER}",
        );
    }
}

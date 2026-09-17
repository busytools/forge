//! App-level spawn command handlers. Called from
//! [`crate::Workspace::dispatch`] when an App-level
//! `Command::SpawnProject` / `SpawnSession` / `StartDefault` arrives.
//! Each handler resolves the id the session will run under, emits
//! `SessionUpdate::Spawning` under it, and kicks off the agent spawn.
//! The matching `Connected` arrives later under the same key.

use std::sync::Arc;

use forge_agent::client::SessionLaunchSettings;

use crate::mcp::gotify::types::GotifyNotification;
use crate::mcp::peers::facade::PeerStatsDelta;
use crate::mcp::peers::types::WrappedPrompt;
use crate::protocol::{
    Command, SessionUpdate, WorkerSpawnReply, WorkerStatusAction, WorktreeDisposition,
};
use crate::target::ProjectKey;
use crate::workspace::LiveWorkerRefusal;
use crate::workspace::Workspace;
use crate::{SessionSlot, SessionTarget};
use std::fmt::Write as _;

use forge_primitives::slack::SlackMessage;

/// A failed prompt dispatch to a running target unwinds the echo's
/// already-opened turn: without this its bar counts forever and the
/// bucket spinner never drops (mirrors the typed-submit
/// compensation). Per-site warns stay at the call sites.
pub(crate) fn send_dispatch_turn_error(
    workspace: &Workspace,
    key: SessionSlot,
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

/// Stamp the project's `permission_mode` into the launch settings'
/// `permissions.defaultMode`, where the existing
/// `applied_permission_mode` arm in `forge_sdk_worker` picks it up;
/// overrides a launcher-supplied default.
pub(crate) fn stamp_permission_mode(
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

/// Resolve the id the project's lead will run under, emit
/// `SessionUpdate::Spawning` under it, then spawn the agent. The
/// `Connected` event from the resulting `SessionTask` lands under the
/// same key, so the announced bucket is the one the child connects
/// under.
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

    // The id this lead will run under, resolved before the spawn so the
    // bucket announced here is the bucket the child connects under.
    let session_key = match workspace.resolve_slot(&SessionTarget::Named(project_name.to_owned())) {
        Ok(key) => key,
        Err(err) => {
            tracing::warn!(
                target: "forge_workspace::spawn",
                project = project_name,
                error = %err,
                "spawn_project: target resolution failed",
            );
            // No spawn will happen, so record what this project's lead
            // has parked rather than leaving it for a connect that never
            // comes.
            workspace.expire_parked_for_slot(
                &crate::SessionSlot::lead(&project.org, &project.name),
                crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
            );
            try_emit(
                workspace,
                "spawn_project::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key: SessionSlot::lead(&project.org, &project.name),
                    message: format!("agent spawn failed: {err}"),
                    fatal: false,
                },
            );
            return;
        }
    };
    try_emit(
        workspace,
        "spawn_project::Spawning",
        SessionUpdate::Spawning {
            key: session_key.clone(),
            project_name: project_name.to_owned(),
            cwd: project.path.to_string_lossy().to_string(),
            display_name: project.display_path.clone(),
        },
    );

    match workspace.get_agent_handle_at_key(
        SessionTarget::Named(project_name.to_owned()),
        launch_settings,
        Some(session_key.clone()),
        &crate::protocol::SpawnRole::Lead,
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                project = project_name,
                slot = %session_key.display(),
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
            // record everything parked for this project's lead here -
            // otherwise the caller's LLM waits on a spawn that never
            // happened, and a committed delivery is lost unannounced.
            workspace.expire_parked_for_slot(
                &crate::SessionSlot::lead(&project.org, &project.name),
                crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
            );
            try_emit(
                workspace,
                "spawn_project::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key: session_key,
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
    _caller: SessionSlot,
    target_project: String,
    wrapped: WrappedPrompt,
) {
    // Find the target project's running lead session (if any). The
    // `list_projects()` snapshot has `sessions: Vec<SessionView>` per
    // project; `is_open == true` on a session means an Agent is in
    // the workspace pool - that's "running." MUST skip worker
    // sessions: once a worker connects it lands in `view.sessions`
    // too, and a worker can sit at position 0 / be the first
    // is_open session. Returning a worker here dispatches the peer
    // envelope to the worker's chat instead of the lead's, which is
    // wrong (peers address project leads, not workers). Asking for the
    // lead slot by name cannot reach a worker: the label is part of the
    // slot, so the two are distinct keys rather than candidates to
    // subtract.
    let target_running_key =
        workspace.list_projects().into_iter().find(|v| v.name == target_project).and_then(|v| {
            let slot = SessionSlot::lead(&v.org, &v.name);
            workspace.session_is_pooled(&slot).then_some(slot)
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
    // exists in forge.toml, park the envelope for its lead and dispatch
    // SpawnProject.
    let Some(target) = workspace.find_project_view_by_name(&target_project) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            target_project = %target_project,
            "DeliverPeerPrompt target not in forge.toml; dropping"
        );
        return;
    };

    // The lead's own first `Connected` drains the bucket, so the park
    // must land BEFORE the spawn.
    workspace.park_peer_prompt(&crate::SessionSlot::lead(&target.org, &target.name), wrapped);

    // Dispatch SpawnProject. Move target_project
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
    /// The owner still has a row, but the boot wave would skip it, so
    /// nothing can drain a prompt buffered for it and the prompt cannot
    /// reach anyone. `directory` is where its session would have started,
    /// for the warning. Distinct from [`Self::TargetGone`] because the
    /// row is kept: a restored worktree brings the owner back.
    ///
    /// The caller's fate for the entry splits on the kind. A recurring
    /// cron drops this fire and advances - leaving it due would re-fire
    /// every tick, and parking it would grow a bucket nothing drains. A
    /// one-shot stays due instead, because advancing removes a one-shot:
    /// dropping it would throw the prompt away with no later slot.
    TargetCannotBeWoken { directory: std::path::PathBuf },
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

    if let Some(target_key) = live_cron_slot(workspace, &view, team_role) {
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
    match cron_slot_exists(workspace, &view, team_role) {
        CronOwnerCheck::Exists => {}
        CronOwnerCheck::CannotBeWoken { directory } => {
            return CronFireOutcome::TargetCannotBeWoken { directory };
        }
        CronOwnerCheck::Absent => return CronFireOutcome::TargetGone,
        CronOwnerCheck::Unknown => return CronFireOutcome::DispatchFailed,
    }
    // A spawn needs the account map settled: the walk skips every account
    // still `Loading`, so a project whose only account for its model has
    // not settled is refused at the walk, and that refusal is transient
    // where a dead target is not. Deferring the fire is a delay rather
    // than a drop, because the next tick retries once the map settles;
    // parking now would have the spawn refusal expire the prompt instead.
    //
    // This is NOT a boot gate, and do not simplify it into one. The two
    // halves are different kinds of thing: `gateway_ready` is bind-time
    // only and monotonic, while `all_loaded` is defined over the account
    // map, so it goes false again whenever anything re-enters `Loading`.
    // A fire deferred by the second half is a delay for the same reason
    // as the first, not a drop.
    if !workspace.all_accounts_loaded() {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project_name,
            "cron fire deferred: the account map has not settled, so a spawn would be refused \
             and its parked prompt expired",
        );
        return CronFireOutcome::DispatchFailed;
    }
    // A cooldown empties the walk until its reset, which is transient the
    // same way an unsettled map is, and a refusal there expires the park
    // too. A walk empty because no account declares the model is the other
    // case, and that one is permanent - deferring it would retry a broken
    // cron forever, so it is left to fail as before.
    if workspace.project_walk_is_cooling(&view.key) {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project_name,
            "cron fire deferred: every account serving this project's model is cooling, so the \
             wake would be refused and its parked prompt expired",
        );
        return CronFireOutcome::DispatchFailed;
    }
    // Buffer by owner, then wake via resume: SpawnProject resumes the lead,
    // whose reconnect re-spawns the persisted workers; each drains its own
    // bucket on connect.
    let slot = crate::SessionSlot::for_label(&view.org, &view.name, team_role);
    workspace.park_cron(&slot, prompt, missed);
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

/// The live session that fills the cron's slot in `view`: for a lead
/// cron the running lead (the first open session that is not a live
/// worker); for a worker cron the live worker with that label. `None`
/// when no session holds the slot.
fn live_cron_slot(
    workspace: &Arc<Workspace>,
    view: &crate::views::ProjectView,
    team_role: Option<&str>,
) -> Option<SessionSlot> {
    let live = workspace.list_live_workers(&view.key);
    let candidate = if let Some(label) = team_role {
        live.into_iter().find(|w| w.label == label).map(|w| w.slot)
    } else {
        let slot = SessionSlot::lead(&view.org, &view.name);
        workspace.session_is_pooled(&slot).then_some(slot)
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
    /// The owner has a row, but the boot wave would skip it - a resume
    /// would have no directory to start in - so there is nothing to wake
    /// and nothing that would drain a parked prompt. Carries that
    /// directory for the fire's warning.
    CannotBeWoken { directory: std::path::PathBuf },
    /// Conclusively gone: the read succeeded and the label has no row in
    /// the session store.
    Absent,
    /// The durable-worker lookup could not read, so absence is unconfirmed.
    Unknown,
}

/// Whether a cron's slot still has a session that can be woken. A lead's
/// does whenever its project does; a worker's does while its label has a
/// row AND that row can still start, since the boot wave is what
/// re-spawns it. A read failure yields [`CronOwnerCheck::Unknown`] so the
/// fire router leaves the cron rather than deleting a live slot's cron on
/// a transient hiccup.
fn cron_slot_exists(
    workspace: &Arc<Workspace>,
    view: &crate::views::ProjectView,
    team_role: Option<&str>,
) -> CronOwnerCheck {
    let Some(label) = team_role else {
        return CronOwnerCheck::Exists;
    };
    // A worker spawned this second is live before it is connected, and
    // `live_cron_slot` cannot address it until it stamps a session id on
    // connect. Its directory is being created along with it, so it is not
    // unwakeable: the fire parks and the worker's own Connected drains it.
    //
    // `live_worker_with_label`, not a bare label match: a `Failed` entry
    // is kept by design and is NOT live, so counting one would answer
    // "wakeable" for a worker the wave will never start - parking the fire
    // in the bucket this path exists to keep empty, with nothing left to
    // drain it.
    if crate::mcp::workers::types::live_worker_with_label(
        &workspace.list_live_workers(&view.key),
        label,
    )
    .is_some()
    {
        return CronOwnerCheck::Exists;
    }
    match workspace.stored_worker_row(&view.key, label) {
        Ok(None) => CronOwnerCheck::Absent,
        Ok(Some(row)) => {
            let directory = crate::mcp::workers::types::worker_tag_dir(
                &view.path,
                &row.label,
                matches!(row.is_git_repo, Some(true)),
            );
            let can_start = crate::mcp::workers::types::worker_row_can_start(
                &view.path,
                &row.label,
                row.is_git_repo,
                row.session_id.is_some(),
            );
            if can_start {
                CronOwnerCheck::Exists
            } else {
                CronOwnerCheck::CannotBeWoken { directory }
            }
        }
        Err(_) => CronOwnerCheck::Unknown,
    }
}

/// Deliver a matched Gotify `notification` into `project` as a plain user
/// turn AND echo it into the target's chat as a notification block. When
/// `team_role` names a running team worker, deliver straight to it;
/// otherwise deliver to the project lead - echo + dispatch a
/// `Command::Prompt` if it's running, else park it for the target's slot
/// and dispatch `Command::SpawnProject`
/// (`SessionTask` drains + echoes on Connected via
/// `deliver_parked_gotify`). A team-worker subscription with NO live entry
/// falls through to lead delivery (spawning the project brings the team
/// up); a live-but-not-yet-connected worker parks for its own label
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
        } else {
            // Still spawning: park it for the worker's slot, drained by
            // its own first `Connected`.
            workspace.park_gotify(&worker_key, notification);
        }
        return;
    }

    let running_lead =
        workspace.list_projects().into_iter().find(|v| v.name == project).and_then(|v| {
            let slot = SessionSlot::lead(&v.org, &v.name);
            workspace.session_is_pooled(&slot).then_some(slot)
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

    // Asleep: park it for the project's lead and spawn the project (only
    // if it's a real forge.toml project).
    let Some(view) = workspace.find_project_view_by_name(project) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project,
            "gotify delivery target gone from forge.toml; skipping",
        );
        return;
    };

    workspace.park_gotify(&crate::SessionSlot::lead(&view.org, &view.name), notification);

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

/// The user-turn prose for a delivered Slack message. It carries the ids a
/// reply needs - conversation, ts, thread - because the only way an agent
/// can answer in place is to feed those back to `slack__post` or
/// `slack__edit`. The session chat parses this shape back into a Slack block
/// (`forge_tui::ui::peer_block`), so the bracketed header and the
/// `<author>: ` line are a contract with it.
pub(crate) fn slack_message_to_prose(message: &SlackMessage) -> String {
    let author = message.user.as_deref().unwrap_or("unknown");
    let mut out = format!(
        "[Slack - workspace '{}', {}] id {} ts {}{}\n{}: {}",
        message.workspace,
        message.conversation_label,
        message.conversation,
        message.ts,
        message
            .thread_ts
            .as_deref()
            .map(|thread| format!(" in thread {thread}"))
            .unwrap_or_default(),
        author,
        message.text,
    );
    for file in &message.files {
        let _ = writeln!(out, "\n[file {} {}]", file.id, file.name);
    }
    out
}

/// Deliver one matched Slack message to its subscriber's session: dispatch
/// it now when that session is running, buffer it on the session's own
/// domain when it is still spawning. Mirrors [`deliver_gotify_message`],
/// including its rule that a worker-owned subscription falls through to
/// the lead only when the worker is gone entirely (teardown removed its
/// subscriptions first, so this is the despawn race, not steady state) -
/// and that fall-through commits under the worker's own dedupe key, so a
/// durable worker that respawns later re-delivers what it missed.
///
/// Returns whether the message reached a destination it can be read from:
/// dispatched, or buffered for one that will. `false` tells the pump the
/// cursor must not advance past this message, so a sweep re-runs it -
/// which is why the dedupe entry commits only after a successful hand-off.
pub(crate) fn deliver_slack_message(
    workspace: &Arc<Workspace>,
    project: &str,
    team_role: Option<&str>,
    message: SlackMessage,
) -> bool {
    let prose = slack_message_to_prose(&message);
    // A sweep re-runs a batch after a 429, a failed watermark write or a
    // crash; the re-run must drop what was already handed over. "Already
    // delivered" is success for the caller: the cursor may advance.
    if workspace.slack_delivery_seen(project, team_role, &message) {
        tracing::debug!(
            target: "forge_workspace::spawn",
            project = %project,
            conversation = %message.conversation,
            ts = %message.ts,
            "slack message already delivered; dropping the re-run",
        );
        return true;
    }

    if let Some(role) = team_role
        && let Some(worker_key) = team_worker_key(workspace, project, role)
    {
        let connected = workspace
            .domain_session_for(&worker_key)
            .is_some_and(|d| d.lock().session_id.is_some());
        if connected {
            if let Err(err) = workspace.dispatch_workspace_prompt(&worker_key, prose.clone()) {
                tracing::warn!(
                    target: "forge_workspace::spawn",
                    project = %project,
                    role = %role,
                    error = ?err,
                    "slack deliver to running team worker failed",
                );
                send_dispatch_turn_error(workspace, worker_key, &err);
                return false;
            }
            // Echo only once the dispatch lands: a failed one returns false so
            // the sweep re-runs the message, and an echo pushed before it would
            // paint the block twice for a turn the LLM sees once.
            push_slack_message_into_chat(workspace, &worker_key, &prose);
            workspace.slack_delivery_commit(project, team_role, &message);
        } else {
            // Still spawning: commit the dedupe (the sweep must not
            // re-run it) and park it for the worker's slot, drained by
            // its own first `Connected`. The slot carries the worker's
            // own org and project, so a project dropped from forge.toml
            // since its spawn still keys correctly.
            workspace.slack_delivery_commit(project, team_role, &message);
            workspace.park_slack(&worker_key, message);
        }
        return true;
    }

    let running_lead =
        workspace.list_projects().into_iter().find(|v| v.name == project).and_then(|v| {
            let slot = SessionSlot::lead(&v.org, &v.name);
            workspace.session_is_pooled(&slot).then_some(slot)
        });

    if let Some(target_key) = running_lead {
        if let Err(err) = workspace.dispatch_workspace_prompt(&target_key, prose.clone()) {
            tracing::warn!(
                target: "forge_workspace::spawn",
                project = %project,
                error = ?err,
                "slack deliver to running project failed",
            );
            send_dispatch_turn_error(workspace, target_key, &err);
            return false;
        }
        // Echo after the dispatch lands, so the sweep's re-run of a failed
        // delivery does not paint a second block for the same message.
        push_slack_message_into_chat(workspace, &target_key, &prose);
        workspace.slack_delivery_commit(project, team_role, &message);
        return true;
    }

    // Asleep: park it for the project's lead and spawn the project (only
    // if it's a real forge.toml project). A target missing from
    // forge.toml returns false uncommitted: the sweep re-runs it, and
    // delivery resumes if the project returns or the subscription is
    // removed - the pump sees a decision, not a silent drop.
    let Some(view) = workspace.find_project_view_by_name(project) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project,
            "slack delivery target gone from forge.toml; leaving it for the next sweep",
        );
        return false;
    };

    workspace.slack_delivery_commit(project, team_role, &message);
    workspace.park_slack(&crate::SessionSlot::lead(&view.org, &view.name), message);

    if let Err(err) = workspace.dispatch(Command::SpawnProject {
        project_name: project.to_owned(),
        launch_settings: SessionLaunchSettings::default(),
    }) {
        tracing::warn!(
            target: "forge_workspace::spawn",
            project = %project,
            error = ?err,
            "slack fire SpawnProject dispatch failed",
        );
    }
    true
}

/// The team worker labelled `label` in `project`, if a live entry
/// exists - regardless of whether it has finished connecting. The
/// caller checks connectedness to decide dispatch-vs-buffer.
fn team_worker_key(workspace: &Arc<Workspace>, project: &str, label: &str) -> Option<SessionSlot> {
    let view = workspace.list_projects().into_iter().find(|v| v.name == project)?;
    workspace.list_live_workers(&view.key).into_iter().find(|w| w.label == label).map(|w| w.slot)
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
    target_key: &SessionSlot,
    wrapped: &WrappedPrompt,
) {
    let _ = workspace.update_sender().send(SessionUpdate::PeerEnvelopeAppended {
        key: target_key.clone(),
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
    target_key: &SessionSlot,
    notification: &GotifyNotification,
) {
    let _ = workspace.update_sender().send(SessionUpdate::GotifyNotificationAppended {
        key: target_key.clone(),
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
    target_key: &SessionSlot,
    text: &str,
) {
    let _ = workspace
        .update_sender()
        .send(SessionUpdate::CronPromptAppended { key: target_key.clone(), text: text.to_owned() });
}

/// Emit a typed `SlackMessageAppended` so the target session's TUI chat buffer
/// shows the inbound Slack block. Mirrors
/// [`push_gotify_notification_into_chat`] - the target's `claude` subprocess
/// still receives the prose via a separate `Command::Prompt` dispatch; this
/// only drives the visible chat echo.
pub(crate) fn push_slack_message_into_chat(
    workspace: &Workspace,
    target_key: &SessionSlot,
    prose: &str,
) {
    let _ = workspace.update_sender().send(SessionUpdate::SlackMessageAppended {
        key: target_key.clone(),
        prose: prose.to_owned(),
    });
}

/// Spawn for a non-lead session row. The slot names the session and the
/// store row under it holds the id to resume, so this resumes via
/// `SessionTarget::Session`.
pub(crate) fn handle_spawn_session(
    workspace: &Arc<Workspace>,
    slot: &SessionSlot,
    role: &crate::protocol::SpawnRole,
    launch_settings: SessionLaunchSettings,
) {
    let Some(parent) = workspace.project_for_slot(slot) else {
        tracing::warn!(
            target: "forge_workspace::spawn",
            slot = %slot.display(),
            "Command::SpawnSession for a slot no project declares; ignoring"
        );
        try_emit(
            workspace,
            "spawn_session::unknown_session",
            SessionUpdate::ServiceStatus {
                severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
                message: format!("Session {} names no configured project", slot.display()),
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
            key: slot.clone(),
            project_name: parent.name.clone(),
            cwd,
            display_name,
        },
    );

    match workspace.get_agent_handle_at_key(
        SessionTarget::Session(slot.clone()),
        launch_settings,
        Some(slot.clone()),
        role,
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                slot = %slot.display(),
                "spawn dispatched for session resume"
            );
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                slot = %slot.display(),
                error = %err,
                "spawn_session: get_agent_handle failed"
            );
            // No SessionTask exists to run its ConnectionFailed arm, so
            // record everything parked for this session's slot here.
            workspace.expire_parked_for_slot(
                slot,
                crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
            );
            try_emit(
                workspace,
                "spawn_session::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key: slot.clone(),
                    message: format!("agent spawn failed: {err}"),
                    fatal: false,
                },
            );
        }
    }
}

/// Startup spawn. Resolves the default project (or the named one
/// passed on argv) and spawns its lead under the id it will run under.
/// Failure before the first Connected emits `SessionUpdate::FatalError`
/// so TUI exits cleanly.
pub(crate) fn handle_start_default(
    workspace: &Arc<Workspace>,
    project_name: Option<String>,
    mut launch_settings: SessionLaunchSettings,
) {
    // Resolved before `target` takes `project_name`: a spawn that fails
    // has to expire what this project's lead has parked.
    let lead_project = match project_name.as_deref() {
        Some(name) => workspace.find_project_view_by_name(name),
        None => Some(workspace.config.default_project().clone()),
    };
    let target = match project_name {
        Some(name) => SessionTarget::Named(name),
        None => SessionTarget::Default,
    };

    apply_lead_charter(&mut launch_settings);

    let session_key = match workspace.resolve_slot(&target) {
        Ok(key) => key,
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                error = %err,
                "start_default: target resolution failed"
            );
            // The same shape the spawn's own Err arm emits: startup is
            // fatal, so the typed failure follows the connection one.
            // No slot was resolved, so the failure names the project it
            // was asked for.
            try_emit(
                workspace,
                "start_default::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key: lead_project.as_ref().map_or_else(
                        || SessionSlot::lead("unknown", "unknown"),
                        |p| SessionSlot::lead(&p.org, &p.name),
                    ),
                    message: format!("agent spawn failed: {err}"),
                    fatal: true,
                },
            );
            try_emit(
                workspace,
                "start_default::FatalError",
                SessionUpdate::FatalError(forge_primitives::error::AppError::ConnectionFailed),
            );
            return;
        }
    };
    match workspace.get_agent_handle_at_key(
        target,
        launch_settings,
        Some(session_key.clone()),
        &crate::protocol::SpawnRole::Lead,
    ) {
        Ok(_handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                slot = %session_key.display(),
                "startup spawn dispatched"
            );
        }
        Err(err) => {
            tracing::error!(
                target: "forge_workspace::spawn",
                error = %err,
                "start_default: get_agent_handle failed"
            );
            // No SessionTask exists to run its ConnectionFailed arm, so
            // record everything parked for this project's lead here.
            if let Some(project) = lead_project {
                workspace.expire_parked_for_slot(
                    &crate::SessionSlot::lead(&project.org, &project.name),
                    crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
                );
            }
            try_emit(
                workspace,
                "start_default::ConnectionFailed",
                SessionUpdate::ConnectionFailed {
                    key: session_key,
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

/// The caller-facing refusal for an at-cap worker spawn. One source so
/// the classifier pin in the facade tests tracks the real text.
pub(crate) fn worker_limit_reached_message(project: &str, live: usize, cap: usize) -> String {
    let workers = if live == 1 { "worker" } else { "workers" };
    format!(
        "worker limit reached: project '{project}' has {live} {workers} live and its cap is {cap} (the project's max_workers in forge.toml, default {}); despawn one first, or raise/remove max_workers",
        crate::config::DEFAULT_MAX_WORKERS_PER_PROJECT
    )
}

/// The per-worker half of a spawn request, bundled so a caller cannot
/// transpose `kick` with `resume_kick` - both are `Option<String>` and
/// the mistake is silent.
pub(crate) struct WorkerSpawnArgs {
    pub label: String,
    pub charter: String,
    pub kick: Option<String>,
    pub resume_kick: Option<String>,
    pub interactive: bool,
}

/// Handle a `Command::SpawnWorker`: insert a `Spawning` worker entry
/// in `live_workers[project_key]`, dispatch a spawn for the id the
/// worker will run under (or the id being resumed) with the charter
/// threaded onto `SessionLaunchSettings`, then reply on `return_to`
/// with that id and the tag value. The Connected handler in
/// `session_task::translate_event` writes the actual JSONL tag row
/// and transitions the entry from Spawning to Running (or rolls back
/// on tag-write failure).
///
/// The reply's session id is informational: the LLM's `workers__spawn`
/// caller addresses the worker by label, not by id, and logs the id to
/// have a stable handle on the row.
pub(crate) fn handle_spawn_worker(
    workspace: &Arc<Workspace>,
    project_key: ProjectKey,
    args: WorkerSpawnArgs,
    spawned_by: SessionSlot,
    resume_existing: Option<&str>,
    from_boot_respawn: bool,
    return_to: tokio::sync::oneshot::Sender<Result<WorkerSpawnReply, String>>,
) {
    let WorkerSpawnArgs { label, charter, kick, resume_kick, interactive } = args;
    let label = label.as_str();
    let resume_kick = resume_kick.as_deref();
    // Verify the project exists before minting the worker's id. A worker
    // that already has a row takes its git-repo-ness from that row: it is
    // the flag the launchpad and the boot wave read to decide whether the
    // directory this spawn would enter is there, so probing again here
    // could compose a different cwd than the one they cleared. Only a
    // first spawn probes the project path - a blocking FS call,
    // deliberately BEFORE the live_workers critical section below so it
    // never widens the dedup window. Either way the result feeds both the
    // WorkerEntry flag and the `--worktree` extra-arg threading below.
    let projects = workspace.list_projects();
    let Some(view) = projects.iter().find(|v| v.key == project_key) else {
        let _ = return_to.send(Err(format!("project not found: {}", project_key.as_str())));
        return;
    };
    let is_git = workspace
        .recorded_worker_is_git_repo(&project_key, label)
        .unwrap_or_else(|error| {
            tracing::warn!(
                target: "forge_workspace::spawn",
                event_name = "worker_row_gitness_unreadable",
                project = %project_key.as_str(),
                label = %label,
                %error,
                "reading the worker's recorded gitness failed; probing the project path, \
                 which may compose a different directory than the row names",
            );
            None
        })
        .unwrap_or_else(|| forge_agent::env::worktree::is_git_repo(&view.path));

    // The pool key a fresh worker spawns under: an id minted here and
    // recorded under the worker's slot before the child starts, so the
    // pool, the registry entry, the gateway binding and the CLI's own
    // `--session-id` all name the same string and nothing has to move on
    // `Connected`. A resume keys on the id being resumed instead.
    // The worker's slot, and the id it runs under. A fresh spawn mints
    // and records the id here; a resume adopts the one being resumed, so
    // the row names the occupant the child will actually run as.
    let is_resume = resume_existing.is_some();
    let slot = SessionSlot::worker(&view.org, &view.name, label);
    let session_id = match resume_existing {
        Some(resuming) => forge_primitives::SessionId::new(resuming),
        None => forge_primitives::SessionId::new(uuid::Uuid::new_v4().to_string()),
    };
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
    // The entry is keyed by the worker's slot, and carries the id it
    // runs under: a fresh one was minted above and recorded below, once
    // the guard admits the spawn, so the pool, the registry and the
    // child's `--session-id` agree from the first instant and `Connected`
    // has nothing to move.
    let entry = crate::mcp::workers::types::WorkerEntry {
        label: label.to_owned(),
        charter: charter.clone(),
        slot: slot.clone(),
        session_id: Some(session_id.clone()),
        status: forge_primitives::WorkerLiveness::Spawning,
        spawned_at: std::time::SystemTime::now(),
        spawned_by,
        needs_tag: !is_resume,
        is_git_repo_at_spawn: is_git,
        diagnostic: None,
        kick,
    };
    // Label uniqueness AND the project's worker cap, enforced atomically
    // at this shared core so neither dispatch source - the boot
    // re-spawn or an MCP `workers__spawn` - can double-insert and fork
    // two subprocesses onto one worktree, or overshoot the cap on
    // genuinely-concurrent dispatches. The cap is per project: the
    // project's `max_workers` override, else the
    // default. Boot re-spawns pass no cap: they restore persisted
    // workers the user already had, and their spawn reply is dropped,
    // so a refusal there could never reach a caller.
    let cap = (!from_boot_respawn).then(|| {
        workspace
            .project_for_key(&project_key)
            .and_then(|project| project.max_workers)
            .unwrap_or(crate::config::DEFAULT_MAX_WORKERS_PER_PROJECT)
    });
    if let Err(refusal) =
        workspace.insert_live_worker_if_label_absent(&project_key, entry.clone(), cap)
    {
        match refusal {
            LiveWorkerRefusal::LabelLive(existing) => {
                let existing_session = existing.display();
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
            }
            LiveWorkerRefusal::AtCap { live, cap } => {
                tracing::info!(
                    target: "forge_workspace::spawn",
                    project = %project_key.as_str(),
                    label = %label,
                    live,
                    cap,
                    "spawn_worker: refused, at the concurrent worker cap",
                );
                let _ = return_to.send(Err(worker_limit_reached_message(
                    project_key.as_str(),
                    live,
                    cap,
                )));
            }
        }
        return;
    }
    // The row is the whole registry entry a boot re-spawns from, so it
    // carries the spawn args alongside the id rather than leaving them in
    // memory. One path writes it for both spawns and re-spawns; what a
    // re-spawn must not restate is the `kick` handled just below.
    //
    // AFTER the guard above, which is the point of the order: both
    // refusals return without spawning, and a row written first would
    // outlive them. A refused duplicate would have overwritten the
    // RUNNING worker's row with a fresh id and the new args, so the next
    // boot would resume an id no session ever ran under; an at-cap
    // refusal would leave a row the boot re-spawn wave picks up with no
    // cap, bringing back a worker the caller was told does not exist.
    // The row's `kick` is the worker's first turn, which only a first
    // spawn states. A resume's live kick is the restart note (or the row's
    // `resume_kick`), and writing that here would make it the worker's
    // opening turn on every later `--new` re-spawn.
    let kick_field = if is_resume { None } else { entry.kick.as_deref() };
    let durability_warning = match workspace.record_worker_row(
        &project_key,
        label,
        session_id.as_str(),
        &charter,
        kick_field,
        resume_kick,
        interactive,
        is_git,
    ) {
        Ok(()) => None,
        Err(error) => {
            tracing::warn!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %label,
                %error,
                "recording the worker's row failed; it will not survive a restart",
            );
            Some(format!(
                "recording this worker for durability failed ({error}); it will not survive a forge restart"
            ))
        }
    };
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
    let target = if resume_existing.is_some() {
        SessionTarget::Session(slot.clone())
    } else {
        SessionTarget::FreshInProject { slot: slot.clone() }
    };
    match workspace.get_agent_handle_at_key(
        target,
        settings,
        None,
        &crate::protocol::SpawnRole::Worker {
            label: label.to_owned(),
            // The same predicate the synchronous rollback below deletes
            // under: a resume and a boot re-spawn adopt a row that was
            // already there, so only a spawn that minted this one may
            // take it away.
            wrote_row: !is_resume && !from_boot_respawn,
        },
    ) {
        Ok(handle) => {
            tracing::info!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %label,
                slot = %slot.display(),
                "spawn dispatched for worker"
            );
            // The walk lands on a saturated or bailed account only when
            // nothing else in the pin declares the project's model, so
            // the lead hears about it at spawn rather than when the
            // worker stalls on a 429.
            let rate_limited_account =
                handle.display_name().and_then(|name| workspace.degraded_account_name(&name));
            // Reply to the LLM with the id the worker runs under. The
            // LLM addresses subsequent calls by label; the session_id
            // field is informational, and it is no longer a placeholder
            // the rekey machinery has to correct.
            let _ = return_to.send(Ok(WorkerSpawnReply {
                session_id: session_id.as_str().to_owned(),
                tag,
                rate_limited_account,
                // Set when the row could not be written above; the boot
                // re-spawn paths drop the reply, so they read the warn
                // instead.
                durability_warning,
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
            // A fresh spawn the caller asked for creates its row above, so
            // a failure here has to take the row with it: the caller is
            // told the spawn failed and the live entry is rolled back, and
            // a row left behind brings the worker back on the next boot.
            // A resume and a boot re-spawn are not that case - their row
            // pre-existed and is the only handle on the id being resumed,
            // so deleting it would lose the worker rather than let it
            // retry.
            if !is_resume && !from_boot_respawn {
                let _ = workspace.delete_worker_row(&project_key, label);
            }
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
    let _ = workspace.delete_worker_row(project_key, label);
    // The row is gone, so nothing re-spawns this label: its durable state
    // has no owner left to wake and goes with it.
    workspace.remove_gotify_subscriptions_for_worker(project_key, label);
    // Same for its Slack subscriptions: a despawned worker cannot strand
    // records that would reload at boot, and a delivery for it must not
    // fall through to the lead.
    workspace.remove_slack_subscriptions_for_worker(project_key, label);
    workspace.stop_slack_subsystem_if_idle();
    workspace.delete_crons_for_worker(project_key, label);
    // Closes exactly one worker, so the non-cascading primitive. The
    // cascading variant would reach the same result: it cascades only
    // from a lead slot, and this one carries the worker's own label.
    // Per-row close must only affect the worker being closed, so the
    // narrower call is the one to read here.
    workspace.release_session(&entry.slot);
    workspace.expire_inflight_for_closed_worker(project_key, label);
    // A payload parked for this label while it was still spawning has no
    // session left to drain it.
    workspace.expire_parked_for_slot(
        &entry.slot,
        crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
    );
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

    // `lead` is not a worker label: it is the project lead's own row, the
    // stored id an ordinary boot resumes the lead from, and the spawn path
    // reserves the label so no worker can hold it. Clearing that row and
    // reporting a despawn would orphan the lead's conversation.
    if label == crate::store::sessions::LEAD_LABEL {
        let _ = respond.send(DespawnResult::NotFound);
        return;
    }

    // Peek the latest-spawned matching worker WITHOUT removing it, so a
    // blocked despawn leaves it live.
    let live =
        workspace.list_live_workers(project_key).into_iter().rev().find(|w| w.label == label);

    // The gitness this despawn acts on: a live entry stamped it at spawn,
    // and a stranded row records it - once the entry is gone the row is
    // the only place it exists, and the worktree it names is still on
    // disk to clean. Neither source answering means there is no worker by
    // this label at all.
    let is_git_repo = match live.as_ref() {
        Some(entry) => Some(entry.is_git_repo_at_spawn),
        None => match workspace.recorded_worker_is_git_repo(project_key, label) {
            Ok(recorded) => recorded,
            // A row that cannot be read is not an absent one, and reading
            // it is how this call decides whether there is anything here.
            Err(error) => {
                let _ = respond.send(DespawnResult::Failed {
                    reason: format!(
                        "could not read the durable row for '{label}': {error}; whether one \
                         exists is unknown"
                    ),
                });
                return;
            }
        },
    };
    let Some(is_git_repo) = is_git_repo else {
        let _ = respond.send(DespawnResult::NotFound);
        return;
    };

    // Resolve the worktree path for git-repo workers (claude's
    // `<project_root>/.claude/worktrees/<label>/`). Non-git workers
    // have no worktree to clean.
    let project_view = workspace.list_projects().into_iter().find(|v| v.key == *project_key);
    let worktree_path = if is_git_repo {
        project_view
            .as_ref()
            .map(|v| crate::mcp::workers::types::worker_tag_dir(&v.path, label, true))
    } else {
        None
    };

    // A worker's worktree carries its own build tree, so the removal
    // below deletes hundreds of thousands of files and can run for
    // minutes. Nothing else on this path is slow, so these phase records
    // are what tells a long close apart from a hung one (#1113).
    let worktree_display =
        worktree_path.as_ref().map_or_else(|| "none".to_owned(), |path| path.display().to_string());
    tracing::info!(
        target: "forge_workspace::spawn",
        project = %project_key.as_str(),
        label = %label,
        force,
        worktree = %worktree_display,
        "despawn: starting",
    );

    // Dirty-check BEFORE teardown: block (nothing torn down) when the
    // worktree is dirty and `force` is not set. `force` skips the probe
    // rather than ignoring its verdict.
    let dirty_reason = if force {
        None
    } else {
        worktree_path
            .as_ref()
            .and_then(|path| forge_agent::env::worktree::worktree_dirty_reason(path))
    };
    tracing::info!(
        target: "forge_workspace::spawn",
        project = %project_key.as_str(),
        label = %label,
        dirty = dirty_reason.is_some(),
        reason = dirty_reason.as_deref().unwrap_or("none"),
        "despawn: dirty verdict",
    );
    if let Some(reason) = dirty_reason {
        let _ = respond.send(DespawnResult::Blocked { reason });
        return;
    }

    // Teardown. A live worker goes through `teardown_worker`, which kills
    // the subprocess on drop, removes the entry, deletes the row and
    // clears the records and payloads addressed to it.
    //
    // A stranded row has no entry to tear down: the row is the whole of
    // it, so it goes here with the records and payloads its label still
    // owns. Without that, a despawn would report a worker gone while
    // leaving exactly what the live path exists to clear (#1142).
    if live.is_some() {
        // The single-threaded command loop means nothing mutated
        // `live_workers` between the peek above and here, but re-checking
        // the removal is defensive.
        if teardown_worker(workspace, project_key, label).is_none() {
            let _ = respond.send(DespawnResult::NotFound);
            return;
        }
    } else {
        match workspace.delete_worker_row(project_key, label) {
            Ok(true) => {}
            Ok(false) => {
                let _ = respond.send(DespawnResult::NotFound);
                return;
            }
            // A store that could not be written is not an absent row:
            // reporting NotFound would send the caller looking for a
            // worker that is right there. `delete_worker_row` has already
            // logged why, so the caller gets the reason and the log has
            // the site.
            Err(error) => {
                let _ = respond.send(DespawnResult::Failed {
                    reason: format!(
                        "could not clear the durable row for '{label}': {error}; whether one \
                         exists is unknown, so nothing was reported as removed"
                    ),
                });
                return;
            }
        }
        workspace.remove_gotify_subscriptions_for_worker(project_key, label);
        workspace.remove_slack_subscriptions_for_worker(project_key, label);
        workspace.stop_slack_subsystem_if_idle();
        workspace.delete_crons_for_worker(project_key, label);
        workspace.expire_inflight_for_closed_worker(project_key, label);
        if let Some(view) = project_view.as_ref() {
            workspace.expire_parked_for_slot(
                &SessionSlot::worker(&view.org, &view.name, label),
                crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
            );
        }
        tracing::info!(
            target: "forge_workspace::spawn",
            project = %project_key.as_str(),
            label = %label,
            "despawn: no live worker matched; cleared its stranded durable row and records",
        );
    }

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
        // A stranded row whose worktree is already gone is the definition
        // of that state - it is why the boot wave skips the row - so there
        // is nothing to remove and git's failure over an untracked path
        // would be a warning about nothing. A live worker's worktree
        // vanishing is the opposite: anomalous, and the live shape below
        // keeps reporting what git said.
        Some(path) if live.is_none() && !path.exists() => {
            tracing::debug!(
                target: "forge_workspace::spawn",
                project = %project_key.as_str(),
                label = %label,
                worktree = %path.display(),
                "despawn: the stranded row's worktree is already gone; nothing to remove",
            );
            None
        }
        Some(path) => match forge_agent::env::worktree::remove_worktree(path, force) {
            Ok(()) => {
                tracing::info!(
                    target: "forge_workspace::spawn",
                    project = %project_key.as_str(),
                    label = %label,
                    worktree = %path.display(),
                    "despawn: worktree removed",
                );
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
        (None, _) => WorktreeDisposition::untouched(is_git_repo),
        (Some(path), Some(_)) if path.exists() => WorktreeDisposition::RemovalFailed,
        (Some(_), _) => WorktreeDisposition::Removed,
    };
    // Only a torn-down live worker has a pane row to remove; a stranded
    // row's label reaches the launchpad by reading the store per frame, so
    // it needs no event.
    if let Some(entry) = live.as_ref() {
        emit_worker_removed(workspace, project_key, entry, worktree);
    }

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
/// `None` when it parked (the target's Connected handler drains the
/// bucket, doing the bump + render + dispatch). Mirrors
/// the sleeping-peer buffering in `handle_deliver_peer_prompt` so the
/// bump bookkeeping happens exactly once, at real delivery time.
fn buffer_prompt_until_connected(
    workspace: &Arc<Workspace>,
    slot: &crate::SessionSlot,
    target_key: &SessionSlot,
    wrapped: WrappedPrompt,
) -> Option<WrappedPrompt> {
    let Some(domain) = workspace.domain_session_for(target_key) else {
        // No DomainSession to park against (target vanished); let the
        // caller proceed - its dispatch warns on the missing target.
        return Some(wrapped);
    };
    if domain.lock().session_id.is_some() {
        return Some(wrapped);
    }
    workspace.park_peer_prompt(slot, wrapped);
    None
}

pub(crate) fn handle_deliver_worker_prompt(
    workspace: &Arc<Workspace>,
    _caller: SessionSlot,
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
    let target_key = entry.slot.clone();

    // A worker addressed before it finishes its Connected handshake has
    // no session_id yet, so a bare Command::Prompt would be dropped by
    // execute_command_via_handle. Park it for the worker's label instead -
    // its Connected handler drains the bucket (bump + render + dispatch)
    // exactly like the sleeping-peer path. Skips the tag retry / stamp /
    // dispatch below.
    let Some(wrapped) = buffer_prompt_until_connected(workspace, &target_key, &target_key, wrapped)
    else {
        return;
    };

    // Opportunistic tag-write retry: if this worker was spawned idle
    // (no initial_prompt), the JSONL didn't exist at Connected and
    // the tag-write at that point exhausted into a deferred state.
    // claude is about to process this turn, which means it's about
    // to write the JSONL - kick off a fire-and-forget retry now.
    if entry.needs_tag
        && let Some(session_id) = entry.session_id.clone()
    {
        let cwd = workspace
            .list_projects()
            .into_iter()
            .find(|view| view.key == *project_key)
            .map(|view| view.path.to_string_lossy().into_owned());
        if let Some(cwd) = cwd {
            workspace.retry_worker_tag_opportunistic(
                project_key,
                &target_key,
                session_id.as_str(),
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
/// the lead's `SessionSlot` (resolved at Tool dispatch from the
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
    _caller: SessionSlot,
    target_lead_key: &SessionSlot,
    wrapped: WrappedPrompt,
) {
    // Defensive: confirm the lead session is still in the pool. If it
    // closed since the worker captured its `spawned_by_session_id`,
    // drop the prompt with a warn - same shape as `DeliverWorkerPrompt`
    // does for a worker that vanished between dispatch and handler.
    if !workspace.pool.lock().contains_key(target_lead_key) {
        tracing::warn!(
            target: "forge_workspace::spawn",
            slot = %target_lead_key.display(),
            "deliver_worker_prompt_to_lead: lead session not in pool (closed since dispatch)"
        );
        return;
    }

    // Same pre-Connect guard as the sibling-worker path: if the lead
    // hasn't stamped its session_id yet, buffer for its Connected drain
    // rather than dispatching a Command::Prompt that would be dropped.
    let Some(wrapped) =
        buffer_prompt_until_connected(workspace, target_lead_key, target_lead_key, wrapped)
    else {
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
            slot = %target_lead_key.display(),
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

    /// The project path the shared fixture points at. The maintainer's own
    /// checkout, which is a git repo, so the gitness probe answers true for
    /// it; a fixture that needs a different answer writes its own.
    const FIXTURE_PROJECT_PATH: &str = "~/Projects/forge";

    fn write_forge_toml(dir: &std::path::Path, project_path: &str) {
        fs::write(
            forge_toml_path(dir),
            format!(
                r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "{project_path}"
auto_start = true
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#
            ),
        )
        .expect("write forge.toml");
    }

    #[test]
    fn project_permission_mode_stamps_into_absent_settings() {
        let mut settings = SessionLaunchSettings::default();
        stamp_permission_mode(
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
            "the project mode reaches the settings JSON the worker's applied_permission_mode arm reads",
        );
    }

    #[test]
    fn project_permission_mode_overrides_the_launcher_default_and_keeps_siblings() {
        let mut settings = SessionLaunchSettings {
            settings: Some(serde_json::json!({
                "permissions": { "defaultMode": "default" },
                "model": "haiku",
            })),
            ..SessionLaunchSettings::default()
        };
        stamp_permission_mode(
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
            "the project mode wins over the launcher's session default",
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
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
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
    /// `SessionUpdate::Spawning` under the id the lead will run under,
    /// so the TUI can show a Waking placeholder before the agent reaches
    /// Connected and the bucket it draws is the one that connects.
    #[tokio::test]
    async fn spawn_project_known_project_announces_the_id_it_will_run_under() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        handle_spawn_project(&workspace, "forge", SessionLaunchSettings::default());

        let update = rx.try_recv().expect("Spawning emit");
        match update {
            SessionUpdate::Spawning { key, project_name, .. } => {
                assert_eq!(
                    key,
                    SessionSlot::lead("Default", "forge"),
                    "the announced key is the project's lead slot",
                );
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
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
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
    /// Drives the failure by naming a slot no project declares:
    /// `project_for_slot` returns None, the handler warns and returns
    /// after a `ServiceStatus` note. This regression test confirms it
    /// does NOT emit a fatal envelope.
    #[tokio::test]
    async fn spawn_session_unknown_session_emits_no_fatal() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        handle_spawn_session(
            &workspace,
            &SessionSlot::from_str_for_test("no-such-session-id"),
            &crate::protocol::SpawnRole::Lead,
            SessionLaunchSettings::default(),
        );

        // An unknown slot warns and returns, so the only envelope this
        // path may send is the non-fatal ServiceStatus. The important
        // assertion is that nothing here is a FatalError.
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
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        let caller = SessionSlot::from_str_for_test("caller-1");
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
    /// project parks the prompt for that project's lead and triggers a
    /// SpawnProject. The bucket is addressed by `(org, project, label)`,
    /// so no synthetic key is anywhere in the path; the drain side is
    /// pinned in `session_task`, which delivers a parked prompt on the
    /// owner's first `Connected`.
    #[tokio::test]
    async fn handle_deliver_peer_prompt_sleeping_target_parks_for_the_projects_lead() {
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
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        // Intercept dispatch so the spawn this delivery triggers does not
        // run inline and expire the park it just made: with no `claude`
        // binary the spawn's failure arm releases the slot, which is the
        // production behaviour but hides the parking under test.
        workspace.enable_test_dispatch_intercept();
        let caller = SessionSlot::from_str_for_test("caller-sleep");
        let w = fixture_wrapped();

        handle_deliver_peer_prompt(&workspace, caller, "gateway-backend".to_owned(), w.clone());

        // The sleeping branch parks the envelope for the project's lead
        // under `(org, project, None)`. EXACTLY ONE bucket holds our
        // wrapped prompt - assert on the typed correlation id as well, so
        // a mis-keyed parking cannot pass by parking twice.
        let parked = workspace
            .parked_by_slot
            .lock()
            .get(&crate::SessionSlot::lead("Default", "gateway-backend"))
            .map(|parked| parked.peer.clone())
            .unwrap_or_default();
        assert_eq!(parked.len(), 1, "the project's lead bucket holds the wrapped prompt");
        assert_eq!(
            parked[0].correlation_id, w.correlation_id,
            "and it is the payload that was handed to the delivery",
        );
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
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
        )
        .expect("write forge.toml");
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("new"));
        let mut rx = workspace.subscribe().expect("subscribe");

        let caller = SessionSlot::from_str_for_test("caller-tell");
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
            slot: SessionSlot::from_str_for_test(key),
            session_id: None,
            status: forge_primitives::WorkerLiveness::Running,
            spawned_at: std::time::SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    use forge_primitives::slack::SlackFile;

    fn slack_msg(text: &str) -> SlackMessage {
        SlackMessage {
            workspace: "acme".to_owned(),
            conversation: "D1".to_owned(),
            conversation_label: "U9".to_owned(),
            ts: "100.000001".to_owned(),
            thread_ts: None,
            user: Some("U9".to_owned()),
            text: text.to_owned(),
            files: Vec::new(),
        }
    }

    #[test]
    fn a_slack_message_for_a_worker_targets_the_worker_not_the_lead() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("forge", "/tmp/slack-forge");
        let key = ws.list_projects().into_iter().find(|v| v.name == "forge").expect("seeded").key;
        // Delivery routes by the live entry's slot, so the fixture carries
        // the one production derives for this worker.
        let worker_key = crate::SessionSlot::worker("TestOrg", "forge", "tester");
        let mut entry = fake_worker_entry("tester", "worker-uuid");
        entry.slot = worker_key.clone();
        ws.insert_live_worker(&key, entry);

        deliver_slack_message(&ws, "forge", Some("tester"), slack_msg("hello"));

        let parked =
            ws.parked_by_slot.lock().get(&worker_key).map_or(0, |parked| parked.slack.len());
        assert_eq!(parked, 1, "the message is parked for the worker that subscribed");
        assert_eq!(
            ws.parked_by_slot
                .lock()
                .get(&crate::SessionSlot::lead("TestOrg", "forge"))
                .map_or(0, |parked| parked.slack.len()),
            0,
            "a worker-owned subscription never falls through to the lead",
        );
    }

    /// A live delivery to a connected team worker dispatches the prose AND
    /// echoes the block into that worker's chat. The CLI never echoes a
    /// stdin-injected prompt back, so without the echo the agent works the
    /// message and the user sees nothing; asserting only the echo would let a
    /// regression through where the block paints and the agent never receives it.
    #[test]
    fn deliver_slack_message_to_a_connected_worker_dispatches_and_echoes() {
        let (ws, mut update_rx) = Workspace::testing_stub();
        ws.seed_test_project("forge", "/tmp/slack-worker-echo");
        let key = ws.list_projects().into_iter().find(|v| v.name == "forge").expect("seeded").key;
        ws.insert_live_worker(&key, fake_worker_entry("tester", "worker-uuid"));
        let worker_key = SessionSlot::from_str_for_test("worker-uuid");
        ws.mark_session_connected_for_test(&worker_key, "worker-uuid");
        ws.enable_test_dispatch_intercept();

        deliver_slack_message(&ws, "forge", Some("tester"), slack_msg("hello"));

        let dispatched = ws.drain_test_dispatch_buffer();
        assert!(
            dispatched.iter().any(|c| matches!(
                c, crate::protocol::Command::Prompt { key, text, .. }
                    if *key == worker_key && text.contains("hello")
            )),
            "the worker receives the message as a prompt: {dispatched:?}",
        );

        let mut echoed = false;
        while let Ok(u) = update_rx.try_recv() {
            if matches!(
                u,
                crate::protocol::SessionUpdate::SlackMessageAppended { key, prose }
                    if key == worker_key
                        && prose.starts_with("[Slack")
                        && prose.contains("hello")
            ) {
                echoed = true;
            }
        }
        assert!(
            echoed,
            "a connected worker's delivery emits a SlackMessageAppended echo with the body",
        );
    }

    /// A worker spawn that fails rolls its live entry back, so no live row
    /// lingers with no session behind it. The failure is the same
    /// `FreshInProject` miss the project-spawn arm hits: the overlay satisfies
    /// `list_projects` but not the config lookup inside the spawn.
    /// A fresh worker is keyed by the id it will run under: minted and
    /// recorded before the child starts, so the registry entry, the pool
    /// and the child's `--session-id` all name one string and `Connected`
    /// has nothing to move. Verified on a worker rather than a lead,
    /// because the worker used to be the path that spawned under a
    /// placeholder and rekeyed later.
    #[tokio::test]
    async fn a_fresh_worker_is_keyed_by_the_id_it_runs_under() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        ws.seed_test_ready_account("Stargate");
        ws.seed_test_gateway_ready(true);
        let key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("fixture project")
            .key;
        let (tx, rx) = tokio::sync::oneshot::channel();

        handle_spawn_worker(
            &ws,
            key,
            WorkerSpawnArgs {
                label: "tester".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );

        let reply = rx.await.expect("spawn replies").expect("spawn succeeds");
        assert!(
            uuid::Uuid::parse_str(&reply.session_id).is_ok(),
            "the worker is told the id it runs under, not a placeholder: {}",
            reply.session_id,
        );
        let entry = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .and_then(|view| ws.list_live_workers(&view.key).into_iter().next())
            .expect("the worker is registered");
        assert_eq!(
            entry.slot,
            SessionSlot::worker("Default", "forge", "tester"),
            "the registry entry is keyed by the worker's slot",
        );
        assert!(ws.pool.lock().contains_key(&entry.slot), "and so is the pool");
        let stored = {
            let db = ws.db.lock();
            let db = db.as_ref().expect("db");
            crate::store::sessions::get(db, "Default", "forge", "tester")
                .expect("read")
                .and_then(|row| row.session_id)
        };
        assert_eq!(stored.as_deref(), Some(reply.session_id.as_str()), "and the store row");
    }

    /// Drive a spawn that fails at dispatch and return the durable row it
    /// left behind. The project is in the test overlay but not in
    /// `forge.toml`, and the config lookup inside the spawn is what misses.
    ///
    /// A row is seeded first, under the same `(org, project, label)` the
    /// spawn will write, so a missing row afterwards means one was removed
    /// rather than that none was ever written - without it, `is_none()`
    /// passes just as well against a `record_worker_row` that never ran.
    async fn failed_spawn_leftover_row(
        resume_existing: Option<&str>,
        from_boot_respawn: bool,
        seeded_is_git: Option<bool>,
    ) -> Option<crate::store::sessions::SessionRecord> {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        ws.seed_test_project("overlayonly", "/tmp/slack-worker-rollback");
        let key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "overlayonly")
            .expect("seeded project present")
            .key;
        {
            let db = ws.db.lock();
            crate::store::sessions::put(
                db.as_ref().expect("db"),
                &crate::store::sessions::SessionRecord {
                    org: "TestOrg".to_owned(),
                    project: "overlayonly".to_owned(),
                    label: "tester".to_owned(),
                    session_id: Some("tester-id".to_owned()),
                    charter: None,
                    kick: None,
                    resume_kick: None,
                    interactive: None,
                    is_git_repo: seeded_is_git,
                },
            )
            .expect("seed the row the spawn will write");
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &ws,
            key.clone(),
            WorkerSpawnArgs {
                label: "tester".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            resume_existing,
            from_boot_respawn,
            tx,
        );

        assert!(matches!(rx.await, Ok(Err(_))), "the spawn reports the failure to its caller");
        assert!(
            ws.list_live_workers(&key).is_empty(),
            "and the placeholder entry is rolled back, not left live",
        );
        let db = ws.db.lock();
        crate::store::sessions::get(db.as_ref().expect("db"), "TestOrg", "overlayonly", "tester")
            .expect("read")
    }

    #[tokio::test]
    async fn a_failed_worker_spawn_rolls_the_live_entry_back() {
        let row = failed_spawn_leftover_row(None, false, None).await;
        assert!(
            row.is_none(),
            "the durable row goes with the rollback: left behind, the next boot re-spawns a \
             worker the caller was told had failed",
        );
    }

    /// A RESUME whose dispatch fails keeps its row, because that row is the
    /// only durable handle on the id being resumed - deleting it loses the
    /// worker rather than letting it retry. Nothing else reaches this arm
    /// with the flag set, so without this the guard's negative half is
    /// pinned by nothing.
    #[tokio::test]
    async fn a_failed_resume_spawn_keeps_the_row_holding_the_id() {
        let row = failed_spawn_leftover_row(Some("tester-id"), false, None).await;
        assert_eq!(
            row.and_then(|row| row.session_id).as_deref(),
            Some("tester-id"),
            "the rollback leaves the row that names the id being resumed",
        );
    }

    /// The same for a boot re-spawn, which carries no `resume_existing` when
    /// it comes up fresh under `--new` - so the resume flag alone does not
    /// separate it from a spawn a caller asked for, and the boot flag is
    /// what keeps its row.
    ///
    /// That path mints a new id and writes it over the row, so what the
    /// guard protects here is the row's survival rather than this id: the
    /// boot wave owns the worker, and deleting the row on a failure would
    /// drop it from every later restart.
    #[tokio::test]
    async fn a_failed_boot_respawn_keeps_its_row() {
        let row = failed_spawn_leftover_row(None, true, None).await;
        assert!(row.is_some(), "the boot re-spawn keeps the row a later restart re-spawns from");
    }

    /// A resume delivers the restart note (or the row's `resume_kick`) as
    /// this connection's kick, and that text must not land in the row's
    /// `kick`: that field is the worker's original first turn, and it is
    /// what a later `--new` re-spawn opens the worker with. Written, the
    /// resume text becomes the worker's opening turn for the rest of its
    /// life.
    #[tokio::test]
    async fn a_resume_spawn_leaves_the_stored_kick_alone() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        ws.seed_test_ready_account("Stargate");
        ws.seed_test_gateway_ready(true);
        let key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("fixture project")
            .key;
        // The row a real first spawn writes, non-git so the resume runs in
        // the project root rather than a worktree that is not there.
        ws.record_worker_row(
            &key,
            "tester",
            "tester-id",
            "charter",
            Some("original kick"),
            None,
            false,
            false,
        )
        .expect("seed the row the resume re-writes");

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &ws,
            key.clone(),
            WorkerSpawnArgs {
                label: "tester".to_owned(),
                charter: "charter".to_owned(),
                // What `dispatch_worker_respawns` hands a resuming worker.
                kick: Some("This session was restarted by forge; continue.".to_owned()),
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            Some("tester-id"),
            true,
            tx,
        );
        let reply = rx.await.expect("spawn replies");
        assert!(reply.is_ok(), "the resume spawn succeeds: {reply:?}");

        assert_eq!(
            ws.list_live_workers(&key).into_iter().next().and_then(|entry| entry.kick).as_deref(),
            Some("This session was restarted by forge; continue."),
            "the resuming connection is kicked with the resume text",
        );
        let stored = ws
            .worker_rows_for_project(&key)
            .into_iter()
            .find(|row| row.label == "tester")
            .expect("the row survives the resume");
        assert_eq!(
            stored.kick.as_deref(),
            Some("original kick"),
            "the row keeps the worker's original first turn; overwriting it makes the restart \
             note the worker's opening turn on every later --new re-spawn",
        );
    }

    /// A first spawn writes the kick it opened the worker with: the row's
    /// `kick` is the worker's first turn, and a later `--new` re-spawn
    /// delivers it. A row that never got it opens the worker with nothing.
    #[tokio::test]
    async fn a_first_spawn_writes_the_kick_it_was_given() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        ws.seed_test_ready_account("Stargate");
        ws.seed_test_gateway_ready(true);
        let key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("fixture project")
            .key;

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &ws,
            key.clone(),
            WorkerSpawnArgs {
                label: "tester".to_owned(),
                charter: "charter".to_owned(),
                kick: Some("opening turn".to_owned()),
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let reply = rx.await.expect("spawn replies");
        assert!(reply.is_ok(), "the first spawn succeeds: {reply:?}");

        let stored = ws
            .worker_rows_for_project(&key)
            .into_iter()
            .find(|row| row.label == "tester")
            .expect("the row the spawn wrote");
        assert_eq!(
            stored.kick.as_deref(),
            Some("opening turn"),
            "the row keeps the kick the spawn opened the worker with; dropped, a later --new \
             re-spawn opens the worker with no first turn at all",
        );
    }

    /// The spawn composes the working directory from the gitness the ROW
    /// records, in preference to probing the project - that is what keeps
    /// it on the directory the launchpad already cleared. A fixture whose
    /// row agrees with the disk cannot tell the two apart, so this one
    /// disagrees: the row says worktree, the project path is not a repo.
    #[tokio::test]
    async fn a_spawn_keeps_the_rows_recorded_gitness_over_a_probe() {
        let row = failed_spawn_leftover_row(Some("tester-id"), false, Some(true)).await;
        assert_eq!(
            row.and_then(|row| row.is_git_repo),
            Some(true),
            "the spawn writes back the gitness the row recorded, not what a probe of the \
             project path would answer",
        );
    }

    /// The other direction of the row-over-probe preference: with no row to
    /// read, the probe is still the answer. A first spawn into a git
    /// project that recorded `false` here would pass no `--worktree`, and
    /// every later resume, the launchpad and the cron router would read
    /// that false record as the directory the worker runs in.
    ///
    /// The spawn has to succeed for the row to survive: a refused fresh
    /// spawn rolls its row back, so there would be nothing left to read.
    #[tokio::test]
    async fn a_first_spawn_probes_for_gitness_when_there_is_no_row() {
        let dir = tempdir().expect("tempdir");
        let repo = tempdir().expect("git project dir");
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(repo.path())
            .status()
            .expect("spawn git");
        assert!(status.success(), "fixture precondition: git init");
        write_forge_toml(dir.path(), &repo.path().to_string_lossy());
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        ws.seed_test_ready_account("Stargate");
        ws.seed_test_gateway_ready(true);
        let key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("fixture project")
            .key;
        // Deliberately no row: this is the label's first spawn.

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &ws,
            key,
            WorkerSpawnArgs {
                label: "tester".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let admitted = rx.await.expect("reply").is_ok();
        assert!(admitted, "fixture precondition: the spawn is admitted, so its row survives");

        let db = ws.db.lock();
        let row =
            crate::store::sessions::get(db.as_ref().expect("db"), "Default", "forge", "tester")
                .expect("read")
                .expect("the spawn recorded its row");
        drop(db);
        assert_eq!(
            row.is_git_repo,
            Some(true),
            "with no row to read the probe is the answer; recording false here would send \
             every later resume, the launchpad and the cron router to the project root",
        );
    }

    /// A spawn refused before it reaches the project leaves the caller's
    /// parked payload where it is, so the caller's own expiry still reaches
    /// it rather than the workspace having consumed it on the way past.
    #[tokio::test]
    async fn a_refused_spawn_leaves_the_parked_payload_to_the_caller() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        ws.seed_test_ready_account("Stargate");
        ws.park_slack(
            &crate::SessionSlot::lead("Default", "missing"),
            slack_msg("parked while asleep"),
        );

        let result = ws.get_agent_handle_at_key(
            crate::target::SessionTarget::FreshInProject {
                slot: SessionSlot::worker("TestOrg", "not-in-config", "minted-worker-id"),
            },
            forge_agent::client::SessionLaunchSettings::default(),
            None,
            &crate::protocol::SpawnRole::Worker { label: "reviewer".to_owned(), wrote_row: false },
        );

        assert!(result.is_err(), "a target mapping to no project is refused");
        let owner = crate::SessionSlot::lead("Default", "missing");
        assert_eq!(
            ws.parked_by_slot.lock().get(&owner).map_or(0, |parked| parked.slack.len()),
            1,
            "the refusal left the bucket alone, so the caller's expiry can still reach it",
        );

        ws.expire_parked_for_slot(
            &owner,
            crate::mcp::peers::types::PeerFailureReason::TargetConnectionFailed,
        );
        assert_eq!(
            ws.parked_by_slot.lock().get(&owner).map_or(0, |parked| parked.slack.len()),
            0,
            "and the caller's expiry is what records the loss",
        );
    }

    /// The synchronous spawn-failure arm is reachable: a project present only
    /// in the test overlay resolves by name but not by target, so the spawn
    /// fails before any `SessionTask` exists. Everything a delivery parked for
    /// that project's lead has to be recorded there, because nothing else will
    /// drain a bucket the session never reached.
    #[test]
    fn a_failed_project_spawn_expires_the_parked_payloads() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("overlayonly", "/tmp/slack-overlay-only");
        ws.park_slack(
            &crate::SessionSlot::lead("TestOrg", "overlayonly"),
            slack_msg("parked while asleep"),
        );

        crate::spawn::handle_spawn_project(
            &ws,
            "overlayonly",
            forge_agent::client::SessionLaunchSettings::default(),
        );

        assert_eq!(
            ws.parked_by_slot
                .lock()
                .get(&crate::SessionSlot::lead("TestOrg", "overlayonly"))
                .map_or(0, |parked| parked.slack.len()),
            0,
            "the failed spawn records the messages parked for that project's lead",
        );
    }

    /// A delivery whose dispatch fails must not echo: `false` tells the sweep
    /// to re-run the message, and an echo pushed first would paint the block a
    /// second time on that re-run, for a turn the LLM never received.
    #[test]
    fn a_failed_worker_dispatch_leaves_no_echo_for_the_sweep_to_duplicate() {
        let (ws, mut update_rx) = Workspace::testing_stub();
        ws.seed_test_project("forge", "/tmp/slack-worker-doomed");
        let key = ws.list_projects().into_iter().find(|v| v.name == "forge").expect("seeded").key;
        ws.insert_live_worker(&key, fake_worker_entry("tester", "worker-uuid"));
        let worker_key = SessionSlot::from_str_for_test("worker-uuid");
        // Connected, but with no pooled handle and no conn: the dispatch fails.
        ws.mark_session_connected_for_test(&worker_key, "worker-uuid");

        let delivered = deliver_slack_message(&ws, "forge", Some("tester"), slack_msg("hello"));

        assert!(!delivered, "a failed dispatch tells the sweep to re-run the message");
        let echoed = std::iter::from_fn(|| update_rx.try_recv().ok())
            .any(|u| matches!(u, crate::protocol::SessionUpdate::SlackMessageAppended { .. }));
        assert!(!echoed, "a failed worker dispatch must not echo a SlackMessageAppended");
    }

    /// The ids in the prose are the arguments a reply takes: without them
    /// the agent cannot answer in the place the message came from.
    #[test]
    fn slack_prose_carries_the_ids_a_reply_needs() {
        let mut message = slack_msg("please look");
        message.thread_ts = Some("100.0".to_owned());
        message.files = vec![SlackFile {
            id: "F1".to_owned(),
            name: "notes.txt".to_owned(),
            url_private: "https://files.slack.com/x".to_owned(),
        }];

        let prose = slack_message_to_prose(&message);
        assert!(prose.contains("D1"), "the conversation id is present: {prose}");
        assert!(prose.contains("100.000001"), "the ts is present: {prose}");
        assert!(prose.contains("in thread 100.0"), "the thread is present: {prose}");
        assert!(prose.contains("F1"), "the file id is present: {prose}");
    }

    /// The header and the `<author>: ` line are a contract with the chat
    /// block's detector, so the whole shape is pinned: a reformat here
    /// silently reverts the block to painting nothing.
    #[test]
    fn slack_prose_names_the_workspace_and_the_author() {
        let prose = slack_message_to_prose(&slack_msg("hello there"));
        assert_eq!(
            prose, "[Slack - workspace 'acme', U9] id D1 ts 100.000001\nU9: hello there",
            "the exact prose the chat block's detector keys on",
        );
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
            WorkerSpawnArgs {
                label: "reviewer".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
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

    /// A refused duplicate must leave the RUNNING worker's row exactly as
    /// it was. The row is written after the guard for this reason: written
    /// first, the refusal would have overwritten the live worker's id,
    /// charter and kick with the refused attempt's, and the next boot
    /// would resume an id no session ever ran under, orphaning the
    /// transcript it belongs to.
    ///
    /// The sibling test above cannot see this: it runs on a `testing_stub`
    /// with no DB, so the row write fails there and the ordering does not
    /// matter.
    #[tokio::test]
    async fn a_refused_duplicate_leaves_the_running_workers_row_alone() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let ws = Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace"));
        ws.seed_test_ready_account("Stargate");
        ws.seed_test_gateway_ready(true);
        let key = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("fixture project")
            .key;
        let spawn = |charter: &str| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            handle_spawn_worker(
                &ws,
                key.clone(),
                WorkerSpawnArgs {
                    label: "steward".to_owned(),
                    charter: charter.to_owned(),
                    kick: None,
                    resume_kick: None,
                    interactive: false,
                },
                SessionSlot::from_str_for_test("lead"),
                None,
                false,
                tx,
            );
            rx
        };

        let first = spawn("mind the queues")
            .await
            .expect("reply channel")
            .expect("the first spawn is admitted")
            .session_id;

        let refused = spawn("a second, refused charter").await.expect("reply channel");
        let refusal = refused.expect_err("the second spawn for a live label is refused");
        assert!(
            refusal.contains("already live"),
            "and it is the duplicate arm that refused, not some other failure: {refusal}",
        );

        let rows = ws.worker_rows_for_project(&key);
        let row = rows.iter().find(|r| r.label == "steward").expect("the running worker's row");
        assert_eq!(
            row.session_id.as_deref(),
            Some(first.as_str()),
            "the row still names the id the running worker was admitted under",
        );
        assert_eq!(
            row.charter.as_deref(),
            Some("mind the queues"),
            "and keeps the args it was admitted with, not the refused attempt's",
        );
    }

    /// Stub whose `forge.toml` caps the `forge` project at the given
    /// override via the entry's `max_workers`, loaded through the
    /// real config path because the full spawn below reads
    /// `config.projects`. The `notes` project stays at the default.
    /// Projects point at throwaway dirs: a real repo path would make
    /// `is_git` true and hang `--worktree <label>` spawns off the
    /// actual checkout.
    fn stub_with_project_cap(
        limit: usize,
    ) -> (Arc<Workspace>, (tempfile::TempDir, tempfile::TempDir)) {
        let dir = tempdir().expect("config tempdir");
        let projects_dir = tempdir().expect("projects tempdir");
        let forge_path = projects_dir.path().join("forge").display().to_string();
        let notes_path = projects_dir.path().join("notes").display().to_string();
        fs::create_dir_all(&forge_path).expect("create forge project dir");
        fs::create_dir_all(&notes_path).expect("create notes project dir");
        fs::write(
            forge_toml_path(dir.path()),
            format!(
                r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "{forge_path}"
max_workers = {limit}
model = "claude-sonnet-5"

[[orgs.projects]]
name = "notes"
path = "{notes_path}"
model = "claude-sonnet-5"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#
            ),
        )
        .expect("write forge.toml");
        let workspace =
            Arc::new(Workspace::new_for_test(dir.path().to_owned()).expect("workspace new"));
        workspace.seed_test_ready_account("Stargate");
        (workspace, (dir, projects_dir))
    }

    fn seeded_project(workspace: &Arc<Workspace>) -> ProjectKey {
        workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("seeded project present")
            .key
    }

    /// #976: a lead-driven spawn at the project's worker cap
    /// replies a clean error naming the limit and inserts no entry.
    #[tokio::test]
    async fn spawn_worker_over_limit_refuses_and_creates_nothing() {
        let (workspace, _config_dir) = stub_with_project_cap(2);
        let project = seeded_project(&workspace);
        workspace.insert_live_worker(&project, fake_worker_entry("w1", "w1"));
        workspace.insert_live_worker(&project, fake_worker_entry("w2", "w2"));

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project.clone(),
            WorkerSpawnArgs {
                label: "w3".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );

        let err = rx.await.expect("reply").expect_err("at-limit spawn must refuse");
        assert!(err.contains("worker limit reached"), "names the refusal: {err}");
        assert!(err.contains("cap is 2"), "names the cap: {err}");
        assert_eq!(
            workspace.list_live_workers(&project).len(),
            2,
            "the refused spawn creates no worker"
        );

        // Label precedence: at the cap, a DUPLICATE label reports the
        // collision, not the cap - the lead is pointed at the remedy
        // that matches the actual problem.
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project.clone(),
            WorkerSpawnArgs {
                label: "w1".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let err = rx.await.expect("reply").expect_err("a live label must refuse");
        assert!(
            err.contains("already live") && !err.contains("worker limit reached"),
            "the collision wins over the cap: {err}"
        );
    }

    /// The cap is PER PROJECT: the gate counts only the spawning
    /// project's live workers, against that project's own
    /// `max_workers` override, else the default of
    /// 2. Workers live in other projects neither consume the budget
    /// nor raise it.
    #[tokio::test]
    async fn the_cap_is_per_project() {
        let (workspace, _config_dir) = stub_with_project_cap(1);
        let forge_project = seeded_project(&workspace);
        let notes_project = workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "notes")
            .expect("seeded notes project present")
            .key;
        // forge sits at its own cap (override 1); notes still has its
        // full default budget of 2.
        workspace.insert_live_worker(&forge_project, fake_worker_entry("w1", "w1"));

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            notes_project.clone(),
            WorkerSpawnArgs {
                label: "w2".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let reply = rx.await.expect("reply");
        assert!(
            reply.is_ok(),
            "a worker in another project must not consume notes' budget: {:?}",
            reply.err()
        );
        if let Ok(reply) = reply {
            workspace.release_session(&SessionSlot::from_str_for_test(reply.session_id));
        }

        // notes fills to its own default cap of 2 and refuses past it.
        workspace.insert_live_worker(&notes_project, fake_worker_entry("w3", "w3"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            notes_project.clone(),
            WorkerSpawnArgs {
                label: "w4".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let err = rx.await.expect("reply").expect_err("notes at its cap must refuse");
        assert!(err.contains("worker limit reached"), "names the refusal: {err}");
        assert!(err.contains("cap is 2"), "names notes' own cap: {err}");
        assert!(
            err.contains(&format!("project '{}' has 2 workers live", notes_project.as_str())),
            "notes' refusal counts only notes' workers: {err}"
        );

        // forge's override still governs forge.
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            forge_project.clone(),
            WorkerSpawnArgs {
                label: "w5".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let err = rx.await.expect("reply").expect_err("forge at its cap must refuse");
        assert!(err.contains("cap is 1"), "names forge's override: {err}");
        assert!(
            err.contains(&format!("project '{}' has 1 worker live", forge_project.as_str())),
            "forge's refusal counts only forge's workers, singular: {err}"
        );
    }

    /// The override raises as well as lowers: a cap above the default
    /// admits past 2. Every other cap test also passes if the
    /// resolution clamps the override to the default, and the raise
    /// direction is the one the forge project's own config depends on.
    #[tokio::test]
    async fn a_cap_above_the_default_admits_past_two() {
        let (workspace, _config_dir) = stub_with_project_cap(3);
        let project = seeded_project(&workspace);
        workspace.insert_live_worker(&project, fake_worker_entry("w1", "w1"));
        workspace.insert_live_worker(&project, fake_worker_entry("w2", "w2"));

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project.clone(),
            WorkerSpawnArgs {
                label: "w3".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let reply = rx.await.expect("reply");
        assert!(
            reply.is_ok(),
            "the override must raise the cap past the default: {:?}",
            reply.err()
        );
        assert_eq!(
            workspace.list_live_workers(&project).len(),
            3,
            "the admitted spawn creates its worker"
        );
        if let Ok(reply) = reply {
            workspace.release_session(&SessionSlot::from_str_for_test(reply.session_id));
        }
    }

    /// The notice the lead reads: a worker spawned onto an account the
    /// walk had to take while it was bailed comes back named in the
    /// reply, so the lead hears it at spawn rather than when the worker
    /// stalls. The join from the spawned handle's account to the reply
    /// is the whole mechanism, and a regression there would drop the
    /// notice silently.
    #[tokio::test]
    async fn a_worker_spawned_onto_a_degraded_account_reports_it() {
        let (workspace, _config_dir) = stub_with_project_cap(2);
        let project = seeded_project(&workspace);
        workspace.account_pool().set_loading(
            &forge_gateway::AccountKey("Stargate".to_owned()),
            forge_gateway::LoadingState::Bailed,
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project.clone(),
            WorkerSpawnArgs {
                label: "w1".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let reply = rx.await.expect("reply").expect("the spawn is admitted");
        assert_eq!(
            reply.rate_limited_account.as_deref(),
            Some("Stargate"),
            "the bailed account is named in the reply, which is what raises the notice",
        );
        workspace.release_session(&SessionSlot::from_str_for_test(reply.session_id));
    }

    /// The atomicity pin, at the layer it lives: concurrent
    /// `insert_live_worker_if_label_absent` calls at cap 1 admit
    /// exactly one and refuse the rest, whatever order the lock grants.
    /// Direct inserts, so no spawn machinery can retire an entry
    /// mid-race. A barrier releases all callers at once; the count only
    /// stays at 1 if the cap check and the push share the lock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cap_gate_admits_exactly_one() {
        let (workspace, _config_dir) = stub_with_project_cap(1);
        let project = seeded_project(&workspace);
        let barrier = Arc::new(tokio::sync::Barrier::new(32));

        let mut handles = Vec::new();
        for n in 0..32 {
            let workspace = Arc::clone(&workspace);
            let project = project.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                let entry = fake_worker_entry(&format!("w{n}"), &format!("w{n}-session"));
                workspace.insert_live_worker_if_label_absent(&project, entry, Some(1))
            }));
        }

        let mut admitted = 0;
        let mut refused = 0;
        for handle in handles {
            match handle.await.expect("insert task joins") {
                Ok(()) => admitted += 1,
                Err(LiveWorkerRefusal::AtCap { live, cap }) => {
                    refused += 1;
                    assert_eq!((live, cap), (1, 1), "the refusal names the count and the cap");
                }
                Err(other) => panic!("no label collisions at distinct labels: {other:?}"),
            }
        }
        assert_eq!(admitted, 1, "exactly one insert wins the cap");
        assert_eq!(refused, 31, "every other insert is refused by the cap");
        assert_eq!(
            workspace.list_live_workers(&project).len(),
            1,
            "the pool holds exactly the one admitted entry"
        );
    }

    /// The same gate through the full handler: eight concurrent
    /// lead-driven spawns at cap 1, and the live (non-`Failed`) count
    /// never exceeds 1.
    ///
    /// Deliberately NOT "exactly one Ok": a winner whose subprocess
    /// dies asynchronously is transitioned to `Failed` by the
    /// spawn-failure handler, which frees its slot for a queued spawn -
    /// the count invariant survives, the reply count does not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_spawns_keep_the_live_count_under_the_cap() {
        let (workspace, _config_dir) = stub_with_project_cap(1);

        let mut handles = Vec::new();
        for n in 0..8 {
            let workspace = Arc::clone(&workspace);
            handles.push(tokio::spawn(async move {
                let project = seeded_project(&workspace);
                let (tx, rx) = tokio::sync::oneshot::channel();
                handle_spawn_worker(
                    &workspace,
                    project,
                    WorkerSpawnArgs {
                        label: format!("w{n}"),
                        charter: "charter".to_owned(),
                        kick: None,
                        resume_kick: None,
                        interactive: false,
                    },
                    SessionSlot::from_str_for_test("lead"),
                    None,
                    false,
                    tx,
                );
                rx.await.expect("reply")
            }));
        }

        let mut winners = 0;
        let mut winner_sessions = Vec::new();
        for handle in handles {
            match handle.await.expect("spawn task joins") {
                Ok(reply) => {
                    winners += 1;
                    winner_sessions.push(SessionSlot::from_str_for_test(reply.session_id));
                }
                Err(message) => {
                    assert!(
                        message.contains("worker limit reached"),
                        "the only refusal is the cap, never a collision or other error: {message}"
                    );
                }
            }
        }
        assert!(winners >= 1, "the first arrival always gets the free slot");
        let project = seeded_project(&workspace);
        let live = workspace.list_live_workers(&project).iter().filter(|w| w.is_live()).count();
        assert!(live <= 1, "no overshoot past the cap; got {live} live workers");
        // Release closes each winner's Command channel, so the
        // SessionTask's disconnect() reaps the client; kill_on_drop is
        // the backstop.
        for session in winner_sessions {
            workspace.release_session(&session);
        }
    }

    /// #976's open question, decided: the boot-time respawn wave is
    /// EXEMPT from the cap. Persisted workers are state the user
    /// already had, and the boot path drops the spawn reply, so an
    /// over-limit refusal there would strand rows silently with no
    /// retry. The exempt spawn proceeds past the cap.
    #[tokio::test]
    async fn boot_respawn_spawn_is_exempt_from_the_limit() {
        let (workspace, _config_dir) = stub_with_project_cap(2);
        let project = seeded_project(&workspace);
        workspace.insert_live_worker(&project, fake_worker_entry("w1", "w1"));
        workspace.insert_live_worker(&project, fake_worker_entry("w2", "w2"));

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project.clone(),
            WorkerSpawnArgs {
                label: "w3".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            true,
            tx,
        );

        let reply = rx.await.expect("reply");
        assert!(reply.is_ok(), "boot respawn must not be refused by the cap: {:?}", reply.err());
        assert_eq!(
            workspace.list_live_workers(&project).len(),
            3,
            "the exempt spawn creates its worker"
        );
        if let Ok(reply) = reply {
            workspace.release_session(&SessionSlot::from_str_for_test(reply.session_id));
        }
    }

    /// #976: a despawn frees its slot; the next lead-driven spawn
    /// succeeds.
    #[tokio::test]
    async fn despawn_frees_a_spawn_slot() {
        let (workspace, _config_dir) = stub_with_project_cap(1);
        let project = seeded_project(&workspace);
        workspace.insert_live_worker(&project, fake_worker_entry("w1", "w1"));

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project.clone(),
            WorkerSpawnArgs {
                label: "w2".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let err = rx.await.expect("reply").expect_err("at-limit spawn must refuse");
        assert!(err.contains("worker limit reached"), "names the refusal: {err}");

        let (despawn_tx, despawn_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project, "w1", false, despawn_tx);
        let outcome = despawn_rx.await.expect("despawn reply");
        assert!(
            matches!(outcome, crate::protocol::DespawnResult::Despawned { .. }),
            "the worker despawns: {outcome:?}"
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project.clone(),
            WorkerSpawnArgs {
                label: "w2".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let reply = rx.await.expect("reply");
        assert!(reply.is_ok(), "the freed slot lets the next spawn through: {:?}", reply.err());
        if let Ok(reply) = reply {
            workspace.release_session(&SessionSlot::from_str_for_test(reply.session_id));
        }
    }

    /// A `Failed` worker holds no cap slot, matching the label-dedup
    /// which already treats `Failed` as not-live. One connection
    /// failure must not wedge every later spawn until a manual despawn.
    #[tokio::test]
    async fn a_failed_worker_does_not_consume_a_cap_slot() {
        let (workspace, _config_dir) = stub_with_project_cap(1);
        let project = seeded_project(&workspace);
        let mut failed = fake_worker_entry("dead", "dead-1");
        failed.status = forge_primitives::WorkerLiveness::Failed;
        workspace.insert_live_worker(&project, failed);

        let (tx, rx) = tokio::sync::oneshot::channel();
        handle_spawn_worker(
            &workspace,
            project,
            WorkerSpawnArgs {
                label: "fresh".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead"),
            None,
            false,
            tx,
        );
        let reply = rx.await.expect("reply");
        assert!(reply.is_ok(), "the Failed worker must not consume the slot: {:?}", reply.err());
        if let Ok(reply) = reply {
            workspace.release_session(&SessionSlot::from_str_for_test(reply.session_id));
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

    /// A stub over a configured `proj-x` with a real store, plus the key
    /// that project resolves under and the update receiver the stub minted.
    /// The despawn fall-through needs the store: it reads and writes the
    /// worker's durable row, so a stub with no DB never reaches the arm.
    struct StoreBackedStub {
        workspace: Arc<Workspace>,
        project: ProjectKey,
        rx: tokio::sync::mpsc::UnboundedReceiver<SessionUpdate>,
        /// Held so the store outlives the test.
        _dir: tempfile::TempDir,
    }

    fn store_backed_stub() -> StoreBackedStub {
        let (workspace, rx) = Workspace::testing_stub();
        workspace.seed_test_project("proj-x", "/tmp/proj-x");
        let dir = tempdir().expect("tempdir");
        workspace.install_db_for_test(
            crate::store::Db::open(&dir.path().join("db.redb")).expect("open db"),
        );
        let project = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/proj-x")),
        );
        StoreBackedStub { workspace, project, rx, _dir: dir }
    }

    /// A durable row can outlive its live worker - the tag-write rollback
    /// removes the entry without the row - and the row is then the only
    /// handle on a worker the next boot re-spawns. Despawn is the tool a
    /// lead reaches for to remove a durable worker, so it must clear the
    /// row rather than report NotFound over it (#1142).
    #[tokio::test]
    async fn despawn_clears_a_stranded_row_with_no_live_worker() {
        let StoreBackedStub { workspace, project, mut rx, .. } = store_backed_stub();
        workspace
            .record_worker_row(
                &project,
                "stranded",
                "stranded-id",
                "charter",
                Some("kick"),
                None,
                false,
                false,
            )
            .expect("seed the row whose live worker is gone");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project, "stranded", false, tx);
        let result = resp_rx.await.expect("despawn result");

        assert!(
            matches!(result, crate::protocol::DespawnResult::Despawned { .. }),
            "clearing the row is a despawn, not a NotFound over the row it just removed: \
             {result:?}",
        );
        assert!(
            workspace.worker_rows_for_project(&project).is_empty(),
            "the stranded row must not survive the despawn that reported the worker gone",
        );
        assert!(
            rx.try_recv().is_err(),
            "nothing live was torn down, so there is no Removed event to emit",
        );
    }

    /// A despawn takes the worker's durable records with it, and a
    /// stranded row's label can still own subscriptions and crons from
    /// before it was orphaned. Clearing the row alone would report a
    /// completed despawn while leaving exactly the records the live path
    /// exists to clear.
    #[tokio::test]
    async fn despawn_clears_a_stranded_workers_subscriptions_and_crons() {
        let StoreBackedStub { workspace, project, .. } = store_backed_stub();
        workspace
            .record_worker_row(&project, "stranded", "stranded-id", "c", None, None, false, false)
            .expect("seed the stranded row");
        let sub = forge_primitives::GotifySubscription {
            id: uuid::Uuid::new_v4(),
            project: "proj-x".to_owned(),
            team_role: Some("stranded".to_owned()),
            applications: Vec::new(),
            min_priority: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        };
        workspace.add_gotify_subscription(sub.clone(), true);
        workspace.push_cron(forge_primitives::CronEntry {
            id: forge_primitives::CronId::from("stranded-cron"),
            project_name: "proj-x".to_owned(),
            kind: forge_primitives::CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: "stand-up".to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: None,
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: Some("stranded".to_owned()),
        });

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project, "stranded", false, tx);
        assert!(
            matches!(
                resp_rx.await.expect("result"),
                crate::protocol::DespawnResult::Despawned { .. }
            ),
            "the row was cleared, so the despawn reports the worker gone",
        );

        assert!(
            workspace.gotify_subscriptions_for_project("proj-x").iter().all(|s| s.id != sub.id),
            "the label's subscription goes with the worker it was registered for",
        );
        assert!(
            workspace.crons_for_project("proj-x").is_empty(),
            "and so do its crons, which would otherwise keep firing for a label with no worker",
        );
    }

    /// `lead` is the project lead's own row, not a worker: it is the
    /// stored id an ordinary boot resumes the lead from. A despawn for it
    /// finds no live worker, and the fall-through must not treat that as a
    /// stranded worker row and clear it.
    #[tokio::test]
    async fn despawn_refuses_to_clear_the_lead_row() {
        let StoreBackedStub { workspace, project, mut rx, .. } = store_backed_stub();
        workspace
            .record_worker_row(
                &project,
                crate::store::sessions::LEAD_LABEL,
                "lead-uuid",
                "the lead's charter",
                None,
                None,
                false,
                false,
            )
            .expect("seed the lead's row");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project, crate::store::sessions::LEAD_LABEL, false, tx);

        assert!(
            matches!(resp_rx.await.expect("result"), crate::protocol::DespawnResult::NotFound),
            "there is no worker by that label, so the answer is NotFound",
        );
        // Read the row directly: `worker_rows_for_project` excludes the
        // lead's row by design, so it cannot witness this either way.
        let stored = {
            let db = workspace.db.lock();
            crate::store::sessions::get(
                db.as_ref().expect("db"),
                "TestOrg",
                "proj-x",
                crate::store::sessions::LEAD_LABEL,
            )
            .expect("read")
        };
        assert!(
            stored.is_some(),
            "the lead's row must survive a despawn for its label; cleared, the next boot mints a \
             fresh id and orphans the lead's conversation",
        );
        assert!(rx.try_recv().is_err(), "nothing live was torn down, so no Removed event");
    }

    /// Despawning an unknown label reports NotFound and emits nothing. The
    /// store is real, so the answer comes from the fall-through's own
    /// "no row there" arm rather than from a stub that cannot reach it.
    #[tokio::test]
    async fn despawn_unknown_label_reports_not_found() {
        let StoreBackedStub { workspace, project, mut rx, .. } = store_backed_stub();
        // A sibling row, so "nothing was cleared" cannot pass because the
        // store was empty.
        workspace
            .record_worker_row(&project, "other", "other-id", "c", None, None, false, false)
            .expect("seed a row the despawn must leave alone");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project, "missing", false, tx);

        assert!(matches!(resp_rx.await.expect("result"), crate::protocol::DespawnResult::NotFound));
        assert_eq!(
            workspace.worker_rows_for_project(&project).len(),
            1,
            "and the sibling row is untouched",
        );
        assert!(rx.try_recv().is_err(), "no events for unknown label");
    }

    /// A despawn whose store cannot be read or written is not a despawn of
    /// an absent worker: reporting NotFound there sends the caller looking
    /// for a worker that may be right there, with the row still on disk.
    #[tokio::test]
    async fn despawn_reports_a_store_failure_apart_from_an_absent_label() {
        let (workspace, _rx) = Workspace::testing_stub();
        // A configured project with no store: the shape of a forge that
        // came up without its database.
        workspace.seed_test_project("proj-x", "/tmp/proj-x");
        let project = ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/proj-x")),
        );

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project, "stranded", false, tx);

        let result = resp_rx.await.expect("result");
        let crate::protocol::DespawnResult::Failed { reason } = result else {
            panic!("an unreadable store must not be reported as an absent row; got {result:?}");
        };
        assert!(
            reason.contains("stranded"),
            "the failure names the label it could not clear: {reason}",
        );
    }

    /// The same failure at the surface a caller actually reads: the facade
    /// must hand it back as a failed despawn, since `workers__despawn`
    /// renders `UnknownLabel` as "no live worker with label ...", which
    /// sends the lead looking for a worker that is right there.
    #[tokio::test]
    async fn a_despawn_store_failure_is_not_an_unknown_label() {
        let dir = tempdir().expect("tempdir");
        write_forge_toml(dir.path(), FIXTURE_PROJECT_PATH);
        let config = crate::config::load_from_dir(dir.path()).expect("load config");
        // No db installed: the shape of a forge that came up without its
        // store, where whether a row exists cannot be answered at all.
        let (workspace, _rx) = Workspace::testing_stub_with_config(dir.path().to_owned(), config)
            .expect("stub over the fixture config");
        let facade = crate::mcp::workers::facade::ProdWorkerFacade::from_arc(&workspace);

        let error = facade
            .despawn_worker(&SessionSlot::lead("Default", "forge"), "stranded", false)
            .await
            .expect_err("a store that cannot be read is not a despawn of an absent worker");

        assert!(
            matches!(error, crate::mcp::workers::facade::WorkerDespawnError::DispatchFailed { .. }),
            "the caller must hear a failed despawn rather than an unknown label: {error:?}",
        );
    }

    /// Build a workspace whose single project points at a temp git
    /// repo, add a worktree at `<project>/.claude/worktrees/<label>`,
    /// and register a live git-worker under it. The worktree is built
    /// from the project path `list_projects` reports so the handler's
    /// `worker_tag_dir` resolves to the exact on-disk worktree (avoids
    /// the macOS /tmp symlink mismatch). Returns the pieces the test
    /// asserts on plus the tempdir guards (which must outlive the test).
    fn git_despawn_fixture(
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
                "[[orgs]]\nname = \"Default\"\naccounts = [\"Stargate\"]\n\n[[orgs.projects]]\nname = \"forge\"\npath = \"{repo_path_str}\"\n\n[[accounts]]\ndisplay_name = \"Stargate\"\ntoken = \"t\"\nmodels = [\"claude-sonnet-5\"]\nprovider = \"anthropic\"\n"
            ),
        )
        .expect("write forge.toml");

        let workspace =
            Arc::new(Workspace::new_for_test(config.path().to_owned()).expect("workspace new"));
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
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer");
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

    /// The git fixture with its live entry gone and a durable row left
    /// behind: the shape the tag-write rollback leaves, and the one a
    /// despawn has to reach without a live worker to ask.
    fn stranded_git_despawn_fixture(
        label: &str,
    ) -> (Arc<Workspace>, ProjectKey, std::path::PathBuf, tempfile::TempDir, tempfile::TempDir)
    {
        let (workspace, project_key, wt, repo, config) = git_despawn_fixture(label);
        workspace.remove_latest_worker(&project_key, label);
        workspace
            .record_worker_row(
                &project_key,
                label,
                "stranded-id",
                "c",
                Some("kick"),
                None,
                false,
                // The row is what says this worker runs in a worktree, so
                // it is what the despawn has to read the gitness from.
                true,
            )
            .expect("seed the row that outlived its worker");
        (workspace, project_key, wt, repo, config)
    }

    /// A stranded git worker is still a worker the tool must clean up
    /// after: the row carries the gitness, so the despawn removes the
    /// worktree and reaps the branch with no live entry to ask. Skipped,
    /// the worktree is orphaned once the row is gone - nothing is left
    /// for a later despawn to resolve it from.
    #[tokio::test]
    async fn despawn_clears_a_stranded_git_workers_worktree() {
        let (workspace, project_key, wt, repo, _config) = stranded_git_despawn_fixture("reviewer");
        assert!(wt.exists(), "the worktree exists before the despawn");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);
        let result = resp_rx.await.expect("result");

        assert!(
            matches!(
                result,
                crate::protocol::DespawnResult::Despawned {
                    worktree_cleanup_warning: None,
                    branch_cleanup_warning: None,
                }
            ),
            "a stranded worker's clean worktree is removed like any other: {result:?}"
        );
        assert!(!wt.exists(), "the stranded worktree is removed, not orphaned");
        assert!(
            !branch_exists(repo.path(), "worktree-reviewer"),
            "and the branch behind it is reaped"
        );
        assert!(
            workspace.worker_rows_for_project(&project_key).is_empty(),
            "with the row it was resolved from",
        );
    }

    /// A stranded row whose worktree is already gone is the shape the boot
    /// wave skips, so the despawn has nothing to remove and must not report
    /// a removal failure over it. The live shape reports git's failure for
    /// the same state, which `despawn_reports_removed_when_the_worktree_is_
    /// already_off_disk` pins.
    #[tokio::test]
    async fn a_stranded_row_whose_worktree_is_gone_despawns_quietly() {
        let (workspace, project_key, wt, repo, _config) = stranded_git_despawn_fixture("reviewer");
        run_git(repo.path(), &["worktree", "remove", wt.to_str().expect("utf8 path")]);
        assert!(!wt.exists(), "nothing on disk before the despawn");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", true, tx);
        let result = resp_rx.await.expect("result");

        assert!(
            matches!(
                result,
                crate::protocol::DespawnResult::Despawned {
                    worktree_cleanup_warning: None,
                    branch_cleanup_warning: None,
                }
            ),
            "a worktree that was never there is not a cleanup failure: {result:?}"
        );
        assert!(
            workspace.worker_rows_for_project(&project_key).is_empty(),
            "and the row still goes, which is the whole point of the call",
        );
    }

    /// A dirty stranded worktree blocks before anything is cleared, the
    /// same way a live one does: the row survives, so a later despawn (or
    /// `force`) still has the worker to act on.
    #[tokio::test]
    async fn a_dirty_stranded_worktree_blocks_before_the_row_goes() {
        let (workspace, project_key, wt, _repo, _config) = stranded_git_despawn_fixture("reviewer");
        std::fs::write(wt.join("uncommitted.txt"), "work in progress").expect("dirty the worktree");

        let (tx, resp_rx) = tokio::sync::oneshot::channel();
        handle_despawn_worker(&workspace, &project_key, "reviewer", false, tx);

        assert!(
            matches!(
                resp_rx.await.expect("result"),
                crate::protocol::DespawnResult::Blocked { .. }
            ),
            "a dirty worktree blocks the despawn",
        );
        assert!(wt.exists(), "and the worktree is left alone");
        assert_eq!(
            workspace.worker_rows_for_project(&project_key).len(),
            1,
            "a blocked despawn clears nothing, so the row it would have taken is still there",
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
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer");
        let mut rx = workspace.subscribe().expect("subscribe");

        handle_close_worker(&workspace, &project_key, "reviewer");

        assert_eq!(drain_removed_dispositions(&mut rx), vec![WorktreeDisposition::Intact]);
        assert!(wt.exists(), "the x button does not touch the worktree");
    }

    /// A despawn that removed the worktree says so on the event the
    /// close toast is built from.
    #[tokio::test]
    async fn despawn_reports_the_worktree_removed() {
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer");
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
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer");
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
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer");
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
        write_forge_toml(config.path(), FIXTURE_PROJECT_PATH);
        let workspace =
            Arc::new(Workspace::new_for_test(config.path().to_owned()).expect("workspace new"));

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
            WorkerSpawnArgs {
                label: "reviewer".to_owned(),
                charter: "charter".to_owned(),
                kick: None,
                resume_kick: None,
                interactive: false,
            },
            SessionSlot::from_str_for_test("lead-uuid"),
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
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer");
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
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer");
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
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer");
        run_git(&wt, &["checkout", "-q", "-b", "feature-x"]);
        std::fs::write(wt.join("work.txt"), "the worker's real work").expect("write work");
        run_git(&wt, &["add", "."]);
        run_git(&wt, &["commit", "-q", "-m", "real work"]);
        workspace.save_review_threads("forge", "feature-x", &[review_thread("a")]);
        workspace
            .submit_review(
                "forge",
                "feature-x",
                None,
                &[],
                SessionSlot::from_str_for_test("reviewer"),
            )
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
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer");
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
        let (workspace, project_key, wt, repo, _config) = git_despawn_fixture("reviewer");
        let branch = forge_agent::env::worktree::worktree_branch(&wt).expect("worktree branch");
        let thread = review_thread;
        workspace.save_review_threads("forge", &branch, &[thread("a")]);
        workspace.save_review_threads("forge", "survivor", &[thread("b")]);
        // Seal a review on each branch (empty thread set just mints the row).
        let origin = SessionSlot::from_str_for_test("reviewer");
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
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer");
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
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer");
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
        let (workspace, project_key, wt, _repo, _config) = git_despawn_fixture("reviewer");
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
        let caller = SessionSlot::from_str_for_test("caller-1");

        handle_deliver_worker_prompt(&workspace, caller, &project, "missing", fixture_wrapped());
        // No panic, no dispatch attempted. (We can't easily observe
        // "no dispatch" without a stubbed dispatch; the absence of a
        // panic + dropped channels is the test.)
    }

    /// A worker addressed before it Connects (session_id still None)
    /// must have the prompt parked for its label, not dropped - its
    /// Connected handler drains the bucket.
    #[tokio::test]
    async fn deliver_to_unconnected_worker_buffers_the_prompt() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("forge", "/tmp/deliver-unconnected");
        let project = workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("seeded project")
            .key;
        // The slot production derives for the seeded project's worker.
        let worker_key = SessionSlot::worker("TestOrg", "forge", "builder");
        workspace.insert_live_worker(
            &project,
            crate::mcp::workers::types::WorkerEntry {
                label: "builder".into(),
                charter: "c".into(),
                slot: worker_key.clone(),
                session_id: None,
                status: forge_primitives::WorkerLiveness::Spawning,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead("TestOrg", "forge"),
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
            SessionSlot::lead("TestOrg", "forge"),
            &project,
            "builder",
            fixture_wrapped(),
        );

        assert_eq!(
            workspace
                .parked_by_slot
                .lock()
                .get(&crate::SessionSlot::worker("TestOrg", "forge", "builder"))
                .map_or(0, |parked| parked.peer.len()),
            1,
            "prompt parked for the worker's Connected drain, not dropped"
        );
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
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
            ),
        )
        .expect("write forge.toml");
        let workspace = Arc::new(Workspace::new_for_test(toml_dir.path().to_owned()).expect("new"));

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
        // The slot production derives for the worker under this project.
        let session_key = SessionSlot::worker(&project_view.org, &project_view.name, "idle");

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
                slot: session_key.clone(),
                // The entry carries the id the worker runs under: the
                // opportunistic tag retry reads it from here.
                session_id: Some(forge_primitives::SessionId::new(&session_id)),
                status: forge_primitives::WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by: SessionSlot::lead(&project_view.org, &project_view.name),
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
        let caller = SessionSlot::from_str_for_test("caller-1");
        handle_deliver_worker_prompt(&workspace, caller, &project_key, "idle", fixture_wrapped());

        // Wait briefly for the spawned task to finish the retry +
        // status update. The retry should succeed on first attempt
        // because the JSONL exists.
        let needs_tag = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let entries = workspace.list_live_workers(&project_key);
                let entry = entries.iter().find(|e| e.slot == session_key);
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
                caller: SessionSlot::from_str_for_test("lead-uuid"),
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

    /// A worker closed while it was still spawning drops whatever was buffered
    /// for it. The delivery already committed, so nothing re-delivers it, and
    /// the release is the last point that can record the loss.
    #[test]
    fn close_worker_records_the_delivery_parked_for_its_label() {
        let (workspace, _rx) = Workspace::testing_stub();
        workspace.seed_test_project("forge", "/tmp/close-parked");
        let project = workspace
            .list_projects()
            .into_iter()
            .find(|v| v.name == "forge")
            .expect("seeded project")
            .key;
        // Teardown expires the parked bucket by the entry's slot, so the
        // fixture carries the one production derives for this worker.
        let worker_key = SessionSlot::worker("TestOrg", "forge", "reviewer");
        let mut entry = fake_worker_entry("reviewer", "worker-1");
        entry.slot = worker_key.clone();
        workspace.insert_live_worker(&project, entry);
        workspace.register_domain_session(worker_key.clone(), None);
        workspace.park_slack(&worker_key, slack_msg("buffered while spawning"));

        handle_close_worker(&workspace, &project, "reviewer");

        assert_eq!(
            workspace.parked_by_slot.lock().get(&worker_key).map_or(0, |parked| parked.slack.len()),
            0,
            "the closed worker's parked delivery is recorded, not left for a later session",
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
            DEFAULT_LEAD_CHARTER.contains("only when the next stage needs the context back"),
            "chain despawn closes at absorption; re-spawn waits for the next stage: {DEFAULT_LEAD_CHARTER}",
        );
    }
}

//! How the overlay opens and closes: target resolution, the spawned
//! scans and their event channel, the drain pump that lands results,
//! and the install/drop of the overlay state on `App`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

use super::state::DiffOverlayState;
use super::threads::hydrate_threads;
use super::types::{DiffOverlayEvent, DiffScanKind, DiffScope, NavOutcome};
use crate::app::App;
use crate::app::view::{ActiveView, set_active_view};
use forge_primitives::git_diff::RepoGate;
use forge_workspace::env::git_diff::hunks::ScanOutcome;

/// Which scope the initial `/diff` open should land on, resolved from the
/// branch's persisted review threads so a reopen shows the user's
/// comments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitialScope {
    /// A whole-diff thread exists; open on "All changes".
    WholeDiff,
    /// Only commit-scoped threads exist; open on the commit carrying the
    /// most-recently-updated one (its sha).
    Commit(String),
    /// No threads; open on the first commit when the branch has commits
    /// ahead, else whole-diff.
    Default,
}

/// Pick the initial scope from the branch's persisted threads: whole-diff
/// when any whole-diff thread exists (the pre-scope behavior), else the
/// commit carrying the most-recently-updated comment, else the default.
fn initial_scope_from_threads(
    threads: &[forge_primitives::ReviewThread],
) -> InitialScope {
    if threads.iter().any(|t| t.commit.is_none()) {
        return InitialScope::WholeDiff;
    }
    threads
        .iter()
        .max_by(|a, b| a.updated_at.cmp(&b.updated_at))
        .and_then(|t| t.commit.clone())
        .map_or(InitialScope::Default, InitialScope::Commit)
}

/// Resolve an [`InitialScope`] against a freshly-scanned commit list into
/// the commit to open (index + sha), or `None` for whole-diff. A chosen
/// sha no longer in the list falls back to the first commit.
fn resolve_initial_commit(
    initial: &InitialScope,
    commits: &[forge_workspace::env::git_diff::hunks::CommitMeta],
) -> Option<(usize, String)> {
    match initial {
        InitialScope::WholeDiff => None,
        InitialScope::Default => commits.first().map(|c| (0, c.sha.clone())),
        InitialScope::Commit(sha) => commits
            .iter()
            .position(|c| &c.sha == sha)
            .map(|idx| (idx, sha.clone()))
            .or_else(|| commits.first().map(|c| (0, c.sha.clone()))),
    }
}

/// The branch the overlay reviews under, read live from the checkout
/// being diffed. Same `git rev-parse` against the same
/// `git_scan_cwd_for_session`-resolved path the review MCP's
/// `resolve_scope` queries, so a review filed here is keyed exactly
/// where its reader looks. `None` on a detached HEAD or a failed read.
async fn review_branch(cwd: &Path) -> Option<String> {
    match forge_workspace::env::git_diff::current_branch(cwd).await {
        Ok(branch) => branch,
        Err(gate) => {
            tracing::warn!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_branch_unresolved",
                message = "git reported no branch for the diff checkout; a review filed here cannot be keyed or read back",
                outcome = "degraded",
                cwd = %cwd.display(),
                gate = ?gate,
            );
            None
        }
    }
}

/// Spawn the initial `/diff` scan and post a [`DiffOverlayEvent`] when
/// it completes. Best-effort send - receiver going away (app shutdown)
/// just drops the result. Resolves the review branch off `cwd`, then
/// scans the commit list and picks the landing scope from that branch's
/// persisted threads: a commit scope scans that commit's diff upfront
/// (the rest lazily on navigation) and opens on it; otherwise it scans
/// the whole diff and opens whole-diff mode.
pub fn spawn_fetch(
    cwd: PathBuf,
    target: String,
    project: Option<String>,
    workspace: Option<Arc<forge_workspace::Workspace>>,
    seq: u64,
    tx: std_mpsc::Sender<DiffOverlayEvent>,
) {
    tokio::task::spawn_local(async move {
        let branch = review_branch(&cwd).await;
        // Open on the scope that holds the branch's persisted review
        // threads, so a reopen lands where the user's comments are
        // instead of the first commit. A load failure just falls back to
        // the default scope; the post-open `hydrate_threads` hits the
        // same error and surfaces the notice against the open overlay.
        let initial = match (&project, &branch, &workspace) {
            (Some(project), Some(branch), Some(workspace)) => workspace
                .load_review_threads(project, branch)
                .map_or(InitialScope::Default, |threads| initial_scope_from_threads(&threads)),
            _ => InitialScope::Default,
        };
        let commits = forge_workspace::env::git_diff::hunks::scan_commits(&cwd, &target).await;
        // Resolve the initial scope against the freshly-scanned commits:
        // `Some((idx, sha))` opens on that commit (its diff scanned
        // upfront), `None` opens the whole-branch diff.
        let open_commit = resolve_initial_commit(&initial, &commits);
        let (files, scanner_ok, untracked_suppressed, commit_body, scope) =
            if let Some((idx, sha)) = open_commit {
                let o = forge_workspace::env::git_diff::hunks::scan_commit(&cwd, &sha).await;
                let body =
                    forge_workspace::env::git_diff::hunks::scan_commit_body(&cwd, &sha).await;
                (o.files, o.scanner_ok, 0, Some(body), DiffScope::Commit(idx))
            } else {
                let ScanOutcome { files, scanner_ok, untracked_suppressed } =
                    forge_workspace::env::git_diff::hunks::scan(&cwd, &target).await;
                (files, scanner_ok, untracked_suppressed, None, DiffScope::WholeDiff)
            };
        let _ = tx.send(DiffOverlayEvent {
            cwd,
            target,
            files,
            scanner_ok,
            untracked_suppressed,
            seq,
            kind: DiffScanKind::Initial { commits, branch, scope },
            commit_body,
        });
    });
}

/// Spawn a lazy scan for one scope (a commit's own diff, or the whole
/// branch for "All changes") and post it back as a
/// [`DiffScanKind::Scope`] event. `sha` is `Some` for a commit scope,
/// `None` for whole-diff (which scans `target`). Reuses the current
/// `seq` (no bump) so a scope scan spawned during navigation is dropped
/// only if a fresh `/diff` supersedes the whole overlay.
fn spawn_scope_fetch(
    cwd: PathBuf,
    target: String,
    scope: DiffScope,
    sha: Option<String>,
    seq: u64,
    tx: std_mpsc::Sender<DiffOverlayEvent>,
) {
    tokio::task::spawn_local(async move {
        let (outcome, commit_body) = match &sha {
            Some(sha) => {
                let outcome = forge_workspace::env::git_diff::hunks::scan_commit(&cwd, sha).await;
                let body = forge_workspace::env::git_diff::hunks::scan_commit_body(&cwd, sha).await;
                (outcome, Some(body))
            }
            None => (forge_workspace::env::git_diff::hunks::scan(&cwd, &target).await, None),
        };
        let ScanOutcome { files, scanner_ok, untracked_suppressed } = outcome;
        let _ = tx.send(DiffOverlayEvent {
            cwd,
            target,
            files,
            scanner_ok,
            untracked_suppressed,
            seq,
            kind: DiffScanKind::Scope(scope),
            commit_body,
        });
    });
}

/// Outcome of resolving the default `/diff` target from the active
/// session's Inspector GIT snapshot. Distinguishes every "nothing
/// to open" case so the caller can surface a specific system-
/// message rather than collapsing distinct failures onto a single
/// "no changes" line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefaultTarget {
    /// Resolved a concrete ref to diff against (`"HEAD"` for the
    /// worktree case, the default branch name for the clean
    /// feature-branch case).
    Ref(String),
    /// Inspector GIT scanner hasn't produced a snapshot yet. The
    /// poll fires ~10s after session start; a fresh-launch user
    /// who hits `/diff` immediately can land here.
    NoSnapshot,
    /// Active session's cwd isn't inside a git repository.
    NotARepo,
    /// The Inspector scanner itself failed (subprocess crash,
    /// timeout, oversize output). Distinct from `NotARepo` because
    /// the user IS in a repo; git just couldn't run. The snapshot's
    /// `repo_gate` is `RepoGate::ScannerFailed`.
    ScannerFailed,
    /// Snapshot has `branch_ahead` populated (so the scanner sees
    /// committed work) but the default branch itself couldn't be
    /// resolved - no `origin/HEAD`, no local `main`, no local
    /// `master`. Distinct from `Clean` because there ARE changes;
    /// we just don't know which ref to compare against. User needs
    /// to pass an explicit `/diff <ref>`.
    ///
    /// In the current scan logic this is structurally unreachable
    /// because `branch_ahead` is only constructed when
    /// `default_branch` resolved. Kept as a defensive case so a
    /// future refactor that decouples the two doesn't accidentally
    /// collapse this into `Clean`.
    NoDefault,
    /// Working tree is clean against the resolved default branch.
    /// Genuine "no changes". Branch name is surfaced in the
    /// system notice when known so the user knows what they're
    /// (not) diffing against.
    Clean { default_branch: Option<String> },
}

/// Resolve the default `/diff` target from the active session's
/// Inspector GIT snapshot. Mirrors the auto-detect logic the `/diff`
/// slash command uses; shared with the Inspector `🦉` click path.
///
/// Known race: the snapshot can be up to ~10 s stale because the
/// inspector's git-diff scanner polls on that cadence. If the user
/// switches branches and clicks `🦉` within that window, the resolved
/// target may not match the live working tree. Mitigation: the scan
/// itself ALWAYS runs fresh - only the *target ref* (e.g. `main` vs
/// `master`) can be wrong. Worst-case the user sees "no changes" and
/// reruns `/diff <ref>` explicitly. Not worth the synchronous
/// refresh cost on the click hot-path.
pub fn resolve_default_target(app: &App) -> DefaultTarget {
    let Some(snapshot) = app.active_session().and_then(|s| s.git_diff_snapshot.as_ref()) else {
        return DefaultTarget::NoSnapshot;
    };
    // Scanner crash and not-a-repo are distinct surfaces; map the gate
    // before any layer check.
    match snapshot.repo_gate {
        RepoGate::ScannerFailed => return DefaultTarget::ScannerFailed,
        RepoGate::NotARepo => return DefaultTarget::NotARepo,
        RepoGate::InRepo => {}
    }
    // Layer 1 wins when both layers are populated: a dirty tree is
    // what the user clicks `🦉` to inspect, and `HEAD` covers the
    // uncommitted edits. The committed-but-unmerged work
    // (`branch_ahead`) is reachable via an explicit `/diff <default>`
    // - auto-detect prefers the more-recent surface.
    if snapshot.worktree.is_populated() {
        return DefaultTarget::Ref("HEAD".to_owned());
    }
    if snapshot.branch_ahead.is_populated() {
        return match snapshot.default_branch.as_deref() {
            Some(default) => DefaultTarget::Ref(default.to_owned()),
            None => DefaultTarget::NoDefault,
        };
    }
    // No layer populated: clean tree on the default branch (or on a
    // branch with no commits ahead). The renderer hands the user
    // back the resolved default for context.
    DefaultTarget::Clean { default_branch: snapshot.default_branch.clone() }
}

/// Kick off a diff scan against `target` and post the result
/// through the overlay event channel. Pushes a system message
/// (via `app::slash::push_system_message`) on every failure path -
/// workspace not ready, no active session, empty cwd - so callers
/// don't need to handle that themselves. Used by `/diff <target>`
/// directly; `open_default` builds on top of it for the auto-detect
/// path.
pub fn open_with_target(app: &mut App, target: String) {
    let Some(cwd_raw) = app.active_session().map(|s| s.cwd_raw.clone()) else {
        crate::app::slash::push_system_message(app, "Cannot open diff: no active session.");
        return;
    };
    if cwd_raw.is_empty() {
        crate::app::slash::push_system_message(app, "Cannot open diff: active session has no cwd.");
        return;
    }
    let cwd = resolve_active_diff_cwd(app, &cwd_raw);
    let project = app.active_session().and_then(|s| s.project.clone());
    let workspace = app.workspace.clone();
    // Bump the seq before spawning so the new scan's events
    // outrank anything still in flight from an earlier /diff call.
    // Old events arriving on the channel after this bump will be
    // dropped by drain_events as superseded.
    app.diff_scan_seq = app.diff_scan_seq.wrapping_add(1);
    let seq = app.diff_scan_seq;
    spawn_fetch(cwd, target, project, workspace, seq, app.diff_overlay_event_tx.clone());
}

/// Resolve the cwd a diff scan should run against for the active
/// session. Workers spawned in a git repo run inside claude's
/// `--worktree <label>` fork at
/// `<project_root>/.claude/worktrees/<label>`, but `cwd_raw` varies
/// by lifecycle: fresh spawns carry the project root, resumed
/// sessions carry the worktree path itself.
/// `git_scan_cwd_for_session` anchors on the worker's project_key so
/// both lifecycle states converge on the same final path. Mirror its
/// call from `git_diff::apply_timer_tick` so the overlay opens
/// against the worker's branch, not the lead's. For lead sessions,
/// non-git workers, or any session not registered as a live worker,
/// `git_scan_cwd_for_session` returns `cwd_raw` unchanged.
fn resolve_active_diff_cwd(app: &App, cwd_raw: &str) -> PathBuf {
    let cwd_raw_path = PathBuf::from(cwd_raw);
    let Some(active_key) = app.active_session_key.as_ref() else {
        return cwd_raw_path;
    };
    debug_assert!(
        app.workspace.is_some(),
        "workspace unset after init (diff_overlay::resolve_active_diff_cwd); MVVM contract violated",
    );
    if let Some(workspace) = app.workspace.as_ref() {
        workspace.git_scan_cwd_for_session(active_key, &cwd_raw_path)
    } else {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "diff_overlay_workspace_unset",
            message = "App.workspace is None during diff overlay cwd resolution; using cwd_raw without worker-cwd resolution",
            outcome = "fallback",
            key = %active_key.as_str(),
        );
        cwd_raw_path
    }
}

/// Auto-detect the diff target from the Inspector GIT snapshot and
/// kick off a scan. Pushes a distinct system notice on each of the
/// "nothing to open" cases so the user sees something actionable
/// instead of a generic "no changes". Shared entry point for the
/// `/diff` slash command (no arg) and the Inspector `🦉` click.
pub fn open_default(app: &mut App) {
    match resolve_default_target(app) {
        DefaultTarget::Ref(target) => open_with_target(app, target),
        DefaultTarget::NoSnapshot => {
            crate::app::slash::push_system_message(
                app,
                "Git scanner hasn't run yet - try /diff again in a moment.",
            );
        }
        DefaultTarget::NotARepo => {
            crate::app::slash::push_system_message(app, "Not a git repository.");
        }
        DefaultTarget::ScannerFailed => {
            crate::app::slash::push_system_message(
                app,
                "Git scanner hit an error - see tracing logs (target: agent.env_git). Try /diff again in a moment.",
            );
        }
        DefaultTarget::NoDefault => {
            crate::app::slash::push_system_message(
                app,
                "Branch has changes but the default ref couldn't be resolved (no origin/HEAD, no main, no master). Run /diff <ref> with an explicit target.",
            );
        }
        DefaultTarget::Clean { default_branch } => {
            let message = match default_branch {
                Some(name) => format!("No changes vs {name}."),
                None => "No changes vs HEAD.".to_owned(),
            };
            crate::app::slash::push_system_message(app, message);
        }
    }
}

/// Max events drained per main-loop tick. At most one scan is in
/// flight per `/diff` invocation in practice, but the bounded loop
/// matches the established pattern in `app::git_diff::drain_events`
/// and `app::file_index::drain_events` so a stalled producer can't
/// block the render loop arbitrarily long.
const EVENT_DRAIN_BUDGET: usize = 8;

/// Drain pending scan results and install the overlay state. Called
/// from the main loop alongside the other event-channel consumers.
///
/// Events are dropped (silently) when the user has navigated away
/// since the scan started:
/// - `app.active_view != ActiveView::Chat` - user opened config /
///   session picker / launchpad / another overlay while the scan
///   was running. Yanking them into the diff view would be
///   surprising.
/// - `event.cwd` doesn't match the active session's `cwd_raw` -
///   user switched sessions mid-scan; the result is for a stale
///   project, and crosstalking it into the new session would
///   confuse.
///
/// Both cases log at DEBUG so a future "why didn't /diff open?"
/// triage can correlate the event. No chat message is pushed -
/// the user explicitly navigated away, so a notice arriving later
/// would be noise. The user can rerun `/diff` if they want the
/// scan they kicked off.
pub fn drain_events(app: &mut App) {
    for _ in 0..EVENT_DRAIN_BUDGET {
        let event = match app.diff_overlay_event_rx.try_recv() {
            Ok(event) => event,
            Err(std_mpsc::TryRecvError::Empty | std_mpsc::TryRecvError::Disconnected) => return,
        };
        // Superseded by a newer /diff invocation - silent drop.
        // No user notice because they didn't navigate away or
        // close anything; they just retriggered and the older
        // scan's result is no longer relevant.
        if event.seq != app.diff_scan_seq {
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_drain_skipped_superseded",
                message = "diff scan completed after a newer /diff superseded it; dropping result",
                outcome = "skipped",
                target_ref = %event.target,
                event_seq = event.seq,
                latest_seq = app.diff_scan_seq,
            );
            continue;
        }
        // Comparison must use the SAME resolved cwd that the scan spawn
        // passed - the event's `cwd` echoes whatever the scanner
        // received. For worker sessions the scanner runs against the
        // worktree fork (`<project_root>/.claude/worktrees/<label>`),
        // not the raw `cwd_raw`, so comparing against `cwd_raw` would
        // silently drop every worker event.
        let active_cwd = app
            .active_session()
            .map(|s| s.cwd_raw.clone())
            .map(|raw| resolve_active_diff_cwd(app, &raw));
        if active_cwd.as_deref() != Some(event.cwd.as_path()) {
            // Silent drop - a scan for the OLD session crosstalking into
            // the now-active one would confuse. Rerun /diff explicitly.
            tracing::debug!(
                target: crate::logging::targets::APP_SESSION,
                event_name = "diff_overlay_drain_skipped_cwd",
                message = "diff scan completed but session cwd changed; dropping result",
                outcome = "skipped",
                scan_cwd = %event.cwd.display(),
                active_cwd = ?active_cwd,
            );
            continue;
        }
        if matches!(event.kind, DiffScanKind::Initial { .. }) {
            // Initial open: only land it while the user is still in chat
            // (they'd be surprised to be yanked into the overlay after
            // navigating away). Silent drop + DEBUG otherwise.
            if app.active_view != ActiveView::Chat {
                tracing::debug!(
                    target: crate::logging::targets::APP_SESSION,
                    event_name = "diff_overlay_drain_skipped_view",
                    message = "diff scan completed but active view changed; dropping result",
                    outcome = "skipped",
                    target_ref = %event.target,
                    active_view = ?app.active_view,
                );
                continue;
            }
            let state = DiffOverlayState::new_initial(event);
            open(app, state);
            // Load + re-anchor persisted threads for whatever scope the
            // initial open landed on.
            hydrate_threads(app);
        } else if let DiffScanKind::Scope(scope) = event.kind {
            // A lazy per-scope scan lands into the already-open overlay
            // (view == Diff). If it closed while the scan ran, drop it.
            if let Some(overlay) = app.diff_overlay.as_mut() {
                // `install_scan` swaps the files into view only when the
                // landed scope is the one currently shown; hydrate that
                // scope's persisted threads against those files. An
                // out-of-order scan that only cached off-scope is skipped -
                // hydrating it would re-anchor against the wrong files.
                let landed_current = overlay.scope == scope;
                overlay.install_scan(scope, event.files, event.scanner_ok, event.commit_body);
                app.needs_redraw = true;
                if landed_current {
                    hydrate_threads(app);
                }
            } else {
                tracing::debug!(
                    target: crate::logging::targets::APP_SESSION,
                    event_name = "diff_overlay_drain_skipped_closed",
                    message = "per-scope scan completed but the overlay was closed; dropping",
                    outcome = "skipped",
                    target_ref = %event.target,
                );
            }
        }
    }
}

/// Kick off the lazy scan for `scope` against the overlay's cwd/target,
/// reusing the current scan seq (no bump - it's the same overlay
/// session, not a fresh `/diff`).
/// After a navigation, spawn the scope's scan when it wasn't cached, and
/// request a redraw. The scan lands back through the overlay event
/// channel (see [`spawn_scope_fetch`] / [`drain_events`]).
pub(super) fn after_nav(app: &mut App, outcome: NavOutcome) {
    match outcome {
        NavOutcome::NeedsScan(scope) => spawn_scope_scan(app, scope),
        // A cached scope installs its files without a scan, so this is
        // the only chance to rebuild its cards. They are a projection of
        // the store, and the copy left over from the last visit predates
        // whatever happened in the scope just left.
        NavOutcome::Ready => hydrate_threads(app),
    }
    app.needs_redraw = true;
}

fn spawn_scope_scan(app: &mut App, scope: DiffScope) {
    let Some(overlay) = app.diff_overlay.as_ref() else { return };
    let cwd = overlay.cwd.clone();
    let target = overlay.target.clone();
    let sha = match scope {
        DiffScope::WholeDiff => None,
        DiffScope::Commit(i) => overlay.commits.get(i).map(|c| c.sha.clone()),
    };
    let seq = app.diff_scan_seq;
    spawn_scope_fetch(cwd, target, scope, sha, seq, app.diff_overlay_event_tx.clone());
}

/// Install `state` on `app.diff_overlay` and transition the active
/// view to [`ActiveView::Diff`]. Wired up by the `/diff` slash
/// command's drain pump; the Inspector `🦉` click reuses the same
/// path in a follow-up commit.
fn open(app: &mut App, state: DiffOverlayState) {
    app.diff_overlay = Some(state);
    set_active_view(app, ActiveView::Diff);
    app.needs_redraw = true;
}

/// Drop the overlay state and transition back to chat. The Esc submit
/// path lives in
/// [`close_with_submit`](super::reviews::close_with_submit) - call this
/// directly only when
/// comments have already been handled (or the caller is the Esc-cancel
/// path for the active input editor).
pub(super) fn close(app: &mut App) {
    app.diff_overlay = None;
    set_active_view(app, ActiveView::Chat);
    app.needs_redraw = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::diff_overlay::test_support::*;
    use forge_primitives::review::{
        ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus,
    };
    use forge_workspace::env::git_diff::resolver;

    #[test]
    fn resolve_active_diff_cwd_routes_git_worker_to_worktree_path() {
        // Bug #208: workers spawned with `is_git_repo_at_spawn = true`
        // run inside `.claude/worktrees/<label>/`, but `cwd_raw`
        // carries the lead's project root. The overlay must resolve
        // to the worker's worktree so the diff opens against the
        // worker's branch, not an empty lead diff.
        use forge_primitives::WorkerLiveness;
        use forge_workspace::{ProjectKey, SessionKey, WorkerEntry};

        let mut app = App::test_default();
        let workspace =
            app.workspace.clone().expect("App::test_default seeds a workspace via testing_stub");

        // Seed a loaded project so `git_scan_cwd_for_session` can
        // resolve the project_root via `project_root_for_key`. The
        // post-#232 implementation composes the worktree path from
        // the project_root rather than `cwd_raw`, so a worker entry
        // without a matching project would now fall back to cwd_raw.
        let project_root = "/tmp/project";
        workspace.seed_test_project("forge", project_root);
        let project_key = ProjectKey::new_for_test(
            forge_workspace::userdata::catalog::scan::project_key_for_directory(Some(project_root)),
        );
        let worker_key = SessionKey::from_session_id("worker-uuid");
        workspace.insert_live_worker(
            &project_key,
            WorkerEntry {
                label: "implementer".into(),
                charter: "test charter".into(),
                session_key: worker_key.clone(),
                status: WorkerLiveness::Running,
                spawned_at: std::time::SystemTime::UNIX_EPOCH,
                spawned_by_session_id: "lead-uuid".into(),
                needs_tag: false,
                is_git_repo_at_spawn: true,
                diagnostic: None,
                kick: None,
            },
        );

        let mut session = crate::app::session::UiSession::new(worker_key.clone());
        session.cwd_raw = project_root.into();
        app.sessions.insert(worker_key.clone(), session);
        app.active_session_key = Some(worker_key);

        let resolved = resolve_active_diff_cwd(&app, project_root);
        assert_eq!(resolved, PathBuf::from("/tmp/project/.claude/worktrees/implementer"));
    }

    #[test]
    fn resolve_active_diff_cwd_returns_cwd_raw_for_lead_session() {
        // Lead sessions (and non-worker callers in general) get
        // `cwd_raw` back unchanged - the worker resolution short-
        // circuits via `worker_lookup_for_session` returning None.
        let mut app = App::test_default();
        let lead_key = forge_workspace::SessionKey::from_session_id("lead-uuid");
        let mut session = crate::app::session::UiSession::new(lead_key.clone());
        session.cwd_raw = "/tmp/project".into();
        app.sessions.insert(lead_key.clone(), session);
        app.active_session_key = Some(lead_key);

        let resolved = resolve_active_diff_cwd(&app, "/tmp/project");
        assert_eq!(resolved, PathBuf::from("/tmp/project"));
    }

    // ---- resolve_default_target: one test per DefaultTarget arm ----

    #[test]
    fn resolve_default_target_no_snapshot_when_unscanned() {
        let app = app_with_target_snapshot(None);
        assert_eq!(resolve_default_target(&app), DefaultTarget::NoSnapshot);
    }

    #[test]
    fn resolve_default_target_not_a_repo() {
        let app =
            app_with_target_snapshot(Some(target_snapshot(RepoGate::NotARepo, false, false, None)));
        assert_eq!(resolve_default_target(&app), DefaultTarget::NotARepo);
    }

    #[test]
    fn resolve_default_target_scanner_failed() {
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::ScannerFailed,
            false,
            false,
            None,
        )));
        assert_eq!(resolve_default_target(&app), DefaultTarget::ScannerFailed);
    }

    #[test]
    fn resolve_default_target_dirty_worktree_diffs_head() {
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::InRepo,
            true,
            false,
            Some("main"),
        )));
        assert_eq!(resolve_default_target(&app), DefaultTarget::Ref("HEAD".to_owned()));
    }

    #[test]
    fn resolve_default_target_worktree_wins_over_branch_ahead() {
        // Layer 1 precedence: a dirty tree resolves to HEAD even when
        // the branch is also ahead of its default.
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::InRepo,
            true,
            true,
            Some("main"),
        )));
        assert_eq!(resolve_default_target(&app), DefaultTarget::Ref("HEAD".to_owned()));
    }

    #[test]
    fn resolve_default_target_branch_ahead_diffs_default() {
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::InRepo,
            false,
            true,
            Some("main"),
        )));
        assert_eq!(resolve_default_target(&app), DefaultTarget::Ref("main".to_owned()));
    }

    #[test]
    fn resolve_default_target_branch_ahead_without_default_is_nodefault() {
        let app =
            app_with_target_snapshot(Some(target_snapshot(RepoGate::InRepo, false, true, None)));
        assert_eq!(resolve_default_target(&app), DefaultTarget::NoDefault);
    }

    #[test]
    fn resolve_default_target_clean_tree_surfaces_default_branch() {
        let app = app_with_target_snapshot(Some(target_snapshot(
            RepoGate::InRepo,
            false,
            false,
            Some("main"),
        )));
        assert_eq!(
            resolve_default_target(&app),
            DefaultTarget::Clean { default_branch: Some("main".to_owned()) }
        );
    }

    // ---- commit mode: scope, navigation, comment scoping ----

    #[test]
    fn open_prefers_whole_diff_when_whole_diff_threads_exist() {
        // A whole-diff thread keeps priority even alongside commit-scoped
        // ones - the pre-scope behavior. No threads at all -> default.
        let threads = [scope_thread("cs", Some("sha1"), "t2"), scope_thread("wd", None, "t1")];
        assert_eq!(initial_scope_from_threads(&threads), InitialScope::WholeDiff);
        assert_eq!(initial_scope_from_threads(&[]), InitialScope::Default);
    }

    #[test]
    fn open_lands_on_commit_with_persisted_thread() {
        // No whole-diff thread: the most-recently-updated commit-scoped
        // thread's commit is chosen, and it maps to that commit's index.
        let threads = [
            scope_thread("a", Some("sha0"), "2026-07-20T10:00:00Z"),
            scope_thread("b", Some("sha1"), "2026-07-21T10:00:00Z"),
        ];
        let pref = initial_scope_from_threads(&threads);
        assert_eq!(pref, InitialScope::Commit("sha1".to_owned()), "newest commit-scoped wins");

        let commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        assert_eq!(
            resolve_initial_commit(&pref, &commits),
            Some((1, "sha1".to_owned())),
            "the chosen sha maps to Commit(1)",
        );
    }

    #[test]
    fn resolve_initial_commit_defaults_and_falls_back() {
        let commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        assert_eq!(
            resolve_initial_commit(&InitialScope::Default, &commits),
            Some((0, "sha0".to_owned())),
            "default opens the first commit when the branch has commits",
        );
        assert_eq!(
            resolve_initial_commit(&InitialScope::Default, &[]),
            None,
            "default with no commits opens whole-diff",
        );
        assert_eq!(
            resolve_initial_commit(&InitialScope::WholeDiff, &commits),
            None,
            "whole-diff never resolves to a commit",
        );
        assert_eq!(
            resolve_initial_commit(&InitialScope::Commit("gone".to_owned()), &commits),
            Some((0, "sha0".to_owned())),
            "a vanished commit sha falls back to the first commit",
        );
    }

    // ---- durable review threads (persist / re-anchor / drift) ----

    /// The review store key comes from the checkout being diffed, never
    /// from the session's cached git snapshot. The two diverge whenever
    /// the snapshot is stale or belongs to a session that is no longer
    /// the one under review, and the reader (`ProdReviewFacade::
    /// resolve_scope`) queries git live - so a review filed under the
    /// cached name is written where nothing looks for it.
    #[tokio::test(flavor = "current_thread")]
    async fn the_review_branch_comes_from_the_checkout_not_the_cached_snapshot() {
        let repo = tempfile::tempdir().expect("tempdir");
        init_repo(repo.path(), "feat/live");
        let (mut app, _dir) = review_app();
        let key = app.active_session_key.clone().expect("active key");
        let session = app.sessions.get_mut(&key).expect("session");
        session.cwd_raw = repo.path().to_string_lossy().into_owned();
        session.git_diff_snapshot = Some(forge_primitives::git_diff::GitDiffSnapshot {
            branch: forge_primitives::git::GitBranch::Named("stale-cache".to_owned()),
            default_branch: Some("main".to_owned()),
            repo_gate: RepoGate::InRepo,
            pushed_sha: None,
            worktree: forge_primitives::git_diff::LayerState::Clean,
            branch_ahead: forge_primitives::git_diff::LayerState::Clean,
            pr: None,
            closes: Vec::new(),
            pr_fetched_at: None,
        });
        set_active_view(&mut app, ActiveView::Chat);

        tokio::task::LocalSet::new()
            .run_until(async {
                open_with_target(&mut app, "HEAD".to_owned());
                // Loop on the state, count as a cap only: the spawn
                // behind this runs five git subprocesses, each against a
                // 10s timeout, so any budget short of that is asserting
                // on how loaded the runner is.
                let mut opened = false;
                for _ in 0..1500 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    drain_events(&mut app);
                    if app.diff_overlay.is_some() {
                        opened = true;
                        break;
                    }
                }
                assert!(opened, "the diff scan never landed");
            })
            .await;

        assert_eq!(
            overlay(&app).branch.as_deref(),
            Some("feat/live"),
            "the overlay keys on the live checkout, not the snapshot's stale name",
        );
    }

    #[test]
    fn drain_hydrates_commit_scoped_thread_on_navigation() {
        // The REAL path: a lazy Commit(1) scan lands via drain_events after
        // the user stepped to that commit. Its persisted thread must
        // hydrate against the just-installed files - the bug was
        // drain_events gating hydration on whole-diff scope, so navigating
        // to a commit never re-anchored its threads.
        let (mut app, _dir) = review_app();
        let ws = app.workspace.clone().expect("ws");
        ws.save_review_threads(
            "forge",
            "feat",
            &[forge_primitives::ReviewThread {
                id: "c1".to_owned(),
                anchor: ReviewAnchor {
                    path: "src/x.rs".to_owned(),
                    side: ReviewSide::New,
                    line: 5,
                    content_hash: resolver::content_hash("let a = 1;"),
                    context: Vec::new(),
                    base_ref: "main".to_owned(),
                },
                comments: vec![ReviewComment {
                    author: ReviewAuthor::User,
                    text: "on commit one".to_owned(),
                    at: String::new(),
                    review_id: None,
                }],
                status: ReviewStatus::Open,
                created_at: "t0".to_owned(),
                updated_at: "t0".to_owned(),
                commit: Some("sha1".to_owned()),
            }],
        );

        // Overlay open in commit mode; the user has navigated to Commit(1)
        // (scope already set), its scan in flight.
        let mut overlay = DiffOverlayState::new(
            PathBuf::from("/tmp/repo"),
            "main".to_owned(),
            vec![single_hunk_file("src/x.rs", vec![added_line("noop", 1)])],
        );
        overlay.branch = Some("feat".to_owned());
        overlay.commits = vec![commit_meta("sha0", "first"), commit_meta("sha1", "second")];
        overlay.commit_cache = vec![None, None];
        overlay.scope = DiffScope::Commit(1);
        app.diff_overlay = Some(overlay);

        // The lazy Commit(1) scan lands with sha1's file content.
        app.diff_overlay_event_tx
            .send(DiffOverlayEvent {
                cwd: PathBuf::from("/tmp/repo"),
                target: "main".to_owned(),
                files: vec![single_hunk_file("src/x.rs", vec![added_line("let a = 1;", 5)])],
                scanner_ok: true,
                untracked_suppressed: 0,
                seq: app.diff_scan_seq,
                kind: DiffScanKind::Scope(DiffScope::Commit(1)),
                commit_body: Some("second".to_owned()),
            })
            .expect("send scope event");

        drain_events(&mut app);

        let comments = &app.diff_overlay.as_ref().expect("overlay").comments;
        assert_eq!(comments.len(), 1, "the commit's persisted thread hydrated on navigation");
        assert_eq!(comments[0].thread.id, "c1");
        assert_eq!(comments[0].commit.as_deref(), Some("sha1"));
        assert!(comments[0].persisted, "hydrated from redb");
    }
}

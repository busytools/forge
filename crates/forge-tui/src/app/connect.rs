//! App creation and connection startup. User actions flow out via
//! [`forge_workspace::Workspace::dispatch`] with
//! [`forge_workspace::Command::Spawn*`] / `StartDefault` for
//! App-level kicks and per-session commands otherwise.

mod session_start;
pub(crate) mod type_converters;

use super::config::ConfigState;
use super::dialog::DialogState;
use super::plugins::PluginsState;
use super::state::RenderCacheBudget;
use super::view::ActiveView;
use super::{App, AppStatus, FocusManager, HelpView};
use crate::Cli;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Shorten a path for display: substitute `~` for the home directory prefix.
fn shorten_cwd_for_display(cwd: &std::path::Path) -> String {
    let cwd_str = cwd.to_string_lossy().to_string();
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy().to_string();
        if cwd_str.starts_with(&home_str) {
            return format!("~{}", &cwd_str[home_str.len()..]);
        }
    }
    cwd_str
}

pub(crate) use session_start::{SessionStartReason, begin_resume_session, start_new_session};

/// Build `SessionLaunchSettings` for the startup spawn path.
pub(crate) fn session_launch_settings_for_startup(
    app: &App,
) -> forge_workspace::SessionLaunchSettings {
    session_start::session_launch_settings_for_reason(
        app,
        session_start::SessionStartReason::Startup,
    )
}

/// Build `SessionLaunchSettings` for the resume / sleeping-session
/// spawn path.
pub(crate) fn session_launch_settings_for_resume(
    app: &App,
) -> forge_workspace::SessionLaunchSettings {
    session_start::session_launch_settings_for_reason(
        app,
        session_start::SessionStartReason::Resume,
    )
}

/// Create the `App` struct in `Connecting` state and load shared
/// settings state. `cwd_raw` is sourced from `forge.toml` (per
/// Hard Rule #14) - chat-direct mode picks up `project.path`,
/// launchpad mode leaves it empty.
pub fn create_app(cli: &Cli, workspace: Arc<forge_workspace::Workspace>) -> App {
    create_app_impl(cli, workspace, None)
}

fn create_app_impl(
    cli: &Cli,
    workspace: Arc<forge_workspace::Workspace>,
    perf_log: Option<std::path::PathBuf>,
) -> App {
    // Resolve the pre-Connect seed cwd from `forge.toml`:
    //
    // - `forge <project>` (chat-direct): look up `project.path`.
    // - `forge` (launchpad): no project picked, leave both empty.
    //   Trust + file_index init handle an empty cwd cleanly; the
    //   first per-project Connected event populates the real
    //   bucket's `cwd_raw` from the agent's reported cwd.
    let project_path = cli
        .project
        .as_deref()
        .and_then(|name| workspace.list_projects().into_iter().find(|p| p.name == name))
        .map(|p| p.path);
    let cwd_display = project_path.as_ref().map(|p| shorten_cwd_for_display(p)).unwrap_or_default();
    let cwd_raw =
        project_path.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();

    let (file_index_event_tx, file_index_event_rx) = std::sync::mpsc::channel();
    let (git_diff_event_tx, git_diff_event_rx) = std::sync::mpsc::channel();
    let (dictate_devices_tx, dictate_devices_rx) = std::sync::mpsc::channel();
    let (review_waiting_event_tx, review_waiting_event_rx) = std::sync::mpsc::channel();
    let (cli_version_event_tx, cli_version_event_rx) = std::sync::mpsc::channel();
    let (diff_overlay_event_tx, diff_overlay_event_rx) = std::sync::mpsc::channel();
    let (usage_overlay_event_tx, usage_overlay_event_rx) = std::sync::mpsc::channel();
    let (process_scan_event_tx, process_scan_event_rx) = std::sync::mpsc::channel();
    // Opt-in plugin auto-update ([plugins] auto_update): one
    // inventory refresh + eligible updates in the background, results
    // landing in the plugins pane state. The plugin CLI needs a cwd;
    // a launchpad boot has none, so it borrows the first project's.
    // Runs after App construction so the seeded run guards `u`/`c`
    // from the first frame.
    let boot_cwd_raw = if cwd_raw.is_empty() {
        workspace
            .list_projects()
            .first()
            .map(|project| project.path.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        cwd_raw.clone()
    };
    let boot_cli = crate::app::plugins::UpdateCli::real();
    let boot_workspace = workspace.clone();
    crate::app::git_diff::spawn_periodic_timer(git_diff_event_tx.clone());
    crate::app::cli_version::spawn_fetch(cli_version_event_tx.clone());
    crate::app::process_scanner::spawn_ticker(process_scan_event_tx.clone());
    // Kick off the 60 s background account-usage poller. Boot
    // seeded from the redb store in `Workspace::new`; `main`
    // started per-account loading tasks via
    // `start_account_loading_tasks` (which subsumed the old single
    // initial probe in #246); the poller carries the refresh
    // cadence from here on.
    workspace.start_usage_poller();
    let perf_path = perf_log.or_else(|| match crate::logging::resolve_perf_path(cli) {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!(
                target: crate::logging::targets::APP_PERF,
                event_name = "perf_telemetry_unavailable",
                message = "failed to resolve perf telemetry sidecar path",
                outcome = "failure",
                telemetry_channel = "perf_sidecar",
                perf_schema = "forge-perf/v1",
                error = %err,
            );
            None
        }
    });
    let perf = perf_path.as_deref().and_then(|path| {
        let logger = crate::perf::PerfLogger::open(path);
        if logger.is_some() {
            tracing::info!(
                target: crate::logging::targets::APP_PERF,
                event_name = "perf_telemetry_enabled",
                message = "perf telemetry sidecar enabled",
                outcome = "success",
                telemetry_channel = "perf_sidecar",
                perf_schema = "forge-perf/v1",
                perf_log = %path.display()
            );
        } else {
            tracing::warn!(
                target: crate::logging::targets::APP_PERF,
                event_name = "perf_telemetry_unavailable",
                message = "failed to enable perf telemetry sidecar",
                outcome = "failure",
                telemetry_channel = "perf_sidecar",
                perf_schema = "forge-perf/v1",
                perf_log = %path.display()
            );
        }
        logger
    });

    // Subscribe to workspace's SessionUpdate channel BEFORE any
    // `Command::StartDefault` dispatch so the first emit is delivered
    // to the App event loop.
    let update_rx = workspace.subscribe().unwrap_or_else(|| {
        // Second-subscribe paths only occur if construction is
        // accidentally called twice (a misconfiguration). Returning a
        // dummy receiver keeps the App constructable so the error
        // surfaces in the regular event-loop diagnostics rather than
        // an unwrap on the App field type.
        let (_, rx) = mpsc::unbounded_channel();
        rx
    });
    let update_tx = workspace.update_sender();

    let pre_connect_key = forge_workspace::SessionKey::from_session_id(App::PRE_CONNECT_KEY);
    let mut pre_connect_session = super::session::UiSession::new(pre_connect_key.clone());
    pre_connect_session.messages =
        vec![super::ChatMessage::welcome(crate::FORGE_VERSION, "", &cwd_display, "-")];
    pre_connect_session.cwd = cwd_display;
    pre_connect_session.cwd_raw = cwd_raw;
    // Pre-register a handle-less DomainSession for the pre-Connect
    // key. The spawn handler later stamps the live `Arc<AgentHandle>`
    // onto this same domain entry when
    // `get_agent_handle_with_spawn_key` runs.
    let pre_connect_domain = workspace.register_domain_session(pre_connect_key.clone(), None);
    drop(pre_connect_domain);
    let mut sessions = std::collections::HashMap::new();
    sessions.insert(pre_connect_key.clone(), pre_connect_session);
    // Tier-based default for side panes: visible at Wide, hidden
    // elsewhere. Both panes use the same threshold (Wide tier) so
    // narrow / medium terminals start with a chat-only layout the
    // user can grow via the pane toggles (Cmd+Left / Cmd+Right,
    // Ctrl+Left / Ctrl+Right elsewhere) if they want the chrome
    // back. Nothing is persisted - each forge launch re-derives
    // from the current terminal width.
    let (initial_term_width, _) = crossterm::terminal::size().unwrap_or((0, 0));
    let panes_visible_by_default = initial_term_width >= crate::ui::layout::WIDE_TIER_MIN_WIDTH;
    let projects_pane_visible = panes_visible_by_default;
    let inspector_pane_visible = panes_visible_by_default;
    // Boot view: preflight, on every route. It hands over to the
    // launchpad picker for `forge` and straight into chat for
    // `forge <project>`; `startup_project` is what decides which, and
    // it is the only thing that decides it - argv selection is final,
    // with no remembered-last-pick. Snapshot launchpad state from
    // `[ui]` settings up-front so the picker doesn't shift if the user
    // edits forge.toml mid-session.
    let active_view = ActiveView::Launchpad;
    let ui_settings = workspace.ui_settings();
    let spinner_style = ui_settings.spinner;
    let initial_launchpad_state = crate::app::LaunchpadState {
        selected_index: 0,
        opened_at: std::time::Instant::now(),
        scroll_offset: 0,
    };
    let mut app = App {
        active_view,
        emoji: None,
        config: ConfigState::default(),
        settings_home_override: None,
        status: AppStatus::Connecting,
        should_quit: false,
        preflight_done: false,
        preflight_cancel_drawn: false,
        spawn_deferred_logged: false,
        exit_error: None,
        start_new_run: cli.new,
        workspace: Some(workspace),
        #[rustfmt::skip] #[cfg(feature = "testing")] test_dispatched_permission_outcomes: std::cell::RefCell::new(Vec::new()),
        #[rustfmt::skip] #[cfg(feature = "testing")] test_dispatched_question_outcomes: std::cell::RefCell::new(Vec::new()),
        #[rustfmt::skip] #[cfg(feature = "testing")] test_notifications: std::cell::RefCell::new(Vec::new()),
        sessions,
        active_session_key: Some(pre_connect_key),
        pending_spawn_focus: None,
        forge_crons: Vec::new(),
        forge_schedule_rows: Vec::new(),
        gotify_subs: Vec::new(),
        gotify_connected: false,
        help_view: HelpView::Keys,
        help_open: false,
        help_dialog: DialogState::default(),
        help_visible_count: 0,
        update_rx,
        update_tx,
        file_index_event_tx,
        file_index_event_rx,
        git_diff_event_tx,
        git_diff_event_rx,
        dictate_devices_tx,
        dictate_devices_rx,
        dictate_devices: None,
        dictate_devices_in_flight: false,
        dictate_devices_dirty: false,
        review_waiting_event_tx,
        review_waiting_event_rx,
        cli_version_event_tx,
        cli_version_event_rx,
        diff_overlay_event_tx,
        diff_overlay_event_rx,
        usage_overlay_event_tx,
        usage_overlay_event_rx,
        diff_scan_seq: 0,
        cli_version_info: None,
        process_scan_event_tx,
        process_scan_event_rx,
        spinner_frame: 0,
        spinner_last_advance_at: None,
        spinner_style,
        spinner_epoch: std::time::Instant::now(),
        repaint_cadence: ui_settings.fps,
        spinner_picker: None,
        model_picker: None,
        account_picker: None,
        dictate_picker: None,
        dictate_key: crate::app::dictate_key::DictateKeyState::default(),
        dictate_take_pending: false,
        tools_collapsed: true,
        #[cfg(any(test, feature = "testing"))]
        last_invalidation_level: std::cell::Cell::new(None),
        projects_pane_visible,
        projects_pane_scroll_offset: 0,
        projects_pane_overlay_open: false,
        inspector_pane_visible,
        inspector_pane_overlay_open: false,
        pane_hit_targets: Vec::new(),
        layout: crate::ui::layout::AppLayout::default(),
        force_redraw: false,
        focus: FocusManager::default(),
        plugins: PluginsState::default(),
        launchpad: initial_launchpad_state,
        diff_overlay: None,
        usage_overlay: None,
        cached_frame_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        scrollbar_drag: None,
        rendered_chat_lines: Vec::new(),
        rendered_chat_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        rendered_input_lines: Vec::new(),
        rendered_input_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        pointer_shape: crate::app::events::mouse::PointerShape::Default,
        emitted_pointer_shape: None,
        needs_keyboard_flags_restore: false,
        rendered_inspector_body_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        rendered_projects_pane_body_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        paste_burst: super::paste_burst::PasteBurstDetector::new(),
        needs_redraw: true,
        notifications: super::notify::NotificationManager::new(),
        perf,
        render_cache_budget: RenderCacheBudget::default(),
        fps_ema: None,
        last_frame_at: None,
        connection_started: false,
        startup_project: cli.project.clone(),
        replay_in_progress: false,
    };

    if let Err(err) = super::config::initialize_shared_state(&mut app) {
        tracing::warn!(
            target: crate::logging::targets::APP_CONFIG,
            event_name = "shared_settings_init_failed",
            message = "failed to initialize shared settings state",
            outcome = "failure",
            error_message = %err,
        );
        app.config.last_error = Some(err);
    }

    crate::app::plugins::maybe_spawn_boot_auto_update(
        &boot_workspace,
        &mut app,
        boot_cwd_raw,
        boot_workspace.plugin_settings(),
        boot_cli,
    );

    app.rebuild_history_retention_accounting();
    app.rebuild_render_cache_accounting();
    // Re-derive the welcome's account-line shape now that the App
    // has its workspace pointer set. This lets workspace-mode
    // sessions render `Account: …` from the very first frame
    // instead of a hidden line that pops in once data arrives.
    app.sync_welcome_snapshot();
    super::file_index::restart(&mut app);
    app
}

/// Kick off the startup connection via the workspace command bus, once
/// per process. From the launchpad that is one `SpawnProject` per
/// auto-start project and nothing focused; with a project argv it is
/// `StartDefault` for that project. The workspace owns the spawn from
/// there.
pub fn start_connection(app: &mut App) {
    if app.connection_started {
        return;
    }

    let Some(workspace) = app.workspace.as_ref() else {
        tracing::error!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "start_connection_without_workspace",
            message = "start_connection invoked without a workspace; refusing to spawn bridge",
            outcome = "failure",
        );
        app.connection_started = true;
        return;
    };

    // Two halves, each doing distinct work.
    //
    // The plan has to exist, or the spawn falls back to round-robin and
    // can land a session on an account the project's org does not
    // allow. Read off the plan rather than the account map: the map is
    // published on one lock acquisition and the plan written on a
    // second, so an observer can see every account settled while the
    // plan is still absent.
    //
    // And every account has to have settled, because that is the
    // condition preflight hands over on. The plan excludes a bailed
    // account, so a released spawn routes sessions to the accounts
    // that can take them, when any exist - with every account bailed
    // the plan is empty and the spawn can fall back on a dead one.
    //
    // Neither half waits on the dictation weights, so the models load
    // alongside the session rather than delaying it.
    let workspace_ready = workspace.assignment_plan_ready();
    if !workspace_ready || !crate::ui::preflight::accounts_settled(app) {
        if !app.spawn_deferred_logged {
            app.spawn_deferred_logged = true;
            tracing::info!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                event_name = "startup_spawn_deferred",
                message = "holding the boot spawn until the account plan is ready",
                plan_ready = workspace_ready,
            );
        }
        return;
    }
    if app.spawn_deferred_logged {
        tracing::info!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "startup_spawn_released",
            message = "account plan ready; starting the boot spawn",
        );
    }

    app.connection_started = true;
    let mut launch_settings = session_start::session_launch_settings_for_reason(
        app,
        session_start::SessionStartReason::Startup,
    );
    // Boot wave only: --new makes the auto_start leads start fresh
    // instead of resuming (and cascades to their workers). Stamped on
    // the shared boot settings so every dispatch below carries it - the
    // focused project's StartDefault and the rest's SpawnProject alike.
    // Later click-to-spawn builds its own settings where force_new
    // defaults false, so it keeps resuming.
    launch_settings.force_new = app.start_new_run;

    // Launchpad branch: the user invoked `forge` without an argv, so
    // no project is focused. Every `auto_start = true` project spawns
    // immediately. The account picker consults whatever usage data
    // is currently in memory (loaded from the on-disk cache at boot,
    // refreshed by the 60s background poller). No warm-gate - when
    // the cache is empty (cold install) the picker falls through to
    // forge.toml definition order; once data lands, subsequent spawns
    // see the right tier.
    if app.startup_project.is_none() {
        let auto_start = workspace.auto_start_project_names();
        for project_name in auto_start {
            let cmd = forge_workspace::Command::SpawnProject {
                project_name: project_name.clone(),
                launch_settings: launch_settings.clone(),
            };
            if let Err(err) = workspace.dispatch(cmd) {
                tracing::error!(
                    target: crate::logging::targets::BRIDGE_LIFECYCLE,
                    event_name = "launchpad_auto_start_dispatch_failed",
                    error = %err,
                    project = %project_name,
                    "launchpad auto_start dispatch failed",
                );
            }
        }
        return;
    }

    // Chat branch. Only reachable with a project argv - the arm above
    // takes every other case - so the first match arm always wins here
    // and that project is the focused spawn. The `None` arms are
    // exhaustiveness over `Option<String>`, not a reachable path.
    let auto_start = workspace.auto_start_project_names();
    let dispatch_targets: Vec<Option<String>> = match (&app.startup_project, auto_start.as_slice())
    {
        (Some(name), _) => vec![Some(name.clone())],
        (None, []) => vec![None], // Falls through to default in StartDefault.
        (None, names) => names.iter().cloned().map(Some).collect(),
    };

    for (i, project_name) in dispatch_targets.iter().enumerate() {
        // Only the first project gets `StartDefault` semantics
        // (which sets it as the focused tab); the rest go via
        // `SpawnProject` and land in the Projects pane silently.
        // No warm-up gating - see launchpad branch above.
        let cmd = if i == 0 {
            forge_workspace::Command::StartDefault {
                project_name: project_name.clone(),
                launch_settings: launch_settings.clone(),
            }
        } else {
            forge_workspace::Command::SpawnProject {
                project_name: project_name.clone().unwrap_or_default(),
                launch_settings: launch_settings.clone(),
            }
        };
        if let Err(err) = workspace.dispatch(cmd) {
            tracing::error!(
                target: crate::logging::targets::BRIDGE_LIFECYCLE,
                event_name = "start_connection_dispatch_failed",
                error = %err,
                project = ?project_name,
                "auto_start dispatch failed",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::App;
    use crate::Cli;
    use std::sync::Arc;

    /// Redirects the perf sidecar into a caller-owned directory so
    /// tests never append to the user's real diagnostic log.
    fn create_app_for_test(
        cli: &Cli,
        workspace: Arc<forge_workspace::Workspace>,
        perf_dir: &std::path::Path,
    ) -> App {
        super::create_app_impl(
            cli,
            workspace,
            Some(perf_dir.join(crate::logging::DEFAULT_PERF_FILE_NAME)),
        )
    }

    /// Ensure `forge/` exists and return it, so tests write forge's
    /// config + state where forge reads them (not the legacy fallback).
    fn forge_dir(config_dir: &std::path::Path) -> std::path::PathBuf {
        let forge = config_dir.join("forge");
        std::fs::create_dir_all(&forge).expect("forge/ dir");
        forge
    }

    fn write_default_forge_toml(dir: &std::path::Path, project_path: &std::path::Path) {
        let project_path_str = project_path.to_string_lossy().replace('\\', "/");
        std::fs::write(
            forge_dir(dir).join("forge.toml"),
            format!(
                "[[orgs]]\nname = \"Default\"\naccounts = [\"Stargate\"]\n\n[[orgs.projects]]\nname = \"forge-test\"\npath = \"{project_path_str}\"\nauto_start = true\n\n[[accounts]]\ndisplay_name = \"Stargate\"\nconfig_dir = \"~/.claude-stargate\"\nprovider = \"anthropic\"\n"
            ),
        )
        .expect("write forge.toml");
    }

    fn cli_with(project: Option<&str>) -> Cli {
        Cli {
            project: project.map(str::to_owned),
            new: false,
            generate_completion: None,
            diagnostics_preset: None,
            log_file: None,
            log_filter: None,
            perf_log: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_app_launchpad_mode_leaves_cwd_raw_empty() {
        // No argv → launchpad mode → no project picked → pre-connect
        // bucket carries an empty `cwd_raw`. This is the invariant
        // the `find_running_bucket_for_path` simplification depends
        // on: pre-connect can never collide with a real project's
        // `path` because there's nothing to compare against.
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        write_default_forge_toml(config_dir.path(), project_dir.path());
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");

        let cli = cli_with(None);

        let local = tokio::task::LocalSet::new();
        let app = local
            .run_until(async { create_app_for_test(&cli, Arc::new(workspace), config_dir.path()) })
            .await;

        assert!(
            app.cwd_raw().is_empty(),
            "launchpad-mode pre-connect should leave cwd_raw empty, got {:?}",
            app.cwd_raw(),
        );
        assert!(app.workspace.is_some(), "workspace should be wired");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_app_chat_direct_mode_seeds_cwd_raw_from_forge_toml() {
        // `forge <project>` → chat-direct mode → pre-connect bucket
        // carries the project's `path` from `forge.toml`, NOT the
        // process working directory. That's the architectural fix:
        // forge.toml is the source of truth for project paths;
        // `std::env::current_dir()` is intentionally not consulted.
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        write_default_forge_toml(config_dir.path(), project_dir.path());
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");

        let cli = cli_with(Some("forge-test"));

        let local = tokio::task::LocalSet::new();
        let app = local
            .run_until(async { create_app_for_test(&cli, Arc::new(workspace), config_dir.path()) })
            .await;

        assert_eq!(
            app.cwd_raw(),
            project_dir.path().to_string_lossy(),
            "chat-direct pre-connect should carry the project's path from forge.toml",
        );
        assert!(app.workspace.is_some(), "workspace should be wired");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_app_boots_into_launchpad_when_no_argv() {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        write_default_forge_toml(config_dir.path(), project_dir.path());
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");
        let cli = cli_with(None);
        let local = tokio::task::LocalSet::new();
        let app = local
            .run_until(async { create_app_for_test(&cli, Arc::new(workspace), config_dir.path()) })
            .await;
        assert_eq!(app.active_view, crate::app::ActiveView::Launchpad);
    }

    /// Boot the app the way `create_app` does, intercept dispatches
    /// rather than spawning, and run `start_connection` once.
    async fn boot_and_dispatch(
        project: Option<&str>,
        accounts_ready: bool,
    ) -> (App, Vec<forge_workspace::Command>, tempfile::TempDir, tempfile::TempDir) {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        write_default_forge_toml(config_dir.path(), project_dir.path());
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");
        workspace.enable_test_dispatch_intercept();
        if accounts_ready {
            // Also computes the assignment plan, which is the other half
            // of what the gate waits on.
            workspace.seed_test_ready_account("Stargate");
        }
        let cli = cli_with(project);
        let local = tokio::task::LocalSet::new();
        let workspace = Arc::new(workspace);
        let mut app = local
            .run_until(async {
                create_app_for_test(&cli, Arc::clone(&workspace), config_dir.path())
            })
            .await;
        super::start_connection(&mut app);
        let dispatched = workspace.drain_test_dispatch_buffer();
        (app, dispatched, config_dir, project_dir)
    }

    /// `forge <project>` has to spawn the project it was given.
    ///
    /// Picking the arm off `active_view` instead reads `Launchpad` on
    /// both routes, so it takes the no-project arm, spawns only
    /// `auto_start`, and the user lands in a chat with no session and no
    /// error.
    #[tokio::test(flavor = "current_thread")]
    async fn the_named_project_is_what_boot_spawns_on_the_argv_route() {
        let (_app, dispatched, _c, _p) = boot_and_dispatch(Some("forge-test"), true).await;
        let started: Vec<&str> = dispatched
            .iter()
            .filter_map(|cmd| match cmd {
                forge_workspace::Command::StartDefault { project_name, .. } => {
                    project_name.as_deref()
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            started,
            ["forge-test"],
            "the argv project is the focused spawn; got {dispatched:?}",
        );
    }

    /// Nothing spawns before the account plan exists. Without it the
    /// spawn falls back to round-robin and can land a session on an
    /// account the project's org does not allow.
    #[tokio::test(flavor = "current_thread")]
    async fn nothing_spawns_before_the_account_plan_is_ready() {
        let (app, dispatched, _c, _p) = boot_and_dispatch(Some("forge-test"), false).await;
        assert!(
            dispatched.is_empty(),
            "a fresh account map has no plan yet, so boot must dispatch nothing; got {dispatched:?}",
        );
        assert!(!app.connection_started, "and it must stay armed rather than marking itself done");
        assert!(app.spawn_deferred_logged, "the deferral is recorded once, so it is diagnosable");
    }

    /// The two halves of the gate do distinct work: the plan closes the
    /// window where the account map reads settled before the plan is
    /// written, and settled-not-terminal stops a subprocess racing
    /// accounts that are still resolving.
    ///
    /// **Settled is terminal, not `Ready`: a bailed account releases the
    /// spawn.** The plan excludes it and the pollers keep re-probing it,
    /// so holding boot waits on nothing that could change it.
    ///
    /// **The plan half is UNKILLABLE from a test, and that is a property
    /// of the code rather than a gap.** Production reaches every
    /// terminal state through the loader, which recomputes the plan on
    /// each one, so plan-absent-with-accounts-settled exists only inside
    /// the real two-lock window between the map being published and the
    /// plan being written. The test seeding deliberately skips the
    /// recompute, so a seeded plan-absent state proves nothing about
    /// production. Dropping `!workspace_ready` therefore passes, and no
    /// test would change that. Do not add one to chase it.
    #[tokio::test(flavor = "current_thread")]
    async fn the_plan_and_settled_are_both_required() {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        write_default_forge_toml(config_dir.path(), project_dir.path());
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");
        workspace.enable_test_dispatch_intercept();

        // Plan present, one account back mid-flight: the second half
        // holds. Flipping a Ready account to Loading after the seeding
        // keeps the plan - it is monotonic - so this isolates the
        // settled half from the plan half.
        workspace.seed_test_ready_account("Stargate");
        assert!(workspace.assignment_plan_ready(), "seeding Ready computes the plan");
        workspace.seed_test_account_state("Stargate", forge_workspace::LoadingState::Loading);

        let cli = cli_with(Some("forge-test"));
        let local = tokio::task::LocalSet::new();
        let workspace = Arc::new(workspace);
        let mut app = local
            .run_until(async {
                create_app_for_test(&cli, Arc::clone(&workspace), config_dir.path())
            })
            .await;
        super::start_connection(&mut app);
        assert!(
            workspace.drain_test_dispatch_buffer().is_empty(),
            "an account still resolving holds the spawn even with a plan in hand",
        );

        // The same account settling Bailed releases it: degraded rides
        // along, and the plan routes sessions to the accounts that can
        // take them.
        workspace.seed_test_account_state("Stargate", forge_workspace::LoadingState::Bailed);
        super::start_connection(&mut app);
        assert!(
            !workspace.drain_test_dispatch_buffer().is_empty(),
            "a bailed account must not hold the spawn forever",
        );
    }

    /// Preflight runs on every route, so `forge <project>` boots into
    /// it too rather than straight into chat - and `startup_project`,
    /// not the view, is what says where it hands over afterwards.
    ///
    /// `start_connection` branches on the same field, so the two agree
    /// about where a boot is headed without the view standing in for
    /// it.
    #[tokio::test(flavor = "current_thread")]
    async fn create_app_boots_into_preflight_with_argv_and_remembers_the_project() {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        write_default_forge_toml(config_dir.path(), project_dir.path());
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");
        let cli = cli_with(Some("forge-test"));
        let local = tokio::task::LocalSet::new();
        let app = local
            .run_until(async { create_app_for_test(&cli, Arc::new(workspace), config_dir.path()) })
            .await;

        assert_eq!(
            app.active_view,
            crate::app::ActiveView::Launchpad,
            "every route boots into preflight, argv or not",
        );
        assert!(!app.preflight_done, "and nothing seeds the latch; it is earned by completing");
        assert_eq!(
            app.startup_project.as_deref(),
            Some("forge-test"),
            "the named project is what the handover reads to land in chat rather than the picker",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_app_threads_new_flag_onto_start_new_run() {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        write_default_forge_toml(config_dir.path(), project_dir.path());
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");
        let mut cli = cli_with(Some("forge-test"));
        cli.new = true;
        let local = tokio::task::LocalSet::new();
        let app = local
            .run_until(async { create_app_for_test(&cli, Arc::new(workspace), config_dir.path()) })
            .await;
        assert!(app.start_new_run, "--new threads onto App.start_new_run");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_app_seeds_ui_settings_from_config() {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        let project_path_str = project_dir.path().to_string_lossy().replace('\\', "/");
        std::fs::write(
            forge_dir(config_dir.path()).join("forge.toml"),
            format!(
                "[[orgs]]\nname = \"Default\"\naccounts = [\"Stargate\"]\n\n[[orgs.projects]]\nname = \"forge-test\"\npath = \"{project_path_str}\"\nauto_start = true\n\n[[accounts]]\ndisplay_name = \"Stargate\"\nconfig_dir = \"~/.claude-stargate\"\nprovider = \"anthropic\"\n\n[ui]\nspinner = \"ember\"\nfps = 60\n"
            ),
        )
        .expect("write forge.toml");
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");
        let cli = cli_with(None);
        let local = tokio::task::LocalSet::new();
        let app = local
            .run_until(async { create_app_for_test(&cli, Arc::new(workspace), config_dir.path()) })
            .await;
        assert_eq!(app.spinner_style, forge_workspace::SpinnerStyle::Ember);
        // Deliberately not the default, or the assertion would pass on a
        // config value that never reached the App.
        assert_eq!(app.repaint_cadence, forge_workspace::RepaintCadence::from_fps(60));
        assert_ne!(app.repaint_cadence, forge_workspace::RepaintCadence::default());
    }

    #[cfg(feature = "perf")]
    #[tokio::test(flavor = "current_thread")]
    async fn create_app_for_test_puts_the_perf_log_in_the_caller_s_directory() {
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        write_default_forge_toml(config_dir.path(), project_dir.path());
        let workspace = forge_workspace::Workspace::new_for_test(config_dir.path().to_owned())
            .expect("workspace");
        let cli = cli_with(None);
        let local = tokio::task::LocalSet::new();
        let app = local
            .run_until(async { create_app_for_test(&cli, Arc::new(workspace), config_dir.path()) })
            .await;
        drop(app);

        let contents = std::fs::read_to_string(config_dir.path().join("forge-perf.log"))
            .expect("perf sidecar should open under the directory the caller supplied");
        assert!(
            contents.contains("run_started"),
            "this boot's run header should land in the redirected log, got {contents:?}"
        );
    }

    #[test]
    fn startup_launch_settings_default_force_new_false() {
        // The boundary: only the boot dispatch stamps force_new. The
        // builders default it false, so click-to-spawn / launchpad /
        // peer-spawn (which build their own settings) keep resuming.
        let app = crate::app::App::test_default();
        let settings = super::session_launch_settings_for_startup(&app);
        assert!(!settings.force_new, "non-boot launch settings default force_new=false");
    }
}

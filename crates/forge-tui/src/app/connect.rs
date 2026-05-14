//! App creation and connection startup.
//!
//! After Phase 4 of the MVVM refactor the boundary is locked:
//! TUI subscribes to `Workspace::subscribe()` once and reads
//! [`forge_workspace::SessionUpdate`] envelopes directly (no
//! `ClientEvent::WorkspaceUpdate` wrapper). User actions flow
//! out via [`forge_workspace::Workspace::dispatch`] with
//! [`forge_workspace::Command::Spawn*`] / `StartDefault` for App-
//! level kicks and per-session commands otherwise.

mod session_start;
pub(crate) mod type_converters;

use super::config::ConfigState;
use super::dialog::DialogState;
use super::plugins::PluginsState;
use super::state::{RenderCacheBudget, SessionPickerState};
use super::trust;
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

/// Create the `App` struct in `Connecting` state and load shared settings state.
///
/// `cwd_raw` and `cwd` are seeded from the process working directory
/// so trust + file index init have a sensible value to work with;
/// the `Connected` event overwrites them with the agent's reported
/// cwd once the workspace-backed agent finishes its handshake.
pub fn create_app(cli: &Cli, workspace: Arc<forge_workspace::Workspace>) -> App {
    // Seed cwd from the process working directory until the Connected
    // event delivers the agent's actual cwd. Trust + file_index init
    // need a non-empty value; reading `current_dir()` here is fine
    // because forge is always invoked from the project root the user
    // wants to operate on.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let cwd_display = shorten_cwd_for_display(&cwd);

    let (file_index_event_tx, file_index_event_rx) = std::sync::mpsc::channel();
    let (git_diff_event_tx, git_diff_event_rx) = std::sync::mpsc::channel();
    let (cli_version_event_tx, cli_version_event_rx) = std::sync::mpsc::channel();
    let (process_scan_event_tx, process_scan_event_rx) = std::sync::mpsc::channel();
    crate::app::git_diff::spawn_periodic_timer(git_diff_event_tx.clone());
    crate::app::cli_version::spawn_fetch(Arc::clone(&workspace), cli_version_event_tx.clone());
    crate::app::process_scanner::spawn_ticker(process_scan_event_tx.clone());
    // Kick off the workspace's 30s account-usage poller. Fetches OAuth
    // usage for every [[accounts]] entry; results land in the
    // workspace's account-usage cache and the TUI's bottom panel
    // reads from there via `Workspace::usage_for`.
    workspace.start_usage_poller();
    let perf_path = match crate::logging::resolve_perf_path(cli) {
        Ok(path) => path,
        Err(err) => {
            tracing::warn!(
                target: crate::logging::targets::APP_PERF,
                event_name = "perf_telemetry_unavailable",
                message = "failed to resolve perf telemetry sidecar path",
                outcome = "failure",
                telemetry_channel = "perf_sidecar",
                perf_schema = "forge-perf/v1",
                perf_append = cli.perf_append,
                error = %err,
            );
            None
        }
    };
    let perf = perf_path.as_deref().and_then(|path| {
        let logger = crate::perf::PerfLogger::open(path, cli.perf_append);
        if logger.is_some() {
            tracing::info!(
                target: crate::logging::targets::APP_PERF,
                event_name = "perf_telemetry_enabled",
                message = "perf telemetry sidecar enabled",
                outcome = "success",
                telemetry_channel = "perf_sidecar",
                perf_schema = "forge-perf/v1",
                perf_log = %path.display(),
                perf_append = cli.perf_append,
            );
        } else {
            tracing::warn!(
                target: crate::logging::targets::APP_PERF,
                event_name = "perf_telemetry_unavailable",
                message = "failed to enable perf telemetry sidecar",
                outcome = "failure",
                telemetry_channel = "perf_sidecar",
                perf_schema = "forge-perf/v1",
                perf_log = %path.display(),
                perf_append = cli.perf_append,
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
    pre_connect_session.cwd_raw = cwd.to_string_lossy().to_string();
    // Pre-register a handle-less DomainSession for the pre-Connect
    // key. The spawn handler later stamps the live `Arc<AgentHandle>`
    // onto this same domain entry when
    // `get_agent_handle_with_spawn_key` runs.
    let pre_connect_domain = workspace.register_domain_session(pre_connect_key.clone(), None);
    drop(pre_connect_domain);
    let mut sessions = std::collections::HashMap::new();
    sessions.insert(pre_connect_key.clone(), pre_connect_session);
    // Snapshot the persisted side-pane visibility before moving
    // `workspace` into the App struct below.
    let projects_pane_visible = workspace.projects_pane_visible();
    let inspector_pane_visible = workspace.inspector_pane_visible();
    let mut app = App {
        active_view: ActiveView::Chat,
        config: ConfigState::default(),
        trust: trust::TrustState::default(),
        settings_home_override: None,
        status: AppStatus::Connecting,
        should_quit: false,
        exit_error: None,
        workspace: Some(workspace),
        workspace_update_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        #[rustfmt::skip] #[cfg(feature = "testing")] test_dispatched_permission_outcomes: std::cell::RefCell::new(Vec::new()),
        #[rustfmt::skip] #[cfg(feature = "testing")] test_dispatched_question_outcomes: std::cell::RefCell::new(Vec::new()),
        sessions,
        active_session_key: Some(pre_connect_key),
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
        cli_version_event_tx,
        cli_version_event_rx,
        cli_version_info: None,
        process_scan_event_tx,
        process_scan_event_rx,
        spinner_frame: 0,
        spinner_last_advance_at: None,
        tools_collapsed: true,
        projects_pane_visible,
        projects_pane_overlay_open: false,
        inspector_pane_visible,
        inspector_pane_overlay_open: false,
        pane_hit_targets: Vec::new(),
        layout: crate::ui::layout::AppLayout::default(),
        force_redraw: false,
        focus: FocusManager::default(),
        plugins: PluginsState::default(),
        session_picker: SessionPickerState::default(),
        cached_frame_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        scrollbar_drag: None,
        rendered_chat_lines: Vec::new(),
        rendered_chat_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        rendered_input_lines: Vec::new(),
        rendered_input_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        rendered_inspector_body_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        paste_burst: super::paste_burst::PasteBurstDetector::new(),
        needs_redraw: true,
        notifications: super::notify::NotificationManager::new(),
        perf,
        render_cache_budget: RenderCacheBudget::default(),
        fps_ema: None,
        last_frame_at: None,
        startup_connection_requested: false,
        connection_started: false,
        startup_resume_id: None,
        startup_resume_requested: false,
        startup_session_picker_requested: false,
        startup_recent_sessions_loaded: false,
        startup_session_picker_resolved: false,
        startup_project: cli.project.clone(),
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

    app.rebuild_history_retention_accounting();
    app.rebuild_render_cache_accounting();
    // Re-derive the welcome's account-line shape now that the App
    // has its workspace pointer set. This lets workspace-mode
    // sessions render `Account: …` from the very first frame
    // instead of a hidden line that pops in once data arrives.
    app.sync_welcome_snapshot();
    trust::initialize(&mut app);
    super::file_index::restart(&mut app);
    app
}

/// Kick off the startup connection via the workspace command bus.
/// Replaces the legacy `bridge_lifecycle::run_connection_task` path
/// with a single `Command::StartDefault` dispatch — workspace owns
/// the spawn from there.
pub fn start_connection(app: &mut App) {
    if !app.startup_connection_requested || app.connection_started {
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

    app.connection_started = true;
    let launch_settings = session_start::session_launch_settings_for_reason(
        app,
        session_start::SessionStartReason::Startup,
    );
    let project_name = app.startup_project.clone();
    if let Err(err) =
        workspace.dispatch(forge_workspace::Command::StartDefault { project_name, launch_settings })
    {
        tracing::error!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "start_connection_dispatch_failed",
            error = %err,
            "Command::StartDefault dispatch failed",
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::Cli;
    use std::sync::Arc;

    fn write_default_forge_toml(dir: &std::path::Path, project_path: &std::path::Path) {
        let project_path_str = project_path.to_string_lossy().replace('\\', "/");
        std::fs::write(
            dir.join("forge.toml"),
            format!(
                "[[projects]]\nname = \"forge-test\"\npath = \"{project_path_str}\"\ndefault = true\naccounts = [\"Subspace\"]\n\n[[accounts]]\ndisplay_name = \"Subspace\"\nconfig_dir = \"~/.claude-subspace\"\n"
            ),
        )
        .expect("write forge.toml");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn create_app_wires_workspace_and_seeds_cwd_from_process() {
        // Workspace::new requires a forge.toml in the config dir. The
        // project path doesn't have to exist on disk — `create_app`
        // doesn't read it.
        let config_dir = tempfile::tempdir().expect("tempdir");
        let project_dir = tempfile::tempdir().expect("project tempdir");
        write_default_forge_toml(config_dir.path(), project_dir.path());
        let workspace =
            forge_workspace::Workspace::new(config_dir.path().to_owned()).await.expect("workspace");

        let cli = Cli {
            project: None,
            generate_completion: None,
            enable_logs: false,
            diagnostics_preset: None,
            log_file: None,
            log_filter: None,
            log_append: false,
            enable_perf: false,
            perf_log: None,
            perf_append: false,
        };

        let local = tokio::task::LocalSet::new();
        let app = local.run_until(async { super::create_app(&cli, Arc::new(workspace)) }).await;

        // cwd is seeded from the process; the Connected event later
        // overwrites it with the agent's reported value.
        assert!(!app.cwd_raw().is_empty(), "cwd_raw should be seeded from process cwd");
        assert!(app.workspace.is_some(), "workspace should be wired");
    }
}

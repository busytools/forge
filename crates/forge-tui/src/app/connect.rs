//! App creation and bridge connection lifecycle.
//!
//! Submodules:
//! - `bridge_lifecycle`: spawning the bridge, init handshake, event-relay loop +
//!   inline `AgentEvent` → `ClientEvent` translation
//! - `type_converters`: bridge wire types -> app model types (consumed by
//!   `bridge_lifecycle` and the App-side SDK message dispatcher)

mod bridge_lifecycle;
mod session_start;
pub(crate) mod type_converters;

use super::config::ConfigState;
use super::dialog::DialogState;
use super::plugins::PluginsState;
use super::state::{
    CacheMetrics, HistoryRetentionPolicy, HistoryRetentionStats, RenderCacheBudget,
    SessionPickerState,
};
use super::trust;
use super::view::ActiveView;
use super::{App, AppStatus, ChatViewport, FocusManager, HelpView, SelectionState, TodoItem};
use crate::Cli;
use crate::agent::client::SessionLaunchSettings;
use crate::agent::events::ClientEvent;
use crate::agent::model;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc;

struct StartConnectionParams {
    event_tx: mpsc::UnboundedSender<ClientEvent>,
    workspace: Rc<forge_workspace::Workspace>,
    session_launch_settings: SessionLaunchSettings,
    /// Project name from the CLI's positional `<PROJECT>` argument.
    /// `None` opens the `default = true` project; `Some(name)`
    /// resolves to [`forge_workspace::SessionTarget::Named`].
    project: Option<String>,
}

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

/// Create the `App` struct in `Connecting` state and load shared settings state.
///
/// `cwd_raw` and `cwd` are seeded from the process working directory
/// so trust + file index init have a sensible value to work with;
/// the `Connected` event overwrites them with the agent's reported
/// cwd once the workspace-backed agent finishes its handshake.
pub fn create_app(cli: &Cli, workspace: Rc<forge_workspace::Workspace>) -> App {
    // Seed cwd from the process working directory until the Connected
    // event delivers the agent's actual cwd. Trust + file_index init
    // need a non-empty value; reading `current_dir()` here is fine
    // because forge is always invoked from the project root the user
    // wants to operate on.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let cwd_display = shorten_cwd_for_display(&cwd);

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (file_index_event_tx, file_index_event_rx) = std::sync::mpsc::channel();
    let terminals: crate::agent::events::TerminalMap =
        Rc::new(std::cell::RefCell::new(HashMap::new()));
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

    let mut app = App {
        active_view: ActiveView::Chat,
        config: ConfigState::default(),
        trust: trust::TrustState::default(),
        settings_home_override: None,
        messages: vec![super::ChatMessage::welcome(
            env!("CARGO_PKG_VERSION"),
            "-",
            &cwd_display,
            "-",
        )],
        message_retained_bytes: Vec::new(),
        retained_history_bytes: 0,
        viewport: ChatViewport::new(),
        input: super::InputState::new(),
        status: AppStatus::Connecting,
        resuming_session_id: None,
        pending_command_label: None,
        pending_command_ack: None,
        should_quit: false,
        exit_error: None,
        session_id: None,
        conn: None,
        workspace: Some(workspace),
        session_scope_epoch: 0,
        current_model: None,
        cwd_raw: cwd.to_string_lossy().to_string(),
        cwd: cwd_display,
        files_accessed: 0,
        mode: None,
        config_options: std::collections::BTreeMap::new(),
        login_hint: None,
        pending_compact_clear: false,
        help_view: HelpView::Keys,
        help_open: false,
        help_dialog: DialogState::default(),
        help_visible_count: 0,
        pending_interaction_ids: Vec::new(),
        cancelled_turn_pending_hint: false,
        pending_cancel_origin: None,
        pending_auto_submit_after_cancel: false,
        event_tx,
        event_rx,
        file_index_event_tx,
        file_index_event_rx,
        spinner_frame: 0,
        spinner_last_advance_at: None,
        active_turn_assistant_message_idx: None,
        tools_collapsed: true,
        active_task_ids: HashSet::new(),
        tool_call_scopes: HashMap::new(),
        terminals,
        force_redraw: false,
        tool_call_index: HashMap::new(),
        todos: Vec::<TodoItem>::new(),
        show_todo_panel: false,
        todo_scroll: 0,
        todo_selected: 0,
        focus: FocusManager::default(),
        available_commands: Vec::new(),
        plugins: PluginsState::default(),
        available_agents: Vec::new(),
        available_models: Vec::new(),
        recent_sessions: Vec::new(),
        session_picker: SessionPickerState::default(),
        cached_frame_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        selection: Option::<SelectionState>::None,
        scrollbar_drag: None,
        rendered_chat_lines: Vec::new(),
        rendered_chat_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        rendered_input_lines: Vec::new(),
        rendered_input_area: ratatui::layout::Rect::new(0, 0, 0, 0),
        mention: None,
        file_index: super::file_index::FileIndexState::default(),
        slash: None,
        subagent: None,
        pending_submit: None,
        paste_burst: super::paste_burst::PasteBurstDetector::new(),
        pending_paste_text: String::new(),
        pending_paste_session: None,
        active_paste_session: None,
        next_paste_session_id: 1,
        pending_images: Vec::new(),
        cached_todo_compact: None,
        git_context: super::git_context::GitContextState::default(),
        session_usage: super::SessionUsageState::default(),
        usage: super::UsageState::default(),
        mcp: super::McpState::default(),
        fast_mode_state: model::FastModeState::Off,
        observed_permission_mode: None,
        observed_effort: None,
        subagent_attribution: std::collections::HashMap::new(),
        observed_assistant_model: None,
        runtime_session_state: None,
        prompt_suggestion: None,
        last_rate_limit_update: None,
        turn_notice_refs: Vec::new(),
        is_compacting: false,
        account_info: None,
        active_account_display_name: None,
        oauth_credentials: None,
        turn_state: super::SessionTurnState::default(),
        terminal_tool_calls: Vec::new(),
        terminal_tool_call_membership: HashSet::new(),
        needs_redraw: true,
        notifications: super::notify::NotificationManager::new(),
        perf,
        render_cache_budget: RenderCacheBudget::default(),
        render_cache_slots: Vec::new(),
        render_cache_total_bytes: 0,
        render_cache_protected_bytes: 0,
        render_cache_evictable: std::collections::BTreeSet::new(),
        render_cache_tail_msg_idx: None,
        history_retention: HistoryRetentionPolicy::default(),
        history_retention_stats: HistoryRetentionStats::default(),
        cache_metrics: CacheMetrics::default(),
        fps_ema: None,
        last_frame_at: None,
        last_chat_render_trace_state: None,
        last_active_turn_height_state: None,
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
    trust::initialize(&mut app);
    super::file_index::restart(&mut app);
    app
}

/// Spawn the background bridge task.
pub fn start_connection(app: &mut App) {
    if !app.startup_connection_requested || app.connection_started {
        return;
    }

    let Some(workspace) = app.workspace.as_ref().map(Rc::clone) else {
        tracing::error!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "start_connection_without_workspace",
            message = "start_connection invoked without a workspace; refusing to spawn bridge",
            outcome = "failure",
        );
        // Latch connection_started so the broken-invariant path only fires
        // once -- start_connection is called from the event loop every
        // iteration, and without this guard we'd pile up duplicate fatal
        // events forever.
        app.connection_started = true;
        // Surface the broken invariant as a fatal connection failure so
        // the event loop exits cleanly instead of spinning in Connecting.
        bridge_lifecycle::emit_connection_failed(
            &app.event_tx,
            "internal: workspace not initialised in App; cannot spawn bridge".to_owned(),
            crate::error::AppError::ConnectionFailed,
        );
        return;
    };

    app.connection_started = true;
    let params = StartConnectionParams {
        event_tx: app.event_tx.clone(),
        workspace,
        session_launch_settings: session_start::session_launch_settings_for_reason(
            app,
            session_start::SessionStartReason::Startup,
        ),
        project: app.startup_project.clone(),
    };
    let conn_slot: Rc<std::cell::RefCell<Option<ConnectionSlot>>> =
        Rc::new(std::cell::RefCell::new(None));
    let conn_slot_writer = Rc::clone(&conn_slot);

    tokio::task::spawn_local(async move {
        bridge_lifecycle::run_connection_task(params, conn_slot_writer).await;
    });

    CONN_SLOT.with(|slot| {
        debug_assert!(
            slot.borrow().is_none(),
            "CONN_SLOT already populated -- start_connection() called twice?"
        );
        *slot.borrow_mut() = Some(conn_slot);
    });
}

/// Shared slot for passing `Arc<forge_agent::AgentHandle>` from the background task to the event loop.
pub struct ConnectionSlot {
    pub conn: Arc<forge_agent::AgentHandle>,
}

thread_local! {
    pub static CONN_SLOT: std::cell::RefCell<Option<Rc<std::cell::RefCell<Option<ConnectionSlot>>>>> =
        const { std::cell::RefCell::new(None) };
}

/// Take the connection data from the thread-local slot.
pub(super) fn take_connection_slot() -> Option<ConnectionSlot> {
    CONN_SLOT.with(|slot| slot.borrow().as_ref().and_then(|inner| inner.borrow_mut().take()))
}

#[cfg(test)]
mod tests {
    use crate::Cli;
    use std::rc::Rc;

    fn write_default_forge_toml(dir: &std::path::Path, project_path: &std::path::Path) {
        let project_path_str = project_path.to_string_lossy().replace('\\', "/");
        std::fs::write(
            dir.join("forge.toml"),
            format!(
                "[[projects]]\nname = \"forge-test\"\npath = \"{project_path_str}\"\ndefault = true\n\n[[accounts]]\ndisplay_name = \"Subspace\"\nconfig_dir = \"~/.claude-subspace\"\n"
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

        let app = super::create_app(&cli, Rc::new(workspace));

        // cwd is seeded from the process; the Connected event later
        // overwrites it with the agent's reported value.
        assert!(!app.cwd_raw.is_empty(), "cwd_raw should be seeded from process cwd");
        assert!(app.workspace.is_some(), "workspace should be wired");
    }
}

use clap::Parser;
use forge_tui::Cli;
use forge_tui::error::AppError;
use std::path::PathBuf;
use std::rc::Rc;
use tracing::info_span;

// Binary entry — `process::exit` is the only way to set a non-zero
// exit code without unwinding, which matters for clean tty restoration.
#[allow(clippy::exit)]
fn main() {
    if let Err(err) = run() {
        if let Some(app_error) = extract_app_error(&err) {
            eprintln!("{}", app_error.user_message());
            std::process::exit(app_error.exit_code());
        }
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _logging = forge_tui::logging::LoggingRuntime::init(&cli)?;
    let perf_path = forge_tui::logging::resolve_perf_path(&cli)?;

    #[cfg(not(feature = "perf"))]
    if perf_path.is_some() {
        return Err(anyhow::anyhow!(
            "perf telemetry requires a binary built with `--features perf`"
        ));
    }

    {
        let startup_bootstrap_span = info_span!(
            target: forge_tui::logging::targets::APP_LIFECYCLE,
            "startup_bootstrap",
            resume_requested = false,
            perf_telemetry_requested = perf_path.is_some(),
        );
        let _entered = startup_bootstrap_span.enter();
    }

    let rt = tokio::runtime::Runtime::new()?;
    let local_set = tokio::task::LocalSet::new();

    rt.block_on(local_set.run_until(async move {
        // Phase 0: build the workspace orchestrator. Surface its
        // load errors to stderr (TUI hasn't started yet, no tty
        // restoration needed) and exit non-zero.
        let config_dir = resolve_config_dir();
        let workspace = match forge_workspace::Workspace::new(config_dir).await {
            Ok(w) => Rc::new(w),
            Err(err) => return Err(anyhow::anyhow!("forge: {err}")),
        };

        // Phase 1: create app in Connecting state (instant, no I/O)
        let mut app = forge_tui::app::create_app(&cli, workspace);

        // Phase 2: start non-session startup work + TUI.
        // The bridge itself is started from the TUI loop only after trust is accepted.
        forge_tui::app::start_service_status_check(&app);
        let result = forge_tui::app::run_tui(&mut app).await;

        // Kill any spawned terminal child processes before exiting
        forge_tui::agent::events::kill_all_terminals(&app.terminals);

        if let Some(app_error) = app.exit_error.take() {
            return Err(anyhow::Error::new(app_error));
        }

        result
    }))
}

/// Resolve the Claude config directory: honour `$CLAUDE_CONFIG_DIR`
/// (ignoring empty values), else fall back to `$HOME/.claude`.
/// Mirrors `forge_sdk::claude_config_dir` — kept in-crate for now
/// since forge-tui doesn't otherwise depend on forge-sdk and the
/// duplication is two branches. Consolidate when more callers need it.
fn resolve_config_dir() -> PathBuf {
    if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = value.trim_end_matches('/');
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".claude")
}

fn extract_app_error(err: &anyhow::Error) -> Option<AppError> {
    err.chain().find_map(|cause| cause.downcast_ref::<AppError>().cloned())
}

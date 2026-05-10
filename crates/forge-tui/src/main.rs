use clap::{CommandFactory, Parser};
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

    // Short-circuit: --generate-completion writes a completion script
    // to stdout and exits before any logging / TUI / workspace setup.
    if let Some(shell) = cli.generate_completion {
        let mut cmd = Cli::command();
        clap_complete::generate(shell, &mut cmd, "forge", &mut std::io::stdout());
        return Ok(());
    }

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

        // Validate the CLI's positional `<PROJECT>` arg, if any, before
        // entering the TUI. This turns `forge xyz` (where `xyz` isn't in
        // forge.toml) into a clean stderr error + non-zero exit instead
        // of a TUI flash followed by ConnectionFailed.
        if let Some(name) = cli.project.as_deref()
            && let Err(err) = workspace.validate_project_name(name)
        {
            return Err(anyhow::anyhow!("forge: {err}"));
        }

        // Phase 1: create app in Connecting state (instant, no I/O).
        // App holds an Rc clone; main retains the original so we can
        // reclaim ownership and call `shutdown().await` after the
        // event loop returns.
        let mut app = forge_tui::app::create_app(&cli, Rc::clone(&workspace));

        // Phase 2: start non-session startup work + TUI.
        // The bridge itself is started from the TUI loop only after trust is accepted.
        forge_tui::app::start_service_status_check(&app);
        let result = forge_tui::app::run_tui(&mut app).await;

        // Kill any spawned terminal child processes before exiting
        forge_tui::agent::events::kill_all_terminals(&app.terminals);

        let exit_error = app.exit_error.take();

        // Phase 3: drop the App so its Rc<Workspace> clone is released,
        // then reclaim ownership of the workspace and drain the agent
        // pool gracefully. If any background task (e.g. the connection
        // task in `run_connection_task`) still holds an Rc clone,
        // `try_unwrap` fails — we log and skip the explicit shutdown,
        // letting `Drop` on Workspace + `kill_on_drop` on the
        // subprocesses handle teardown.
        drop(app);
        match Rc::try_unwrap(workspace) {
            Ok(workspace) => {
                workspace.shutdown().await;
            }
            Err(_) => {
                tracing::warn!(
                    target: forge_tui::logging::targets::APP_LIFECYCLE,
                    event_name = "workspace_shutdown_skipped",
                    message = "workspace Rc still held at exit; agents drop via Drop instead of shutdown",
                    outcome = "skipped",
                );
            }
        }

        if let Some(app_error) = exit_error {
            return Err(anyhow::Error::new(app_error));
        }

        result
    }))
}

/// Resolve the Claude config directory at the forge-tui orchestration
/// boundary: honour `$CLAUDE_CONFIG_DIR` (ignoring empty values), else
/// fall back to `$HOME/.claude`. After this point, the resolved path
/// is threaded as a typed `PathBuf` (via `Workspace::new`, which in
/// turn binds each `Agent::spawn(config_dir)` to its account's path).
/// forge-sdk exposes `claude_config_dir_from_env() -> Option<PathBuf>`
/// for the env-only branch; the host-default fallback lives here so
/// the SDK stays opinion-free about "what to do when env is unset".
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

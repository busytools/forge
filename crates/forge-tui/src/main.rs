use clap::Parser;
use forge_tui::Cli;
use forge_tui::error::AppError;
use tracing::info_span;

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
            resume_requested = matches!(
                cli.command,
                Some(forge_tui::Command::Resume { .. })
            ),
            perf_telemetry_requested = perf_path.is_some(),
        );
        let _entered = startup_bootstrap_span.enter();
    }

    let rt = tokio::runtime::Runtime::new()?;
    let local_set = tokio::task::LocalSet::new();

    rt.block_on(local_set.run_until(async move {
        // Phase 1: create app in Connecting state (instant, no I/O)
        let mut app = forge_tui::app::create_app(&cli);

        // Phase 2: start non-session startup work + TUI.
        // The bridge itself is started from the TUI loop only after trust is accepted.
        forge_tui::app::start_service_status_check(&app);
        let result = forge_tui::app::run_tui(&mut app).await;
        maybe_print_resume_hint(&app, result.is_ok());

        // Kill any spawned terminal child processes before exiting
        forge_tui::agent::events::kill_all_terminals(&app.terminals);

        if let Some(app_error) = app.exit_error.take() {
            return Err(anyhow::Error::new(app_error));
        }

        result
    }))
}

fn extract_app_error(err: &anyhow::Error) -> Option<AppError> {
    err.chain().find_map(|cause| cause.downcast_ref::<AppError>().cloned())
}

fn maybe_print_resume_hint(app: &forge_tui::app::App, success: bool) {
    if !success {
        return;
    }
    let Some(session_id) = app.session_id.as_ref() else {
        return;
    };
    eprintln!("Resume this session: forge resume {session_id}");
}

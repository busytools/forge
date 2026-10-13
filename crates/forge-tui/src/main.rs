use clap::{CommandFactory, Parser};
use forge_tui::Cli;
use forge_tui::error::AppError;
use std::io::Write;
use std::sync::Arc;
use tracing::info_span;

// Binary entry - `process::exit` carries the exit code out without
// unwinding. Clean tty restoration no longer depends on that: the TUI's guard
// restores the terminal on every path, unwinding included.
#[allow(clippy::exit)]
fn main() {
    if let Err(err) = run() {
        if let Some(app_error) = extract_app_error(&err) {
            report_error(app_error.user_message());
            std::process::exit(app_error.exit_code());
        }
        report_error(&format!("{err}"));
        std::process::exit(1);
    }
}

/// Say why forge is exiting, without panicking on the way out: `eprintln!`
/// panics when the write fails, and by the time forge is here stderr can be
/// gone already.
fn report_error(message: &str) {
    let _ = writeln!(std::io::stderr(), "{message}");
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
    // Raise the FD soft limit BEFORE any code that opens sockets /
    // files / pipes so the bump applies to every subsequent opener
    // (tokio runtime, workspace I/O, etc). Sits AFTER tracing init so
    // the bump's log line lands. See #251.
    forge_tui::startup::raise_fd_limit();
    forge_tui::startup::report_build_provenance();

    {
        let startup_bootstrap_span = info_span!(
            target: forge_tui::logging::targets::APP_LIFECYCLE,
            "startup_bootstrap",
            resume_requested = false,
        );
        let _entered = startup_bootstrap_span.enter();
    }

    let rt = tokio::runtime::Runtime::new()?;
    let local_set = tokio::task::LocalSet::new();

    rt.block_on(local_set.run_until(async move {
        // Build the workspace orchestrator and start the subsystem wave -
        // one shared boot with the headless server (`forge_server::boot`),
        // which also validates the CLI's positional `<PROJECT>` before any
        // view comes up. Surface load errors to stderr (the TUI has not
        // started yet, no tty restoration needed) and exit non-zero.
        let config_dir = match forge_server::boot::resolve_config_dir() {
            Ok(p) => p,
            Err(err) => return Err(anyhow::anyhow!("forge: {err}")),
        };
        let workspace = forge_server::boot::start_core(config_dir, cli.project.as_deref())?;
        let _serving = forge_server::boot::serve_socket(&workspace).await?;

        // Create the app (instant, no I/O). The TUI holds an
        // `Arc<Workspace>` clone; main keeps the original so it
        // can drain the pool after the event loop returns.
        let mut app = forge_tui::app::create_app(&cli, Arc::clone(&workspace));

        let result = forge_tui::app::run_tui(&mut app).await;

        let exit_error = app.exit_error.take();

        // Drop the App so its `Arc<Workspace>` clone releases, then
        // drain the agent pool. Background tasks holding Arc clones
        // observe their command channel closing and exit on their
        // own - `shutdown` is synchronous.
        drop(app);
        workspace.shutdown();

        if let Some(app_error) = exit_error {
            return Err(anyhow::Error::new(app_error));
        }

        result
    }))
}

fn extract_app_error(err: &anyhow::Error) -> Option<AppError> {
    err.chain().find_map(|cause| cause.downcast_ref::<AppError>().cloned())
}

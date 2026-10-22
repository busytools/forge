//! The headless server: the whole core, no view.
//!
//! Boots the workspace and every subsystem, serves the socket, and parks
//! until interrupted - the boot itself is `forge_server::boot`, one call.
//!
//! The bin is named `forge`: the name the terminal's own binary held, and
//! the name `scripts/install.sh` installs.

use clap::{CommandFactory, Parser};
use forge_server::cli::Cli;
use forge_server::error::AppError;
use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

/// Report the failure and carry its exit code out.
///
/// `ExitCode` rather than `process::exit`: the code leaves `main` the same
/// way, and an early exit that skips destructors has nothing to skip here -
/// `run` has already dropped the workspace by the time this reports.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if let Some(app_error) = extract_app_error(&err) {
                report_error(app_error.user_message());
                ExitCode::from(u8::try_from(app_error.exit_code()).unwrap_or(1))
            } else {
                report_error(&format!("{err}"));
                ExitCode::FAILURE
            }
        }
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
    // to stdout and exits before any logging / workspace setup.
    if let Some(shell) = cli.generate_completion {
        let mut cmd = Cli::command();
        clap_complete::generate(shell, &mut cmd, "forge", &mut std::io::stdout());
        return Ok(());
    }

    let _logging = forge_server::logging::LoggingRuntime::init(&cli)?;
    // Raise the FD soft limit BEFORE any code that opens sockets /
    // files / pipes so the bump applies to every subsequent opener
    // (tokio runtime, workspace I/O, etc). Sits AFTER tracing init so
    // the bump's log line lands. See #251.
    forge_server::startup::raise_fd_limit();
    forge_server::startup::report_build_provenance();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        // Every stop signal a system sends a long-lived server is registered
        // BEFORE anything binds: a SIGTERM racing the socket's bind would
        // otherwise meet the default disposition and kill the process
        // mid-boot. Ctrl-C (SIGINT), SIGTERM and SIGHUP then take the same
        // path, so a supervisor's stop and a terminal closing both release
        // the single-instance lock and the store exactly as an interactive
        // Ctrl-C does - and each waits for the sessions' teardown, which is
        // where the session children and their groups are killed.
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

        let config_dir = match forge_server::boot::resolve_config_dir() {
            Ok(p) => p,
            Err(err) => return Err(anyhow::anyhow!("forge: {err}")),
        };
        let workspace = forge_server::boot::start_core(config_dir, cli.project.as_deref())?;
        // A headless server with no view serves nothing if the socket could
        // not bind: a supervisor watching the process would see a healthy
        // zombie, so the failure is fatal. The handle is kept: the stop path
        // takes the socket down before it drains, so no command can start a
        // session behind the drain.
        let mut socket = match forge_server::boot::serve_socket(&workspace).await? {
            forge_server::boot::SocketServing::Bound(handle) => Some(handle),
            forge_server::boot::SocketServing::Disabled => None,
            forge_server::boot::SocketServing::BindFailed => {
                return Err(anyhow::anyhow!(
                    "forge: the socket could not bind; a headless server with no view serves \
                     nothing (see the server_socket_bind_failed record)"
                ));
            }
        };
        // The boot spawn releases once its accounts settle: with no view to
        // hold it back, it goes out as a task.
        forge_server::boot::start_boot_wave(Arc::clone(&workspace), cli.project.clone(), cli.new);

        // Park until one of the stop signals lands; the subsystems own
        // everything else.
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
            _ = sighup.recv() => {}
        }
        // The serve loop is dropped before the drain. Live connections
        // outlive the abort, so it is not what refuses a spawn: the
        // shutting-down gate is, and it is already up.
        if let Some(handle) = socket.take() {
            handle.abort();
        }
        // Awaited, not fired and left: the runtime is about to drop, and a
        // dropped runtime does not poll the session tasks' exit paths. A
        // teardown that ran out of budget exits non-zero, so a supervisor
        // can tell a clean stop from one that left a tree.
        let unready = workspace.shutdown().await;
        forge_server::boot::stop_result(&unready)?;
        Ok(())
    })
}

fn extract_app_error(err: &anyhow::Error) -> Option<AppError> {
    err.chain().find_map(|cause| cause.downcast_ref::<AppError>().cloned())
}

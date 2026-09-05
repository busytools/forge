use clap::{CommandFactory, Parser};
use forge_tui::Cli;
use forge_tui::error::AppError;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info_span;

// Binary entry - `process::exit` is the only way to set a non-zero
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
    // Raise the FD soft limit BEFORE any code that opens sockets /
    // files / pipes so the bump applies to every subsequent opener
    // (tokio runtime, workspace I/O, etc). Sits AFTER tracing init so
    // the bump's log line lands. See #251.
    forge_tui::startup::raise_fd_limit();
    forge_tui::startup::report_build_provenance();
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
        // Build the workspace orchestrator. Surface load errors to
        // stderr (TUI hasn't started yet, no tty restoration needed)
        // and exit non-zero.
        let config_dir = match resolve_config_dir() {
            Ok(p) => p,
            Err(err) => return Err(anyhow::anyhow!("forge: {err}")),
        };
        let workspace = match forge_workspace::Workspace::new(config_dir) {
            Ok(w) => Arc::new(w),
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

        // Kick off the per-account boot-time loading state machine
        // (one task per `[[accounts]]` entry) in the background. Each
        // task drives its account from `Loading` through to a
        // terminal `Ready` or `Bailed`; the launchpad consults
        // `all_loaded()` across the map to decide when to un-dim the
        // project rows. The redb store has already seeded the
        // in-memory map at `Workspace::new`. The 60 s poller starts
        // after, run by `start_usage_poller` from
        // forge-tui's connect path.
        workspace.start_account_loading_tasks();

        // Fetch, verify and load the dictation models, on the same
        // preflight screen as the accounts. No-op unless `[dictate]
        // enabled` is set: a 3 GB download is opt-in.
        workspace.start_dictate_preflight();

        // Spawn the worker-kick drainer task (#259). Routes every
        // `maybe_kick_worker_on_connected` enqueue through a single
        // `KICK_DISPATCH_INTERVAL`-spaced dispatcher so multi-worker
        // boots don't hit Anthropic's per-IP burst limit. Idempotent
        // - the start helper no-ops a second call.
        workspace.start_kick_dispatcher();

        // Boot catch-up: one tick fires each overdue cron once, advancing
        // past the missed slots (a cron missed while forge was down fires
        // now, not once per skipped tick). Runs after Workspace::new so
        // spawn-to-fire can spawn an asleep project's session.
        workspace.fire_due_crons(std::time::SystemTime::now());

        // Start the durable-cron scheduler: wakes every ~60s and fires
        // crons as they come due, advancing/removing each.
        workspace.start_cron_scheduler();

        // Start the Gotify subsystem when configured with at least one
        // durable subscription loaded at boot; no-op otherwise.
        workspace.start_gotify_subsystem();

        // Drop review state left behind by branches deleted since the
        // last run. Worker teardown only catches a branch already gone at
        // that moment, and a branch usually outlives its worker.
        workspace.start_review_branch_sweep();

        // Create the app (instant, no I/O). The TUI holds an
        // `Arc<Workspace>` clone; main keeps the original so it
        // can drain the pool after the event loop returns.
        let mut app = forge_tui::app::create_app(&cli, Arc::clone(&workspace));

        forge_tui::app::start_service_status_check(&app);
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

/// Resolve the Claude config directory at the forge-tui orchestration
/// boundary: honour `$CLAUDE_CONFIG_DIR` (ignoring empty values), else
/// fall back to `$HOME/.claude`. After this point, the resolved path
/// is threaded as a typed `PathBuf` (via `Workspace::new`, which in
/// turn binds each `Agent::spawn(config_dir)` to its account's path).
///
/// Per hard rule #14 - no cwd-derived fallbacks. When `$CLAUDE_CONFIG_DIR`
/// is unset/empty AND `dirs::home_dir()` returns None, refuse to launch
/// rather than substituting `./.claude`.
fn resolve_config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(value) = std::env::var("CLAUDE_CONFIG_DIR") {
        let trimmed = value.trim_end_matches('/');
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    dirs::home_dir().map(|h| h.join(".claude")).ok_or_else(|| {
        anyhow::anyhow!("$CLAUDE_CONFIG_DIR unset and could not resolve home directory")
    })
}

fn extract_app_error(err: &anyhow::Error) -> Option<AppError> {
    err.chain().find_map(|cause| cause.downcast_ref::<AppError>().cloned())
}

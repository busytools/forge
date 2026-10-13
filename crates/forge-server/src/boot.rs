//! The boot the server runs: the config dir, the workspace, the subsystem
//! wave, the socket, and the boot spawn.
//!
//! Moved here from the view's `main.rs` so the core comes up under one
//! function for a view and for no view alike, and so no view's removal can
//! take the boot with it.

use std::path::PathBuf;
use std::sync::Arc;

use forge_workspace::Workspace;

/// Resolve the Claude config directory at the boot boundary: honour
/// `$CLAUDE_CONFIG_DIR` (ignoring empty values), else fall back to
/// `$HOME/.claude`. After this point, the resolved path is threaded as a
/// typed `PathBuf` (via `Workspace::new`, which in turn binds each
/// `Agent::spawn(config_dir)` to its account's path).
///
/// Per hard rule #14 - no cwd-derived fallbacks. When `$CLAUDE_CONFIG_DIR`
/// is unset/empty AND `dirs::home_dir()` returns None, refuse to launch
/// rather than substituting `./.claude`.
pub fn resolve_config_dir() -> anyhow::Result<PathBuf> {
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

/// Build the workspace and start the whole subsystem wave, in the order the
/// boot has always started it.
///
/// `project` is the CLI's positional, validated here because a mistyped
/// name has to fail the boot the same way for every caller; what each
/// caller then DOES with it differs (the headless server starts that
/// project's sessions).
///
/// Not `async`: every step is a spawn or a store write, and the await on
/// anything that actually does I/O belongs to `serve_socket`. Nothing here
/// needs a `LocalSet` either - every task in this wave is a plain
/// `tokio::spawn`, verified by the headless boot test exercising it
/// without one.
pub fn start_core(config_dir: PathBuf, project: Option<&str>) -> anyhow::Result<Arc<Workspace>> {
    let workspace = match Workspace::new(config_dir) {
        Ok(w) => Arc::new(w),
        Err(err) => return Err(anyhow::anyhow!("forge: {err}")),
    };

    // Validate the CLI's positional `<PROJECT>` arg, if any, before any
    // view or spawn wave. This turns `forge xyz` (where `xyz` isn't in
    // forge.toml) into a clean stderr error + non-zero exit instead of a
    // boot that fails deeper with a worse message.
    if let Some(name) = project
        && let Err(err) = workspace.validate_project_name(name)
    {
        return Err(anyhow::anyhow!("forge: {err}"));
    }

    // Kick off the per-account boot-time loading state machine (one task
    // per `[[accounts]]` entry) in the background. Each task drives its
    // account from `Loading` through to a terminal `Ready` or `Bailed`.
    // The redb store has already seeded the in-memory map at
    // `Workspace::new`. The 60 s poller is its own task
    // (`Workspace::start_usage_poller`), not part of this call.
    workspace.start_account_loading_tasks();

    // Bind the gateway's inference listener and open the boot gate for
    // it. Every spawned session's base URL names this port: a failed bind
    // holds the gate shut AND spawn attempts are refused at the spawn
    // entries until forge restarts.
    workspace.start_gateway_listener();

    // Load the model catalogue's cache and refresh a stale feed in the
    // background. No-op unless `[dictate] enabled` is set, and never on
    // the boot path: the fetch is what the models page's freshness line
    // reports on.
    workspace.start_dictate_catalogue();

    // Fetch, verify and load the dictation models, on the same screen the
    // accounts appear on. No-op unless `[dictate] enabled` is set: a 3 GB
    // download is opt-in.
    workspace.start_dictate_preflight();

    // Spawn the worker-kick drainer task (#259). Routes every
    // `maybe_kick_worker_on_connected` enqueue through a single
    // `KICK_DISPATCH_INTERVAL`-spaced dispatcher so multi-worker boots
    // don't hit Anthropic's per-IP burst limit. Idempotent - the start
    // helper no-ops a second call.
    workspace.start_kick_dispatcher();

    // Boot catch-up: one tick fires each overdue cron once, advancing
    // past the missed slots (a cron missed while forge was down fires
    // now, not once per skipped tick). Runs after Workspace::new so
    // spawn-to-fire can spawn an asleep project's session.
    workspace.fire_due_crons(std::time::SystemTime::now());

    // Start the durable-cron scheduler: wakes every ~60s and fires
    // crons as they come due, advancing/removing each.
    workspace.start_cron_scheduler();

    // Start the chase sweep: every ~30s the board's misses are derived
    // and the crossings delivered - the pass that makes the task list
    // drive instead of report.
    workspace.start_chase_sweep();

    // Start the Gotify subsystem when configured with at least one
    // durable subscription loaded at boot; no-op otherwise.
    workspace.start_gotify_subsystem();

    // Prove each configured `[[slack]]` token with `auth.test`. A
    // failure is logged and that workspace stays dormant; it never
    // stops the boot. No-op with no `[[slack]]` entry.
    workspace.start_slack_verification();

    // Start one Slack pump per workspace with a durable subscription
    // loaded at boot; no-op otherwise. The subscribe tool starts one
    // for a workspace that gains its first.
    workspace.start_slack_subsystem();

    // Drop review state left behind by branches deleted since the
    // last run. Worker teardown only catches a branch already gone at
    // that moment, and a branch usually outlives its worker.
    workspace.start_review_branch_sweep();

    Ok(workspace)
}

/// What [`serve_socket`] did.
///
/// Its own type rather than an `Option` so the caller decides what an
/// unbound socket means to it: a headless server with no view serves
/// nothing and treats the failure as fatal.
pub enum SocketServing {
    /// `[server] enabled = false`: nothing serves, by configuration.
    Disabled,
    /// The socket is listening; the handle ends when the process does.
    Bound(tokio::task::JoinHandle<()>),
    /// The bind failed, its log line already written.
    BindFailed,
}

/// Serve the socket on the address `[server]` names, spawning the serve
/// task and returning its handle when one is serving.
///
/// A bind that fails never fails the boot here: this is not on the boot
/// path, and the failure is logged plus reported as
/// [`SocketServing::BindFailed`] so the caller's boot cannot die of it.
pub async fn serve_socket(workspace: &Arc<Workspace>) -> anyhow::Result<SocketServing> {
    let server = workspace.server_config();
    if !server.enabled {
        tracing::debug!(
            target: crate::logging::targets::APP_LIFECYCLE,
            event_name = "server_socket_disabled",
            "[server] enabled = false; nothing is serving this run",
        );
        return Ok(SocketServing::Disabled);
    }
    let addr = std::net::SocketAddr::new(server.bind, server.port);
    // Built from the workspace so every piece the socket needs is
    // derived where the workspace is in hand - the browser relay above
    // all, which must be the one the sessions ask through.
    let state = Arc::new(crate::transport::TransportState::for_workspace(
        workspace,
        workspace.client_config(),
    ));
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            let bound = listener.local_addr().unwrap_or(addr);
            tracing::info!(
                target: crate::logging::targets::APP_LIFECYCLE,
                event_name = "server_socket_listening",
                %bound,
                "the server's socket is listening",
            );
            let handle = tokio::spawn(async move {
                if let Err(error) = crate::transport::serve(state, listener).await {
                    tracing::error!(
                        target: crate::logging::targets::APP_LIFECYCLE,
                        event_name = "server_socket_failed",
                        error = %error,
                        "the socket stopped serving",
                    );
                }
            });
            Ok(SocketServing::Bound(handle))
        }
        Err(error) => {
            tracing::error!(
                target: crate::logging::targets::APP_LIFECYCLE,
                event_name = "server_socket_bind_failed",
                error = %error,
                "the socket is not serving",
            );
            Ok(SocketServing::BindFailed)
        }
    }
}

/// How long the boot wave holds for the accounts and the listener before it
/// fires anyway.
///
/// A view can hold for as long as the reader is on its preflight screen - a
/// signal a headless process does not have. A listener that never binds - a
/// port someone else holds is the case - would otherwise make
/// `all_accounts_loaded` false forever and turn the wave into a spawn that
/// never fires, silently.
const BOOT_WAVE_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Hold until the accounts and the listener are ready, or `timeout` passes.
///
/// `true` when they settled, `false` at expiry; the caller decides what a
/// hold that ran out means (the wave fires anyway and warns). The bound is
/// the whole point - see [`start_boot_wave`] - so it is a parameter, which
/// is what lets a test pin it at a millisecond instead of a minute.
async fn wait_for_accounts(workspace: &Arc<Workspace>, timeout: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while !workspace.all_accounts_loaded() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    workspace.all_accounts_loaded()
}

/// The boot spawn, for a process with no view: hold until the accounts
/// settle, then dispatch the wave a view's connect path dispatches -
/// the named project's lead, or every `auto_start` project, in forge.toml
/// order.
///
/// Settling is terminal rather than `Ready` - a bailed account releases
/// the spawn, because the walk skips it and the pollers keep re-probing
/// it, so holding would wait on nothing.
///
/// **The hold is bounded** (`BOOT_WAVE_WAIT`). On expiry the wave goes
/// out regardless and a WARN names what was not ready: a refused spawn
/// carries its own reason (the spawn entries attach it), which beats a wave
/// that never fires and says nothing.
///
/// Spawned rather than awaited: the boot must not block on accounts, and
/// nothing downstream reads the result.
pub fn start_boot_wave(workspace: Arc<Workspace>, project: Option<String>, force_new: bool) {
    tokio::spawn(async move {
        if !wait_for_accounts(&workspace, BOOT_WAVE_WAIT).await {
            tracing::warn!(
                target: crate::logging::targets::APP_LIFECYCLE,
                event_name = "boot_wave_wait_expired",
                gateway_ready = workspace.gateway_ready(),
                gateway_bind_error = workspace.gateway_bind_error().as_deref().unwrap_or(""),
                "the boot spawn is going out with the accounts or the gateway listener not ready \
                 (a bound listener here means the account map is what held it); a refused spawn \
                 carries its own reason",
            );
        }
        // Boot wave only: --new makes the boot leads start fresh instead
        // of resuming (and cascades to their workers).
        let launch_settings =
            forge_workspace::SessionLaunchSettings { force_new, ..Default::default() };
        let targets: Vec<String> = match project {
            Some(name) => vec![name],
            None => crate::surface::ViewSurface::new(Arc::clone(&workspace)).roster().auto_start(),
        };
        for project_name in targets {
            let cmd = forge_workspace::Command::SpawnProject {
                project_name: project_name.clone(),
                launch_settings: launch_settings.clone(),
            };
            if let Err(err) = workspace.dispatch(cmd) {
                tracing::error!(
                    target: crate::logging::targets::APP_LIFECYCLE,
                    event_name = "boot_wave_dispatch_failed",
                    error = %err,
                    project = %project_name,
                    "boot wave dispatch failed",
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{BOOT_WAVE_WAIT, wait_for_accounts};
    use std::time::Duration;

    /// The hold gives up at its deadline rather than waiting for ever - the
    /// behaviour a listener that never binds would otherwise hang on. Pinned
    /// against the production bound's VALUE as well, so raising it is a
    /// change this test has to be told about.
    #[tokio::test]
    async fn the_boot_wave_hold_expires_rather_than_waiting_for_ever() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        assert!(
            !workspace.all_accounts_loaded(),
            "the fixture's accounts start unsettled, or this test proves nothing",
        );
        assert_eq!(BOOT_WAVE_WAIT, Duration::from_secs(60), "the production bound");

        let settled = wait_for_accounts(&workspace, Duration::from_millis(1)).await;
        assert!(!settled, "the hold gives up at its deadline instead of waiting for ever");
    }

    /// And it releases the moment the accounts settle, without waiting the
    /// bound out.
    #[tokio::test]
    async fn the_boot_wave_hold_releases_once_the_accounts_settle() {
        let (workspace, _dir) = crate::surface::testing::workspace();
        workspace.seed_test_ready_account("Granite");
        workspace.seed_test_ready_account("OpenRouter-TM");
        assert!(workspace.all_accounts_loaded(), "both accounts terminal and the listener ready");

        let started = std::time::Instant::now();
        let settled = wait_for_accounts(&workspace, Duration::from_secs(60)).await;
        assert!(settled, "a settled boot is released at once");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "and it does not wait the bound out: {:?}",
            started.elapsed(),
        );
    }
}

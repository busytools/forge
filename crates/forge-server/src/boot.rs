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

    start_subsystems(&workspace);
    install_browser(&workspace)?;

    Ok(workspace)
}

/// What the deleted `[browser] grace_minutes` key used to supply, until the
/// grace path itself goes.
#[cfg(target_os = "macos")]
const GRACE_MINUTES: u64 = 10;

/// Install the server's browser fleet: constructed here, one host and one
/// driver per profile spawned on first use.
///
/// **A missing app-support directory fails the boot**, the same resolution
/// the durable store needs; the vendored driver stack is not required at
/// boot, so a server without it boots and its browser calls name what is
/// missing instead of the whole server refusing to start.
#[cfg(target_os = "macos")]
fn install_browser(workspace: &Arc<Workspace>) -> anyhow::Result<()> {
    let fleet = Arc::new(
        forge_browser::service::BrowserService::installed()
            .map_err(|why| anyhow::anyhow!("forge: {why}"))?,
    );
    // **The encoding and the idle clock are the `[browser]` keys**: a
    // profile no viewer has touched closes after the bound, and the
    // workspace phrases the one warning a named profile's owner gets.
    let browser = workspace.browser_settings();
    fleet.configure_encoding(browser.fps, browser.quality);
    fleet.configure_idle(
        browser.idle_minutes,
        GRACE_MINUTES,
        Arc::new(IdleWarnings { workspace: Arc::downgrade(workspace) }),
    );
    fleet.start_idle();
    // **A host an unclean exit left behind is reaped before any profile is
    // used.** It still holds the profile's singleton lock, and the first
    // call would wedge on it; the reaper checks the process is really this
    // bundle's host before it signals anything.
    let reaper = Arc::clone(&fleet);
    tokio::spawn(async move { reaper.reap_stale().await });
    workspace.install_browser_host(Arc::new(HostedBrowser(fleet)));
    Ok(())
}

/// The idle warning, delivered through the workspace: it phrases the sentence
/// and puts it in the seat's session, and answers whether it landed, whether
/// a parked hand-off holds the profile, or that there is no seat to tell.
#[cfg(target_os = "macos")]
struct IdleWarnings {
    workspace: std::sync::Weak<Workspace>,
}

#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl forge_browser::idle::IdleWarn for IdleWarnings {
    async fn warn(
        &self,
        seat: Option<&forge_primitives::SessionSlot>,
        name: &str,
        idle_minutes: u64,
        grace_minutes: u64,
        at_close: bool,
    ) -> forge_browser::idle::Verdict {
        use forge_browser::idle::Verdict;
        let Some(workspace) = self.workspace.upgrade() else { return Verdict::Unanswered };
        let (verdict, answer) = tokio::sync::oneshot::channel();
        let command = forge_workspace::Command::BrowserIdleWarning {
            key: seat.cloned(),
            profile: name.to_owned(),
            idle_minutes,
            grace_minutes,
            at_close,
            verdict: Some(verdict),
        };
        if workspace.dispatch(command).is_err() {
            return Verdict::Unanswered;
        }
        // **Delivery is a send into the seat's session, not a wait on the
        // person**, so the answer is immediate; the bound is a wedge-breaker.
        match tokio::time::timeout(std::time::Duration::from_secs(5), answer).await {
            Ok(Ok(forge_workspace::IdleVerdict::Delivered)) => Verdict::Delivered,
            Ok(Ok(forge_workspace::IdleVerdict::Held)) => Verdict::Held,
            Ok(Ok(forge_workspace::IdleVerdict::NoSession)) => Verdict::NoSession,
            Ok(Ok(forge_workspace::IdleVerdict::Clear)) => Verdict::Clear,
            _ => Verdict::Unanswered,
        }
    }
}

/// A platform with no CEF host: the browser tools answer the no-host refusal
/// rather than a server that pretends to have a fleet.
#[cfg(not(target_os = "macos"))]
fn install_browser(_workspace: &Arc<Workspace>) -> anyhow::Result<()> {
    Ok(())
}

/// The fleet behind the workspace's port.
#[cfg(target_os = "macos")]
struct HostedBrowser(Arc<forge_browser::service::BrowserService>);

#[cfg(target_os = "macos")]
#[async_trait::async_trait]
impl forge_workspace::browser::BrowserHost for HostedBrowser {
    async fn call(
        &self,
        seat: &forge_primitives::SessionSlot,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<Vec<forge_primitives::browser::BrowserPart>, String> {
        self.0.call(seat, tool, args).await
    }

    async fn attach(
        &self,
        viewer: u64,
        seat: &forge_primitives::SessionSlot,
        profile: Option<&str>,
    ) -> Result<forge_workspace::browser::BrowserAttachment, String> {
        let attachment = self.0.attach(viewer, seat, profile).await?;
        Ok(forge_workspace::browser::BrowserAttachment {
            key: attachment.key,
            id: attachment.id,
            frames: attachment.frames,
            view: attachment.view,
            nav: attachment.nav,
            failures: attachment.failures,
            files: attachment.files,
            downloads: attachment.downloads,
            audio: attachment.audio,
        })
    }

    async fn answer_files(
        &self,
        profile: &str,
        id: u64,
        files: Vec<forge_primitives::browser::BrowserUpload>,
    ) -> Result<(), String> {
        self.0.answer_files(profile, id, files).await
    }

    async fn detach(&self, viewer: u64) {
        self.0.detach(viewer).await;
    }

    async fn input(
        &self,
        profile: &str,
        viewer: u64,
        seq: u64,
        event: &forge_primitives::browser::BrowserInputEvent,
    ) -> Result<(), String> {
        self.0.input(profile, viewer, seq, event).await
    }

    async fn take_control(&self, profile: &str, viewer: u64) -> Result<(), String> {
        self.0.take_control(profile, viewer).await
    }

    async fn release_control(&self, profile: &str, viewer: u64) {
        self.0.release_control(profile, viewer).await;
    }

    async fn resize(&self, profile: &str, viewer: u64, width: u32, height: u32, scale: f64) {
        self.0.resize(profile, viewer, width, height, scale).await;
    }

    async fn nav(
        &self,
        profile: &str,
        viewer: u64,
        back: bool,
    ) -> Result<forge_primitives::browser::BrowserNavState, String> {
        self.0.nav(profile, viewer, back).await
    }

    async fn nav_state(&self, profile: &str) -> Option<forge_primitives::browser::BrowserNavState> {
        self.0.nav_state(profile).await
    }

    async fn profiles(
        &self,
        seat: &forge_primitives::SessionSlot,
    ) -> Result<Vec<forge_primitives::browser::BrowserProfileRow>, String> {
        self.0.profiles(seat).await
    }

    async fn close_profile(
        &self,
        seat: &forge_primitives::SessionSlot,
        name: &str,
    ) -> Result<(), String> {
        self.0.close(seat, Some(name)).await
    }

    async fn delete_profile(
        &self,
        seat: &forge_primitives::SessionSlot,
        name: &str,
    ) -> Result<(), String> {
        self.0.delete_profile(seat, name).await
    }

    async fn sweep_seats(&self, seats: &[forge_primitives::SessionSlot]) {
        self.0.sweep_seats(seats).await;
    }

    async fn stop(&self) -> Vec<String> {
        self.0.stop_all().await
    }
}

/// The subsystem wave the boot starts, in the order it has always started it.
///
/// A function of its own so a test can run the same list the binary runs: a
/// `start_*` call the wave drops is invisible to any test that starts its
/// subsystem directly.
fn start_subsystems(workspace: &Arc<Workspace>) {
    // Kick off the per-account boot-time loading state machine (one task
    // per `[[accounts]]` entry) in the background. Each task drives its
    // account from `Loading` through to a terminal `Ready` or `Bailed`.
    // The redb store has already seeded the in-memory map at
    // `Workspace::new`.
    workspace.start_account_loading_tasks();

    // The 60 s usage poller, after the loading tasks: they probe each account
    // once at boot and the poller skips its own immediate-fire tick, so this
    // is what refreshes the account snapshots from then on. Without it they
    // hold their boot values for the process's life.
    workspace.start_usage_poller();

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
    // Built from the workspace so every piece the socket needs is derived
    // where the workspace is in hand - the browser fleet above all, which
    // must be the one the sessions' own tools drive.
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

/// The stop's exit: a teardown that did not finish every session leaves a
/// process tree behind, and a supervisor must be able to tell that from a
/// clean stop.
///
/// # Errors
///
/// A non-empty `unready` is an error naming the sessions that outlasted
/// the wait.
pub fn stop_result(unready: &[forge_primitives::SessionSlot]) -> anyhow::Result<()> {
    if unready.is_empty() {
        return Ok(());
    }
    let named: Vec<String> = unready.iter().map(forge_primitives::SessionSlot::display).collect();
    let (subject, tail) = if unready.len() == 1 {
        ("1 session".to_owned(), "its process tree may outlive")
    } else {
        (format!("{} sessions", unready.len()), "their process trees may outlive")
    };
    anyhow::bail!(
        "forge: {subject} did not finish teardown before the wait ran out ({}); {tail} this exit",
        named.join(", "),
    );
}

#[cfg(test)]
mod tests {
    use super::{BOOT_WAVE_WAIT, start_subsystems, stop_result, wait_for_accounts};
    use std::sync::Arc;
    use std::time::Duration;

    use forge_workspace::Workspace;

    /// A stop whose teardown ran out of budget exits non-zero and names
    /// what was still running; a clean stop is a success.
    #[test]
    fn a_teardown_that_did_not_finish_stops_with_a_failure() {
        assert!(stop_result(&[]).is_ok(), "a clean stop is a success");

        let slot = forge_primitives::SessionSlot::new("org", "proj", "slow");
        let error = stop_result(std::slice::from_ref(&slot)).expect_err("a left tree is a failure");
        let rendered = format!("{error}");
        assert!(rendered.contains("org/proj/slow"), "the failure names the session: {rendered}");
        assert!(rendered.contains("its process tree"), "one session reads as one: {rendered}");

        let second = forge_primitives::SessionSlot::new("org", "proj", "slower");
        let both = [slot, second];
        let error = stop_result(&both).expect_err("two left trees are a failure");
        let rendered = format!("{error}");
        assert!(rendered.contains("2 sessions"), "two sessions read as two: {rendered}");
        assert!(rendered.contains("their process trees"), "and take the plural: {rendered}");
    }

    /// A bare boot fixture: one project whose account is base-url'd at a dead
    /// local port (its boot probe fails in milliseconds rather than reaching
    /// an endpoint), dictation off and a gateway port the OS just handed out,
    /// so the wave's other steps no-op or park. The port is picked free and
    /// then handed on - a busy box can take it in between, and the listener's
    /// failed bind is logged rather than read here.
    fn boot_fixture() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let port = listener.local_addr().expect("the port the OS handed out").port();
        drop(listener);
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(
            forge.join("forge.toml"),
            format!(
                "\
[[orgs]]
name = \"TestOrg\"
accounts = [\"Stub\"]

[[orgs.projects]]
name = \"forge\"
path = \"/tmp\"

[[accounts]]
display_name = \"Stub\"
token = \"t\"
provider = \"openrouter\"
base_url = \"http://127.0.0.1:9/api\"
models = [\"claude-sonnet-5\"]

[server]
enabled = false

[gateway]
port = {port}
",
            ),
        )
        .expect("write forge.toml");
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("workspace");
        (Arc::new(workspace), dir)
    }

    /// The wave starts the 60 s usage poller. Its first tick is a minute out
    /// and its pass reaches the network, so the swap-once guard is what a
    /// test can read - a wave without it leaves the account snapshots frozen
    /// at their boot values and the session page's 5h/7d bars never move.
    ///
    /// The wave is driven directly here: `start_core` reaches the machine's
    /// single-instance lock, so the one call from it into this function is the
    /// boundary this test does not cover.
    #[tokio::test]
    async fn the_boot_wave_starts_the_usage_poller() {
        let (workspace, _dir) = boot_fixture();
        assert!(
            !workspace.test_usage_poller_started(),
            "the fixture starts with the poller unstarted, or this test proves nothing",
        );

        start_subsystems(&workspace);

        assert!(workspace.test_usage_poller_started(), "the boot wave starts the usage poller");
    }

    /// The wave starts the dictate prewarm, which is what keeps a take from
    /// paying the model load: started at boot rather than at the first press,
    /// on a task of its own so the fetch, the verify and the load are not on
    /// the boot path. The fixture has dictation switched off, so the guard is
    /// what the test reads; the prewarm's own fetch and load need the real
    /// weights and are exercised by forge-dictate's own gates.
    #[tokio::test]
    async fn the_boot_wave_starts_the_dictate_prewarm() {
        let (workspace, _dir) = boot_fixture();
        assert!(
            !workspace.test_dictate_preflight_started(),
            "the fixture starts with the prewarm unstarted, or this test proves nothing",
        );

        start_subsystems(&workspace);

        assert!(
            workspace.test_dictate_preflight_started(),
            "the boot wave asks for the prewarm; dropping the call leaves every take \
             paying the load",
        );
    }

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

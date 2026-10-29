//! The boot the server runs: the config dir, the workspace, the subsystem
//! wave, the socket, and the boot spawn.
//!
//! Moved here from the view's `main.rs` so the core comes up under one
//! function for a view and for no view alike, and so no view's removal can
//! take the boot with it.

use std::path::{Path, PathBuf};
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
/// Not `async`: every step is a spawn, a store write or the skill install, and the await on
/// anything that actually does I/O belongs to `serve_socket`. Nothing here
/// needs a `LocalSet` either - every task in this wave is a plain
/// `tokio::spawn`, verified by the headless boot test exercising it
/// without one.
pub fn start_core(config_dir: &Path, project: Option<&str>) -> anyhow::Result<Arc<Workspace>> {
    let workspace = match Workspace::new(config_dir.to_path_buf()) {
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

    install_handoff_skill(config_dir);
    start_subsystems(&workspace);
    #[cfg(target_os = "macos")]
    {
        let fleet = install_browser(&workspace)?;
        start_config_watch(&workspace, move |workspace| {
            refresh_browser(workspace, &fleet);
        });
    }
    #[cfg(not(target_os = "macos"))]
    {
        install_browser(&workspace);
        start_config_watch(&workspace, |_workspace| {});
    }

    Ok(workspace)
}

/// Install the generated handoff skill into the config dir.
///
/// **forge.toml is never written; this one file is.** The skill is forge's
/// own text, compared byte for byte, so an upgrade refreshes it and a boot
/// with an identical copy touches nothing.
fn install_handoff_skill(config_dir: &Path) {
    let path = forge_workspace::skill_path(config_dir);
    match forge_workspace::install_skill(config_dir) {
        Ok(forge_workspace::SkillInstall::Unchanged) => {}
        Ok(outcome) => {
            let action = match outcome {
                forge_workspace::SkillInstall::Created => "created",
                forge_workspace::SkillInstall::Replaced => "replaced",
                forge_workspace::SkillInstall::Unchanged => return,
            };
            tracing::info!(
                target: "forge_workspace::handoff",
                event_name = "handoff_skill_installed",
                path = %path.display(),
                action,
                "the handoff skill was written at boot",
            );
        }
        Err(error) => {
            tracing::warn!(
                target: "forge_workspace::handoff",
                event_name = "handoff_skill_install_failed",
                path = %path.display(),
                %error,
                "the handoff skill could not be installed; a handoff turn falls back to the \
                 prompt's own steps",
            );
        }
    }
}

/// Watch `forge.toml` for the process's life and apply each valid edit.
///
/// The watch is a single file (`forge.toml`), never the config directory, so
/// nothing else in that tree can trigger a reload. Each event is a whole-file
/// read: a rejected file leaves the previous config live and logs one WARN,
/// and an applied one logs the tables that moved and the keys that only a
/// restart can take. `apply` runs for the subsystems the boot holds.
fn start_config_watch<F>(workspace: &Arc<Workspace>, apply: F)
where
    F: Fn(&Workspace) + Send + 'static,
{
    let path = workspace.config_path();
    let weak = Arc::downgrade(workspace);
    let (changed, mut edits) = tokio::sync::mpsc::unbounded_channel();
    let watch = match forge_workspace::env::config_watch::watch(path.clone(), changed) {
        Ok(watch) => watch,
        Err(error) => {
            tracing::warn!(
                target: crate::logging::targets::APP_LIFECYCLE,
                event_name = "config_watch_failed",
                path = %path.display(),
                error = %error,
                "forge.toml is not being watched; an edit needs a restart this run",
            );
            return;
        }
    };
    tokio::spawn(async move {
        // The handle lives in this task, so the watch lives as long as the
        // process (or, in a test, the runtime) without a handle outside.
        let _watch = watch;
        loop {
            let _ = edits.recv().await;
            let Some(workspace) = weak.upgrade() else { break };
            match workspace.reload_config() {
                Ok(reload) => {
                    // A save that changed nothing is not an apply.
                    if reload.changed.is_empty() && reload.restart_only.is_empty() {
                        continue;
                    }
                    tracing::info!(
                        target: crate::logging::targets::APP_LIFECYCLE,
                        event_name = "config_reloaded",
                        changed = ?reload.changed,
                        restart_only = ?reload.restart_only,
                        "forge.toml applied to the running forge",
                    );
                    apply(&workspace);
                }
                Err(error) => {
                    tracing::warn!(
                        target: crate::logging::targets::APP_LIFECYCLE,
                        event_name = "config_reload_rejected",
                        path = %workspace.config_path().display(),
                        error = %error,
                        "forge.toml was not applied; the last good config stays live",
                    );
                }
            }
        }
    });
}

/// Push the reloaded `[browser]` knobs and profile names into the fleet.
#[cfg(target_os = "macos")]
fn refresh_browser(workspace: &Workspace, fleet: &Arc<forge_browser::service::BrowserService>) {
    let browser = workspace.browser_settings();
    fleet.configure_encoding(browser.fps, browser.quality);
    fleet.configure_idle(browser.idle_minutes);
    configure_fleet_profiles(workspace, fleet);
}

/// Install the server's browser fleet: constructed here, one host and one
/// driver per profile spawned on first use.
///
/// **A missing app-support directory fails the boot**, the same resolution
/// the durable store needs; the vendored driver stack is not required at
/// boot, so a server without it boots and its browser calls name what is
/// missing instead of the whole server refusing to start.
/// **The join between the config and the fleet's resolver.** The workspace
/// resolves `forge.toml`'s org and project names; the fleet resolves a seat
/// against those maps. Both ends have tests, and this is the copy between
/// them, so it is one named call rather than two lines inside a boot.
#[cfg(target_os = "macos")]
fn configure_fleet_profiles(workspace: &Workspace, fleet: &forge_browser::service::BrowserService) {
    let (orgs, projects) = workspace.browser_profile_config();
    fleet.configure_profiles(orgs, projects);
}

#[cfg(target_os = "macos")]
fn install_browser(
    workspace: &Arc<Workspace>,
) -> anyhow::Result<Arc<forge_browser::service::BrowserService>> {
    let fleet = forge_browser::service::BrowserService::installed()
        .map_err(|why| anyhow::anyhow!("forge: {why}"))?;
    // **The encoding and the idle clock are the `[browser]` keys**: a
    // profile nobody has touched closes after the bound.
    let browser = workspace.browser_settings();
    fleet.configure_encoding(browser.fps, browser.quality);
    fleet.configure_idle(browser.idle_minutes);
    // **The profile names are config's, not a client's**: every seat resolves
    // its profile from `forge.toml` here, at boot, rather than naming one.
    configure_fleet_profiles(workspace, &fleet);
    fleet.start_idle();
    fleet.start_pairing();
    // **A host an unclean exit left behind is reaped before any profile is
    // used.** It still holds the profile's singleton lock, and the first
    // call would wedge on it; the reaper checks the process is really this
    // bundle's host before it signals anything.
    let reaper = Arc::clone(&fleet);
    tokio::spawn(async move { reaper.reap_stale().await });
    workspace.install_browser_host(Arc::new(HostedBrowser(Arc::clone(&fleet))));
    Ok(fleet)
}

/// A platform with no CEF host: the browser tools answer the no-host refusal
/// rather than a server that pretends to have a fleet.
#[cfg(not(target_os = "macos"))]
fn install_browser(_workspace: &Arc<Workspace>) {}

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
        tab: Option<u16>,
    ) -> Result<forge_workspace::browser::BrowserAttachment, String> {
        let attachment = self.0.attach(viewer, seat, tab).await?;
        Ok(forge_workspace::browser::BrowserAttachment {
            key: attachment.key,
            id: attachment.id,
            tab: attachment.tab,
            frames: attachment.frames,
            view: attachment.view,
            tabs: attachment.tabs,
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
        tab: u16,
        id: u64,
        files: Vec<forge_primitives::browser::BrowserUpload>,
    ) -> Result<(), String> {
        self.0.answer_files(profile, tab, id, files).await
    }

    async fn detach(&self, viewer: u64) {
        self.0.detach(viewer).await;
    }

    async fn input(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        seq: u64,
        event: &forge_primitives::browser::BrowserInputEvent,
    ) -> Result<(), String> {
        self.0.input(profile, tab, viewer, seq, event).await
    }

    async fn take_control(&self, profile: &str, tab: u16, viewer: u64) -> Result<(), String> {
        self.0.take_control(profile, tab, viewer).await
    }

    async fn release_control(&self, profile: &str, tab: u16, viewer: u64) {
        self.0.release_control(profile, tab, viewer).await;
    }

    async fn resize(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        width: u32,
        height: u32,
        scale: f64,
    ) {
        self.0.resize(profile, tab, viewer, width, height, scale).await;
    }

    async fn nav(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        back: bool,
    ) -> Result<forge_primitives::browser::BrowserNavState, String> {
        self.0.nav(profile, tab, viewer, back).await
    }

    async fn nav_state(
        &self,
        profile: &str,
        tab: u16,
    ) -> Option<forge_primitives::browser::BrowserNavState> {
        self.0.nav_state(profile, tab).await
    }

    async fn tabs(
        &self,
        seat: &forge_primitives::SessionSlot,
    ) -> Result<forge_primitives::browser::BrowserTabList, String> {
        self.0.tabs(seat).await
    }

    async fn tab_stream(
        &self,
        seat: &forge_primitives::SessionSlot,
    ) -> Result<tokio::sync::watch::Receiver<forge_primitives::browser::BrowserTabList>, String>
    {
        self.0.tab_stream(seat).await
    }

    async fn sweep_sessions(&self, seats: &[forge_primitives::SessionSlot], generation: u64) {
        self.0.sweep_sessions(seats, generation).await;
    }

    async fn replace_seat(&self, seat: &forge_primitives::SessionSlot) {
        self.0.replace_seat(seat).await;
    }

    fn session_ended(&self, seat: &forge_primitives::SessionSlot) {
        self.0.session_ended(seat);
    }

    async fn hand_off_started(
        &self,
        seat: &forge_primitives::SessionSlot,
        tab: u16,
    ) -> Result<(), String> {
        self.0.hand_off_started(seat, tab).await
    }

    async fn hand_off_finished(&self, seat: &forge_primitives::SessionSlot, tab: u16) {
        self.0.hand_off_finished(seat, tab).await;
    }

    async fn warm(&self, seat: &forge_primitives::SessionSlot) {
        self.0.warm(seat).await;
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
    let state = Arc::new(crate::transport::TransportState::for_workspace(workspace));
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            let bound = listener.local_addr().unwrap_or(addr);
            tracing::info!(
                target: crate::logging::targets::APP_LIFECYCLE,
                event_name = "server_socket_listening",
                %bound,
                "the server's socket is listening",
            );
            // **Taken before the serve task is spawned**, so the fold is
            // attached before this returns and the boot wave can spawn a
            // session whose first `Connected` finds a copy to land on.
            let updates = state.surface.subscribe_mirror();
            let handle = tokio::spawn(async move {
                if let Err(error) = crate::transport::serve(state, listener, updates).await {
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
    #[cfg(target_os = "macos")]
    use super::configure_fleet_profiles;
    use super::{
        BOOT_WAVE_WAIT, start_config_watch, start_subsystems, stop_result, wait_for_accounts,
    };
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
        fixture(false)
    }

    /// The same boot, with the socket serving on a port the OS hands out.
    fn socket_fixture() -> (Arc<Workspace>, tempfile::TempDir) {
        fixture(true)
    }

    fn fixture(serve: bool) -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let port = listener.local_addr().expect("the port the OS handed out").port();
        drop(listener);
        let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let socket_port = socket.local_addr().expect("the port the OS handed out").port();
        drop(socket);
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
enabled = {serve}
port = {socket_port}

[gateway]
port = {port}
",
            ),
        )
        .expect("write forge.toml");
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("workspace");
        (Arc::new(workspace), dir)
    }

    /// **`serve_socket` returns with the fold already subscribed.** The boot
    /// wave runs after this returns, so a session spawned there emits its
    /// first `Connected` into a fan-out nobody is holding yet unless the
    /// subscription was taken before the serve task was spawned - and a
    /// dropped `Connected` is a seat whose conversation the transport never
    /// holds. Nothing has connected here: the subscriber is the fold's own.
    #[tokio::test]
    async fn the_socket_returns_with_the_fold_subscribed() {
        let (workspace, _dir) = socket_fixture();
        assert_eq!(workspace.test_subscriber_count(), 0, "nothing is attached before the socket");

        let serving = super::serve_socket(&workspace).await.expect("the socket serves");
        let super::SocketServing::Bound(handle) = serving else {
            panic!("the socket binds on a port the OS handed out");
        };
        // Read before anything awaits: the claim is the order the call returns in.
        assert_eq!(
            workspace.test_subscriber_count(),
            1,
            "the fold's subscription is held when serve_socket returns",
        );
        handle.abort();
    }

    /// A write to the watched file applies to the running workspace, with no
    /// restart and no call from the test: the watch is the only path.
    #[tokio::test]
    async fn a_config_edit_reaches_the_running_workspace() {
        let (workspace, _dir) = boot_fixture();
        let mut updates = workspace.subscribe_answerer();
        start_config_watch(&workspace, |_workspace| {});

        let path = workspace.config_path();
        let raw = std::fs::read_to_string(&path).expect("read");
        std::fs::write(&path, raw.replace("Stub", "Renamed")).expect("write");

        let applied = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = updates.recv().await.expect("the update stream stays open");
                if matches!(&*frame.update, forge_workspace::SessionUpdate::AccountsChanged { .. })
                {
                    break;
                }
            }
        })
        .await;

        assert!(applied.is_ok(), "the watched file's edit applies without a restart");
        assert_eq!(
            workspace.list_projects()[0].accounts,
            vec!["Renamed".to_owned()],
            "the running workspace holds the edited org pin",
        );
    }

    /// **The config's names are the fleet's maps.** The workspace's own
    /// resolution and the fleet's lookup both have tests; this is the boot's
    /// copy between them, so a rename on either side fails here.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_fleets_profile_names_are_the_configs() {
        use forge_primitives::SessionSlot;

        let dir = tempfile::tempdir().expect("tempdir");
        let forge = dir.path().join("forge");
        std::fs::create_dir_all(&forge).expect("forge/");
        std::fs::write(
            forge.join("forge.toml"),
            "\
[[orgs]]
name = \"Busytools\"
accounts = [\"Stub\"]
browser_profile = \"work\"

[[orgs.projects]]
name = \"forge\"
path = \"/tmp\"

[[orgs.projects]]
name = \"apply\"
path = \"/tmp\"
browser_profile = \"job\"

[[accounts]]
display_name = \"Stub\"
token = \"t\"
provider = \"openrouter\"
base_url = \"http://127.0.0.1:9/api\"
models = [\"claude-sonnet-5\"]

[server]
enabled = false

[gateway]
port = 49201
",
        )
        .expect("write forge.toml");
        let workspace = Workspace::new_for_test(dir.path().to_owned()).expect("workspace");
        let fleet = forge_browser::service::BrowserService::at(
            dir.path().join("browser"),
            dir.path().join("browser/stack"),
        );

        configure_fleet_profiles(&workspace, &fleet);

        let key = |project: &str| {
            fleet
                .profile_for(&SessionSlot::lead("Busytools", project))
                .expect("a seat resolves")
                .as_str()
                .to_owned()
        };
        assert_eq!(key("forge"), "Busytools/work", "the org's name arrives");
        assert_eq!(key("apply"), "Busytools/job", "the project's override arrives");
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

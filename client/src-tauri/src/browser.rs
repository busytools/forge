//! The client's browser host: the side that owns the browser process, the
//! profile and the drivers, and answers the asks a session's browser tools
//! send over the socket.
//!
//! **One browser, one driver for the shared context, one per named
//! profile**, and the browser outlives the client: it is launched detached
//! against a data directory under the app-support directory, so logins and
//! cookies survive a client restart and a forge restart touches nothing here
//! (spec section 3). The browser's own context is shared by every session; a
//! NAMED profile is a driver of its own over the same browser, owned by the
//! session that opened it and kept in [`profiles`].
//!
//! The pieces:
//! - [`chromium`] - where the vendored Chromium is, how it is launched, and
//!   how a launch is found again after a restart.
//! - [`profiles`] - which names are usable, who owns one, and what it
//!   reopens from.
//! - [`driver`] - upstream `@playwright/mcp` as a child process, spoken to as
//!   an MCP client.
//!
//! Nothing here decides what a tool MEANS: the ask carries upstream's own
//! tool name and arguments, the driver runs them, and the answer is the parts
//! it returned.

#[cfg(all(desktop, target_os = "macos"))]
pub mod cef;
pub mod chromium;
pub mod custom;
pub mod driver;
pub mod profiles;
pub mod screencast;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::Emitter as _;
use tauri::Manager as _;

use profiles::{DriverStart, Named, Profile, Seat};
use driver::ReplyPart;
use serde_json::Value;
use tokio::sync::Mutex;

/// Where the vendored stack is, and the machine-local directories the browser
/// owns.
#[derive(Clone, Debug)]
pub struct StackPaths {
    /// The vendored tree: node, the driver, the Chromium.
    pub stack: PathBuf,
    /// The browser's own data directory - the `--user-data-dir` a launch is
    /// given: logins, cookies, the HTTP cache, and the `DevToolsActivePort`
    /// file a launch writes.
    pub user_data: PathBuf,
    /// Where upstream's own file-writing tools land when a call names no
    /// filename.
    pub output: PathBuf,
    /// Where a named profile keeps its cookies and its open tabs: one pair of
    /// files per name.
    pub profiles: PathBuf,
}

impl StackPaths {
    /// Resolve from the running app.
    ///
    /// The stack is the bundle's own resource directory, and in a DEV build,
    /// where no resources are copied, the checkout the binary was built from.
    /// Both are places this build knows, never the directory the client
    /// happened to be launched from.
    ///
    /// The three directories are the app's own data directory, and their
    /// absence is an error rather than a fallback: a browser whose profile
    /// landed somewhere unintended is a browser holding logins somewhere
    /// nobody will find them.
    pub fn resolve(app: &tauri::AppHandle) -> Result<Self, String> {
        let resource = app
            .path()
            .resource_dir()
            .map_err(|why| format!("the app's resource directory cannot be resolved: {why}"))?;
        let data = app
            .path()
            .app_data_dir()
            .map_err(|why| format!("the app's data directory cannot be resolved: {why}"))?;
        Ok(Self::from_dirs(&resource, &data))
    }

    /// The derivation itself, apart from the handle that feeds it: the stack
    /// is the bundle's own resource copy when the vendoring is really there,
    /// else the checkout the binary was built from, and the three state
    /// directories hang off the app's data directory - never the resource
    /// directory, and never each other.
    pub fn from_dirs(resource: &Path, data: &Path) -> Self {
        let bundled = resource.join("browser-stack");
        let stack = if bundled.join("node/bin/node").is_file() {
            bundled
        } else {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack")
        };
        Self {
            stack,
            user_data: data.join("browser/user-data"),
            output: data.join("browser/output"),
            profiles: data.join("browser/profiles"),
        }
    }
}

/// The host: the browser's own context, the named ones, and where everything
/// lives.
pub struct BrowserHost {
    paths: Result<StackPaths, String>,
    /// Every `chromium::ensure` runs under this, whichever context asks for
    /// one: a burst of first calls launches ONE browser rather than two onto
    /// one profile - and a hand-off's `show` racing a first call cannot leave
    /// two browsers on one profile either.
    launch: Mutex<()>,
    /// The shared profile: the browser's own context, one driver for every
    /// session that names none.
    shared: Mutex<Option<Arc<Profile>>>,
    /// The named profiles, by name.
    named: Mutex<HashMap<String, Arc<Named>>>,
    /// The takeover's live view, when one is up.
    live: Mutex<Option<screencast::Live>>,
    /// The last frame the view delivered.
    ///
    /// **Kept because the first frame of a static page is also its last.**
    /// The screen mounts a moment after the view opens, so a frame emitted in
    /// between would be lost and the canvas would stay empty; a screen that
    /// asks for the current frame gets it whatever the timing. A plain lock
    /// rather than the async one: the stream task writes it in a breath, and
    /// nothing here ever waits on the browser.
    last_frame: std::sync::Arc<std::sync::Mutex<Option<screencast::Frame>>>,
    /// Whether the takeover is up.
    ///
    /// **The screen's state, not the stream's.** On macOS the picture is the
    /// browser's own view rendered natively, so a screencast that failed to
    /// start (or one that is not needed) must not decide whether a reloaded
    /// window draws the takeover.
    takeover_up: std::sync::atomic::AtomicBool,
    /// Whether any session has driven this client's browser since it came up,
    /// which the strip's row marks (Ved, 2026-10-07).
    used: std::sync::atomic::AtomicBool,
}

impl BrowserHost {
    pub fn new(paths: StackPaths) -> Self {
        Self {
            paths: Ok(paths),
            launch: Mutex::new(()),
            shared: Mutex::new(None),
            named: Mutex::new(HashMap::new()),
            live: Mutex::new(None),
            takeover_up: std::sync::atomic::AtomicBool::new(false),
            used: std::sync::atomic::AtomicBool::new(false),
            last_frame: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// A host that cannot act, for a client whose directories did not
    /// resolve: every call answers why rather than the client refusing to
    /// start over a feature it may never be asked for.
    pub fn unavailable(why: String) -> Self {
        Self {
            paths: Err(why),
            launch: Mutex::new(()),
            shared: Mutex::new(None),
            named: Mutex::new(HashMap::new()),
            live: Mutex::new(None),
            takeover_up: std::sync::atomic::AtomicBool::new(false),
            used: std::sync::atomic::AtomicBool::new(false),
            last_frame: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Run one browser tool for a session.
    ///
    /// No `profile` argument drives the browser's own context. A name drives
    /// the named profile under it, opened on first use by the asking session;
    /// a name another session already holds is refused with the owner's name.
    ///
    /// **Every call resolves the browser first**, so the endpoint it hands the
    /// context is the one the browser answers on NOW: a browser that died (or
    /// was relaunched for a window) moves that endpoint, and the context
    /// rebuilds its driver rather than talking to a port nobody listens on.
    pub async fn call(
        &self,
        seat: &Seat,
        tool: &str,
        args: Value,
    ) -> Result<Vec<ReplyPart>, String> {
        let (profile, args) = take_profile(args)?;
        // A name that cannot be a profile is decided before anything else: no
        // directory, no lock and no browser is consulted to answer it.
        if let Some(name) = profile.as_deref()
            && let Some(refusal) = profiles::name_refusal(name)
        {
            return Err(refusal);
        }
        let paths = self.paths.clone()?;
        let active = self.active_browser(&paths).await?;
        // **The browser has been driven**, which the strip's row draws: a
        // session's call is the only way this client's browser is used.
        self.used.store(true, std::sync::atomic::Ordering::Release);
        let endpoint = format!("http://127.0.0.1:{}", active.port);
        let node = driver::node_path(&paths.stack);
        let cli = driver::cli_path(&paths.stack);
        match profile {
            None => {
                let shared = self.shared_profile().await;
                let start = DriverStart {
                    node: &node,
                    cli: &cli,
                    endpoint: &endpoint,
                    identity: &active.path,
                    output: &paths.output,
                    storage: None,
                    tabs: None,
                };
                let outcome = shared.call(&start, tool, args).await;
                self.forget_a_dead_browser(&paths, &shared, active.port, outcome.is_err()).await;
                outcome
            }
            Some(name) => {
                let named = self.named_profile(seat, &name).await?;
                let start = DriverStart {
                    node: &node,
                    cli: &cli,
                    endpoint: &endpoint,
                    identity: &active.path,
                    output: &paths.output,
                    storage: Some(&named.storage),
                    tabs: Some(&named.tabs),
                };
                let outcome = named.call(&start, tool, args).await;
                self.forget_a_dead_browser(&paths, &named.profile, active.port, outcome.is_err())
                    .await;
                outcome
            }
        }
    }

    /// A call failed: is it the browser that is gone, rather than the tool?
    ///
    /// A plain tool failure - no such element, a refused navigation - leaves
    /// the driver in place; only a browser that stopped answering on the port
    /// this driver was built for drops it, so the next call rebuilds against
    /// a relaunched browser. Bounded by the probe's own read timeout.
    async fn forget_a_dead_browser(
        &self,
        paths: &StackPaths,
        profile: &Profile,
        port: u16,
        failed: bool,
    ) {
        if !failed || chromium::answers_as(&paths.user_data, port).await {
            return;
        }
        profile.drop_driver().await;
    }

    /// Close a named profile, whoever opened it: the client's own UI acting on
    /// the row.
    ///
    /// **The human's door, and no seat.** A session drives only the profile it
    /// opened - the verdict refuses the rest - but the row is the person's, and
    /// a profile whose owning session is GONE is exactly what this is for.
    /// **The save lands before the name is free, and the map's lock is not
    /// held across it**: a save drives the browser and answers on a call's own
    /// clock, while the lock is only for the map. The name goes only once the
    /// save landed - a close that could not persist keeps the profile, so a
    /// failed save is not also a lost name.
    pub async fn close(&self, name: &str) -> Result<(), String> {
        let paths = self.paths.clone()?;
        let entry = {
            let named = self.named.lock().await;
            let Some(entry) = named.get(name).map(Arc::clone) else {
                return Err(format!("no browser profile is open under '{name}'"));
            };
            entry
        };
        let active = self.active_browser(&paths).await?;
        let endpoint = format!("http://127.0.0.1:{}", active.port);
        let node = driver::node_path(&paths.stack);
        let cli = driver::cli_path(&paths.stack);
        let start = DriverStart {
            node: &node,
            cli: &cli,
            endpoint: &endpoint,
            identity: &active.path,
            output: &paths.output,
            storage: Some(&entry.storage),
            tabs: Some(&entry.tabs),
        };
        entry.save(&start).await?;
        // Compared before it goes: a name that was re-opened while the save
        // ran belongs to the new profile, not to what was just saved.
        let mut named = self.named.lock().await;
        if let Some(held) = named.get(name)
            && Arc::ptr_eq(held, &entry)
        {
            named.remove(name);
        }
        Ok(())
    }

    /// Bring the browser up, without any driver.
    ///
    /// **The app's own start, so the browser is there before anything asks
    /// for it** rather than being launched under the first tool call: a
    /// session's call should not pay a cold launch, and a browser that cannot
    /// start at all says so in the app's log at startup instead of as a
    /// failed tool call. The drivers stay lazy - they exist to serve calls,
    /// and one with no calls to serve is a child process held for nothing.
    /// The launched browser's id comes back with its port, so a caller that
    /// must reap it (a test that launched it) can; the app ignores both.
    pub async fn start(&self) -> Result<chromium::ActivePort, String> {
        let paths = self.paths.clone()?;
        self.active_browser(&paths).await
    }

    /// The live browser, launched when nothing is up.
    async fn active_browser(&self, paths: &StackPaths) -> Result<chromium::ActivePort, String> {
        // **The vendored Chromium, headless, one browser for the client.**
        // CEF was measured out on 2026-10-07: its windowed runtime drops
        // CDP-dispatched input whenever the window is not frontmost, and the
        // pinned driver's click wedges on its target bookkeeping - full
        // playwright parity wins over the native view (see `chromium::show`
        // for how the person sees it).
        let _launching = self.launch.lock().await;
        chromium::ensure(&chromium::browser_binary(&paths.stack), &paths.user_data).await
    }

    /// The named profiles this host holds, oldest name first, for its own
    /// strip. A profile whose driver died but whose name is still held is
    /// listed as not running rather than dropped: the name is owned until it
    /// is released, and a row that vanished would read as released.
    pub async fn profiles(&self) -> Vec<ProfileRow> {
        let named = self.named.lock().await;
        let mut rows: Vec<ProfileRow> = named
            .iter()
            .map(|(name, entry)| ProfileRow {
                name: name.clone(),
                owner: entry.owner.to_string(),
                running: entry.profile.is_alive(),
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Bring the browser up VISIBLY for a hand-off's Open: the person's own
    /// browser window, over the same profile the agents drive.
    ///
    /// Serialized with every other launch, so a show racing a first call
    /// cannot leave two browsers on one profile. This is the client's own
    /// act and answers the core nothing: the hand-off's answer is Done or
    /// Not now, and never the window itself.
    pub async fn show(&self) -> Result<chromium::ActivePort, String> {
        let paths = self.paths.clone()?;
        let _launching = self.launch.lock().await;
        chromium::show(&chromium::browser_binary(&paths.stack), &paths.user_data).await
    }

    /// Take the window back down: the browser closes, and the next agent
    /// call relaunches it headless over the same profile. **The hand-off is
    /// not answered by this** - Done or Not now is.
    pub async fn hide(&self) -> Result<(), String> {
        let paths = self.paths.clone()?;
        chromium::hide(&paths.user_data).await;
        Ok(())
    }

    /// Bring the in-app browser view up over the client's window: the
    /// approved takeover.
    ///
    /// **The picture is the browser's own view on macOS**, rendered natively
    /// under the bar; the screencast that platforms without it need is
    /// best-effort - a stream that will not start must not take the takeover
    /// down with it.
    pub async fn takeover_open(&self, app: &tauri::AppHandle) -> Result<(), String> {
        let paths = self.paths.clone()?;
        let active = self.active_browser(&paths).await?;
        let endpoint = format!("ws://127.0.0.1:{}{}", active.port, active.path);
        let emitter = app.clone();
        let kept = std::sync::Arc::clone(&self.last_frame);
        // **The takeover draws frames everywhere** (CEF was measured out):
        // the hand-off's own window is the native surface now, and the
        // in-app view is the stream.
        let with_frames = true;
        let started = screencast::start(&endpoint, with_frames, move |frame| {
            let mut held = kept.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            *held = Some(frame.clone());
            let _ = emitter.emit("browser_frame", frame);
        })
        .await;
        let mut held = self.live.lock().await;
        if let Some(previous) = held.take() {
            previous.stop();
        }
        *held = started.ok();
        self.takeover_up.store(true, std::sync::atomic::Ordering::Release);
        // **The real view comes up under the bar.** CEF may only be touched
        // on the main thread, and this command is not on it.
        #[cfg(all(desktop, target_os = "macos"))]
        {
            let _ = app.run_on_main_thread(|| crate::browser::cef::view::set_visible(true));
        }
        Ok(())
    }

    /// Take the view back down. Idempotent: back, Done and a reloaded window
    /// may each ask.
    pub async fn takeover_close(&self) -> Result<(), String> {
        self.paths.clone()?;
        if let Some(live) = self.live.lock().await.take() {
            live.stop();
        }
        self.takeover_up.store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Whether the shell is holding a takeover up - what a reloaded window
    /// reads to re-draw the screen it was on - and **whether the picture is
    /// the browser's own view** (the desktop), which the screen must know to
    /// stop drawing frames nobody sees.
    pub async fn takeover_state(&self) -> Result<TakeoverState, String> {
        self.paths.clone()?;
        Ok(TakeoverState {
            active: self.takeover_up.load(std::sync::atomic::Ordering::Acquire),
            // The frames path is the picture everywhere again (CEF retired):
            // the native surface is the hand-off's own browser window.
            native: false,
        })
    }

    /// Whether a session has driven this client's browser since it came up.
    pub async fn used(&self) -> Result<bool, String> {
        self.paths.clone()?;
        Ok(self.used.load(std::sync::atomic::Ordering::Acquire))
    }

    /// The page the browser is showing, for the takeover's bar.
    pub async fn takeover_url(&self) -> Result<Option<String>, String> {
        let paths = self.paths.clone()?;
        #[cfg(all(desktop, target_os = "macos"))]
        let port = {
            let _ = paths;
            cef::debug_port()
        };
        #[cfg(not(all(desktop, target_os = "macos")))]
        let port = self.active_browser(&paths).await?.port;
        if port == 0 {
            return Ok(None);
        }
        Ok(chromium::page_url(port).await)
    }

    /// One input event into the live view, as CDP wants it.
    pub async fn takeover_input(&self, method: &str, params: Value) -> Result<(), String> {
        let held = self.live.lock().await;
        let Some(live) = held.as_ref() else {
            return Err("no takeover is up".to_owned());
        };
        live.input(method, params);
        Ok(())
    }

    /// The current frame, for a screen that just mounted: the stream may have
    /// delivered it before the screen could listen.
    pub async fn current_frame(&self) -> Result<Option<screencast::Frame>, String> {
        self.paths.clone()?;
        Ok(self.last_frame.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone())
    }

    /// The shared profile: one, cached, whose driver builds and rebuilds
    /// itself under its own lock.
    ///
    /// Serialized: a burst of calls arriving on a cold host finds ONE profile
    /// and builds ONE driver, because every call runs through it.
    async fn shared_profile(&self) -> Arc<Profile> {
        let mut shared = self.shared.lock().await;
        if let Some(profile) = shared.as_ref() {
            return Arc::clone(profile);
        }
        let profile = Arc::new(Profile::new());
        *shared = Some(Arc::clone(&profile));
        profile
    }

    /// The named profile under `name`, opened when it is not there.
    ///
    /// The map's lock is held across an open, so two sessions naming one fresh
    /// profile race at the verdict rather than both opening: the first opens
    /// and owns it, and the second is refused by the same rule as any other
    /// attach. A profile whose driver died between calls needs nothing here -
    /// its next call rebuilds the driver over the saved files.
    async fn named_profile(&self, seat: &Seat, name: &str) -> Result<Arc<Named>, String> {
        if let Some(refusal) = profiles::name_refusal(name) {
            return Err(refusal);
        }
        let paths = self.paths.clone()?;
        let mut named = self.named.lock().await;
        let held = named.get(name).map(|entry| &entry.owner);
        match profiles::verdict(name, seat, held) {
            profiles::Verdict::Refuse(refusal) => Err(refusal),
            profiles::Verdict::Drive => {
                let Some(entry) = named.get(name).map(Arc::clone) else {
                    return Err(format!("the profile '{name}' went away while it was read"));
                };
                Ok(entry)
            }
            profiles::Verdict::Open => {
                // The saved tabs are reopened by the profile's first CALL,
                // where its driver is built: opening the name here costs
                // nothing, and a session that names a profile and never
                // drives it holds no driver at all.
                let fresh = Arc::new(Named::open(seat.clone(), name, &paths));
                named.insert(name.to_owned(), Arc::clone(&fresh));
                Ok(fresh)
            }
        }
    }
}

/// Take the `profile` argument off a call.
///
/// It chooses the profile and no driver's schema declares it, so it never
/// reaches a tool. A `profile` that is not a name is the call's own mistake,
/// answered rather than guessed at.
fn take_profile(mut args: Value) -> Result<(Option<String>, Value), String> {
    let Some(fields) = args.as_object_mut() else {
        return Ok((None, args));
    };
    let Some(profile) = fields.remove("profile") else {
        return Ok((None, args));
    };
    match profile {
        Value::Null => Ok((None, args)),
        Value::String(name) => Ok((Some(name), args)),
        other => Err(format!("`profile` is the name of a profile, not {other}")),
    }
}

/// One tool's answer, in the shape the frontend's ask handler returns.
#[derive(Debug, serde::Serialize)]
pub struct BrowserReply {
    pub parts: Vec<ReplyPart>,
}

/// Run one browser tool call: the frontend's half of the ask/answer pair.
///
/// The frontend receives a `browser_ask`, invokes this with the seat that
/// asked, and sends the parts back as its answer - so a failure here is the
/// tool's failure, with the reason the driver gave.
#[tauri::command]
pub async fn browser_call(
    host: tauri::State<'_, Arc<BrowserHost>>,
    seat: Seat,
    tool: String,
    args: Value,
) -> Result<BrowserReply, String> {
    host.call(&seat, &tool, args).await.map(|parts| BrowserReply { parts })
}

/// Bring the in-app browser view up over the client's window.
#[tauri::command]
pub async fn browser_takeover_open(
    host: tauri::State<'_, Arc<BrowserHost>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    host.takeover_open(&app).await
}

/// One input event into the live view (`Input.dispatchMouseEvent`,
/// `Input.dispatchKeyEvent`, `Input.insertText`), with its params.
#[tauri::command]
pub async fn browser_takeover_input(
    host: tauri::State<'_, Arc<BrowserHost>>,
    method: String,
    params: Value,
) -> Result<(), String> {
    host.takeover_input(&method, params).await
}

/// The current frame, for a screen that just mounted.
#[tauri::command]
pub async fn browser_takeover_frame(
    host: tauri::State<'_, Arc<BrowserHost>>,
) -> Result<Option<screencast::Frame>, String> {
    host.current_frame().await
}

/// Take the view back down.
#[tauri::command]
pub async fn browser_takeover_close(
    host: tauri::State<'_, Arc<BrowserHost>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    host.takeover_close().await?;
    // **The real view goes down with the screen.** CEF may only be touched
    // on the main thread, and this command is not on it.
    #[cfg(all(desktop, target_os = "macos"))]
    {
        let _ = app.run_on_main_thread(|| crate::browser::cef::view::set_visible(false));
    }
    Ok(())
}

/// What a (reloaded) window reads to re-draw the takeover it was on.
#[derive(serde::Serialize)]
pub struct TakeoverState {
    /// Whether the takeover is up.
    pub active: bool,
    /// Whether the picture is the browser's own view: the screen then draws
    /// no frames of its own.
    pub native: bool,
}

#[tauri::command]
pub async fn browser_takeover_state(
    host: tauri::State<'_, Arc<BrowserHost>>,
) -> Result<TakeoverState, String> {
    host.takeover_state().await
}

/// Bring the browser up visibly, for a hand-off's Open. Answers nothing to
/// the core: the window is the client's act, and Done or Not now is the
/// answer.
#[tauri::command]
pub async fn browser_show(host: tauri::State<'_, Arc<BrowserHost>>) -> Result<(), String> {
    host.show().await.map(|_| ())
}

/// Take the hand-off's window back down; the next agent call relaunches the
/// browser headless over the same profile.
#[tauri::command]
pub async fn browser_hide(host: tauri::State<'_, Arc<BrowserHost>>) -> Result<(), String> {
    host.hide().await
}

/// The page the browser is showing, for the takeover's bar.
#[tauri::command]
pub async fn browser_takeover_url(
    host: tauri::State<'_, Arc<BrowserHost>>,
) -> Result<Option<String>, String> {
    host.takeover_url().await
}

/// Whether a session has driven this client's browser, for the strip's mark.
#[tauri::command]
pub async fn browser_used(host: tauri::State<'_, Arc<BrowserHost>>) -> Result<bool, String> {
    host.used().await
}

/// One named profile, as the client's own browser strip draws it.
#[derive(Debug, serde::Serialize)]
pub struct ProfileRow {
    /// The name a session drives it by.
    pub name: String,
    /// The slot of the session that opened it, as its refusal prints.
    pub owner: String,
    /// Whether its driver is still there to answer.
    pub running: bool,
}

/// The named profiles this host holds, for its own browser strip.
///
/// The profiles are the CLIENT's own state - it owns the drivers - so this is
/// the client reading itself, not a server read; the strip needs no new view
/// surface for it.
#[tauri::command]
pub async fn browser_profiles(
    host: tauri::State<'_, Arc<BrowserHost>>,
) -> Result<Vec<ProfileRow>, String> {
    Ok(host.profiles().await)
}

/// Close a named profile from the client's own UI: the strip's row, acting
/// for the person rather than for a session.
#[tauri::command]
pub async fn browser_profile_close(
    host: tauri::State<'_, Arc<BrowserHost>>,
    name: String,
) -> Result<(), String> {
    host.close(&name).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seat() -> Seat {
        Seat { org: "Busytools".to_owned(), project: "forge".to_owned(), label: "lead".to_owned() }
    }

    /// A host whose directories did not resolve still starts, and every call
    /// names why it cannot act: a client that refused to boot over this would
    /// take the whole app down for a feature nobody may ask for.
    #[tokio::test]
    async fn a_host_with_no_directories_answers_why() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let refused = host.call(&seat(), "browser_close", Value::Null).await;
        assert_eq!(
            refused,
            Err("the app's data directory cannot be resolved".to_owned()),
            "the reason a client can act on, not a panic and not a silence",
        );
    }

    /// A name that cannot be a profile is decided before anything else: no
    /// directory, no lock and no driver is consulted to answer it.
    #[tokio::test]
    async fn a_bad_profile_name_is_refused_before_anything_is_started() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let refused = host
            .call(
                &seat(),
                "browser_navigate",
                json!({ "url": "https://example.com", "profile": "a b" }),
            )
            .await;
        let Err(why) = refused else {
            panic!("a name with a space is refused");
        };
        assert!(why.contains("1 to 64"), "{why}");
    }

    /// Bringing the browser up answers the same reason a call would, and
    /// does not panic: the app's start must survive a machine where the
    /// directories or the stack are missing, since the window does not wait
    /// on either.
    #[tokio::test]
    async fn starting_a_host_with_no_directories_answers_why() {
        let host = BrowserHost::unavailable("the browser stack was never vendored".to_owned());
        assert_eq!(
            host.start().await,
            Err("the browser stack was never vendored".to_owned()),
            "the start says why rather than panicking at the app's boot",
        );
    }

    /// A host whose directories did not resolve answers that before anything
    /// else, closing is idempotent, nothing is held up, and input with no view
    /// names that rather than vanishing.
    #[tokio::test]
    async fn a_takeover_on_an_unavailable_host_answers_the_reason_and_holds_nothing() {
        let host = BrowserHost::unavailable("the browser stack was never vendored".to_owned());
        assert_eq!(
            host.takeover_close().await,
            Err("the browser stack was never vendored".to_owned()),
            "the missing directories answer first",
        );
        assert_eq!(
            host.takeover_input("Input.insertText", serde_json::json!({ "text": "x" })).await,
            Err("no takeover is up".to_owned()),
            "with no view, input says so rather than vanishing",
        );
    }

    /// A host nothing has named holds no profiles, and answers the strip with
    /// an empty list rather than an error: no profiles is a state, not a
    /// failure.
    #[tokio::test]
    async fn a_fresh_host_holds_no_profiles() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        assert!(host.profiles().await.is_empty());
    }

    /// **The derivation `resolve` makes, apart from the handle.** Everything
    /// it produces must hang off the two directories it is given: the state
    /// directories off the DATA dir (a profile that landed beside the binary
    /// would hold logins where a bundle update erases them), and the stack
    /// off the resource dir only when the vendoring is really there.
    #[test]
    fn the_paths_derive_from_the_two_directories_they_are_given() {
        let resource = tempfile::tempdir().expect("a temp dir");
        let data = tempfile::tempdir().expect("a temp dir");

        let absent = StackPaths::from_dirs(resource.path(), data.path());
        assert_eq!(
            absent.stack,
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack"),
            "with nothing vendored, the stack is the checkout the binary was built from",
        );
        assert!(absent.user_data.starts_with(data.path()), "{absent:?}");
        assert!(absent.output.starts_with(data.path()), "{absent:?}");
        assert!(absent.profiles.starts_with(data.path()), "{absent:?}");
        assert!(
            !absent.user_data.starts_with(resource.path()),
            "and none of them hangs off the resource directory: {absent:?}",
        );
        assert_ne!(
            absent.user_data, absent.output,
            "the user data dir is not where output files land"
        );
        assert_ne!(
            absent.profiles, absent.user_data,
            "a profile's files are not the browser's own data dir"
        );

        std::fs::create_dir_all(resource.path().join("browser-stack/node/bin")).expect("dirs");
        std::fs::write(resource.path().join("browser-stack/node/bin/node"), b"").expect("node");
        let bundled = StackPaths::from_dirs(resource.path(), data.path());
        assert_eq!(
            bundled.stack,
            resource.path().join("browser-stack"),
            "and the bundle's own copy wins once the vendoring is there",
        );
    }

    /// `profile` chooses the profile and never reaches a tool; a call without
    /// one is a call for the browser's own context; and a `profile` that is
    /// not a name is the call's mistake, answered.
    #[test]
    fn a_profile_argument_is_taken_off_and_must_be_a_name() {
        let (chosen, rest) =
            take_profile(json!({ "url": "https://example.com", "profile": "hunt" }))
                .expect("a name is taken off");
        assert_eq!(chosen, Some("hunt".to_owned()));
        assert_eq!(rest, json!({ "url": "https://example.com" }));

        let (none, rest) =
            take_profile(json!({ "url": "https://example.com" })).expect("no profile");
        assert_eq!(none, None);
        assert_eq!(rest, json!({ "url": "https://example.com" }));

        let (none, rest) = take_profile(Value::Null).expect("a bare call");
        assert_eq!(none, None);
        assert_eq!(rest, Value::Null);

        let (none, _) = take_profile(json!({ "profile": null })).expect("null is no profile");
        assert_eq!(none, None);

        let refused = take_profile(json!({ "profile": 7 })).expect_err("a number is not a name");
        assert!(refused.contains("`profile` is the name"), "{refused}");
    }
}

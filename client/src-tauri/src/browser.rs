//! The client's browser host: the side that owns the browser process, the
//! profile and the drivers, and answers the asks a session's browser tools
//! send over the socket.
//!
//! **One shared browser for every session, and a browser of its own per
//! named profile**, and each outlives the client: it is launched detached
//! against a data directory under the app-support directory, so logins and
//! cookies survive a client restart and a forge restart touches nothing here
//! (spec section 3). The shared browser is driven by every session that
//! names no profile; a NAMED profile is a browser and a data directory of
//! its own, owned by the session that opened it and kept in [`profiles`] -
//! which is what makes a hand-off's Open able to raise that profile's own
//! window on its own page.
//!
//! The pieces:
//! - [`chromium`] - where the machine's browser is found, how it is launched,
//!   and how a launch is found again after a restart.
//! - [`profiles`] - which names are usable and who owns one.
//! - [`driver`] - upstream `@playwright/mcp` as a child process, spoken to as
//!   an MCP client.
//!
//! Nothing here decides what a tool MEANS: the ask carries upstream's own
//! tool name and arguments, the driver runs them, and the answer is the parts
//! it returned.

pub mod chromium;
pub mod custom;
pub mod driver;
pub mod profiles;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::Manager as _;

use driver::ReplyPart;
use profiles::{DriverStart, Named, Profile, Seat};
use serde_json::Value;
use tokio::sync::Mutex;

/// Where the vendored stack is, and the machine-local directories the browser
/// owns.
#[derive(Clone, Debug)]
pub struct StackPaths {
    /// The vendored tree: node and the driver. The browser itself is the
    /// machine's own, never vendored.
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

/// The host: the browser's own profile, the named ones, and where everything
/// lives.
pub struct BrowserHost {
    paths: Result<StackPaths, String>,
    /// Every `chromium::ensure` runs under this, whichever profile asks for
    /// one: a burst of first calls launches ONE browser rather than two onto
    /// one profile - and a hand-off's `show` racing a first call cannot leave
    /// two browsers on one profile either.
    launch: Mutex<()>,
    /// The shared profile: the browser's own profile, one driver for every
    /// session that names none.
    shared: Mutex<Option<Arc<Profile>>>,
    /// The named profiles, by name.
    named: Mutex<HashMap<String, Arc<Named>>>,
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
            used: std::sync::atomic::AtomicBool::new(false),
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
            used: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Run one browser tool for a session.
    ///
    /// No `profile` argument drives the browser's own profile. A name drives
    /// the named profile under it, opened on first use by the asking session;
    /// a name another session already holds is refused with the owner's name.
    ///
    /// **Every call resolves the browser first**, so the endpoint it hands the
    /// profile is the one the browser answers on NOW: a browser that died (or
    /// was relaunched for a window) moves that endpoint, and the profile
    /// rebuilds its driver rather than talking to a port nobody listens on.
    pub async fn call(
        &self,
        seat: &Seat,
        tool: &str,
        args: Value,
    ) -> Result<Vec<ReplyPart>, String> {
        let (profile, args) = take_profile(args)?;
        // **The shell's own entry, so a parked ask and a never-arrived one
        // stop looking alike.** Measured live 2026-10-07: a call parked with
        // no line like this on every inspected build, and nothing in the
        // client said whether the ask had reached the Rust side at all. This
        // is the host's own routing, not the session's work - the level is
        // INFO for the same reason "the browser is up on port" is.
        tauri_plugin_log::log::info!(
            "a browser tool call reached the host (event_name browser_call_entered, seat {seat}, \
             tool {tool}, profile {profile:?})"
        );
        // A name that cannot be a profile is decided before anything else: no
        // directory, no lock and no browser is consulted to answer it.
        if let Some(name) = profile.as_deref()
            && let Some(refusal) = profiles::name_refusal(name)
        {
            return Err(refusal);
        }
        let paths = self.paths.clone()?;
        let node = driver::node_path(&paths.stack);
        let cli = driver::cli_path(&paths.stack);
        match profile {
            None => {
                let active = self.browser_for(&paths.user_data).await?;
                // **The browser has been driven**, which the strip's row
                // draws: a session's call is the only way a browser of this
                // client's is used. Set after the profile is resolved, so a
                // refused call never lights the mark.
                self.used.store(true, std::sync::atomic::Ordering::Release);
                let endpoint = format!("http://127.0.0.1:{}", active.port);
                let shared = self.shared_profile().await;
                let start = DriverStart {
                    node: &node,
                    cli: &cli,
                    endpoint: &endpoint,
                    identity: &active.path,
                    output: &paths.output,
                };
                let outcome = shared.call(&start, tool, args).await;
                self.forget_a_dead_browser(&paths.user_data, &shared, active.port, outcome.is_err())
                    .await;
                outcome
            }
            Some(name) => {
                let named = self.named_profile(seat, &name).await?;
                let active = self.browser_for(&named.dir).await?;
                self.used.store(true, std::sync::atomic::Ordering::Release);
                let endpoint = format!("http://127.0.0.1:{}", active.port);
                let start = DriverStart {
                    node: &node,
                    cli: &cli,
                    endpoint: &endpoint,
                    identity: &active.path,
                    output: &paths.output,
                };
                let outcome = named.profile.call(&start, tool, args).await;
                self.forget_a_dead_browser(&named.dir, &named.profile, active.port, outcome.is_err())
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
        user_data: &Path,
        profile: &Profile,
        port: u16,
        failed: bool,
    ) {
        if !failed || chromium::answers_as(user_data, port).await {
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
    /// The profile's BROWSER goes with its name: the next call relaunches it
    /// over the same directory, so a close ends the run and never the logins.
    pub async fn close(&self, name: &str) -> Result<(), String> {
        let entry = {
            let named = self.named.lock().await;
            let Some(entry) = named.get(name).map(Arc::clone) else {
                return Err(format!("no browser profile is open under '{name}'"));
            };
            entry
        };
        chromium::hide(&entry.dir).await;
        let mut named = self.named.lock().await;
        named.remove(name);
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
        self.browser_for(&paths.user_data).await
    }

    /// One profile's live browser, launched when nothing is up: the shared
    /// profile's directory for most calls, a named profile's own for its.
    ///
    /// **The machine's own browser, headless.** CEF was measured out on
    /// 2026-10-07: its windowed runtime drops CDP-dispatched input whenever
    /// the window is not frontmost, and the pinned driver's click wedges on
    /// its target bookkeeping - full playwright parity wins over the native
    /// view (see `chromium::show` for how the person sees it). The launch
    /// lock serializes across every profile: one launch at a time, whichever
    /// directory it is for.
    async fn browser_for(&self, user_data: &Path) -> Result<chromium::ActivePort, String> {
        let _launching = self.launch.lock().await;
        let binary = chromium::browser_binary()?;
        chromium::ensure(&binary, user_data).await
    }

    /// The named profiles this host holds, by name, for its own strip. A
    /// profile whose driver died but whose name is still held is
    /// listed as not running rather than dropped: the name is owned until it
    /// is released, and a row that vanished would read as released.
    pub async fn profiles(&self) -> Vec<ProfileRow> {
        let named = self.named.lock().await;
        let mut rows: Vec<ProfileRow> = Vec::new();
        for (name, entry) in named.iter() {
            rows.push(ProfileRow {
                name: name.clone(),
                owner: entry.owner.to_string(),
                running: entry.profile.is_alive(),
                // The same predicate `show` keeps: the marker alone reads a
                // window that an X-close already took down.
                windowed: chromium::windowed(&entry.dir).await,
            });
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Whether the profile's browser is up as a WINDOW right now - the shared
    /// one for `None` - so the strip's button can say hide where the window is
    /// up and show where it is not.
    pub async fn windowed(&self, profile: Option<&str>) -> Result<bool, String> {
        let paths = self.paths.clone()?;
        let dir = match profile {
            Some(name) => {
                if let Some(refusal) = profiles::name_refusal(name) {
                    return Err(refusal);
                }
                paths.profiles.join(name)
            }
            None => paths.user_data,
        };
        Ok(chromium::windowed(&dir).await)
    }

    /// Bring the browser up VISIBLY for a hand-off's Open or the strip's
    /// show: **the profile the call names, or the shared one** - a profile's
    /// window is raised over its own browser, on its own page, which is what
    /// a CAPTCHA hand-off on a named profile needs.
    ///
    /// Serialized with every other launch, so a show racing a first call
    /// cannot leave two browsers on one profile. This is the client's own
    /// act and answers the core nothing: the hand-off's answer is Done or
    /// Not now, and never the window itself.
    pub async fn show(&self, profile: Option<&str>) -> Result<chromium::ActivePort, String> {
        let paths = self.paths.clone()?;
        let dir = match profile {
            Some(name) => {
                if let Some(refusal) = profiles::name_refusal(name) {
                    return Err(refusal);
                }
                paths.profiles.join(name)
            }
            None => paths.user_data,
        };
        let _launching = self.launch.lock().await;
        let binary = chromium::browser_binary()?;
        chromium::show(&binary, &dir).await
    }

    /// Take the window back down: the browser closes, and the next agent
    /// call relaunches it headless over the same directory. **The hand-off is
    /// not answered by this** - Done or Not now is.
    pub async fn hide(&self, profile: Option<&str>) -> Result<(), String> {
        let paths = self.paths.clone()?;
        let dir = match profile {
            Some(name) => {
                // **The same refusal show makes.** A name that cannot be a
                // profile resolves to no directory at all, and without this a
                // `profile: "../user-data"` would close the SHARED browser
                // while the answer claimed the named one came down.
                if let Some(refusal) = profiles::name_refusal(name) {
                    return Err(refusal);
                }
                paths.profiles.join(name)
            }
            None => paths.user_data,
        };
        // **The same launch lock show takes.** A relaunch holds the lock for
        // its whole span and its port file is absent - then present but
        // unanswered - until the browser comes up; a hide racing it would
        // no-op against a browser not yet up - Open then Done would leave the
        // window raised after the answer crossed.
        let _launching = self.launch.lock().await;
        chromium::hide(&dir).await;
        Ok(())
    }

    /// Whether a session has driven this client's browser since it came up.
    pub async fn used(&self) -> Result<bool, String> {
        Ok(self.used.load(std::sync::atomic::Ordering::Acquire))
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
                // The browser and its driver wait for the profile's first
                // CALL: opening the name here costs nothing, and a session
                // that names a profile and never drives it holds neither.
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
/// answered rather than guessed at. **`context` is read as the same thing**:
/// a v6 server's schema still names the argument that way, and an argument
/// left in the call would reach the driver as an unknown key while the SHARED
/// profile drove - the wrong browser, silently.
fn take_profile(mut args: Value) -> Result<(Option<String>, Value), String> {
    let Some(fields) = args.as_object_mut() else {
        return Ok((None, args));
    };
    let Some(profile) = fields.remove("profile").or_else(|| fields.remove("context")) else {
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

/// Bring the browser up visibly, for a hand-off's Open. Answers nothing to
/// the core: the window is the client's act, and Done or Not now is the
/// answer.
#[tauri::command]
pub async fn browser_show(
    host: tauri::State<'_, Arc<BrowserHost>>,
    profile: Option<String>,
) -> Result<(), String> {
    host.show(profile.as_deref()).await.map(|_| ())
}

/// Take the hand-off's window back down; the next agent call relaunches that
/// profile's browser headless over the same directory.
#[tauri::command]
pub async fn browser_hide(
    host: tauri::State<'_, Arc<BrowserHost>>,
    profile: Option<String>,
) -> Result<(), String> {
    host.hide(profile.as_deref()).await
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
    /// Whether its browser is up as a WINDOW right now: the strip's button
    /// says hide where it is, and show where it is not.
    pub windowed: bool,
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

/// Whether a profile's browser is up as a window, for the strip's show/hide
/// button: the shared profile for `None`.
#[tauri::command]
pub async fn browser_windowed(
    host: tauri::State<'_, Arc<BrowserHost>>,
    name: Option<String>,
) -> Result<bool, String> {
    host.windowed(name.as_deref()).await
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
        assert!(
            !host.used().await.expect("the mark reads"),
            "a refused call never lights the used mark - the refusal is decided before anything \
             is started",
        );
    }

    /// **The rows read show's predicate, not the raw marker.** A profile
    /// directory with the headed marker and nothing answering is NOT a
    /// window (the X-close state), and the shell's own reads must say so -
    /// reverting either call site to `launched_windowed` passes everything
    /// else and fails exactly here.
    #[tokio::test]
    async fn the_rows_keep_shows_predicate_not_the_raw_marker() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        std::fs::create_dir_all(&paths.user_data).expect("the shared dir");
        std::fs::write(paths.user_data.join("windowed"), b"").expect("the shared marker");
        let host = BrowserHost::new(paths.clone());
        assert!(
            !host.windowed(None).await.expect("the read"),
            "a marker with nothing answering is not a window - the shared line keeps show's rule",
        );

        let named = Named::open(seat(), "hunt", &paths);
        std::fs::create_dir_all(&named.dir).expect("the profile dir");
        std::fs::write(named.dir.join("windowed"), b"").expect("the profile marker");
        host.named.lock().await.insert("hunt".to_owned(), Arc::new(named));
        let rows = host.profiles().await;
        let row = rows.iter().find(|row| row.name == "hunt").expect("the row");
        assert!(
            !row.windowed,
            "a profile's row keeps show's rule too - a marker with no launch is no window",
        );
    }

    /// **A hide waits for a launch in flight** - the same lock `show` takes,
    /// so a hide cannot race a browser that is not up yet and leave the
    /// window raised after the answer crossed. The lock is what makes it
    /// wait: removing it lets the hide run while a launch holds the lock,
    /// and the assertion below is what catches that.
    #[tokio::test]
    async fn a_hide_waits_for_a_launch_in_flight() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = Arc::new(BrowserHost::new(paths));

        // The lock a launch holds for its whole span, held here by the test.
        let launching = host.launch.lock().await;
        let hiding = tokio::spawn({
            let host = Arc::clone(&host);
            async move { host.hide(None).await }
        });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(
            !hiding.is_finished(),
            "the hide must WAIT on the launch lock, not run against a browser mid-launch",
        );

        drop(launching);
        hiding
            .await
            .expect("the hide task")
            .expect("a hide with no launch up is a no-op, not an error");
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

    /// **A name that cannot be a profile is refused before any directory is
    /// touched.** Without the refusal a traversing name resolves inside the
    /// data root, and a Done claiming the named profile could close the
    /// SHARED browser instead.
    #[tokio::test]
    async fn hide_refuses_a_name_that_cannot_be_a_profile() {
        let resource = tempfile::tempdir().expect("a temp dir");
        let data = tempfile::tempdir().expect("a temp dir");
        let host = BrowserHost::new(StackPaths::from_dirs(resource.path(), data.path()));
        let refused = host.hide(Some("../user-data")).await;
        let Err(why) = refused else {
            panic!("a traversing name is refused");
        };
        assert!(why.contains("1 to 64"), "{why}");
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
    /// one is a call for the browser's own profile; and a `profile` that is
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

        // **The v6 name is read as the same thing.** A v1.1.0 server's schema
        // still calls the argument `context`; left in the call it would reach
        // the driver as an unknown key while the SHARED profile drove, so the
        // one step back is read rather than dropped.
        let (named, rest) =
            take_profile(json!({ "url": "https://example.com", "context": "hunt" }))
                .expect("a v6 name is taken off");
        assert_eq!(named, Some("hunt".to_owned()));
        assert_eq!(rest, json!({ "url": "https://example.com" }));
    }
}

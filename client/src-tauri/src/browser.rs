//! The client's browser host: the side that owns the browser process, the
//! profile and the drivers, and answers the asks a session's browser tools
//! send over the socket.
//!
//! **One shared browser for every session, and a browser of its own per
//! named profile**, each launched detached against a data directory under
//! the app-support directory - so logins and cookies survive a client restart
//! and a forge restart touches nothing here (spec section 3). **The process
//! does not outlive the client that launched it**: the client claims every
//! profile it launches or adopts, reaps its browsers when it exits, and
//! sweeps at boot for browsers whose owning client is gone - the cover for a
//! crash, a SIGKILL, and the update restart's main-thread path, which skips
//! the exit event by design. The shared browser is driven by every session
//! that names no profile; a NAMED profile is a browser and a data directory
//! of its own, owned by the session that opened it and kept in [`profiles`] -
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

#[cfg(target_os = "android")]
pub mod android;
pub mod chromium;
pub mod custom;
pub mod driver;
pub mod profiles;
pub mod window;

#[cfg(desktop)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::Manager as _;

use driver::ReplyPart;
#[cfg(desktop)]
use profiles::Named;
use profiles::{DriverStart, Profile, Seat};
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

    /// The phone's own derivation: `data` is the app's data dir, and the
    /// three state directories (socket, output, profiles) hang off it -
    /// which is what the shell passes to the Kotlin engine as its working
    /// directories. **The driver TREE is not here**: the Kotlin side unpacks
    /// it under filesDir (`files/browser/engine`) and runs node against
    /// that copy; `stack` names the same path only so logs point at the
    /// tree a reader can go look at.
    pub fn for_android(data: &Path) -> Self {
        Self {
            stack: data.join("browser/engine"),
            user_data: data.join("browser/user-data"),
            output: data.join("browser/output"),
            profiles: data.join("browser/profiles"),
        }
    }

    /// The unix socket the in-app driver dials, one per app run: beside the
    /// state directories, never inside one (nothing else may manage it).
    pub fn driver_socket(&self) -> PathBuf {
        self.user_data.parent().unwrap_or(&self.user_data).join("driver.sock")
    }

    /// Resolve from the running app, on the phone.
    #[cfg(target_os = "android")]
    pub fn android<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> Result<Self, String> {
        let data = app
            .path()
            .app_data_dir()
            .map_err(|why| format!("the app's data directory cannot be resolved: {why}"))?;
        Ok(Self::for_android(&data))
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
    #[cfg(desktop)]
    launch: Mutex<()>,
    /// The shared profile: the browser's own profile, one driver for every
    /// session that names none.
    shared: Mutex<Option<Arc<Profile>>>,
    /// The named profiles, by name. The phone holds none (its call refuses a
    /// name), so the map exists only where profiles do.
    #[cfg(desktop)]
    named: Mutex<HashMap<String, Arc<Named>>>,
    /// This client's claims on the profiles it launched or adopted: one entry
    /// per directory, its lock held for the process lifetime. What the exit
    /// reap closes, and what the boot sweep must never read as an orphan.
    #[cfg(desktop)]
    owned: Mutex<HashMap<PathBuf, chromium::ClientLock>>,
    /// The seats whose auto-profile refusal this run has already told a
    /// session about: the create is retried on every unnamed call, and the
    /// note belongs on the first one, not on each.
    #[cfg(desktop)]
    auto_refused: Mutex<std::collections::HashSet<Seat>>,
    /// Whether any session has driven this client's browser since it came up,
    /// which the strip's row marks (Ved, 2026-10-07).
    used: std::sync::atomic::AtomicBool,
    /// The phone's engine, the Kotlin half of the host (the WebView, the CDP
    /// relay, the in-app node): `None` on a host that cannot act at all.
    /// Desktop clients have no field: their engine is a launched browser and
    /// a spawned child.
    #[cfg(target_os = "android")]
    engine: Option<Arc<android::Engine>>,
}

impl BrowserHost {
    pub fn new(paths: StackPaths) -> Self {
        Self {
            paths: Ok(paths),
            #[cfg(desktop)]
            launch: Mutex::new(()),
            shared: Mutex::new(None),
            #[cfg(desktop)]
            named: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            owned: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            auto_refused: Mutex::new(std::collections::HashSet::new()),
            used: std::sync::atomic::AtomicBool::new(false),
            #[cfg(target_os = "android")]
            engine: None,
        }
    }

    /// The phone's host: the same shape plus the engine handle.
    #[cfg(target_os = "android")]
    pub fn android(paths: StackPaths, engine: Arc<android::Engine>) -> Self {
        Self {
            paths: Ok(paths),
            shared: Mutex::new(None),
            used: std::sync::atomic::AtomicBool::new(false),
            engine: Some(engine),
        }
    }

    /// A host that cannot act, for a client whose directories did not
    /// resolve: every call answers why rather than the client refusing to
    /// start over a feature it may never be asked for.
    pub fn unavailable(why: String) -> Self {
        Self {
            paths: Err(why),
            #[cfg(desktop)]
            launch: Mutex::new(()),
            shared: Mutex::new(None),
            #[cfg(desktop)]
            named: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            owned: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            auto_refused: Mutex::new(std::collections::HashSet::new()),
            used: std::sync::atomic::AtomicBool::new(false),
            #[cfg(target_os = "android")]
            engine: None,
        }
    }

    /// Run one browser tool for a session.
    ///
    /// No `profile` argument drives the browser's own profile. A name drives
    /// the named profile under it, opened on first use by the asking session;
    /// a name another session already holds is refused with the owner's name.
    ///
    /// **Every call resolves the browser first**, so the endpoint it hands the
    /// profile is the one the browser answers on NOW. On the desktop a browser
    /// that died moves that endpoint - a close, the person's own X, the
    /// engine taking the window down - and the
    /// profile rebuilds its driver rather than talking to a port nobody
    /// listens on. The phone's browser cannot move: its WebView is the app's
    /// own, so a claimed renderer death keeps the app alive but leaves later
    /// calls failing with the reason until the app is restarted (see the
    /// phone arm below).
    pub async fn call(
        &self,
        seat: &Seat,
        tool: &str,
        args: Value,
    ) -> Result<Vec<ReplyPart>, String> {
        // **The delete verb is a host action, not a driver tool.** It rides
        // the same ask relay as every browser tool - a session calls it and
        // the answer is the tool's own - but it never reaches the driver, so
        // it is answered here, before the profile argument or the browser is
        // consulted. A session may delete only the profile it OWNS (the
        // verdict's own rule); the person's own door is the strip, which
        // takes no seat and can reach a closed profile's files by name.
        if tool == "browser_profile_delete" {
            let name = args.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
            // **The arrival line, for this tool too**: the log below is the
            // host-reached discriminator, and this branch returns before it -
            // a delete that takes its time must not read like an ask that
            // never arrived. No profile is read for it: the name is the whole
            // addressing.
            tauri_plugin_log::log::info!(
                "a browser tool call reached the host (event_name browser_call_entered, seat \
                 {seat}, tool {tool}, profile None, name {name:?})"
            );
            if name.is_empty() {
                return Err(
                    "the shared browser is not a profile, so there is nothing to delete; name \
                     the profile"
                        .to_owned(),
                );
            }
            // The phone runs one browser and holds no named profiles, so the
            // ownership question does not exist there: its delete refuses by
            // name, in its own words.
            #[cfg(target_os = "android")]
            {
                return self.delete(&name).await.map(|()| {
                    vec![ReplyPart::Text { text: format!("deleted the '{name}' profile") }]
                });
            }
            #[cfg(desktop)]
            {
                let paths = self.paths.clone()?;
                // **The same fallback attach uses.** The live map holds this
                // run's entries; a restart leaves it empty while the record
                // still names the seat - so resolving from the map alone
                // would answer the owner "no profile named ... is open to
                // this session" and lock it out of its own profile.
                let owner = {
                    let named = self.named.lock().await;
                    match named.get(&name).map(|entry| entry.owner.clone()) {
                        Some(owner) => Some(owner),
                        None => profiles::owner_of(&paths.profiles.join(&name))?,
                    }
                };
                return match owner {
                    Some(owner) if &owner == seat => self.delete(&name).await.map(|()| {
                        vec![ReplyPart::Text {
                            text: format!("deleted the '{name}' profile and its saved logins"),
                        }]
                    }),
                    Some(owner) => Err(format!(
                        "the profile '{name}' belongs to {owner}; the person can delete it from \
                         the browser strip"
                    )),
                    None => Err(format!(
                        "no profile named '{name}' is open to this session; the person can delete \
                         a closed profile from the browser strip"
                    )),
                };
            }
        }
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
        #[cfg(desktop)]
        {
            let node = driver::node_path(&paths.stack);
            let cli = driver::cli_path(&paths.stack);
            match profile {
                None => {
                    // **The seat's auto profile is ensured on its first
                    // unnamed reach**, before anything is driven: the record
                    // is what keeps the name the seat's across restarts. The
                    // call itself still targets the shared browser - the
                    // ensure never changes it - and a refused create is said
                    // once on the answer below.
                    let auto = self.ensure_auto_profile(seat).await;
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
                    self.forget_a_dead_browser(
                        &paths.user_data,
                        &shared,
                        active.port,
                        outcome.is_err(),
                    )
                    .await;
                    match outcome {
                        Ok(mut parts) => {
                            if let Err(why) = auto {
                                self.note_auto_failure(seat, why, &mut parts).await;
                            }
                            Ok(parts)
                        }
                        Err(why) => Err(why),
                    }
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
                    self.forget_a_dead_browser(
                        &named.dir,
                        &named.profile,
                        active.port,
                        outcome.is_err(),
                    )
                    .await;
                    outcome
                }
            }
        }
        #[cfg(target_os = "android")]
        {
            // **The phone hosts ONE browser - the shared profile** - until
            // WebView storage isolation is its own piece; a named profile is
            // refused with the reason rather than quietly sharing the shared
            // one's logins.
            if let Some(name) = profile {
                return Err(phone_profile_refusal(&name));
            }
            let Some(engine) = self.engine.as_ref() else {
                return Err("the browser engine is not there".to_owned());
            };
            self.used.store(true, std::sync::atomic::Ordering::Release);
            let socket = paths.driver_socket();
            let shared = self.shared_profile().await;
            // **The identity carries the engine's generation, read PER
            // CALL** - a claimed renderer death bumps it, so the held driver
            // reads stale and this call attempts a rebuild. In the steady
            // case the attempt fails at its first step: the in-app node
            // keeps its old socket link open (it only redials a DEAD one),
            // so it never dials the fresh socket the rebuild bound, and the
            // start's accept window times out - the named dial timeout,
            // which carries the restart hint on a warm node. Only in the
            // narrower timing - a death landing mid-start, between accept
            // and pin - is the pin reached; Kotlin's second start resolves
            // `nodeStarted: false` rather than refusing. The claim is
            // exactly that much: the app survives, the page is gone, later
            // calls fail with the named reason, and the repair is
            // restarting the app until the reattach is solved (issue #1931).
            //
            // A read that fails or times out fails the CALL, named: a
            // sentinel identity would store a value no real generation can
            // equal and brick the run to the post-death state on one
            // transient bridge fault.
            let generation = engine
                .generation_now()
                .await
                .map_err(|why| format!("the engine generation could not be read: {why}"))?;
            let identity = format!("webview-{generation}");
            let start = DriverStart {
                socket: &socket,
                identity: &identity,
                ui_origin: "",
                engine,
                output: &paths.output,
            };
            // No `forget_a_dead_browser` here: the phone's engine cannot
            // relaunch within a run (its WebView is the app's own), so a
            // dead driver has nothing to rebuild against - its next call
            // fails with the reason, and a client restart is the repair.
            shared.call(&start, tool, args).await
        }
    }

    /// A call failed: is it the browser that is gone, rather than the tool?
    ///
    /// A plain tool failure - no such element, a refused navigation - leaves
    /// the driver in place; only a browser that stopped answering on the port
    /// this driver was built for drops it, so the next call rebuilds against
    /// a relaunched browser. Bounded by the probe's own read timeout.
    #[cfg(desktop)]
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
    /// The profile's BROWSER goes with its name - a real close, unlike an
    /// answer's lower - and the next call relaunches it over the same
    /// directory, so a close ends the run and never the logins.
    #[cfg(desktop)]
    pub async fn close(&self, name: &str) -> Result<(), String> {
        let entry = {
            let named = self.named.lock().await;
            let Some(entry) = named.get(name).map(Arc::clone) else {
                return Err(format!("no browser profile is open under '{name}'"));
            };
            entry
        };
        chromium::close(&entry.dir).await;
        let mut named = self.named.lock().await;
        named.remove(name);
        Ok(())
    }

    /// The phone holds no named profiles, so the row's close has nothing to
    /// close and says exactly that.
    #[cfg(target_os = "android")]
    pub async fn close(&self, name: &str) -> Result<(), String> {
        self.paths.clone()?;
        Err(format!("no browser profile is open under '{name}'"))
    }

    /// Delete a named profile, wherever it stands: the name's browser is
    /// killed, its entry goes, and its files are removed.
    ///
    /// **The one act that takes the logins.** Close ends the run and keeps
    /// everything; delete ends the run and removes `profiles/<name>` - the
    /// cookies and sessions with it - so it is the person's alone to make
    /// and it is confirmed in the row that makes it.
    ///
    /// **By name, whether or not the profile is open**: a closed profile's
    /// entry is gone while its files remain, and those files are exactly
    /// what a person means to remove. A name with neither an entry nor a
    /// directory is the one refusal. The whole act holds the profile's call
    /// lock first (no call runs while the files go) and then the launch
    /// lock a launch takes (nothing - this process or another - starts a
    /// browser onto a half-removed tree). That is the REVERSE of the call
    /// path's order - it takes and releases the launch locks before the call
    /// lock - so the pair is safe only because nothing ever holds a launch
    /// lock while taking a call lock; a future holder of both, launch first,
    /// would close the cycle. An in-flight call is WAITED for rather than
    /// refused. The close inside runs under the held launch lock, not
    /// through `chromium::hide`, which would take it again.
    #[cfg(desktop)]
    pub async fn delete(&self, name: &str) -> Result<(), String> {
        if let Some(refusal) = profiles::name_refusal(name) {
            return Err(refusal);
        }
        let paths = self.paths.clone()?;
        let dir = paths.profiles.join(name);
        let entry = self.named.lock().await.get(name).map(Arc::clone);
        if entry.is_none() && !dir.exists() {
            return Err(format!("no profile named '{name}' on this client"));
        }
        let _calls = match &entry {
            Some(entry) => Some(entry.profile.call_lock().await),
            None => None,
        };
        // A lock lives beside the port file, so the directory it sits in has
        // to exist before the lock can.
        if let Err(why) = std::fs::create_dir_all(&dir) {
            return Err(format!("the profile's directory cannot be made: {why}"));
        }
        let _launch = chromium::LaunchLock::acquire(&dir).await?;
        chromium::close_held_launch(&dir).await;
        self.named.lock().await.remove(name);
        // The claim goes with the files: a stale entry would read as "this
        // client already owns the directory" for a name created again later.
        self.owned.lock().await.remove(&dir);
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(why) => Err(format!("the profile's files could not be removed: {why}")),
        }
    }

    /// The phone runs one browser and no named profiles, so a delete refuses
    /// by name; the close's way forward - call without the name - is no door
    /// for a delete.
    #[cfg(target_os = "android")]
    pub async fn delete(&self, name: &str) -> Result<(), String> {
        self.paths.clone()?;
        Err(format!("no profile named '{name}' on this client"))
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
    #[cfg(desktop)]
    pub async fn start(&self) -> Result<chromium::ActivePort, String> {
        let paths = self.paths.clone()?;
        self.browser_for(&paths.user_data).await
    }

    /// The phone's own start: the Kotlin engine comes up (WebView, relay,
    /// unpacked tree) with no driver behind it.
    #[cfg(target_os = "android")]
    pub async fn start(&self) -> Result<(), String> {
        self.paths.clone()?;
        let Some(engine) = self.engine.as_ref() else {
            return Err("the browser engine is not there".to_owned());
        };
        engine.ensure().await
    }

    /// This client's claim on `dir`'s browser, taken before anything is
    /// launched or adopted on it.
    ///
    /// **Before the launch, deliberately**: the boot sweep reads a free claim
    /// as "the owning client is gone", so a launch must be owned from the
    /// moment it starts - a claim taken after would let the sweep reap the
    /// browser this client is bringing up. A directory another live client
    /// already holds is not claimed and not owned: this client still drives
    /// the browser on it, and the exit reap leaves it to its own client.
    #[cfg(desktop)]
    async fn claim(&self, dir: &Path) {
        let mut owned = self.owned.lock().await;
        if owned.contains_key(dir) {
            return;
        }
        match chromium::claim(dir) {
            Ok(Some(lock)) => {
                owned.insert(dir.to_path_buf(), lock);
            }
            Ok(None) => {}
            Err(why) => {
                // Not fatal to the call - the browser is driven either way -
                // but the profile has no owner this run, and the sweep
                // routes through this same claim: while the lock cannot be
                // taken, neither this client's exit nor a later boot reaps
                // the browser on it.
                tauri_plugin_log::log::debug!(
                    "a browser profile could not be claimed by this client (event_name \
                     browser_client_lock_refused, profile {}): {why}",
                    dir.display(),
                );
            }
        }
    }

    /// One profile's live browser, launched when nothing is up: the shared
    /// profile's directory for most calls, a named profile's own for its.
    ///
    /// **The machine's own browser, with its window born hidden.** CEF was
    /// measured out on 2026-10-07: its windowed runtime drops CDP-dispatched
    /// input whenever the window is not frontmost, and the pinned driver's
    /// click wedges on its target bookkeeping - full playwright parity wins
    /// over the native view (see `chromium::show` for how the person sees
    /// it). A call launches with no carried page: a fresh browser a session
    /// drives itself starts blank. The launch lock serializes across every
    /// profile: one launch at a time, whichever directory it is for.
    #[cfg(desktop)]
    async fn browser_for(&self, user_data: &Path) -> Result<chromium::ActivePort, String> {
        let _launching = self.launch.lock().await;
        self.claim(user_data).await;
        let binary = chromium::browser_binary()?;
        chromium::ensure(&binary, user_data, None).await
    }

    /// The named profiles this host holds, by name, for its own strip. A
    /// profile whose driver died but whose name is still held is
    /// listed as not running rather than dropped: the name is owned until it
    /// is deleted, and a row that vanished would read as gone.
    ///
    /// **The listing is served FOR A SEAT.** A session's view - which is the
    /// seat's page - is its own rows plus the ownerless legacy ones, and
    /// another seat's profile is not even listed to it. No seat is the
    /// person's own door and answers every row.
    #[cfg(desktop)]
    pub async fn profiles(&self, seat: Option<&Seat>) -> Vec<ProfileRow> {
        let named = self.named.lock().await;
        let mut rows: Vec<ProfileRow> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for (name, entry) in named.iter() {
            rows.push(ProfileRow {
                name: name.clone(),
                owner: entry.owner.to_string(),
                running: entry.profile.is_alive(),
                opened: chromium::ever_opened(&entry.dir),
                // Filled in below, once the registry lock is gone.
                windowed: false,
                closed: false,
            });
            dirs.push(entry.dir.clone());
        }
        drop(named);
        // Probed outside the lock: each probe is a blocking devtools read
        // with a seconds-long budget, and a profile open takes this lock.
        for (row, dir) in rows.iter_mut().zip(&dirs) {
            // The same predicate `show` keeps: the marker alone reads a
            // window that an X-close already took down.
            row.windowed = chromium::windowed(dir).await;
        }
        // **A closed profile is still a profile.** Its entry goes on close
        // while its files stay - the logins a person means to delete - so a
        // directory with no entry is listed too, not running, its owner read
        // from the record; one with no readable record is ownerless, which is
        // also what the sweep will read it as.
        if let Ok(paths) = self.paths.clone()
            && let Ok(entries) = std::fs::read_dir(&paths.profiles)
        {
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else { continue };
                if !kind.is_dir() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                if profiles::name_refusal(&name).is_some()
                    || rows.iter().any(|row| row.name == name)
                {
                    continue;
                }
                let dir = paths.profiles.join(&name);
                let owner = match profiles::owner_of(&dir) {
                    Ok(Some(owner)) => owner.to_string(),
                    Ok(None) | Err(_) => String::new(),
                };
                // A window can be up over a closed profile - the strip's own
                // show launches one without an entry - so the predicate is
                // still asked, on the directory itself.
                let windowed = chromium::windowed(&dir).await;
                let opened = chromium::ever_opened(&dir);
                rows.push(ProfileRow {
                    name,
                    owner,
                    running: false,
                    opened,
                    windowed,
                    closed: true,
                });
            }
        }
        if let Some(seat) = seat {
            let mine = seat.to_string();
            rows.retain(|row| row.owner == mine || row.owner.is_empty());
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// The phone holds no named profiles: an empty list is the honest answer,
    /// not an error - "none" is a state.
    #[cfg(target_os = "android")]
    pub async fn profiles(&self, _seat: Option<&Seat>) -> Vec<ProfileRow> {
        Vec::new()
    }

    /// The phone holds no named profiles, so a sweep has nothing to delete -
    /// and it still exists, because the seat frame's handler is shared with
    /// the desktop.
    #[cfg(target_os = "android")]
    pub async fn sweep(&self, _live: &[Seat]) -> Vec<String> {
        Vec::new()
    }

    /// Whether the profile's browser is up as a WINDOW right now - the shared
    /// one for `None` - so the strip's button can say hide where the window is
    /// up and show where it is not.
    #[cfg(desktop)]
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

    /// The phone's "window" is the takeover: up while the page is shown over
    /// the client, which is exactly what the strip's button reads.
    #[cfg(target_os = "android")]
    pub async fn windowed(&self, profile: Option<&str>) -> Result<bool, String> {
        self.paths.clone()?;
        if let Some(name) = profile {
            return Err(phone_profile_refusal(name));
        }
        let Some(engine) = self.engine.as_ref() else {
            return Err("the browser engine is not there".to_owned());
        };
        engine.windowed().await
    }

    /// Bring the browser up VISIBLY for a hand-off's Open or the strip's
    /// show: **the profile the call names, or the shared one** - a profile's
    /// window is raised over its own browser, on its own page, which is what
    /// a CAPTCHA hand-off on a named profile needs.
    ///
    /// **A hand-off raises only the profile of the seat that asked.** The
    /// dock passes the asking seat down, and another seat's name is refused
    /// where the name resolves to a browser; the person's own show carries no
    /// seat and reaches any of them.
    ///
    /// Serialized with every other launch, so a show racing a first call
    /// cannot leave two browsers on one profile. This is the client's own
    /// act and answers the core nothing: the hand-off's answer is Done or
    /// Not now, and never the window itself.
    #[cfg(desktop)]
    pub async fn show(
        &self,
        profile: Option<&str>,
        seat: Option<&Seat>,
    ) -> Result<chromium::ActivePort, String> {
        let paths = self.paths.clone()?;
        let dir = match profile {
            Some(name) => {
                if let Some(refusal) = profiles::name_refusal(name) {
                    return Err(refusal);
                }
                if let Some(seat) = seat {
                    profiles::raise_refusal(name, &paths.profiles.join(name), seat)?;
                }
                paths.profiles.join(name)
            }
            None => paths.user_data,
        };
        let _launching = self.launch.lock().await;
        self.claim(&dir).await;
        let binary = chromium::browser_binary()?;
        chromium::show(&binary, &dir).await
    }

    /// The phone's Open: raise the takeover (the approved Android pair - the
    /// page full-screen over the client, one slim bar, Back and hardware Back
    /// the door).
    #[cfg(target_os = "android")]
    pub async fn show(&self, profile: Option<&str>, _seat: Option<&Seat>) -> Result<(), String> {
        self.paths.clone()?;
        if let Some(name) = profile {
            return Err(phone_profile_refusal(name));
        }
        let Some(engine) = self.engine.as_ref() else {
            return Err("the browser engine is not there".to_owned());
        };
        engine.show().await
    }

    /// Take the window back out of the person's sight: the browser keeps
    /// running, with its tabs and its page, and the next call drives that
    /// same browser. **The hand-off is not answered by this** - Done or Not
    /// now is.
    #[cfg(desktop)]
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

    /// The phone's Done / Not now / the bar's back: lower the takeover. The
    /// WebView stays exactly where the session left it, which is what the
    /// next Open shows again.
    #[cfg(target_os = "android")]
    pub async fn hide(&self, profile: Option<&str>) -> Result<(), String> {
        self.paths.clone()?;
        if let Some(name) = profile {
            return Err(phone_profile_refusal(name));
        }
        let Some(engine) = self.engine.as_ref() else {
            return Err("the browser engine is not there".to_owned());
        };
        engine.hide().await
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

    /// Delete every named profile whose owning seat `live` does not hold -
    /// and every legacy directory, which no seat holds by definition.
    /// Answers the names that went.
    ///
    /// **The sweep is idempotent**, so it may run on every capable connect
    /// and after every despawn: the server sends the seats that exist, the
    /// client deletes what is no longer any of theirs. The record is read
    /// once, immediately before the delete - not atomically with it: a name
    /// adopted in the instant between the two keeps its profile until the
    /// NEXT sweep (single-user, in-process; the idempotence is the
    /// correction, not a lock). The delete is the same act the UI's own is -
    /// a call in flight is waited out. A failure is logged and leaves the
    /// profile; the sweep never half-deletes.
    #[cfg(desktop)]
    pub async fn sweep(&self, live: &[Seat]) -> Vec<String> {
        let Ok(paths) = self.paths.clone() else { return Vec::new() };
        let Ok(entries) = std::fs::read_dir(&paths.profiles) else { return Vec::new() };
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| profiles::name_refusal(name).is_none())
            .collect();
        names.sort();
        let mut gone = Vec::new();
        for name in names {
            let dir = paths.profiles.join(&name);
            // The record, as it stands just before the delete: a live
            // seat's profile - owned before the frame - is kept, and
            // everything else (another seat's, legacy, or an unreadable
            // record) goes.
            match profiles::owner_of(&dir) {
                Ok(Some(owner)) if live.contains(&owner) => continue,
                Ok(_) | Err(_) => {}
            }
            match self.delete(&name).await {
                Ok(()) => gone.push(name),
                Err(why) => {
                    tauri_plugin_log::log::warn!(
                        "a sweep of the browser profiles could not delete one (event_name \
                         browser_profile_sweep_failed, profile {name}, {why})"
                    );
                }
            }
        }
        gone
    }

    /// Reap the browsers THIS client owns: every profile it claimed, closed
    /// for real. The app's exit door - a host must not outlive its client.
    ///
    /// **Only what it owns**: a directory whose claim is another live
    /// client's is not in the map, so this client's exit never closes a
    /// browser that client is still driving. Run on `RunEvent::Exit`, which
    /// covers a quit and `exit()`; the update restart's main-thread path
    /// skips that event by design (tauri 2.12), and a crash or a SIGKILL
    /// runs no handler at all - [`BrowserHost::reap_orphans`] is the cover
    /// for those.
    #[cfg(desktop)]
    pub async fn reap_owned(&self) -> Vec<String> {
        let dirs: Vec<PathBuf> = self.owned.lock().await.keys().cloned().collect();
        let mut reaped = Vec::new();
        for dir in dirs {
            chromium::close(&dir).await;
            if let Some(name) = dir.file_name().and_then(std::ffi::OsStr::to_str) {
                reaped.push(name.to_owned());
            }
        }
        if !reaped.is_empty() {
            tauri_plugin_log::log::info!(
                "reaped this client's browser hosts on exit (event_name \
                 browser_hosts_reaped_on_exit, profiles {reaped:?})"
            );
        }
        reaped
    }

    /// Reap the browsers in this client's own browser root whose owning
    /// client is gone: every profile directory whose port answers while no
    /// client holds its claim.
    ///
    /// **The boot door, and the only cover for a client that never ran its
    /// exit** - a crash, a SIGKILL, the update restart's main-thread path.
    /// Runs after [`BrowserHost::start`] has claimed the shared profile, so
    /// this client's own launch is never read as an orphan. Scoped to this
    /// app's own root: another app's browsers (a scratch rig's, the person's
    /// own) are never touched.
    #[cfg(desktop)]
    pub async fn reap_orphans(&self) -> Vec<String> {
        let Ok(paths) = self.paths.clone() else { return Vec::new() };
        let mut dirs = vec![paths.user_data.clone()];
        if let Ok(entries) = std::fs::read_dir(&paths.profiles) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    dirs.push(entry.path());
                }
            }
        }
        let mut reaped = Vec::new();
        for dir in dirs {
            if self.owned.lock().await.contains_key(&dir) {
                continue;
            }
            match chromium::claim(&dir) {
                // The claim was free: the client that owned this directory is
                // gone. The look and the close both run UNDER the claim, so
                // no other sweep can interleave with them - and a browser
                // that answers the look is what the dead client left. (A
                // second live client attaching to that browser while the
                // claim is held is not stopped by this - it cannot claim,
                // and attaching is not gated on the claim - but its call
                // fails and its next one relaunches.)
                Ok(Some(lock)) => {
                    if chromium::verified(&dir).await.is_some() {
                        chromium::close(&dir).await;
                        if let Some(name) = dir.file_name().and_then(std::ffi::OsStr::to_str) {
                            reaped.push(name.to_owned());
                        }
                    }
                    drop(lock);
                }
                Ok(None) => {}
                Err(why) => {
                    tauri_plugin_log::log::debug!(
                        "a browser whose client may be gone could not be claimed by the sweep \
                         (event_name browser_orphan_sweep_refused, profile {}): {why}",
                        dir.display(),
                    );
                }
            }
        }
        if !reaped.is_empty() {
            tauri_plugin_log::log::info!(
                "reaped browser hosts whose client is gone (event_name browser_orphans_reaped, \
                 profiles {reaped:?})"
            );
        }
        reaped
    }

    /// The seat's own profile, named after the seat, ensured on the seat's
    /// first reach for the browser that names no profile.
    ///
    /// **The call that creates it still drives the shared browser** - "the
    /// unnamed shared browser stays exactly as it is" - so this reserves the
    /// seat's own name and gives its logins somewhere to start, it does not
    /// change a call's target. A name another seat holds is refused with the
    /// reason and never suffixed: a name that means "this seat" must not
    /// silently mean something else, and the caller falls back to the shared
    /// browser.
    #[cfg(desktop)]
    async fn ensure_auto_profile(&self, seat: &Seat) -> Result<(), String> {
        self.named_profile(seat, &auto_name(seat)).await.map(|_| ())
    }

    /// Say a failed auto create once per seat per run. The create is retried
    /// on every unnamed call, so a note on each would be noise; the refusal
    /// is a fact about the seat's environment rather than forge's health, so
    /// the answer carries it once and the retries stay quiet.
    #[cfg(desktop)]
    async fn note_auto_failure(&self, seat: &Seat, why: String, parts: &mut Vec<ReplyPart>) {
        if !self.auto_refused.lock().await.insert(seat.clone()) {
            return;
        }
        tauri_plugin_log::log::debug!(
            "a seat's auto browser profile was not created (event_name \
             browser_auto_profile_refused, seat {seat}, {why})"
        );
        parts.push(ReplyPart::Text {
            text: format!(
                "Note: this seat's browser profile '{}' could not be created - {why}. This call \
                 drove the shared browser.",
                auto_name(seat),
            ),
        });
    }

    /// The named profile under `name`, opened when it is not there.
    ///
    /// The map's lock is held across an open, so two sessions naming one fresh
    /// profile race at the verdict rather than both opening: the first opens
    /// and owns it, and the second is refused by the same rule as any other
    /// attach. A profile whose driver died between calls needs nothing here -
    /// its next call rebuilds the driver over the saved files.
    ///
    /// **The owner record is read when no live entry holds the name**, so a
    /// client restart cannot turn another seat's profile into an open name:
    /// the record on disk is what the verdict answers from. An unreadable
    /// record refuses the name rather than guessing an owner.
    #[cfg(desktop)]
    async fn named_profile(&self, seat: &Seat, name: &str) -> Result<Arc<Named>, String> {
        if let Some(refusal) = profiles::name_refusal(name) {
            return Err(refusal);
        }
        let paths = self.paths.clone()?;
        let mut named = self.named.lock().await;
        let held = match named.get(name) {
            Some(entry) => Some(entry.owner.clone()),
            None => profiles::owner_of(&paths.profiles.join(name))?,
        };
        match profiles::verdict(name, seat, held.as_ref()) {
            profiles::Verdict::Refuse(refusal) => Err(refusal),
            // **The record outlives the entry.** A restart leaves the map
            // empty while the record still names the seat, so the owner's
            // first call re-opens its own name over the same directory
            // rather than being answered as if it had gone away.
            profiles::Verdict::Drive => match named.get(name).map(Arc::clone) {
                Some(entry) => Ok(entry),
                None => {
                    let fresh = Arc::new(Named::open(seat.clone(), name, &paths)?);
                    named.insert(name.to_owned(), Arc::clone(&fresh));
                    Ok(fresh)
                }
            },
            profiles::Verdict::Open => {
                // The browser and its driver wait for the profile's first
                // CALL: opening the name here costs nothing, and a session
                // that names a profile and never drives it holds neither.
                let fresh = Arc::new(Named::open(seat.clone(), name, &paths)?);
                named.insert(name.to_owned(), Arc::clone(&fresh));
                Ok(fresh)
            }
        }
    }
}

/// The name a seat's own profile takes: its project and label folded into
/// the profile charset.
///
/// **The label alone is not unique.** Every project's lead carries `lead`,
/// and two workers in different projects may share a label, so a
/// label-only name would let the first seat to reach the browser hold it
/// against every other seat of that label for good - "every seat gets one"
/// false for all but one. The project qualifies it.
///
/// **And the project name is not the profile charset** - it may carry a
/// space or reach past the length cap - so every character outside the
/// charset folds to a dash and the result is capped at the name bound
/// ("my app" plus label `lead` becomes `my-app-lead`). The promise is that
/// every seat GETS one, so the derivation must always produce a name the
/// registry accepts; a fold that still cannot is the create's own refusal
/// and the once-per-seat note, the last resort rather than the path. The
/// collision refusal stays the backstop for a hand-made name that takes an
/// auto name first.
#[cfg(desktop)]
fn auto_name(seat: &Seat) -> String {
    let mut folded = String::with_capacity(seat.project.len() + seat.label.len() + 1);
    for ch in seat.project.chars().chain(std::iter::once('-')).chain(seat.label.chars()) {
        folded.push(if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            ch
        } else {
            '-'
        });
    }
    // The registry's own bound; a name past it is refused there, so the
    // tail is dropped rather than the whole derivation.
    folded.truncate(64);
    folded
}

/// Why a named profile cannot be used on the phone yet. One sentence, and
/// the way forward in it: a session that reads a refusal has to know it can
/// simply call without the name.
#[cfg(target_os = "android")]
fn phone_profile_refusal(name: &str) -> String {
    format!(
        "'{name}' cannot be a profile on this client yet: the phone runs one browser, the \
         shared one - a call without a profile reaches it"
    )
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
/// answer. `seat` is the asking session's, when a hand-off is what raises
/// the window - the dock passes it - and `None` is the person's own show,
/// which reaches any profile.
#[tauri::command]
pub async fn browser_show(
    host: tauri::State<'_, Arc<BrowserHost>>,
    profile: Option<String>,
    seat: Option<Seat>,
) -> Result<(), String> {
    host.show(profile.as_deref(), seat.as_ref()).await.map(|_| ())
}

/// Take the hand-off's window back out of sight; the browser keeps running
/// over the same directory, and the next agent call drives it.
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
    /// Whether its browser has EVER run: a launched Chromium leaves its own
    /// state in the data directory, while a profile carrying only the
    /// seat's record has never been opened. **The difference is what the
    /// row's second line says** - "its driver is gone" over a profile that
    /// never had one reads as a fault where "not opened yet" is the truth
    /// (Ved, 2026-10-12).
    pub opened: bool,
    /// Whether its browser is up as a WINDOW right now: the strip's button
    /// says hide where it is, and show where it is not.
    pub windowed: bool,
    /// Whether the host holds no live name entry for it: a closed profile's
    /// browser is down while its files - the logins - are here. The strip's
    /// delete-all takes exactly these, and the page cannot work it out for
    /// itself (a driver that died keeps its entry, and is not closed).
    pub closed: bool,
}

/// The named profiles this host holds, for a page's own browser strip.
///
/// **Served per seat**: the page passes the seat it draws for, and the host
/// answers that seat's rows plus the ownerless ones - another seat's rows are
/// not visible to it. No seat is the person's own door and answers every row.
///
/// The profiles are the CLIENT's own state - it owns the drivers - so this is
/// the client reading itself, not a server read; the strip needs no new view
/// surface for it.
#[tauri::command]
pub async fn browser_profiles(
    host: tauri::State<'_, Arc<BrowserHost>>,
    seat: Option<Seat>,
) -> Result<Vec<ProfileRow>, String> {
    Ok(host.profiles(seat.as_ref()).await)
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

/// Delete a named profile from the client's own UI: the strip's row, acting
/// for the person. **The one act that takes a profile's logins with it**, so
/// the row that makes it confirms it first.
#[tauri::command]
pub async fn browser_profile_delete(
    host: tauri::State<'_, Arc<BrowserHost>>,
    name: String,
) -> Result<(), String> {
    host.delete(&name).await
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

        let named = Named::open(seat(), "hunt", &paths).expect("the open");
        std::fs::create_dir_all(&named.dir).expect("the profile dir");
        std::fs::write(named.dir.join("windowed"), b"").expect("the profile marker");
        host.named.lock().await.insert("hunt".to_owned(), Arc::new(named));
        let rows = host.profiles(None).await;
        let row = rows.iter().find(|row| row.name == "hunt").expect("the row");
        assert!(
            !row.windowed,
            "a profile's row keeps show's rule too - a marker with no launch is no window",
        );
    }

    /// A devtools endpoint that stalls the identity probe mid-request: the
    /// port, a signal that the request arrived, and the gate whose send lets
    /// the answers - identity first, then one page - complete.
    async fn stalling_devtools(
        identity: &str,
    ) -> (u16, tokio::sync::oneshot::Receiver<()>, tokio::sync::oneshot::Sender<()>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let port = listener.local_addr().expect("the bound port").port();
        let identity = identity.to_owned();
        let (reached, in_flight) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut reached = Some(reached);
            let mut gate = Some(gate);
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { continue };
                let mut buffer = [0_u8; 2048];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = if request.starts_with("GET /json/version") {
                    if let Some(reached) = reached.take() {
                        let _ = reached.send(());
                    }
                    if let Some(gate) = gate.take() {
                        let _ = gate.await;
                    }
                    format!("{{\"webSocketDebuggerUrl\":\"ws://127.0.0.1:{port}{identity}\"}}")
                } else {
                    "[{\"type\":\"page\",\"url\":\"https://example.com/x\"}]".to_owned()
                };
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = socket.write_all(answer.as_bytes()).await;
            }
        });
        (port, in_flight, release)
    }

    /// **The strip's list does not hold the registry lock across its probes.**
    /// The window predicate is a blocking devtools read with a seconds-long
    /// budget, and a profile open takes the same lock: held across the probes,
    /// one list read parks an open behind every profile it has to ask about.
    /// The endpoint stalls the probe mid-request, so the lock is asserted at
    /// the one moment it must be free - and the probe's own answer still
    /// lands on the row.
    #[tokio::test]
    async fn a_list_does_not_hold_the_registry_lock_across_its_probes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = Arc::new(BrowserHost::new(paths.clone()));

        let identity = "/devtools/browser/9d1f-77aa";
        let (port, in_flight, release) = stalling_devtools(identity).await;
        let named = Named::open(seat(), "hunt", &paths).expect("the open");
        std::fs::write(named.dir.join("windowed"), b"").expect("the windowed marker");
        std::fs::write(named.dir.join("DevToolsActivePort"), format!("{port}\n{identity}\n"))
            .expect("the port file");
        host.named.lock().await.insert("hunt".to_owned(), Arc::new(named));

        let listing = tokio::spawn({
            let host = Arc::clone(&host);
            async move { host.profiles(None).await }
        });
        // A hang guard, not a deadline: the probe reaches the endpoint in
        // milliseconds, and a run that never signals has no probe at all.
        tokio::time::timeout(std::time::Duration::from_secs(10), in_flight)
            .await
            .expect("the probe reaches the endpoint")
            .expect("the endpoint signals the request");
        assert!(
            host.named.try_lock().is_ok(),
            "the list holds the registry lock across its probe: a profile open waits the probe's \
             whole budget out",
        );
        release.send(()).expect("the endpoint releases");
        let rows = listing.await.expect("the list task");
        let row = rows.iter().find(|row| row.name == "hunt").expect("the row");
        assert!(row.windowed, "the probe's own answer lands on the row");
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
        assert!(host.profiles(None).await.is_empty());
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

    /// **Delete removes the name AND the tree** - the one act that takes the
    /// logins; close keeps them.
    #[tokio::test]
    async fn a_delete_takes_the_name_and_the_tree() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let named = Named::open(seat(), "hunt", &paths).expect("the open");
        std::fs::create_dir_all(&named.dir).expect("the profile dir");
        std::fs::write(named.dir.join("Cookies"), b"the logins").expect("some data");
        host.named.lock().await.insert("hunt".to_owned(), Arc::new(named));
        host.delete("hunt").await.expect("the delete");
        assert!(host.named.lock().await.get("hunt").is_none(), "the entry went");
        assert!(!paths.profiles.join("hunt").exists(), "and the tree with it");
    }

    /// **Delete works by name for a closed profile**: the entry is gone while
    /// the files remain, and those files are what a person means to remove.
    #[tokio::test]
    async fn a_delete_works_by_name_for_a_closed_profile() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        std::fs::create_dir_all(paths.profiles.join("hunt")).expect("the files");
        std::fs::write(paths.profiles.join("hunt").join("Cookies"), b"the logins")
            .expect("some data");
        host.delete("hunt").await.expect("the delete");
        assert!(!paths.profiles.join("hunt").exists(), "the closed profile's files went");
    }

    /// A name with neither an entry nor a directory is the one refusal, in
    /// the shell's own sentence.
    #[tokio::test]
    async fn a_delete_of_nothing_at_all_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths);
        let refused = host.delete("hunt").await.expect_err("nothing to delete");
        assert_eq!(refused, "no profile named 'hunt' on this client");
    }

    /// **A delete waits out a call in flight** - the profile's own lock is
    /// held across a call, and the delete takes it before touching the tree.
    #[tokio::test]
    async fn a_delete_waits_for_a_call_in_flight() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = Arc::new(BrowserHost::new(paths.clone()));
        let named = Arc::new(Named::open(seat(), "hunt", &paths).expect("the open"));
        std::fs::create_dir_all(&named.dir).expect("the profile dir");
        let calls = named.profile.call_lock().await;
        host.named.lock().await.insert("hunt".to_owned(), Arc::clone(&named));

        let deleting = tokio::spawn({
            let host = Arc::clone(&host);
            async move { host.delete("hunt").await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!deleting.is_finished(), "the delete waits while a call holds the profile's lock");
        drop(calls);
        deleting.await.expect("the task joins").expect("the delete");
        assert!(!paths.profiles.join("hunt").exists(), "and then it runs");
    }

    /// **A delete waits for a launch in flight too**: it takes the same
    /// machine-wide lock a launch takes, so nothing starts a browser onto a
    /// half-removed tree.
    #[tokio::test]
    async fn a_delete_waits_for_a_launch_in_flight() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = Arc::new(BrowserHost::new(paths.clone()));
        let profile_dir = paths.profiles.join("hunt");
        std::fs::create_dir_all(&profile_dir).expect("the files");
        let launch = chromium::LaunchLock::acquire(&profile_dir).await.expect("the launch lock");

        let deleting = tokio::spawn({
            let host = Arc::clone(&host);
            async move { host.delete("hunt").await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!deleting.is_finished(), "the delete waits while a launch holds the lock");
        drop(launch);
        deleting.await.expect("the task joins").expect("the delete");
        assert!(!profile_dir.exists(), "and then the tree goes");
    }

    /// **A delete never waits on the lock it already holds.** The act takes
    /// the launch lock across its whole self, so the close inside it runs
    /// UNDER that guard: a second take would spin the full launch deadline
    /// against its own holder before the kill it was going to make anyway.
    /// The port file is a passed launch's, old enough to read as gone, so
    /// the close takes its fast path and any wait on the clock belongs to a
    /// re-take.
    #[tokio::test(start_paused = true)]
    async fn a_delete_does_not_wait_on_the_launch_lock_it_holds() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = Arc::new(BrowserHost::new(paths.clone()));
        let profile_dir = paths.profiles.join("hunt");
        std::fs::create_dir_all(&profile_dir).expect("the files");
        // A port nobody listens on, carried by a port file old enough that
        // the close reads it as a launch gone rather than one coming up.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("a port")
            .local_addr()
            .expect("the address")
            .port();
        let port_file = profile_dir.join("DevToolsActivePort");
        std::fs::write(&port_file, format!("{port}\n/devtools/browser/9d1f-77aa\n"))
            .expect("the port file");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&port_file)
            .expect("the port file")
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("the age");

        let began = tokio::time::Instant::now();
        host.delete("hunt").await.expect("the delete");
        let waited = began.elapsed();
        assert!(
            waited < std::time::Duration::from_secs(5),
            "the delete waited {waited:?} on the launch lock it already holds",
        );
        assert!(!profile_dir.exists(), "and the tree goes");
    }

    /// **The auto profile is created on the seat's first unnamed reach, and
    /// the call still drives the shared browser.** Afterwards the directory
    /// and the record exist - the name is the seat's from now on - while the
    /// profile's own browser was never started: only a call that NAMES it
    /// does that. **No browser is launched by this test**: the shared
    /// directory is a FILE, so the launch refuses before spawning anything,
    /// and the call's failure lands past the ensure, which is the act under
    /// test - whether or not the machine has one of the two browsers to
    /// discover, which is all the refusal's wording varies with. A unit test
    /// that drives the real launch belongs only in the deliberate, ignored
    /// live suite.
    #[tokio::test]
    async fn the_auto_profile_is_created_on_the_first_unnamed_reach() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        // A file where the shared browser's directory should be: the launch
        // cannot make its directory, so nothing spawns, and the failure is
        // past the ensure.
        std::fs::write(&paths.user_data, b"").expect("a file where the shared dir goes");
        let host = BrowserHost::new(paths.clone());
        let who = seat();

        let why = host
            .call(&who, "browser_navigate", json!({ "url": "https://example.com" }))
            .await
            .expect_err("the call cannot drive a shared directory that is a file");
        // The reason is the machine's: where a browser is installed, the
        // launch refuses at the shared directory, and where none is, at the
        // discovery before it. Both are past the ensure, which is what the
        // assertions below pin.
        assert!(
            why.contains("cannot be made") || why.contains("no browser to drive"),
            "the call failed for some other reason: {why}",
        );

        let auto = paths.profiles.join("forge-lead");
        assert_eq!(
            profiles::owner_of(&auto),
            Ok(Some(who)),
            "the seat's own name is recorded, its project qualifying its label",
        );
        assert!(
            !auto.join("DevToolsActivePort").exists(),
            "and nothing was driven through it - the browser waits for a call that names it",
        );
    }

    /// **Two leads, two projects, two auto profiles.** Every lead carries
    /// the label `lead`, so a label-only name would let the first project's
    /// lead hold it against every other lead for good - "every seat gets
    /// one" true for all but one. The auto name qualifies by project, so
    /// each lead gets its own and neither is refused.
    #[tokio::test]
    async fn two_leads_in_different_projects_each_get_their_auto_profile() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let one =
            Seat { org: "Org".to_owned(), project: "one".to_owned(), label: "lead".to_owned() };
        let two =
            Seat { org: "Org".to_owned(), project: "two".to_owned(), label: "lead".to_owned() };

        host.ensure_auto_profile(&one).await.expect("the first lead's own");
        host.ensure_auto_profile(&two).await.expect("the second lead's own, not a collision");

        assert_eq!(
            profiles::owner_of(&paths.profiles.join("one-lead")),
            Ok(Some(one)),
            "the first project's lead owns one-lead",
        );
        assert_eq!(
            profiles::owner_of(&paths.profiles.join("two-lead")),
            Ok(Some(two)),
            "and the second's owns two-lead, rather than being refused the first's name",
        );
    }

    /// **A project name that is not the profile charset still gets its seat
    /// an auto profile.** "my app" carries a space, which the registry
    /// refuses, so the derived name folds it - `my-app-lead` - rather than
    /// the seat never getting one and quietly riding the shared browser.
    #[tokio::test]
    async fn a_project_name_outside_the_charset_folds_into_the_auto_name() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let who =
            Seat { org: "Org".to_owned(), project: "my app".to_owned(), label: "lead".to_owned() };

        host.ensure_auto_profile(&who).await.expect("the folded name is a profile");
        assert_eq!(
            profiles::owner_of(&paths.profiles.join("my-app-lead")),
            Ok(Some(who)),
            "the space folded to a dash and the profile was recorded",
        );
    }

    /// **A taken auto name is refused with the reason, never suffixed**, and
    /// the refusal is said once per seat - the create is retried on every
    /// unnamed call, and a note on each would be noise.
    #[tokio::test]
    async fn a_taken_auto_name_is_refused_without_a_suffix_or_a_repeated_note() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let other = Seat { org: "Other".to_owned(), ..seat() };
        Named::open(other.clone(), "forge-lead", &paths).expect("the other seat's profile");
        let host = BrowserHost::new(paths.clone());
        let who = seat();

        let why =
            host.ensure_auto_profile(&who).await.expect_err("the name belongs to another seat");
        assert!(why.contains("belongs to Other/forge/lead"), "{why}");
        let dirs: Vec<String> = std::fs::read_dir(&paths.profiles)
            .expect("the profiles root")
            .flatten()
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .collect();
        assert_eq!(dirs, vec!["forge-lead".to_owned()], "no silent suffix: {dirs:?}");
        assert_eq!(
            profiles::owner_of(&paths.profiles.join("forge-lead")),
            Ok(Some(other)),
            "and the name did not change hands",
        );

        let mut parts = vec![ReplyPart::Text { text: "navigated".to_owned() }];
        host.note_auto_failure(&who, why.clone(), &mut parts).await;
        assert_eq!(parts.len(), 2, "the first failure rides the answer");
        let said = match &parts[1] {
            ReplyPart::Text { text } => text.clone(),
            ReplyPart::Image { .. } => panic!("the note is text"),
        };
        assert!(said.contains("could not be created"), "{said}");
        host.note_auto_failure(&who, why, &mut parts).await;
        assert_eq!(parts.len(), 2, "the retry is quiet - the seat has been told");
    }

    /// **A restart does not free the name.** The in-memory lease dies with
    /// the client; the record on disk is what the next run reads, so another
    /// seat naming a recorded profile is refused by name even with no live
    /// entry - the leak this closes, where any seat could adopt any name
    /// after a restart. An unreadable record refuses rather than guessing.
    /// **Both refusals land before any browser is consulted**, which is what
    /// lets this test run without launching anything; the owner's own
    /// re-attach is the verdict's `Drive` arm, tested in `profiles`.
    #[tokio::test]
    async fn a_recorded_name_stays_its_seats_across_a_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let owner = seat();
        // The first run: the seat names the profile, which records the owner.
        Named::open(owner.clone(), "hunt", &paths).expect("the open");
        // The client restarted: a fresh host, an empty map, the record alone.
        let host = BrowserHost::new(paths.clone());

        let other = Seat { org: "Other".to_owned(), ..seat() };
        let refused = host
            .call(
                &other,
                "browser_navigate",
                json!({ "url": "https://example.com", "profile": "hunt" }),
            )
            .await
            .expect_err("another seat's recorded profile");
        assert!(refused.contains("belongs to Busytools/forge/lead"), "{refused}");
        assert!(
            refused.contains("despawned") || refused.contains("browser strip"),
            "the refusal names the two doors that do end it: {refused}",
        );

        // **The owner re-attaches.** The record outlives the entry: the
        // owner's first naming after the restart opens the SAME directory
        // again rather than being answered as if the name had gone away.
        // The op is the name resolution, not a call: a call would launch,
        // and a unit test launches nothing.
        let reattached = host.named_profile(&owner, "hunt").await.expect("the owner re-attaches");
        assert_eq!(reattached.owner, owner, "and the entry is the owner's own");
        assert_eq!(
            profiles::owner_of(&paths.profiles.join("hunt")),
            Ok(Some(owner.clone())),
            "over the same record, not a new one",
        );

        // A record that cannot be read refuses with its reason.
        std::fs::write(paths.profiles.join("hunt").join(profiles::OWNER_FILE), b"not json")
            .expect("a tampered record");
        let broken = BrowserHost::new(paths.clone());
        let refused = broken
            .call(
                &owner,
                "browser_navigate",
                json!({ "url": "https://example.com", "profile": "hunt" }),
            )
            .await
            .expect_err("an unreadable record refuses");
        assert!(refused.contains("cannot be read"), "{refused}");
    }

    /// **A session deletes only the profile it owns**, and the shared
    /// browser is never a delete target: each refusal names what to do
    /// instead.
    #[tokio::test]
    async fn a_session_deletes_only_the_profile_it_owns() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let named = Named::open(seat(), "hunt", &paths).expect("the open");
        std::fs::create_dir_all(&named.dir).expect("the profile dir");
        host.named.lock().await.insert("hunt".to_owned(), Arc::new(named));

        let other = Seat { org: "Other".to_owned(), ..seat() };
        let refused = host
            .call(&other, "browser_profile_delete", json!({ "name": "hunt" }))
            .await
            .expect_err("another session's profile");
        assert!(refused.contains("belongs to Busytools/forge/lead"), "{refused}");
        assert!(refused.contains("from the browser strip"), "{refused}");
        assert!(paths.profiles.join("hunt").exists(), "and nothing was removed");

        let no_name = host
            .call(&seat(), "browser_profile_delete", json!({}))
            .await
            .expect_err("the shared browser");
        assert!(no_name.contains("the shared browser is not a profile"), "{no_name}");

        let closed = host
            .call(&seat(), "browser_profile_delete", json!({ "name": "gone" }))
            .await
            .expect_err("a name with no open profile");
        assert!(closed.contains("no profile named 'gone' is open to this session"), "{closed}");

        // Its own, open: the delete runs and the answer is the tool's own
        // sentence.
        let parts = host
            .call(&seat(), "browser_profile_delete", json!({ "name": "hunt" }))
            .await
            .expect("the owner deletes its own");
        let text = parts
            .iter()
            .map(|part| match part {
                driver::ReplyPart::Text { text } => text.clone(),
                driver::ReplyPart::Image { .. } => String::new(),
            })
            .collect::<String>();
        assert!(text.contains("deleted the 'hunt' profile"), "{text}");
        assert!(!paths.profiles.join("hunt").exists(), "the tree went");
    }

    /// **A restart does not lock the owner out of its own delete.** The live
    /// map is empty after one while the record still names the seat, so the
    /// tool resolves the owner the way attach does - from the record - and
    /// the seat deletes its own profile instead of being told nothing is
    /// open to it.
    #[tokio::test]
    async fn the_owner_deletes_its_own_recorded_profile_after_a_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let owner = seat();
        Named::open(owner.clone(), "hunt", &paths).expect("the open");
        Named::open(owner.clone(), "scout", &paths).expect("the other's record");
        // The client restarted: a fresh host, an empty map, the records alone.
        let host = BrowserHost::new(paths.clone());

        let parts = host
            .call(&owner, "browser_profile_delete", json!({ "name": "hunt" }))
            .await
            .expect("the record says the seat owns it");
        let text = parts
            .iter()
            .map(|part| match part {
                driver::ReplyPart::Text { text } => text.clone(),
                driver::ReplyPart::Image { .. } => String::new(),
            })
            .collect::<String>();
        assert!(text.contains("deleted the 'hunt' profile"), "{text}");
        assert!(!paths.profiles.join("hunt").exists(), "and the tree went");

        // Another seat against a record that is not its own: refused BY
        // NAME, not with "nothing is open to this session".
        let other = Seat { org: "Other".to_owned(), ..seat() };
        let refused = host
            .call(&other, "browser_profile_delete", json!({ "name": "scout" }))
            .await
            .expect_err("another seat's record");
        assert!(refused.contains("belongs to Busytools/forge/lead"), "{refused}");
        assert!(paths.profiles.join("scout").exists(), "and nothing was removed");
    }

    /// **A closed profile is still a row**: its directory is listed with no
    /// owner and no driver, or the one act that removes its logins has
    /// nothing to be reached from.
    #[tokio::test]
    async fn a_closed_profiles_files_are_listed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        std::fs::create_dir_all(paths.profiles.join("hunt")).expect("the files");
        // A file where a profile would be is not one.
        std::fs::write(paths.profiles.join("stray.txt"), b"").expect("a stray file");
        let rows = host.profiles(None).await;
        let row = rows.iter().find(|row| row.name == "hunt").expect("the closed profile's row");
        assert_eq!(row.owner, "", "a directory with no record is ownerless");
        assert!(!row.running, "and nothing is running behind it");
        assert!(!rows.iter().any(|row| row.name == "stray.txt"), "a file is not a profile",);
    }

    /// **A closed profile is the one the strip's delete-all takes**: the
    /// entry is what makes a profile open, and close takes it while the files
    /// - the logins - stay. An entry that is not running - no driver started,
    /// or one that died - is not closed, so the bulk door leaves it alone.
    #[tokio::test]
    async fn a_closed_profile_lists_as_closed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let named = Named::open(seat(), "hunt", &paths).expect("the open");
        host.named.lock().await.insert("hunt".to_owned(), Arc::new(named));

        let rows = host.profiles(None).await;
        let row = rows.iter().find(|row| row.name == "hunt").expect("the row");
        assert!(!row.running, "nothing is driving it");
        assert!(!row.closed, "not running is not closed: the entry is held");

        host.close("hunt").await.expect("the close");
        let rows = host.profiles(None).await;
        let row = rows.iter().find(|row| row.name == "hunt").expect("the row");
        assert!(row.closed, "close takes the entry; the row is closed");
        assert!(paths.profiles.join("hunt").exists(), "and its files stay for the person to take");
        assert_eq!(row.owner, seat().to_string(), "a closed row keeps the owner its record names");
    }

    /// **A hand-off raises only the asking seat's own profile's window.**
    /// The dock passes the seat down with the raise, and another seat's name
    /// is refused BEFORE anything launches - which is what lets this test
    /// run without a browser on the machine. The owner's own raise and the
    /// person's seatless one are `raise_refusal`'s own arms, tested there:
    /// driving them through `show` here would launch, and a unit test may
    /// not put a process (or a window) on the machine.
    #[tokio::test]
    async fn a_hand_off_raises_only_the_asking_seats_profile() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let mine = seat();
        let other = Seat { org: "Other".to_owned(), ..seat() };
        Named::open(other, "scout", &paths).expect("the other seat's profile");
        Named::open(mine.clone(), "hunt", &paths).expect("mine");

        let refused =
            host.show(Some("scout"), Some(&mine)).await.expect_err("another seat's profile");
        assert!(refused.contains("belongs to Other/forge/lead"), "{refused}");
        assert!(refused.contains("raises only its own"), "{refused}");
    }

    /// **The sweep takes the gone seats and keeps the live ones.** A seat in
    /// the set survives, a seat out of it goes, a legacy directory (no
    /// record) goes, the shared directory is not a profile and stays, and a
    /// second run has nothing left to do. A delete that fails leaves its
    /// profile and does not stop the sweep from finishing the rest.
    #[tokio::test]
    async fn the_sweep_takes_the_gone_seats_and_keeps_the_live_ones() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let live = seat();
        let gone = Seat { org: "Gone".to_owned(), ..seat() };
        Named::open(live.clone(), "kept", &paths).expect("the live seat's profile");
        Named::open(gone, "swept", &paths).expect("the gone seat's profile");
        std::fs::create_dir_all(paths.profiles.join("legacy")).expect("a legacy dir");
        std::fs::create_dir_all(&paths.user_data).expect("the shared dir");

        let deleted = host.sweep(std::slice::from_ref(&live)).await;
        assert_eq!(deleted, vec!["legacy".to_owned(), "swept".to_owned()], "{deleted:?}");
        assert!(paths.profiles.join("kept").exists(), "a live seat's profile survives");
        assert_eq!(
            profiles::owner_of(&paths.profiles.join("kept")),
            Ok(Some(live.clone())),
            "with its record",
        );
        assert!(!paths.profiles.join("legacy").exists(), "a legacy dir is nobody's and goes");
        assert!(!paths.profiles.join("swept").exists(), "a gone seat's profile goes");
        assert!(paths.user_data.exists(), "the shared directory is not a profile");
        assert!(host.sweep(&[live]).await.is_empty(), "the second sweep has nothing to do");
    }

    /// A sweep delete that fails is logged and leaves the profile standing -
    /// and the sweep goes on: the next name still goes.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_sweep_delete_that_fails_leaves_the_profile_and_the_sweep_continues() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let gone = Seat { org: "Gone".to_owned(), ..seat() };
        Named::open(gone.clone(), "stuck", &paths).expect("the stuck profile");
        Named::open(gone, "after", &paths).expect("the next profile");
        // The root cannot be written, so the two directories cannot be
        // unlinked; the sweep must report the failures and stop nowhere.
        let root = paths.profiles.clone();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500))
            .expect("a read-only root");
        let deleted = host.sweep(std::slice::from_ref(&seat())).await;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("writable again");

        assert!(deleted.is_empty(), "nothing could be deleted: {deleted:?}");
        assert!(paths.profiles.join("stuck").exists(), "the failure left the profile");
        assert!(
            paths.profiles.join("after").exists(),
            "the sweep reached the next name instead of stopping",
        );
    }

    /// **A never-opened profile is not one whose driver is gone.** The row
    /// carries which: a freshly recorded profile has no Chromium state in
    /// its directory, so the strip can say "not opened yet" rather than
    /// implying a driver that was there and left.
    #[tokio::test]
    async fn a_fresh_profile_lists_as_never_opened() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let named = Named::open(seat(), "hunt", &paths).expect("the open");
        host.named.lock().await.insert("hunt".to_owned(), Arc::new(named));

        let rows = host.profiles(None).await;
        let row = rows.iter().find(|row| row.name == "hunt").expect("the row");
        assert!(!row.opened, "a record-only directory has never been launched");
        assert!(!row.running, "and nothing is running behind it");

        // A launch writes Chromium's own state beside the record.
        std::fs::write(paths.profiles.join("hunt").join("Local State"), b"").expect("the state");
        let rows = host.profiles(None).await;
        let row = rows.iter().find(|row| row.name == "hunt").expect("the row");
        assert!(row.opened, "a launched Chromium leaves its own state in the directory");
    }

    /// **The listing is served per seat.** A seat's read answers its own rows
    /// and the ownerless ones and nothing else - another seat's profile is
    /// not even listed to it - while the person's door (no seat) answers
    /// every row, and a closed profile keeps the owner its record names.
    #[tokio::test]
    async fn the_listing_is_served_for_its_seat() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let mine = seat();
        let other = Seat { org: "Other".to_owned(), ..seat() };
        Named::open(mine.clone(), "hunt", &paths).expect("mine");
        Named::open(other, "scout", &paths).expect("theirs");
        std::fs::create_dir_all(paths.profiles.join("legacy")).expect("a legacy dir");

        let rows = host.profiles(Some(&mine)).await;
        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["hunt", "legacy"], "own rows and ownerless ones: {names:?}");
        let legacy = rows.iter().find(|row| row.name == "legacy").expect("the legacy row");
        assert_eq!(legacy.owner, "", "a legacy dir reads ownerless, and stays deletable");

        let all = host.profiles(None).await;
        let names: Vec<&str> = all.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["hunt", "legacy", "scout"],
            "the person's door answers every row: {names:?}",
        );
        let scout = all.iter().find(|row| row.name == "scout").expect("the other seat's row");
        assert_eq!(scout.owner, "Other/forge/lead", "a closed profile keeps its record's owner");
    }
}

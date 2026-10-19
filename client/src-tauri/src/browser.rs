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
//! - [`cef`] - the app's own Chromium: the host process, its frame ring, and
//!   where a host is found again after a restart.
//! - [`profiles`] - which names are usable and who owns one.
//! - [`driver`] - upstream `@playwright/mcp` as a child process, spoken to as
//!   an MCP client.
//! - `visibility` - the page's `document.visibilityState`, and the refusal or
//!   diagnosis a call carries when it is hidden.
//!
//! Nothing here decides what a tool MEANS: the ask carries upstream's own
//! tool name and arguments, the driver runs them, and the answer is the parts
//! it returned - except where the visibility gate refuses a pointer call on a
//! hidden page or carries its state on a snapshot.

#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "macos")]
pub mod cef;
#[cfg(all(desktop, not(target_os = "macos")))]
#[path = "browser/cef_stub.rs"]
pub mod cef;
pub mod custom;
pub mod driver;
#[cfg(desktop)]
pub(crate) mod idle;
pub mod profiles;
#[cfg(desktop)]
mod visibility;
pub mod window;

#[cfg(desktop)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(desktop)]
use tauri::Emitter as _;
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

    /// The shared browser's CEF cache root: **a directory of its own**, so the
    /// Brave data the machine's old launches left is never mistaken for it.
    #[cfg(desktop)]
    pub fn cef_shared(&self) -> PathBuf {
        self.user_data.parent().unwrap_or(&self.user_data).join("cef-profile")
    }

    /// A named profile's CEF cache root, beside its ownership record: the
    /// record and the locks stay in the name's own directory, the cache hangs
    /// under `cef/` so a Brave profile left in the same place is not read as
    /// this profile's cookies.
    #[cfg(desktop)]
    pub fn cef_named(&self, name: &str) -> PathBuf {
        self.profiles.join(name).join("cef")
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

/// How long a launch is given before its caller is told it did not answer.
/// Longer than the host's own bounds put together (the launch deadline plus
/// the directory's lock), so a slow-but-working start is not cut short; the
/// point is that a wedged one becomes words rather than a takeover with a
/// bar that cannot be answered.
#[cfg(desktop)]
const LAUNCH_WAIT: std::time::Duration = std::time::Duration::from_secs(75);

/// The host: the browser's own profile, the named ones, and where everything
/// lives.
pub struct BrowserHost {
    paths: Result<StackPaths, String>,
    /// Every host ensure runs under this, whichever profile asks for one: a
    /// burst of first calls launches ONE host rather than two onto one
    /// profile, and a hand-off's `show` racing a first call cannot leave two
    /// hosts on one profile either.
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
    owned: Mutex<HashMap<PathBuf, profiles::ClientLock>>,
    /// Every host this client has up, by cache root: **one CEF process per
    /// profile**, the same count the machine's Brave had.
    #[cfg(desktop)]
    hosts: Mutex<HashMap<PathBuf, Arc<cef::spawn::ActiveHost>>>,
    /// The open surface: the profile whose frames the page's takeover draws,
    /// `None` while nothing is drawn. The host keeps running under it.
    #[cfg(desktop)]
    surface: Mutex<Option<OpenSurface>>,
    /// Where the page last said its takeover's page area is, and the scale it
    /// is drawn at.
    #[cfg(desktop)]
    surface_rect: Mutex<Option<cef::surface::Rect>>,
    /// The host whose frames the native overlay is drawing, if one is.
    #[cfg(desktop)]
    attached: Mutex<Option<u16>>,
    /// The running app, for the overlay's main-thread placement.
    #[cfg(desktop)]
    app: Mutex<Option<tauri::AppHandle>>,
    /// The idle release's clock per profile: when a seat last used it, when
    /// its seat was last warned, and who that seat was. In-memory on purpose
    /// (see [`idle`]): a client restart grants every browser a fresh window.
    #[cfg(desktop)]
    touches: Mutex<HashMap<PathBuf, idle::Touch>>,
    /// When this client's host came up: an untouched profile's clock starts
    /// here rather than at the epoch, so a restart spares what the last run
    /// left.
    #[cfg(desktop)]
    boot: std::time::Instant,
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
            hosts: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            surface: Mutex::new(None),
            #[cfg(desktop)]
            surface_rect: Mutex::new(None),
            #[cfg(desktop)]
            attached: Mutex::new(None),
            #[cfg(desktop)]
            app: Mutex::new(None),
            #[cfg(desktop)]
            touches: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            boot: std::time::Instant::now(),
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
            hosts: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            surface: Mutex::new(None),
            #[cfg(desktop)]
            surface_rect: Mutex::new(None),
            #[cfg(desktop)]
            attached: Mutex::new(None),
            #[cfg(desktop)]
            app: Mutex::new(None),
            #[cfg(desktop)]
            touches: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            boot: std::time::Instant::now(),
            #[cfg(desktop)]
            auto_refused: Mutex::new(std::collections::HashSet::new()),
            used: std::sync::atomic::AtomicBool::new(false),
            #[cfg(target_os = "android")]
            engine: None,
        }
    }

    /// The boot's browser act (spam-control A4): build the host over the
    /// resolved paths, say where its stack lives, and set the reap going for
    /// whatever a dead client left behind.
    ///
    /// **It launches nothing** - a browser comes up on the first real need (a
    /// call, a raise) and never at app start. A fn rather than lines in the
    /// setup closure so the invariant is drivable: a launch re-added inside
    /// it fails `the_boot_launches_nothing`.
    #[cfg(desktop)]
    pub fn boot_browser(
        paths: Result<StackPaths, String>,
        app: Option<tauri::AppHandle>,
    ) -> Arc<Self> {
        // The host is handed to the frontend whether or not its directories
        // resolve: a client that cannot host says so when it is asked, rather
        // than refusing to start.
        let host = match paths {
            Ok(paths) => {
                tauri_plugin_log::log::info!("browser stack at {}", paths.stack.display());
                let host = Self::new(paths);
                if let Ok(mut slot) = host.app.try_lock() {
                    *slot = app;
                }
                Arc::new(host)
            }
            Err(why) => {
                tauri_plugin_log::log::warn!("browser host unavailable: {why}");
                let host = Self::unavailable(why);
                if let Ok(mut slot) = host.app.try_lock() {
                    *slot = app;
                }
                Arc::new(host)
            }
        };
        // The reap is the only cover for a client that never ran its exit - a
        // crash, a SIGKILL, or the update restart, whose main-thread path
        // skips `RunEvent::Exit` by design. It launches nothing, and with no
        // launch to mistake for one, it reaps only what a previous run left.
        let reap = Arc::clone(&host);
        tauri::async_runtime::spawn(async move {
            reap.reap_orphans().await;
        });
        host
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
                    // **The shared browser is in use from here on**: the touch
                    // is the idle release's clock, and it lands before any
                    // work that can fail - a slow call is not an idle one.
                    // **Under the launch lock**, which is the critical
                    // section the idle release's own verdict-and-kill takes:
                    // a call landing while the sweep asked its question
                    // blocks here, so the sweep re-reads a clock that already
                    // carries this touch rather than killing the browser this
                    // call is about to drive.
                    {
                        let _launching = self.launch.lock().await;
                        self.touch(&paths.cef_shared()).await;
                    }
                    // **The seat's auto profile is ensured on its first
                    // unnamed reach**, before anything is driven: the record
                    // is what keeps the name the seat's across restarts. The
                    // call itself still targets the shared browser - the
                    // ensure never changes it - and a refused create is said
                    // once on the answer below.
                    let auto = self.ensure_auto_profile(seat).await;
                    let active = self.start().await?;
                    // **The browser has been driven**, which the strip's row
                    // draws: a session's call is the only way a browser of this
                    // client's is used. Set after the profile is resolved, so a
                    // refused call never lights the mark.
                    self.used.store(true, std::sync::atomic::Ordering::Release);
                    let endpoint = format!("http://127.0.0.1:{}", active.port);
                    let identity = active.identity();
                    let shared = self.shared_profile().await;
                    let start = DriverStart {
                        node: &node,
                        cli: &cli,
                        endpoint: &endpoint,
                        identity: &identity,
                        output: &paths.output,
                    };
                    let outcome = shared.call(&start, tool, args).await;
                    self.forget_a_dead_browser(
                        &paths.cef_shared(),
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
                    let cache = profiles::cef_cache(&named.dir);
                    // The idle release's clock: the named profile is in use -
                    // under the launch lock, the same critical section the
                    // sweep's verdict-and-kill takes (see the shared arm), and
                    // held through the launch so a close cannot land between.
                    let active = {
                        let launched = self.launch_locked(&cache).await?;
                        let _launching = self.launch.lock().await;
                        self.touch(&cache).await;
                        launched
                    };
                    self.used.store(true, std::sync::atomic::Ordering::Release);
                    let endpoint = format!("http://127.0.0.1:{}", active.port);
                    let identity = active.identity();
                    let start = DriverStart {
                        node: &node,
                        cli: &cli,
                        endpoint: &endpoint,
                        identity: &identity,
                        output: &paths.output,
                    };
                    let outcome = named.profile.call(&start, tool, args).await;
                    self.forget_a_dead_browser(
                        &cache,
                        &named.profile,
                        active.port,
                        outcome.is_err(),
                    )
                    .await;
                    match outcome {
                        Ok(parts) => Ok(parts),
                        Err(why) => Err(why),
                    }
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

    /// A call failed: is it the host that is gone, rather than the tool?
    ///
    /// A plain tool failure - no such element, a refused navigation - leaves
    /// the driver in place; only a host that stopped answering on the port
    /// this driver was built for is dropped, so the next call rebuilds
    /// against a relaunched host.
    #[cfg(desktop)]
    async fn forget_a_dead_browser(
        &self,
        cache: &Path,
        profile: &Profile,
        port: u16,
        failed: bool,
    ) {
        if !failed || cef::spawn::probe(port).await {
            return;
        }
        profile.drop_driver().await;
        self.hosts.lock().await.remove(cache);
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
        let dir = {
            let named = self.named.lock().await;
            match named.get(name).map(|entry| entry.dir.clone()) {
                Some(dir) => dir,
                None => self.paths.clone()?.profiles.join(name),
            }
        };
        if !self.named.lock().await.contains_key(name) && !dir.exists() {
            return Err(format!("no browser profile is open under '{name}'"));
        }
        let cache = profiles::cef_cache(&dir);
        self.end_host(&cache).await;
        let mut named = self.named.lock().await;
        named.remove(name);
        Ok(())
    }

    /// End the host on one cache root: the process, its record and its ring.
    /// A `surface` drawing it closes first, so no frame outlives the host.
    #[cfg(desktop)]
    async fn end_host(&self, cache: &Path) {
        let _launching = self.launch.lock().await;
        let _lock = profiles::LaunchLock::acquire(cache).await.ok();
        self.end_host_locked(cache).await;
    }

    /// [`BrowserHost::end_host`] with the launch mutex and the directory's
    /// flock already held, so a close cannot remove a record a launch is
    /// about to write - and so a delete, which holds both, does not take its
    /// own lock again.
    #[cfg(desktop)]
    async fn end_host_locked(&self, cache: &Path) {
        {
            let mut surface = self.surface.lock().await;
            if surface.as_ref().is_some_and(|open| open.dir == cache) {
                *surface = None;
            }
        }
        let host = self.hosts.lock().await.remove(cache);
        let record = host
            .as_ref()
            .map(|host| cef::spawn::Record {
                pid: host.pid,
                port: host.port,
                ring: cef::spawn::ring_name(host.port),
            })
            .or_else(|| cef::spawn::read_record(cache));
        if let Some(record) = record {
            cef::spawn::end_verified(record.pid, record.port).await;
            cef::osr::Ring::unlink(&record.ring);
            cef::spawn::remove_record(cache);
        }
        self.reap_singleton(cache).await;
        // A lock the host's own directory held is released with it; the
        // client's claim stays until this client exits or deletes.
        let _ = self.reconcile_surface().await;
    }

    /// **A host with no record is still a host.** The record is the client's
    /// trail; Chromium's own `SingletonLock` is the host's. One whose record
    /// was lost, or whose record the sweep already removed, is read from
    /// there: reaped only when the pid is this very executable, so a reused
    /// number is never signalled.
    #[cfg(desktop)]
    async fn reap_singleton(&self, cache: &Path) {
        if let Some(pid) = cef::spawn::singleton_pid(cache)
            && cef::spawn::running(pid)
            && cef::spawn::is_our_binary(pid)
        {
            tauri_plugin_log::log::info!(
                "reaping a browser host its record no longer names (event_name \
                 cef_singleton_reaped, profile {}, pid {pid})",
                cache.display(),
            );
            cef::spawn::end(pid).await;
        }
        cef::spawn::clear_singleton(cache);
    }

    /// Record that `dir` was just used. **A touch clears the streak**:
    /// whatever warning stood, the browser is in use again, and its seat will
    /// be told afresh if it idles again.
    #[cfg(desktop)]
    pub(crate) async fn touch(&self, dir: &Path) {
        let mut touches = self.touches.lock().await;
        let entry = touches.entry(dir.to_path_buf()).or_insert(idle::Touch {
            at: self.boot,
            warned: None,
            caller: None,
        });
        entry.at = std::time::Instant::now();
        entry.warned = None;
    }

    /// Note that this profile's seat was just warned (or that its close's own
    /// ask came back held): the grace runs from here.
    #[cfg(desktop)]
    pub(crate) async fn note_warned(&self, dir: &Path, at: std::time::Instant) {
        let mut touches = self.touches.lock().await;
        if let Some(entry) = touches.get_mut(dir) {
            entry.warned = Some(at);
        }
    }

    /// Every profile the idle release may act on, with its clock: the named
    /// profiles (the seat to warn is their **creator** - the profile's owner,
    /// whoever called it last) and the shared browser (whoever last called
    /// it, `None` when no one has).
    ///
    /// **A browser that is not there is not idle, and a window that is up is
    /// in use.** A dead profile has nothing to release, and a raised
    /// window - the marker over a live launch with a page - is the
    /// person's: never warned about, never closed.
    #[cfg(desktop)]
    pub(crate) async fn idle_candidates(&self) -> Vec<idle::Candidate> {
        let Ok(paths) = self.paths.clone() else { return Vec::new() };
        let touches = {
            let touches = self.touches.lock().await;
            touches.clone()
        };
        let untouched = idle::Touch { at: self.boot, warned: None, caller: None };
        // **The registry lock is not held across the probes** - the same
        // discipline the strip's list keeps: a devtools read parks an open
        // behind every profile it has to ask about.
        let rows: Vec<(String, PathBuf, Seat)> = {
            let named = self.named.lock().await;
            named
                .iter()
                .map(|(name, entry)| (name.clone(), entry.dir.clone(), entry.owner.clone()))
                .collect()
        };
        let mut out = Vec::new();
        for (name, dir, owner) in rows {
            let cache = profiles::cef_cache(&dir);
            if !self.host_up(&cache).await || self.surface_is(&cache).await {
                continue;
            }
            let touch = touches.get(&cache).cloned().unwrap_or_else(|| untouched.clone());
            out.push(idle::Candidate {
                name: Some(name),
                dir: cache,
                // **The creator, not the last caller** (spam-control A1): a
                // named profile's warning belongs to the seat that made it.
                // The distinction is structural rather than observed - a
                // non-owner cannot drive a named profile at all
                // (`profiles::verdict` refuses another seat's name), so the
                // recorded caller could only ever be the owner; naming the
                // owner here is what makes the targeting explicit.
                touch: idle::Touch { caller: Some(owner), ..touch },
            });
        }
        let shared = paths.cef_shared();
        if self.host_up(&shared).await && !self.surface_is(&shared).await {
            let touch = touches.get(&shared).cloned().unwrap_or(untouched);
            out.push(idle::Candidate { name: None, dir: shared, touch });
        }
        out
    }

    /// Whether a host answers on this cache root, whether this client
    /// launched it or found it.
    #[cfg(desktop)]
    async fn host_up(&self, cache: &Path) -> bool {
        // **The guard goes before the await.** Held through the body, the
        // map's lock would wait on the probe it is about to run.
        let held = self.hosts.lock().await.get(cache).cloned();
        if let Some(host) = held {
            return cef::spawn::probe(host.port).await;
        }
        if let Some(record) = cef::spawn::read_record(cache) {
            return cef::spawn::probe(record.port).await;
        }
        false
    }

    /// The host this client holds for `cache`, if it still answers; one that
    /// no longer does is dropped from the map.
    ///
    /// **The map's guard goes before the probe and the re-lock.** An `if let`
    /// keeps its scrutinee's temporary alive through the body, so the map
    /// stayed locked across the probe - and the removal then waited on the
    /// same lock its own future held. That was the wedge a stale host met.
    #[cfg(desktop)]
    async fn live_host(&self, cache: &Path) -> Option<Arc<cef::spawn::ActiveHost>> {
        let held = self.hosts.lock().await.get(cache).cloned();
        let host = held?;
        tauri_plugin_log::log::debug!(
            "browser launch: probing the held host (event_name cef_launch_stage, stage probing, \
             pid {}, port {})",
            host.pid,
            host.port,
        );
        if cef::spawn::probe(host.port).await {
            return Some(host);
        }
        self.hosts.lock().await.remove(cache);
        None
    }

    /// Whether the page's takeover is drawing this cache root's frames.
    #[cfg(desktop)]
    pub(crate) async fn surface_is(&self, cache: &Path) -> bool {
        self.surface.lock().await.as_ref().is_some_and(|open| open.dir == cache)
    }

    /// The idle release closing one profile: the named path takes the row's
    /// own close (the name is given up and the next call relaunches), and the
    /// shared browser is closed where it stands - it is not a profile to
    /// remove. **The clock goes with it**: the entry a closed profile leaves
    /// would otherwise re-nudge and re-close a dead browser every pass.
    #[cfg(desktop)]
    pub(crate) async fn close_idle(&self, name: Option<&str>) {
        match name {
            Some(name) => {
                let dir = {
                    let named = self.named.lock().await;
                    named.get(name).map(|entry| profiles::cef_cache(&entry.dir))
                };
                let _ = self.close(name).await;
                if let Some(cache) = dir {
                    self.touches.lock().await.remove(&cache);
                }
            }
            None => {
                if let Ok(paths) = self.paths.clone() {
                    self.end_host(&paths.cef_shared()).await;
                    self.touches.lock().await.remove(&paths.cef_shared());
                }
            }
        }
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
    /// through `hide`, which would take it again.
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
        // **The launch mutex first, then the directory's flock**: the same
        // order every launch takes, so a delete and a launch cannot each hold
        // one of the pair.
        let _launching = self.launch.lock().await;
        // A lock lives beside the port file, so the directory it sits in has
        // to exist before the lock can.
        if let Err(why) = std::fs::create_dir_all(&dir) {
            return Err(format!("the profile's directory cannot be made: {why}"));
        }
        let cache = profiles::cef_cache(&dir);
        if let Err(why) = std::fs::create_dir_all(&cache) {
            return Err(format!("the profile's cache directory cannot be made: {why}"));
        }
        let _launch = profiles::LaunchLock::acquire(&cache).await?;
        self.end_host_locked(&cache).await;
        self.named.lock().await.remove(name);
        // The claim goes with the files: a stale entry would read as "this
        // client already owns the directory" for a name created again later.
        self.owned.lock().await.remove(&cache);
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

    /// Ensure the SHARED browser is up, without any driver.
    ///
    /// **On demand, never at app start** (spam-control A4): the boot brings
    /// no browser up, and this is the door the call path takes for the shared
    /// browser - a second ensure attaches to the launch that is already
    /// there. The drivers stay lazy: they exist to serve calls, and one with
    /// no calls to serve is a child process held for nothing. The launched
    /// browser's id comes back with its port, so a caller that must reap it
    /// (a live test that launched it) can; the call path ignores both.
    #[cfg(desktop)]
    pub async fn start(&self) -> Result<Arc<cef::spawn::ActiveHost>, String> {
        let paths = self.paths.clone()?;
        self.launch_locked(&paths.cef_shared()).await
    }

    /// One profile's live host under the launch mutex, bounded: the shared
    /// critical section every launch, close and reap takes, with a deadline
    /// so a wedged launch cannot hold the person's doors shut.
    #[cfg(desktop)]
    async fn launch_locked(&self, cache: &Path) -> Result<Arc<cef::spawn::ActiveHost>, String> {
        match tokio::time::timeout(LAUNCH_WAIT, async {
            let _launching = self.launch.lock().await;
            tauri_plugin_log::log::debug!(
                "browser launch: mutex held (event_name cef_launch_stage, stage mutex, profile \
                 {})",
                cache.display(),
            );
            self.browser_for_locked(cache).await
        })
        .await
        {
            Ok(answer) => answer,
            Err(_) => {
                tauri_plugin_log::log::warn!(
                    "browser launch: timed out (event_name cef_launch_timeout, profile {})",
                    cache.display(),
                );
                Err("the browser did not start in time - try again".to_owned())
            }
        }
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
    async fn claim(&self, cache: &Path) -> bool {
        let mut owned = self.owned.lock().await;
        if owned.contains_key(cache) {
            return true;
        }
        match profiles::claim(cache) {
            Ok(Some(lock)) => {
                owned.insert(cache.to_path_buf(), lock);
                true
            }
            Ok(None) => false,
            Err(why) => {
                // Not fatal to the call - the browser is driven either way -
                // but the profile has no owner this run, and the sweep
                // routes through this same claim: while the lock cannot be
                // taken, neither this client's exit nor a later boot reaps
                // the browser on it.
                tauri_plugin_log::log::debug!(
                    "a browser profile could not be claimed by this client (event_name \
                     browser_client_lock_refused, profile {}): {why}",
                    cache.display(),
                );
                false
            }
        }
    }

    /// One profile's live host, spawned when nothing is up: the shared
    /// profile's cache root for most calls, a named profile's own for its.
    ///
    /// **Under the client's launch mutex and the profile's launch flock**,
    /// so a burst of calls - and a second client - brings up ONE host onto a
    /// cache root. What is already there is adopted (the record names its
    /// port and ring); a record whose port no longer answers is cleaned up
    /// and replaced.
    #[cfg(desktop)]
    async fn browser_for_locked(
        &self,
        cache: &Path,
    ) -> Result<Arc<cef::spawn::ActiveHost>, String> {
        if !cef::available() {
            return Err(cef_unavailable());
        }
        self.claim(cache).await;
        tauri_plugin_log::log::debug!(
            "browser launch: claimed (event_name cef_launch_stage, stage claimed, profile {})",
            cache.display(),
        );
        // **A Brave-era profile moves once, on its first CEF launch.** Both
        // engines write Chromium's own profile, so the migration is a copy;
        // the client's own Brave launches used the same mock keychain the
        // host sets, so their cookies decrypt here.
        let paths = self.paths.clone()?;
        let legacy = if cache == paths.cef_shared() {
            paths.user_data.clone()
        } else {
            cache.parent().map(Path::to_path_buf).unwrap_or_else(|| cache.to_path_buf())
        };
        match profiles::migrate_brave(&legacy, cache) {
            Ok(true) => tauri_plugin_log::log::info!(
                "a Brave-era profile was moved into the CEF cache (event_name \
                 cef_profile_migrated, profile {})",
                cache.display(),
            ),
            Ok(false) => {}
            Err(why) => tauri_plugin_log::log::warn!(
                "a Brave-era profile could not be moved into the CEF cache (event_name \
                 cef_profile_migration_failed, profile {}, {why})",
                cache.display(),
            ),
        }
        tauri_plugin_log::log::debug!(
            "browser launch: migrated (event_name cef_launch_stage, stage migrated, profile {})",
            cache.display(),
        );
        if let Some(host) = self.live_host(cache).await {
            return Ok(host);
        }
        tauri_plugin_log::log::debug!(
            "browser launch: waiting on the directory lock (event_name cef_launch_stage, stage \
             locking, profile {})",
            cache.display(),
        );
        let _lock = profiles::LaunchLock::acquire(cache).await?;
        tauri_plugin_log::log::debug!(
            "browser launch: directory lock held (event_name cef_launch_stage, stage locked, \
             profile {})",
            cache.display(),
        );
        if let Some(record) = cef::spawn::read_record(cache) {
            if cef::spawn::probe(record.port).await {
                match cef::osr::Ring::open(&record.ring) {
                    Ok(ring) => {
                        let host = Arc::new(cef::spawn::ActiveHost {
                            pid: record.pid,
                            port: record.port,
                            ring: Arc::new(ring),
                            profile: cache.to_path_buf(),
                        });
                        self.hosts.lock().await.insert(cache.to_path_buf(), Arc::clone(&host));
                        return Ok(host);
                    }
                    Err(why) => {
                        // A live browser whose ring is gone cannot be drawn:
                        // it is ended and replaced rather than driven blind.
                        tauri_plugin_log::log::warn!(
                            "a browser host's ring could not be mapped; replacing it (event_name \
                             cef_ring_missing, profile {}, {why})",
                            cache.display(),
                        );
                        cef::spawn::end_verified(record.pid, record.port).await;
                        cef::spawn::remove_record(cache);
                    }
                }
            } else {
                // A stale record: the client that owned it never ran its
                // exit. The host no longer answers, so its pid is only a
                // number now - the ring and the record go, the pid is never
                // signalled.
                tauri_plugin_log::log::info!(
                    "a recorded browser host is gone; replacing it (event_name \
                     cef_record_stale, profile {}, pid {})",
                    cache.display(),
                    record.pid,
                );
                cef::osr::Ring::unlink(&record.ring);
                cef::spawn::remove_record(cache);
                // The dead host's singleton files go too, so the replacement
                // starts onto a clean profile.
                cef::spawn::clear_singleton(cache);
            }
        }
        // **The page reports the surface's own box shortly after it opens**;
        // this is only the size a host paints before that report.
        let (width, height) = (1100_u32, 620_u32);
        // **A port that was free a moment ago can be taken by the time the
        // host binds it** - the check and the bind are not one act. A launch
        // that dies that fast is retried on the next free port rather than
        // reported as a browser failure; a launch that fails for its own
        // reason stops at the first attempt's words.
        let mut launched: Option<(u32, u16, String)> = None;
        let mut last = String::new();
        for _ in 0..3 {
            let port = cef::spawn::free_port()
                .ok_or_else(|| "no free port for a browser host's devtools endpoint".to_owned())?;
            let ring_name = cef::spawn::ring_name(port);
            tauri_plugin_log::log::debug!(
                "browser launch: spawning (event_name cef_launch_stage, stage spawning, profile \
                 {}, port {port})",
                cache.display(),
            );
            let pid = cef::spawn::spawn(cache, port, &ring_name, width, height)?;
            // **The record is written before the wait.** A host that dies
            // between spawn and this line would otherwise be invisible to the
            // boot sweep, and CEF's singleton lock would keep the profile
            // unusable until someone killed the process by hand.
            let record = cef::spawn::Record { pid, port, ring: ring_name.clone() };
            if let Err(why) = cef::spawn::write_record(cache, &record) {
                cef::spawn::end(pid).await;
                cef::osr::Ring::unlink(&ring_name);
                return Err(why);
            }
            match cef::spawn::wait_ready_for(pid, port, cache).await {
                Ok(()) => {
                    tauri_plugin_log::log::debug!(
                        "browser launch: the endpoint answered (event_name cef_launch_stage, stage \
                         ready, pid {pid}, port {port})",
                    );
                    launched = Some((pid, port, ring_name));
                    break;
                }
                Err(why) => {
                    cef::spawn::end(pid).await;
                    cef::osr::Ring::unlink(&ring_name);
                    cef::spawn::remove_record(cache);
                    last = why;
                }
            }
        }
        let Some((pid, port, ring_name)) = launched else {
            return Err(last);
        };
        let ring = match cef::osr::Ring::open(&ring_name) {
            Ok(ring) => Arc::new(ring),
            Err(why) => {
                cef::spawn::end(pid).await;
                cef::osr::Ring::unlink(&ring_name);
                cef::spawn::remove_record(cache);
                return Err(why);
            }
        };
        let host =
            Arc::new(cef::spawn::ActiveHost { pid, port, ring, profile: cache.to_path_buf() });
        self.hosts.lock().await.insert(cache.to_path_buf(), Arc::clone(&host));
        tauri_plugin_log::log::info!(
            "the browser host is up (event_name cef_host_spawned, pid {pid}, port {port}, profile \
             {})",
            cache.display(),
        );
        Ok(host)
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
        let mut caches: Vec<PathBuf> = Vec::new();
        for (name, entry) in named.iter() {
            let cache = profiles::cef_cache(&entry.dir);
            rows.push(ProfileRow {
                name: name.clone(),
                owner: entry.owner.to_string(),
                running: entry.profile.is_alive(),
                opened: cef_opened(&cache),
                windowed: false,
                closed: false,
            });
            caches.push(cache);
        }
        drop(named);
        for (row, cache) in rows.iter_mut().zip(&caches) {
            // The surface is the client's own state, not a probe: the strip's
            // button says hide exactly while the takeover draws this profile.
            row.windowed = self.surface_is(cache).await;
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
                // A surface can be open over a closed profile - the strip's
                // own show spawns one without an entry - so the predicate is
                // still asked, on the cache root itself.
                let cache = profiles::cef_cache(&dir);
                let windowed = self.surface_is(&cache).await;
                let opened = cef_opened(&cache);
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
        let cache = match profile {
            Some(name) => {
                if let Some(refusal) = profiles::name_refusal(name) {
                    return Err(refusal);
                }
                paths.cef_named(name)
            }
            None => paths.cef_shared(),
        };
        Ok(self.surface_is(&cache).await)
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
    ) -> Result<Arc<cef::spawn::ActiveHost>, String> {
        let paths = self.paths.clone()?;
        let (name, dir) = match profile {
            Some(name) => {
                if let Some(refusal) = profiles::name_refusal(name) {
                    return Err(refusal);
                }
                if let Some(seat) = seat {
                    profiles::raise_refusal(name, &paths.profiles.join(name), seat)?;
                }
                (Some(name.to_owned()), paths.profiles.join(name))
            }
            None => (None, paths.cef_shared()),
        };
        let cache = if profile.is_some() { profiles::cef_cache(&dir) } else { dir };
        tauri_plugin_log::log::info!(
            "the browser surface is asked to open (event_name browser_show_entered, profile \
             {profile:?}, cache {})",
            cache.display(),
        );
        // **The launch and the surface claim are one act under the lock**, so
        // a close racing Open cannot leave a live surface over a dead host.
        // **Bounded**, because a launch that never answers must become the
        // page's failure sentence rather than a takeover whose bar cannot be
        // answered: a locked launch is exactly what left Open and back to
        // chat waiting for ever.
        let host = self.launch_locked(&cache).await?;
        // A raise is use: the person is being shown this browser, so the
        // idle release's clock restarts and any standing warning is spent.
        // **The claim goes under the same lock a close takes**, so Open and
        // back to chat can never interleave into a surface with no bar.
        let claimed = tokio::time::timeout(LAUNCH_WAIT, async {
            let _launching = self.launch.lock().await;
            self.touch(&cache).await;
            // One surface at a time: opening this profile's takes the window,
            // and a previous profile's host keeps running under it.
            *self.surface.lock().await =
                Some(OpenSurface { profile: name, dir: cache, host: Arc::clone(&host) });
            // **A placement that never landed fails the Open**, so the page
            // draws the sentence instead of a live takeover with no picture.
            self.reconcile_surface().await
        })
        .await;
        match claimed {
            Ok(answer) => answer?,
            Err(_) => return Err("the browser did not start in time - try Open again".to_owned()),
        }
        Ok(host)
    }

    /// The page said where its takeover's page area is: place the overlay
    /// there, and resize the host's view to match.
    #[cfg(desktop)]
    pub async fn set_surface_rect(&self, rect: cef::surface::Rect) {
        *self.surface_rect.lock().await = Some(rect);
        let _ = self.reconcile_surface().await;
    }

    /// Make the native half match the open surface: one overlay, drawing the
    /// open profile's host, at the page's last reported rect. Idempotent -
    /// every report calls it. **Answers the placement failure**, so an Open
    /// whose overlay never landed says why instead of returning success.
    #[cfg(desktop)]
    async fn reconcile_surface(&self) -> Result<(), String> {
        let open = self.surface.lock().await.clone();
        let rect = *self.surface_rect.lock().await;
        let app = self.app.lock().await.clone();
        let mut attached = self.attached.lock().await;
        match (open, rect, app.as_ref()) {
            (Some(open), Some(rect), Some(app)) => {
                if *attached == Some(open.host.port) {
                    cef::surface::set_rect(app, &open.host, rect);
                } else {
                    if attached.take().is_some() {
                        cef::surface::detach(app);
                    }
                    match cef::surface::attach(app, Arc::clone(&open.host), rect).await {
                        Ok(()) => *attached = Some(open.host.port),
                        Err(why) => {
                            tauri_plugin_log::log::warn!(
                                "the browser surface could not be opened (event_name \
                                 cef_surface_failed, profile {}, {why})",
                                open.dir.display(),
                            );
                            // **The page is told, not left on a promise.** A
                            // surface that could not open draws its sentence
                            // and keeps the hand-off parked.
                            *self.surface.lock().await = None;
                            let _ = app.emit("browser_surface_failed", why.clone());
                            return Err(why);
                        }
                    }
                }
                Ok(())
            }
            _ => {
                if attached.take().is_some()
                    && let Some(app) = app.as_ref()
                {
                    cef::surface::detach(app);
                }
                Ok(())
            }
        }
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
    pub async fn hide(&self, profile: Option<&str>) -> Result<bool, String> {
        let paths = self.paths.clone()?;
        let cache = match profile {
            Some(name) => {
                // **The same refusal show makes.** A name that cannot be a
                // profile resolves to no directory at all, and without this a
                // `profile: "../user-data"` would close the SHARED surface
                // while the answer claimed the named one came down.
                if let Some(refusal) = profiles::name_refusal(name) {
                    return Err(refusal);
                }
                paths.cef_named(name)
            }
            None => paths.cef_shared(),
        };
        // **The same launch lock show takes, waited on with a bound.** A show
        // holds it until the host answers; a hide racing it would no-op
        // against a host not yet up - Open then Done would leave the surface
        // up after the answer crossed - and an unbounded wait here is what
        // made back to chat do nothing at all.
        let Ok(_launching) =
            tokio::time::timeout(std::time::Duration::from_secs(20), self.launch.lock()).await
        else {
            return Err("the browser is still starting - try back to chat again".to_owned());
        };
        // A lower is the same use a raise is: the browser was just in front
        // of someone, and the clock restarts with it.
        self.touch(&cache).await;
        let mut surface = self.surface.lock().await;
        // **Whether this call actually lowered the page the person sees.**
        // A Done for another profile leaves that profile's surface up, and
        // the page's store has to keep drawing its bar over it.
        let lowered = !surface.as_ref().is_some_and(|open| open.dir != cache);
        if lowered {
            *surface = None;
        }
        drop(surface);
        let _ = self.reconcile_surface().await;
        Ok(lowered)
    }

    /// The phone's Done / Not now / the bar's back: lower the takeover. The
    /// WebView stays exactly where the session left it, which is what the
    /// next Open shows again.
    #[cfg(target_os = "android")]
    pub async fn hide(&self, profile: Option<&str>) -> Result<bool, String> {
        self.paths.clone()?;
        if let Some(name) = profile {
            return Err(phone_profile_refusal(name));
        }
        let Some(engine) = self.engine.as_ref() else {
            return Err("the browser engine is not there".to_owned());
        };
        engine.hide().await.map(|()| true)
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
        let caches: Vec<PathBuf> = self.owned.lock().await.keys().cloned().collect();
        let mut reaped = Vec::new();
        for cache in caches {
            self.end_host(&cache).await;
            if let Some(name) = cache.file_name().and_then(std::ffi::OsStr::to_str) {
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
    /// **The boot sweep, and the only cover for a client that never ran its
    /// exit** - a crash, a SIGKILL, the update restart's main-thread path.
    /// **It launches nothing** (spam-control A4: the client start brings no
    /// browser up): run at boot, before anything this run has claimed, so
    /// only what a previous run left can read as an orphan. Scoped to this
    /// app's own root: another app's browsers (a scratch rig's, the person's
    /// own) are never touched.
    #[cfg(desktop)]
    pub async fn reap_orphans(&self) -> Vec<String> {
        let Ok(paths) = self.paths.clone() else { return Vec::new() };
        let mut caches = vec![paths.cef_shared()];
        if let Ok(entries) = std::fs::read_dir(&paths.profiles) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    caches.push(profiles::cef_cache(&entry.path()));
                }
            }
        }
        let mut reaped = Vec::new();
        for cache in caches {
            if self.owned.lock().await.contains_key(&cache) {
                continue;
            }
            // The same lock a launch takes, so the sweep never removes a
            // record a launch in this process has just written.
            let _launching = self.launch.lock().await;
            match profiles::claim(&cache) {
                // The claim was free: the client that owned this cache root
                // is gone. The record names the host it left - killed if it
                // still answers - and the ring name, which macOS does NOT
                // release when the host dies.
                Ok(Some(lock)) => {
                    if let Some(record) = cef::spawn::read_record(&cache) {
                        cef::spawn::end_verified(record.pid, record.port).await;
                        cef::osr::Ring::unlink(&record.ring);
                        cef::spawn::remove_record(&cache);
                        if let Some(name) = cache.file_name().and_then(std::ffi::OsStr::to_str) {
                            reaped.push(name.to_owned());
                        }
                    }
                    self.reap_singleton(&cache).await;
                    drop(lock);
                }
                Ok(None) => {}
                Err(why) => {
                    tauri_plugin_log::log::debug!(
                        "a browser whose client may be gone could not be claimed by the sweep \
                         (event_name browser_orphan_sweep_refused, profile {}): {why}",
                        cache.display(),
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
/// The sentence a client without its engine answers, named rather than a
/// timeout: a shipped bundle missing its framework is a reinstall.
#[cfg(desktop)]
fn cef_unavailable() -> String {
    "the client is missing its Chromium framework (the app bundle was built without it) - \
     reinstall forge"
        .to_owned()
}

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
) -> Result<bool, String> {
    host.hide(profile.as_deref()).await
}

/// Take the client window full screen for the browser takeover, or give the
/// window back.
///
/// **The shell owns the window, so the page asks it rather than reaching for
/// the window API**: the takeover is the app's own shape, and the window's
/// full-screen state is what makes it a takeover rather than a bigger page.
///
/// **The simple full screen, not the native Space toggle.** The green
/// button's `toggleFullScreen:` was refused on this window (the app stays
/// windowed, measured 2026-10-19, even activated at level 0); the runtime's
/// simple full screen hides the title bar, dock and menu bar and sets the
/// window to the screen's frame, which is the takeover the person sees.
#[cfg(desktop)]
#[tauri::command]
pub async fn browser_takeover(app: tauri::AppHandle, on: bool) -> Result<(), String> {
    let Some(window) = app.get_webview_window("main") else {
        return Err("the client window is not there".to_owned());
    };
    let outcome = window.set_simple_fullscreen(on);
    tauri_plugin_log::log::info!(
        "the takeover changed the window's full screen (event_name browser_takeover_fullscreen, \
         on {on}, ok {})",
        outcome.is_ok(),
    );
    outcome.map_err(|why| format!("the window could not change full screen: {why}"))
}

/// Where the takeover's page area is, in CSS pixels from the viewport's
/// top-left, and the device pixel ratio it is drawn at. The surface is placed
/// and sized from this; it is reported on mount and on every resize.
#[cfg(desktop)]
#[tauri::command]
pub async fn browser_surface_rect(
    host: tauri::State<'_, Arc<BrowserHost>>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
) -> Result<(), String> {
    host.set_surface_rect(cef::surface::Rect { x, y, width, height, scale }).await;
    Ok(())
}

/// Whether a session has driven this client's browser, for the strip's mark.
#[tauri::command]
pub async fn browser_used(host: tauri::State<'_, Arc<BrowserHost>>) -> Result<bool, String> {
    host.used().await
}

/// Whether a browser has EVER run on this cache root: Chromium writes its
/// `Default` directory on the first launch, while a profile carrying only the
/// seat's record has never been opened.
#[cfg(desktop)]
fn cef_opened(cache: &Path) -> bool {
    cache.join("Default").is_dir()
}

/// The surface the page's takeover is drawing: one profile's frames, from the
/// host behind it. The host keeps running when the surface closes.
#[cfg(desktop)]
#[derive(Clone)]
pub struct OpenSurface {
    /// The name the person opened, `None` for the shared browser.
    pub profile: Option<String>,
    /// The host's cache root: one surface per profile at a time.
    pub dir: PathBuf,
    pub host: Arc<cef::spawn::ActiveHost>,
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

    /// **A stale held host must not deadlock the map that holds it.** The
    /// probe runs after the map's guard is released and the removal takes the
    /// lock afresh; the old order held the map across the probe and then
    /// waited on its own guard for ever - the live wedge's root.
    #[tokio::test]
    async fn a_stale_held_host_is_dropped_without_deadlocking() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let cache = paths.cef_shared();
        std::fs::create_dir_all(&cache).expect("the cache root");
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a port");
        let port = listener.local_addr().expect("the address").port();
        drop(listener);
        let ring =
            Arc::new(cef::osr::Ring::create("/forge-cef-deadlock-test", 8, 8).expect("a ring"));
        host.hosts.lock().await.insert(
            cache.clone(),
            Arc::new(cef::spawn::ActiveHost { pid: 999_999, port, ring, profile: cache.clone() }),
        );

        let dropped =
            tokio::time::timeout(std::time::Duration::from_secs(5), host.live_host(&cache)).await;
        assert!(dropped.is_ok(), "the probe and the removal must not wait on the map's own guard");
        assert!(dropped.expect("bounded").is_none(), "a dead host is no host");
        assert!(host.hosts.lock().await.get(&cache).is_none(), "and it was dropped");
        cef::osr::Ring::unlink("/forge-cef-deadlock-test");
    }

    /// **The strip reads the surface, not the host.** A profile whose host is
    /// up but whose frames are not in the takeover reads as no surface, and
    /// one whose takeover is drawing reads as up - the two states the button's
    /// show/hide turns on.
    #[tokio::test]
    async fn the_strip_reads_the_surface_not_the_host() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        assert!(
            !host.windowed(None).await.expect("the read"),
            "no surface is open before anything is shown",
        );
        assert!(
            !host.windowed(Some("hunt")).await.expect("the read"),
            "a named profile with no surface reads as not up",
        );
        // A host record alone - no surface - still reads as not drawn: the
        // record is what the boot sweep uses, never what the button reads.
        std::fs::create_dir_all(paths.cef_named("hunt")).expect("the cache root");
        std::fs::write(
            cef::spawn::record_path(&paths.cef_named("hunt")),
            br#"{"pid":999999,"port":9999,"ring":"/forge-cef-9999"}"#,
        )
        .expect("the record");
        assert!(!host.windowed(Some("hunt")).await.expect("the read"), "a record is not a surface");
    }

    /// A devtools endpoint that stalls the identity probe mid-request: the
    /// port, a signal that the request arrived, and the gate whose send lets
    /// the answers - identity first, then one page - complete.
    #[cfg(target_os = "macos")]
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
                // **The whole head, however the segments fall**: a single
                // peek can catch half a handshake on a loaded machine (measured
                // here 2026-10-17: the upgrade headers landed in a second
                // segment, the ws branch missed them, and the client got the
                // HTTP answer as garbage).
                let mut read;
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    read = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        socket.peek(&mut buffer),
                    )
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(0);
                    let head = String::from_utf8_lossy(&buffer[..read]);
                    if head.contains("\r\n\r\n") || tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                let request = String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase();
                if request.contains("upgrade: websocket") {
                    // The window predicate asks over the devtools websocket: a
                    // page, a session for it, and the window it lives in, all
                    // answered.
                    let Ok(mut ws) = tokio_tungstenite::accept_async(socket).await else {
                        continue;
                    };
                    use futures_util::{SinkExt as _, StreamExt as _};
                    use tokio_tungstenite::tungstenite::Message;
                    while let Some(Ok(frame)) = ws.next().await {
                        let Message::Text(text) = frame else { continue };
                        let Ok(call) = serde_json::from_str::<Value>(&text) else { continue };
                        let id = call.get("id").cloned().unwrap_or(Value::Null);
                        let method = call
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let result = match method.as_str() {
                            "Target.getTargets" => json!({
                                "targetInfos": [{ "targetId": "P1", "type": "page",
                                                  "url": "https://example.com/x" }],
                            }),
                            "Target.attachToTarget" => json!({ "sessionId": "SESSION1" }),
                            "Browser.getWindowForTarget" => json!({
                                "windowId": 7,
                                "bounds": { "left": 40, "top": 60 },
                            }),
                            _ => Value::Null,
                        };
                        let answer = json!({ "id": id, "result": result });
                        let _ = ws.send(Message::Text(answer.to_string().into())).await;
                    }
                    continue;
                }
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = if request.starts_with("GET /json/version") {
                    if let Some(reached) = reached.take() {
                        let _ = reached.send(());
                    }
                    if let Some(gate) = gate.take() {
                        let _ = gate.await;
                    }
                    format!(
                        "{{\"Browser\":\"Chrome/154.0\",\"webSocketDebuggerUrl\":\"ws://127.0.0.1:{port}{identity}\"}}"
                    )
                } else {
                    "[{\"id\":\"P1\",\"type\":\"page\",\"url\":\"https://example.com/x\"}]"
                        .to_owned()
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
        let refused = host.start().await.expect_err("the ensure says why");
        assert_eq!(
            refused, "the browser stack was never vendored",
            "the ensure says why rather than launching anything",
        );
    }

    /// **No browser launches at client start** (spam-control A4), driven the
    /// way the setup closure drives it: the boot's browser act runs under a
    /// mock Tauri app over a scratch stack, and nothing may come up. A launch
    /// re-added inside `boot_browser` fails here; only one added in the
    /// closure around the call is beyond a test that cannot start a real app.
    #[tokio::test]
    async fn the_boot_launches_nothing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let watched = paths.cef_shared();
        // The setup closure's own shape, driven directly: a registered setup
        // only runs on an event loop (the mock's `run` blocks until a window
        // closes), and the actions are what this pin is about.
        let setup = move |app: &mut tauri::App<tauri::test::MockRuntime>| {
            // No app handle: the pin is about what the boot launches, and a
            // surface cannot open with nothing shown.
            let host = BrowserHost::boot_browser(Ok(paths), None);
            app.manage(host);
            Ok::<(), Box<dyn std::error::Error>>(())
        };
        let mut app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("the mock app builds");
        setup(&mut app).expect("the boot's browser act runs");
        let host = app.state::<Arc<BrowserHost>>();
        assert!(
            host.idle_candidates().await.is_empty(),
            "nothing is a candidate before anything launches",
        );
        // A launch re-added as a spawn (the spawner's API is async) answers a
        // beat later, so the absence is believed over a bounded wait: a host
        // record shows within a second or so of a real start, and three is
        // generous - a real launch fails somewhere in here.
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < until {
            assert!(
                cef::spawn::read_record(&watched).is_none(),
                "the boot's browser act launches nothing",
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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
        let cache = profiles::cef_cache(&profile_dir);
        std::fs::create_dir_all(&cache).expect("the cache root");
        let launch = profiles::LaunchLock::acquire(&cache).await.expect("the launch lock");

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
        // A record naming a dead host: the close finds nothing to end and
        // must not wait on its own lock to discover it.
        let cache = profiles::cef_cache(&profile_dir);
        std::fs::create_dir_all(&cache).expect("the cache root");
        cef::spawn::write_record(
            &cache,
            &cef::spawn::Record { pid: 999_999, port: 9999, ring: "/forge-cef-9999".to_owned() },
        )
        .expect("the record");

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
        // A file where the shared host's cache root should be: the launch
        // cannot make its directory or take its lock, so nothing spawns, and
        // the failure is past the ensure.
        std::fs::write(paths.cef_shared(), b"").expect("a file where the shared cache goes");
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
            why.contains("cannot be made")
                || why.contains("cannot be opened")
                // A platform without the engine refuses by name before the
                // directory is touched; the ensure above still ran.
                || why.contains("packaged for macOS"),
            "the call failed for some other reason: {why}",
        );

        let auto = paths.profiles.join("forge-lead");
        assert_eq!(
            profiles::owner_of(&auto),
            Ok(Some(who)),
            "the seat's own name is recorded, its project qualifying its label",
        );
        assert!(
            cef::spawn::read_record(&profiles::cef_cache(&auto)).is_none(),
            "and nothing was driven through it - the host waits for a call that names it",
        );
    }

    /// **The idle release's clock resets on every touch, and the candidates
    /// name the seat to warn** - while a browser that is not there, and a
    /// surface that is up, are never candidates at all.
    ///
    /// macOS only: it stands a DevTools endpoint up and needs the real
    /// probe behind it, and a platform without the engine has no host to
    /// release.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn the_idle_clock_resets_and_dead_or_raised_browsers_are_no_candidates() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = Arc::new(BrowserHost::new(paths.clone()));
        let who = seat();
        let named = Named::open(who.clone(), "hunt", &paths).expect("the open");
        host.named.lock().await.insert("hunt".to_owned(), Arc::new(named));
        let hunt = paths.profiles.join("hunt");
        let cache = profiles::cef_cache(&hunt);
        std::fs::create_dir_all(&cache).expect("the cache root");
        host.touch(&cache).await;

        // No record: a host that is not there is not idle.
        assert!(
            host.idle_candidates().await.is_empty(),
            "a dead profile and the unlaunched shared browser are no candidates",
        );

        // A live host record: the candidate appears, and the clock carries
        // the seat.
        let identity = "/devtools/browser/idle";
        let (port, in_flight, release) = stalling_devtools(identity).await;
        cef::spawn::write_record(
            &cache,
            &cef::spawn::Record {
                pid: std::process::id(),
                port,
                ring: "/forge-cef-idle".to_owned(),
            },
        )
        .expect("the record");
        let first = tokio::spawn({
            let host = Arc::clone(&host);
            async move { host.idle_candidates().await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), in_flight)
            .await
            .expect("the probe reaches the endpoint")
            .expect("the endpoint signals the request");
        release.send(()).expect("the endpoint releases");
        let candidates = first.await.expect("the candidates");
        let row = candidates
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some("hunt"))
            .expect("the named row with a live host");
        // **The profile's creator, and the seat the warning goes to**: the
        // seat under test is both creator and caller - no other seat can
        // drive a named profile - so what this holds is the targeting, not a
        // discrimination against a last caller (see `idle_candidates`).
        assert_eq!(
            row.touch.caller,
            Some(who.clone()),
            "the candidate carries the profile's creator",
        );
        assert!(row.touch.warned.is_none(), "a fresh touch carries no warning");

        let warned_at = std::time::Instant::now();
        host.note_warned(&cache, warned_at).await;
        host.touch(&cache).await;
        let candidates = host.idle_candidates().await;
        let row = candidates
            .iter()
            .find(|candidate| candidate.name.as_deref() == Some("hunt"))
            .expect("the named row");
        assert!(row.touch.warned.is_none(), "a touch clears the warning");
        assert_eq!(row.touch.caller, Some(who.clone()), "and keeps the seat it had");

        // The surface over the live host: the takeover is drawing it, so it
        // is in use and never a candidate - it is the person's.
        let ring = Arc::new(
            cef::osr::Ring::create("/forge-cef-idle-surface", 64, 64).expect("a test ring"),
        );
        *host.surface.lock().await = Some(OpenSurface {
            profile: Some("hunt".to_owned()),
            dir: cache.clone(),
            host: Arc::new(cef::spawn::ActiveHost {
                pid: std::process::id(),
                port,
                ring: Arc::clone(&ring),
                profile: cache.clone(),
            }),
        });
        assert!(host.idle_candidates().await.is_empty(), "a drawn surface is in use, not idle",);
        cef::osr::Ring::unlink("/forge-cef-idle-surface");

        let shared = candidates.iter().find(|candidate| candidate.name.is_none());
        assert!(shared.is_none(), "the shared browser has not launched at all");
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

        // A host launch writes Chromium's own profile under the cache root.
        let default = profiles::cef_cache(&paths.profiles.join("hunt")).join("Default");
        std::fs::create_dir_all(&default).expect("the profile");
        let rows = host.profiles(None).await;
        let row = rows.iter().find(|row| row.name == "hunt").expect("the row");
        assert!(row.opened, "a launched Chromium leaves its own profile in the cache root");
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

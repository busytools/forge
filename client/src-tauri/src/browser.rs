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
    /// The vendored tree: node and the driver. The engine is the app's own
    /// Chromium, laid into the bundle rather than vendored here.
    pub stack: PathBuf,
    /// The pre-CEF browser's data directory, kept only as the migration's
    /// source: the profile's own `Default` is copied out of it once. New
    /// profiles live under `cef_shared`/`cef_named`.
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
    /// call that names it `@shared`. An omitted profile is the seat's own.
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
    /// Where the page last said its takeover's page area is, the scale it is
    /// drawn at, and which mount measured it.
    #[cfg(desktop)]
    surface_rect: Mutex<Option<ReportedRect>>,
    /// The refused reports of one open: the distinct rects seen and how many
    /// seen reports have repeated, so a page that can never place fails its
    /// open instead of being asked forever.
    #[cfg(desktop)]
    refusals: Mutex<Refusals>,
    /// The window's page area a test build stands in for, since a real
    /// `AppHandle` cannot be built in this crate's tests.
    #[cfg(all(desktop, any(test, feature = "testing")))]
    window_override: Mutex<Option<(f64, f64)>>,
    /// What the refusal path told the page, in a test or `testing` build: the
    /// events are the page's only door back, and an unemitted one is silence.
    #[cfg(all(desktop, any(test, feature = "testing")))]
    emitted: std::sync::Mutex<Vec<SurfaceEvent>>,
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
            refusals: Mutex::new(Refusals::default()),
            #[cfg(all(desktop, any(test, feature = "testing")))]
            window_override: Mutex::new(None),
            #[cfg(all(desktop, any(test, feature = "testing")))]
            emitted: std::sync::Mutex::new(Vec::new()),
            #[cfg(desktop)]
            attached: Mutex::new(None),
            #[cfg(desktop)]
            app: Mutex::new(None),
            #[cfg(desktop)]
            touches: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            boot: std::time::Instant::now(),
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
            refusals: Mutex::new(Refusals::default()),
            #[cfg(all(desktop, any(test, feature = "testing")))]
            window_override: Mutex::new(None),
            #[cfg(all(desktop, any(test, feature = "testing")))]
            emitted: std::sync::Mutex::new(Vec::new()),
            #[cfg(desktop)]
            attached: Mutex::new(None),
            #[cfg(desktop)]
            app: Mutex::new(None),
            #[cfg(desktop)]
            touches: Mutex::new(HashMap::new()),
            #[cfg(desktop)]
            boot: std::time::Instant::now(),
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
    /// No `profile` argument drives the asking seat's own profile. A name
    /// drives the named profile under it, opened on first use by the asking
    /// session; `@shared` drives the browser's own profile; a name another
    /// session already holds is refused with the owner's name.
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
            if name.is_empty() || name == profiles::SHARED_PROFILE {
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
        // **Which browser this call drives is decided here, not implied.**
        // An omitted profile is the calling seat's own; the shared browser is
        // reachable only by naming it `@shared` (see [`profile_target`]).
        #[cfg(desktop)]
        let target = profile_target(profile.as_deref(), seat);
        // **The shell's own entry, so a parked ask and a never-arrived one
        // stop looking alike.** Measured live 2026-10-07: a call parked with
        // no line like this on every inspected build, and nothing in the
        // client said whether the ask had reached the Rust side at all. This
        // is the host's own routing, not the session's work - the level is
        // INFO for the same reason "the browser is up on port" is. **It names
        // the resolved profile**, which is what a tab-leak report reads to
        // see which browser a call actually drove.
        #[cfg(desktop)]
        let entered = match &target {
            ProfileTarget::Shared => profiles::SHARED_PROFILE.to_owned(),
            ProfileTarget::Named(name) => name.clone(),
        };
        #[cfg(target_os = "android")]
        let entered = profile.clone().unwrap_or_else(|| profiles::SHARED_PROFILE.to_owned());
        tauri_plugin_log::log::info!(
            "a browser tool call reached the host (event_name browser_call_entered, seat {seat}, \
             tool {tool}, profile {entered})"
        );
        // A name that cannot be a profile is decided before anything else: no
        // directory, no lock and no browser is consulted to answer it. The
        // reserved shared name is resolved above and is not a profile.
        if let Some(name) = profile.as_deref()
            && name != profiles::SHARED_PROFILE
            && let Some(refusal) = profiles::name_refusal(name)
        {
            return Err(refusal);
        }
        let paths = self.paths.clone()?;
        #[cfg(desktop)]
        {
            let node = driver::node_path(&paths.stack);
            let cli = driver::cli_path(&paths.stack);
            match target {
                ProfileTarget::Shared => {
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
                    let active = self.start().await?;
                    // **The browser has been driven**, which the strip's row
                    // draws: a session's call is the only way a browser of this
                    // client's is used. Set after the profile is resolved, so a
                    // refused call never lights the mark.
                    self.used.store(true, std::sync::atomic::Ordering::Release);
                    // **The page is shown before it is driven.** A hidden page
                    // is the first-call-of-a-fire failure #2455 names; the
                    // host owns the only lever, and this is the cue.
                    active.show().await;
                    let endpoint = format!("http://127.0.0.1:{}", active.port);
                    let identity = active.identity();
                    let cache = paths.cef_shared();
                    tauri_plugin_log::log::info!(
                        "a browser call drives a profile's own host (event_name \
                         browser_call_host, seat {seat}, tool {tool}, profile {}, pid {}, port \
                         {}, cache {})",
                        profiles::SHARED_PROFILE,
                        active.pid,
                        active.port,
                        cache.display(),
                    );
                    let shared = self.shared_profile().await;
                    let start = DriverStart {
                        node: &node,
                        cli: &cli,
                        endpoint: &endpoint,
                        identity: &identity,
                        output: &paths.output,
                        profile: &cache,
                    };
                    match shared.call(&start, tool, args).await {
                        Ok(parts) => {
                            // **A call that changed the page rewrites the
                            // carry**, so a restart opens where the session
                            // left it and not where the last raise did.
                            self.refresh_carry(&cache).await;
                            Ok(parts)
                        }
                        Err(why) => {
                            let why = self.after_a_failure(&cache, &shared, &active, why).await;
                            Err(note_stat_missing(seat, None, tool, why))
                        }
                    }
                }
                ProfileTarget::Named(name) => {
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
                    // The same cue before a named profile's call: the page a
                    // fire drives is never left hidden by the gap since the
                    // last one.
                    active.show().await;
                    let endpoint = format!("http://127.0.0.1:{}", active.port);
                    let identity = active.identity();
                    tauri_plugin_log::log::info!(
                        "a browser call drives a profile's own host (event_name \
                         browser_call_host, seat {seat}, tool {tool}, profile {name}, pid {}, \
                         port {}, cache {})",
                        active.pid,
                        active.port,
                        cache.display(),
                    );
                    let start = DriverStart {
                        node: &node,
                        cli: &cli,
                        endpoint: &endpoint,
                        identity: &identity,
                        output: &paths.output,
                        profile: &cache,
                    };
                    match named.profile.call(&start, tool, args).await {
                        Ok(parts) => {
                            self.refresh_carry(&cache).await;
                            Ok(parts)
                        }
                        Err(why) => {
                            let why =
                                self.after_a_failure(&cache, &named.profile, &active, why).await;
                            Err(note_stat_missing(seat, Some(&name), tool, why))
                        }
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
            // The stat line is the same as the desktop's: the phone's
            // setInputFiles can lose the driver's view of a file too.
            shared
                .call(&start, tool, args)
                .await
                .map_err(|why| note_stat_missing(seat, None, tool, why))
        }
    }

    /// A call failed: is it the host that is gone, rather than the tool?
    ///
    /// A plain tool failure - no such element, a refused navigation - leaves
    /// the driver in place; only a host that stopped answering on the port
    /// this driver was built for is dropped, so the next call rebuilds
    /// against a relaunched host.
    ///
    /// **An attach that never finished is the HOST's failure.** Playwright
    /// creates its backend on the first tool call and waits 30 s for the
    /// browser's protocol handshake with no words from the host; measured
    /// 2026-10-23, a wedged page reported only `initializeServer: Timeout
    /// 30000ms exceeded` while the HTTP endpoint kept answering. That host
    /// is ended, so the next call starts a fresh one, and the answer carries
    /// the host's own identity and last words (#2534).
    #[cfg(desktop)]
    async fn after_a_failure(
        &self,
        cache: &Path,
        profile: &Profile,
        host: &cef::spawn::ActiveHost,
        why: String,
    ) -> String {
        if driver::attach_timed_out(&why) {
            tauri_plugin_log::log::warn!(
                "the driver could not attach to its browser host (event_name \
                 browser_host_attach_timeout, profile {}, pid {}, port {}): {why}",
                cache.display(),
                host.pid,
                host.port,
            );
            profile.drop_driver().await;
            let ended = self.end_host(cache).await;
            return attach_failure(host, ended);
        }
        if cef::spawn::probe(host.port).await {
            return why;
        }
        profile.drop_driver().await;
        self.hosts.lock().await.remove(cache);
        why
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
        let _launching = self.launch.lock().await;
        self.close_locked(name).await
    }

    /// [`BrowserHost::close`] with the launch mutex already held, so the
    /// idle sweep - which holds it across its verdict - can close without
    /// waiting on its own guard.
    #[cfg(desktop)]
    async fn close_locked(&self, name: &str) -> Result<(), String> {
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
        let _lock = profiles::LaunchLock::acquire(&cache).await.ok();
        self.end_host_locked(&cache).await;
        let mut named = self.named.lock().await;
        named.remove(name);
        Ok(())
    }

    /// End the host on one cache root: the process, its record and its ring.
    /// A `surface` drawing it closes first, so no frame outlives the host.
    #[cfg(desktop)]
    async fn end_host(&self, cache: &Path) -> bool {
        let _launching = self.launch.lock().await;
        let _lock = profiles::LaunchLock::acquire(cache).await.ok();
        self.end_host_locked(cache).await
    }

    /// [`BrowserHost::end_host`] with the launch mutex and the directory's
    /// flock already held, so a close cannot remove a record a launch is
    /// about to write - and so a delete, which holds both, does not take its
    /// own lock again.
    #[cfg(desktop)]
    async fn end_host_locked(&self, cache: &Path) -> bool {
        let mut ended = true;
        {
            let mut surface = self.surface.lock().await;
            let lowered = surface.as_ref().is_some_and(|open| open.dir == cache);
            if lowered {
                *surface = None;
            }
            drop(surface);
            // **An ended host takes its open's geometry with it**, so the next
            // open cannot be refused a rect measured for this one.
            if lowered {
                *self.surface_rect.lock().await = None;
                *self.refusals.lock().await = Refusals::default();
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
            // **The ring goes only with a host this client may end.** A port
            // that answers while the pid is not this app's `--cef-host` is
            // another process's; its ring is left alone.
            if cef::spawn::end_verified(record.pid, record.port, cache).await {
                cef::osr::Ring::unlink(&record.ring);
                cef::spawn::remove_record(cache);
            } else {
                ended = false;
                tauri_plugin_log::log::warn!(
                    "a browser record was left alone: its port answers but its pid is not this \
                     app's host (event_name cef_end_refused, profile {}, pid {})",
                    cache.display(),
                    record.pid,
                );
            }
        }
        self.reap_singleton(cache).await;
        // A lock the host's own directory held is released with it; the
        // client's claim stays until this client exits or deletes.
        let _ = self.reconcile_surface().await;
        ended
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
            && cef::spawn::is_our_host(pid, cache, None)
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
    ///
    /// **The caller holds the launch mutex** (the idle sweep's
    /// `close_guarded` does), so this uses the locked paths: taking it again
    /// here would wait on its own future for ever.
    #[cfg(desktop)]
    pub(crate) async fn close_idle(&self, name: Option<&str>) {
        match name {
            Some(name) => {
                let dir = {
                    let named = self.named.lock().await;
                    named.get(name).map(|entry| profiles::cef_cache(&entry.dir))
                };
                let _ = self.close_locked(name).await;
                if let Some(cache) = dir {
                    self.touches.lock().await.remove(&cache);
                }
            }
            None => {
                if let Ok(paths) = self.paths.clone() {
                    let cache = paths.cef_shared();
                    let _lock = profiles::LaunchLock::acquire(&cache).await.ok();
                    self.end_host_locked(&cache).await;
                    self.touches.lock().await.remove(&cache);
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
                        // **An adopted host keeps serving when its veto
                        // cannot be armed**: its page is already open, and
                        // the failure is named rather than taking the browser
                        // away. It is tried twice, because a host that was
                        // still starting when the client adopted it is the
                        // one case a later use cannot recover from itself.
                        #[cfg(target_os = "macos")]
                        for attempt in 0..2 {
                            match cef::veto::install(record.port, cache, None).await {
                                Ok(_) => break,
                                Err(failure) => tauri_plugin_log::log::warn!(
                                    "the passkey veto could not be armed on an adopted host \
                                     (event_name cef_passkey_veto_failed, profile {}, attempt \
                                     {attempt}): {failure}",
                                    cache.display(),
                                ),
                            }
                        }
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
                        if !cef::spawn::end_verified(record.pid, record.port, cache).await {
                            return Err(format!(
                                "the browser on {} answers but is not this app's host; left alone",
                                cache.display()
                            ));
                        }
                        // **The ring goes only with a host this client ended.**
                        // macOS does not release a dead host's segment, and a
                        // respawn takes a fresh port, so this name would leak.
                        cef::osr::Ring::unlink(&record.ring);
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
                // **A silent record proves the port, not the process.** A
                // frozen host can hold the profile while its record looks
                // stale; the singleton lock names it, and reaping by identity
                // ends it rather than double-spawning onto its profile.
                self.reap_singleton(cache).await;
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
        #[cfg(target_os = "macos")]
        let mut veto_retried = false;
        #[cfg(target_os = "macos")]
        let mut veto_why: Option<String> = None;
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
                    // **A host that cannot pause its targets is replaced
                    // once.** Nothing drives a profile whose pages cannot be
                    // vetoed; a second failure ends the launch rather than
                    // serving one unvetoed.
                    #[cfg(target_os = "macos")]
                    let carried = cef::host::carried_page(cache);
                    #[cfg(target_os = "macos")]
                    let carried = (carried != "about:blank").then_some(carried);
                    #[cfg(target_os = "macos")]
                    let carried_url = carried.clone();
                    #[cfg(target_os = "macos")]
                    match cef::veto::install(port, cache, carried).await {
                        Ok(cef::veto::Carried::Failed(why)) => {
                            // **A carried page that did not open is named and
                            // held.** The marker and the hold both stay, so
                            // the next LAUNCH opens it and a client restart
                            // cannot erase it. Nothing retries it from the
                            // Open path: a probe there cannot see a commit.
                            tauri_plugin_log::log::warn!(
                                "the carried page did not open (event_name \
                                 cef_carried_page_failed, profile {}): {why}",
                                cache.display(),
                            );
                            if let Some(url) = &carried_url {
                                hold_carry(cache, url);
                            }
                        }
                        Ok(cef::veto::Carried::Opened) => {
                            // **Accepted is not landed.** The hold waits for a
                            // raise to read a non-blank page.
                            if let Some(url) = &carried_url {
                                hold_carry(cache, url);
                            }
                        }
                        Ok(cef::veto::Carried::None) => {}
                        Err(failure) => {
                            cef::spawn::end(pid).await;
                            cef::osr::Ring::unlink(&ring_name);
                            cef::spawn::remove_record(cache);
                            if veto_retried {
                                tauri_plugin_log::log::warn!(
                                    "the passkey veto failed twice; the host is not served \
                                     (event_name cef_passkey_veto_failed, profile {}, port \
                                     {port}): {failure}",
                                    cache.display(),
                                );
                                return Err(format!(
                                    "the browser host could not be vetoed: {failure}"
                                ));
                            }
                            tauri_plugin_log::log::warn!(
                                "the passkey veto failed; the host is torn down and relaunched \
                                 once (event_name cef_passkey_veto_failed, profile {}, port \
                                 {port}): {failure}",
                                cache.display(),
                            );
                            veto_why = Some(failure.to_string());
                            last = format!("the browser host could not be vetoed: {failure}");
                            veto_retried = true;
                            continue;
                        }
                    }
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
            // **The veto's own reason survives a later attempt's failure**:
            // a relaunch that died at the endpoint must not read as if the
            // veto had never been in question.
            #[cfg(target_os = "macos")]
            if let Some(veto) = veto_why
                && !last.contains(&veto)
            {
                return Err(format!("{last}; {veto}"));
            }
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
            Some(name) => cache_of(&paths, name)?,
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
    /// **A session's hand-off with no profile raises that seat's own
    /// profile**, exactly as its calls drive it; the shared browser is raised
    /// for the person's own show (no seat) or by naming it `@shared`.
    /// **A hand-off raises only the profile of the seat that asked.** The
    /// dock passes the asking seat down, and another seat's name is refused
    /// where the name resolves to a browser; the person's own show carries no
    /// seat and reaches any of them.
    ///
    /// Answers the profile it actually raised, so the page's Done lowers the
    /// browser that came up rather than a name it guessed.
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
        epoch: u64,
    ) -> Result<String, String> {
        let paths = self.paths.clone()?;
        let wanted = profile.map(str::to_owned).or_else(|| seat.map(auto_name));
        let (name, cache) = match wanted.as_deref() {
            None => (None, paths.cef_shared()),
            Some(profiles::SHARED_PROFILE) => {
                (Some(profiles::SHARED_PROFILE.to_owned()), paths.cef_shared())
            }
            Some(name) => {
                if let Some(refusal) = profiles::name_refusal(name) {
                    return Err(refusal);
                }
                if let Some(seat) = seat {
                    profiles::raise_refusal(name, &paths.profiles.join(name), seat)?;
                }
                (Some(name.to_owned()), profiles::cef_cache(&paths.profiles.join(name)))
            }
        };
        tauri_plugin_log::log::info!(
            "the browser surface is asked to open (event_name browser_show_entered, profile \
             {name:?}, cache {})",
            cache.display(),
        );
        // **The answer is the profile actually raised**, read before the
        // surface takes its name: a session's hand-off with no profile
        // resolves to the seat's own, and the page lowers this name on Done.
        let raised = name.clone().unwrap_or_else(|| profiles::SHARED_PROFILE.to_owned());
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
            // **The raised page is shown too, inside the claim.** A takeover
            // opened onto a page that went hidden while nothing drove it
            // would be a frozen picture under the bar; the cue waits under
            // the same lock, so the idle sweep cannot take the free mutex
            // during its ack window and end the host under the claim.
            host.show().await;
            // One surface at a time: opening this profile's takes the window,
            // and a previous profile's host keeps running under it.
            *self.surface.lock().await = Some(OpenSurface {
                profile: name,
                dir: cache.clone(),
                host: Arc::clone(&host),
                epoch,
            });
            // **A placement that never landed fails the Open**, so the page
            // draws the sentence instead of a live takeover with no picture.
            self.reconcile_surface().await?;
            Ok::<(), String>(())
        })
        .await;
        match claimed {
            Ok(answer) => answer?,
            Err(_) => return Err("the browser did not start in time - try Open again".to_owned()),
        }
        // **Every open asks the bar's arrows again.** The page resets them
        // before the surface comes up, and an already-attached host would
        // otherwise answer with a deduped silence until its history moved.
        cef::surface::refresh_history();
        // **The page this raise shows is the page a restart reopens.** A
        // held carry releases only to a real page read; the retry for a
        // carry that never opened belongs to the next LAUNCH, which is the
        // one place the navigation is cheap and honest to repeat.
        if let Some(url) = cef::spawn::page_url(host.port).await {
            self.record_carry(&cache, &url).await;
        }
        Ok(raised)
    }

    /// The raise's and the browsing call's write: a non-web read on a held
    /// profile keeps both the marker and the hold; anything else writes the
    /// marker and lifts the hold. **No retry here** - a retry belongs to the
    /// next launch, which opens the held page.
    #[cfg(desktop)]
    async fn record_carry(&self, cache: &Path, url: &str) {
        let _launching = self.launch.lock().await;
        // **A held carry waits for a REAL page.** The engine's own error page
        // (`chrome-error://chromewebdata/`) is what a failed launch leaves,
        // and releasing on it would erase the page the hold exists for - so
        // every non-web read keeps the hold.
        if held_carry(cache).is_some() && !is_web_page(url) {
            return;
        }
        // **The marker is written before the hold lifts**: a hold beside an
        // old marker is a page the next launch retries; a lifted hold beside
        // a failed write would leave nothing.
        write_carried_page(cache, Some(url));
        release_carry(cache);
    }

    /// Refresh the carried page from the host's own target.
    ///
    /// **The port is resolved and the marker written under the launch lock;
    /// the endpoint probe between them runs unlocked.** The probe has a 2 s
    /// bound and must not hold every launch, close and delete for that long.
    /// The cost is that a probe taken before a raise can land after the
    /// raise's own write - last writer wins, and both are pages the person
    /// was shown. One page target is the whole truth here: the CEF host
    /// serves one page, unlike the Brave-era tab set.
    #[cfg(desktop)]
    pub async fn refresh_carry(&self, cache: &Path) {
        // **The probe is outside the lock; only the write is inside.** The
        // endpoint read has a 2 s bound, and holding the launch mutex across
        // it would make a silent host hold every launch, close and delete for
        // that long. The temp-file rename keeps the write itself safe, and
        // the short lock keeps raises and calls from interleaving it.
        let port = {
            let _launching = self.launch.lock().await;
            let held = self.hosts.lock().await.get(cache).map(|host| host.port);
            match held {
                Some(port) => port,
                None => match cef::spawn::read_record(cache) {
                    Some(record) => record.port,
                    None => return,
                },
            }
        };
        // **A failed probe keeps the carry.** Only an answer - even a blank
        // or non-web one - may rewrite or clear it; a transient /json/list
        // fault must not wipe the page the next launch would open.
        if let Some(url) = cef::spawn::page_url(port).await {
            self.record_carry(cache, &url).await;
        }
    }

    /// The page said where its takeover's page area is: place the overlay
    /// there, and resize the host's view to match.
    #[cfg(desktop)]
    pub async fn set_surface_rect(&self, rect: cef::surface::Rect, epoch: u64) {
        let open = self.surface.lock().await.as_ref().map(|open| open.profile.clone());
        let attached = *self.attached.lock().await;
        tauri_plugin_log::log::debug!(
            "the page reported the surface rect (event_name browser_surface_rect_reported, epoch \
             {epoch}, rect {:.0}x{:.0} at {:.0},{:.0}, scale {}, open {open:?}, attached \
             {attached:?})",
            rect.width,
            rect.height,
            rect.x,
            rect.y,
            rect.scale,
        );
        *self.surface_rect.lock().await = Some(ReportedRect { rect, epoch });
        let _ = self.reconcile_surface().await;
    }

    /// The main window's page area in CSS pixels, when it is there. A test or
    /// `testing` build can pin one: a real `AppHandle` cannot be built in this
    /// crate's tests, and the geometry check is one of the things that must
    /// still run there.
    #[cfg(desktop)]
    async fn window_size(&self) -> Option<(f64, f64)> {
        #[cfg(any(test, feature = "testing"))]
        if let Some(size) = *self.window_override.lock().await {
            return Some(size);
        }
        let app = self.app.lock().await.clone();
        app.as_ref().and_then(app_window_size)
    }

    /// Tell the page something. **A test or `testing` build records it**,
    /// because a real `AppHandle` cannot be built in this crate's tests and
    /// the two events the refusal path sends are the page's only door back.
    #[cfg(desktop)]
    fn surface_event(&self, app: Option<&tauri::AppHandle>, event: SurfaceEvent) {
        #[cfg(any(test, feature = "testing"))]
        if let Ok(mut emitted) = self.emitted.lock() {
            emitted.push(event.clone());
        }
        let Some(app) = app else { return };
        match event {
            SurfaceEvent::Remeasure => {
                let _ = app.emit("browser_surface_remeasure", ());
            }
            SurfaceEvent::Failed => {
                let _ = app.emit("browser_surface_failed", FAILED_SENTENCE.to_owned());
            }
        }
    }

    /// Make the native half match the open surface: one overlay, drawing the
    /// open profile's host, at the page's last reported rect. Idempotent -
    /// every report calls it. **Answers the placement failure**, so an Open
    /// whose overlay never landed says why instead of returning success.
    #[cfg(desktop)]
    async fn reconcile_surface(&self) -> Result<(), String> {
        let open = self.surface.lock().await.clone();
        let reported = *self.surface_rect.lock().await;
        let app = self.app.lock().await.clone();
        let mut attached = self.attached.lock().await;
        match (open, reported) {
            (Some(open), Some(reported)) => {
                let window = self.window_size().await;
                // **A report that is not this open's, or not this window's page
                // area, is never applied.** The area is always the window under
                // the bar, and a measurement that is not (a transient viewport,
                // a surface that is gone) would shrink the live page to a band
                // and nothing re-reports it afterwards. A refusal leaves the
                // stored rect alone - clearing it would race the report that
                // supersedes it - and asks the page to measure again on its
                // next frame.
                if !report_accepted(&reported, open.epoch, window) {
                    let why = if reported.epoch != open.epoch {
                        "it was measured for another open"
                    } else {
                        "it is not the window's page area"
                    };
                    // **Every refusal asks the page to measure again, until a
                    // bound.** A transient that moves the box without resizing
                    // it fires no ResizeObserver, so a refused lone report
                    // would otherwise be the last one; and a rect that keeps
                    // coming back unchanged is a settled wrong layout, which
                    // must reach the sentence rather than be asked forever.
                    let mut seen = self.refusals.lock().await;
                    let action = refusal(&mut seen, open.epoch, reported.rect);
                    drop(seen);
                    tauri_plugin_log::log::info!(
                        "a reported surface rect is dropped because {why} (event_name \
                         browser_surface_rect_refused, epoch {}, open_epoch {}, profile {:?}, \
                         rect {:.0}x{:.0} at {:.0},{:.0}, scale {}, window {window:?})",
                        reported.epoch,
                        open.epoch,
                        open.profile,
                        reported.rect.width,
                        reported.rect.height,
                        reported.rect.x,
                        reported.rect.y,
                        reported.rect.scale,
                    );
                    if action == Refusal::Fail {
                        // **A page area that can never be placed is said.** The
                        // open fails the way a surface that could not start
                        // does, so the bar keeps its try again and the hand-off
                        // stays parked.
                        *self.surface.lock().await = None;
                        *self.surface_rect.lock().await = None;
                        *self.refusals.lock().await = Refusals::default();
                        self.surface_event(app.as_ref(), SurfaceEvent::Failed);
                    } else {
                        self.surface_event(app.as_ref(), SurfaceEvent::Remeasure);
                    }
                    return Ok(());
                }
                // This open placed: its refusals are behind it.
                *self.refusals.lock().await = Refusals::default();
                let ReportedRect { rect, epoch } = reported;
                let Some(app) = app.as_ref() else { return Ok(()) };
                if *attached == Some(open.host.port) {
                    tauri_plugin_log::log::debug!(
                        "the surface rect is applied to the attached surface (event_name \
                         cef_surface_rect_applied, epoch {epoch}, profile {:?}, rect \
                         {:.0}x{:.0} at {:.0},{:.0}, scale {}, window {:?})",
                        open.profile,
                        rect.width,
                        rect.height,
                        rect.x,
                        rect.y,
                        rect.scale,
                        window,
                    );
                    cef::surface::set_rect(app, &open.host, rect);
                } else {
                    if attached.take().is_some() {
                        cef::surface::detach(app);
                    }
                    tauri_plugin_log::log::debug!(
                        "the surface is attached at the reported rect (event_name \
                         cef_surface_attach_requested, epoch {epoch}, profile {:?}, rect \
                         {:.0}x{:.0} at {:.0},{:.0}, scale {}, window {:?})",
                        open.profile,
                        rect.width,
                        rect.height,
                        rect.x,
                        rect.y,
                        rect.scale,
                        window,
                    );
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
                            *self.surface_rect.lock().await = None;
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
    pub async fn hide(&self, profile: Option<&str>, seat: Option<&Seat>) -> Result<bool, String> {
        let paths = self.paths.clone()?;
        // **The same resolution show makes.** A hand-off that named no
        // profile was raised as the asking seat's own, so its Done lowers
        // that browser and not the shared one. The refusal is show's own: a
        // name that cannot be a profile resolves to no directory at all, and
        // without this a `profile: "../user-data"` would close the SHARED
        // surface while the answer claimed the named one came down.
        let wanted = profile.map(str::to_owned).or_else(|| seat.map(auto_name));
        let cache = match wanted.as_deref() {
            None => paths.cef_shared(),
            Some(name) => cache_of(&paths, name)?,
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
        let lowered = self.lower_surface(&cache).await;
        let _ = self.reconcile_surface().await;
        Ok(lowered)
    }

    /// Take the open surface down when the cache is its own, and forget where
    /// its page said it was. **The next open starts with no geometry**: a rect
    /// measured for the surface just lowered is not this window's page area
    /// for the next one, and placing at it is the stale attach (#2470).
    #[cfg(desktop)]
    async fn lower_surface(&self, cache: &Path) -> bool {
        let mut surface = self.surface.lock().await;
        // **Whether this call actually lowered the page the person sees.**
        // A Done for another profile leaves that profile's surface up, and
        // the page's store has to keep drawing its bar over it.
        let lowered = !surface.as_ref().is_some_and(|open| open.dir != cache);
        if lowered {
            *surface = None;
        }
        drop(surface);
        if lowered {
            *self.surface_rect.lock().await = None;
            *self.refusals.lock().await = Refusals::default();
        }
        lowered
    }

    /// The phone's Done / Not now / the bar's back: lower the takeover. The
    /// WebView stays exactly where the session left it, which is what the
    /// next Open shows again.
    #[cfg(target_os = "android")]
    pub async fn hide(&self, profile: Option<&str>, _seat: Option<&Seat>) -> Result<bool, String> {
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
                        if cef::spawn::end_verified(record.pid, record.port, &cache).await {
                            cef::osr::Ring::unlink(&record.ring);
                            cef::spawn::remove_record(&cache);
                            if let Some(name) = cache.file_name().and_then(std::ffi::OsStr::to_str)
                            {
                                reaped.push(name.to_owned());
                            }
                        } else {
                            tauri_plugin_log::log::warn!(
                                "a browser record was left alone by the sweep: its port answers \
                                 but its pid is not this app's host (event_name \
                                 cef_end_refused, profile {}, pid {})",
                                cache.display(),
                                record.pid,
                            );
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

/// The sentence a client without its engine answers, named rather than a
/// timeout: a shipped bundle missing its framework is a reinstall.
#[cfg(target_os = "macos")]
fn cef_unavailable() -> String {
    "the client is missing its Chromium framework (the app bundle was built without it) - \
     reinstall forge"
        .to_owned()
}

/// On a desktop that never packages the engine, the sentence says that
/// instead of asking for a reinstall that would change nothing.
#[cfg(all(desktop, not(target_os = "macos")))]
fn cef_unavailable() -> String {
    cef::REFUSAL.to_owned()
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
/// **The fold is lossy, and a collision is a refusal, never a share.** The
/// project name may carry a space or reach past the length cap, so every
/// character outside the charset folds to a dash and the result is capped at
/// the name bound: "my app" and "my-app" both derive `my-app-lead`, and two
/// long projects can meet after the cap. The first seat to reach the name
/// holds it; the second seat's unnamed calls are refused with the owner's
/// name - loud, and never the shared browser. A hand-made profile that took
/// the name first is the same refusal.
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

/// Which browser a tool call drives.
#[cfg(desktop)]
#[derive(Debug, PartialEq, Eq)]
enum ProfileTarget {
    /// The browser's own profile, reachable only by naming it `@shared`.
    Shared,
    /// A named profile, owned by the seat that opened it.
    Named(String),
}

/// **An omitted profile is the calling seat's own, never the shared
/// browser.** The shared browser's tabs are whatever the person or another
/// seat left there - measured 2026-10-23: a call that omitted `profile`
/// returned the person's HiringCafe tab - so the shared browser is reachable
/// only by naming it. Pure, so the routing is pinned without a browser.
#[cfg(desktop)]
fn profile_target(profile: Option<&str>, seat: &Seat) -> ProfileTarget {
    match profile {
        Some(profiles::SHARED_PROFILE) => ProfileTarget::Shared,
        Some(name) => ProfileTarget::Named(name.to_owned()),
        None => ProfileTarget::Named(auto_name(seat)),
    }
}

/// The cache root an explicitly named browser resolves to: the reserved
/// `@shared` is the browser's own profile, any other name is that profile's
/// directory, and a name that cannot be a profile is refused with the rule
/// rather than reaching for a directory it names.
#[cfg(desktop)]
fn cache_of(paths: &StackPaths, name: &str) -> Result<PathBuf, String> {
    if name == profiles::SHARED_PROFILE {
        return Ok(paths.cef_shared());
    }
    if let Some(refusal) = profiles::name_refusal(name) {
        return Err(refusal);
    }
    Ok(paths.cef_named(name))
}

/// Write the page a restart should reopen beside the profile.
///
/// **A temp file and a rename**: a crash mid-write leaves the previous page
/// rather than a truncated one. A blank or non-web page clears the marker -
/// the file always means "the page the person was actually shown", never a
/// leftover - and only a web address is ever carried.
#[cfg(desktop)]
fn write_carried_page(cache: &Path, url: Option<&str>) {
    let path = cache.join("page");
    let Some(url) = url.filter(|url| url.starts_with("http://") || url.starts_with("https://"))
    else {
        let _ = std::fs::remove_file(&path);
        return;
    };
    let temp = cache.join("page.tmp");
    if let Err(why) = std::fs::write(&temp, url).and_then(|()| std::fs::rename(&temp, &path)) {
        // **Forge's own storage problem, not the session's**: the carry is
        // this client's bookkeeping, and a write that fails is worth a
        // warning that names where and why.
        tauri_plugin_log::log::warn!(
            "the carried page could not be written (event_name cef_page_carry_failed, profile \
             {}, {why})",
            cache.display(),
        );
        let _ = std::fs::remove_file(&temp);
    }
}

/// The carried URL whose navigation has not been seen to land.
///
/// **On disk, not in memory**: a client restart adopts the host, probes the
/// blank page it left, and would otherwise erase the marker - the one case
/// the marker exists for. A non-blank page read releases it.
#[cfg(desktop)]
fn carry_hold_path(cache: &Path) -> std::path::PathBuf {
    cache.join("page-unopened")
}

/// Written the way the marker is: temp file, then rename, so a reader never
/// sees a half-written hold.
#[cfg(target_os = "macos")]
fn hold_carry(cache: &Path, url: &str) {
    let temp = cache.join("page-unopened.tmp");
    if let Err(why) =
        std::fs::write(&temp, url).and_then(|()| std::fs::rename(&temp, carry_hold_path(cache)))
    {
        tauri_plugin_log::log::warn!(
            "the carry hold could not be written (event_name cef_page_hold_failed, profile {}, \
             {why})",
            cache.display(),
        );
        let _ = std::fs::remove_file(&temp);
    }
}

#[cfg(desktop)]
fn held_carry(cache: &Path) -> Option<String> {
    std::fs::read_to_string(carry_hold_path(cache))
        .ok()
        .map(|url| url.trim().to_owned())
        .filter(|url| is_web_page(url))
}

/// Whether a read is a page the next launch could reopen: an origin the
/// engine loads, not its blank state or its own error page.
#[cfg(desktop)]
fn is_web_page(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

#[cfg(desktop)]
fn release_carry(cache: &Path) {
    let _ = std::fs::remove_file(carry_hold_path(cache));
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

/// Log a driver stat failure with the seat and profile that made the call,
/// and hand the reason back untouched - the note itself is appended where the
/// call is routed. The seat lives here, not in the profile, which is why the
/// line is built at this layer.
///
/// **`info!` for a session's tool failure is a rule-20 exception**: the client
/// runs at the Info level, so a lower level would drop the line, and this is
/// the line a recurrence of the measured transient is diagnosed by.
fn note_stat_missing(seat: &Seat, profile: Option<&str>, tool: &str, why: String) -> String {
    if let Some(line) = profiles::stat_missing_line(seat, profile, tool, &why) {
        tauri_plugin_log::log::info!("{line}");
    }
    why
}

/// The answer for a driver that never attached.
///
/// **A bare `initializeServer` timeout names nothing**: not the browser, not
/// the profile, not why. The host's pid, port and its own log's last words
/// are what a recurrence is diagnosed by, and the outcome says what actually
/// happened to the host so the next call is understood: a fresh browser when
/// it was ended, or a host left alone because the record was not this app's
/// to end (#2534).
#[cfg(desktop)]
fn attach_failure(host: &cef::spawn::ActiveHost, ended: bool) -> String {
    let tail = cef::spawn::log_tail(&host.profile);
    let words = if tail.is_empty() {
        "its log is empty".to_owned()
    } else {
        format!("its log ends: {tail}")
    };
    let outcome = if ended {
        "The host was ended through its SIGTERM path, which closes the browser and lets CEF \
         flush the profile, so the next call starts a fresh one with the data intact."
    } else {
        "The host could not be ended: its endpoint answers while its pid is not this app's host, \
         so it was left alone."
    };
    format!(
        "the browser host (pid {}, port {}, profile {}) answered its DevTools endpoint but the \
         driver could not attach within its 30 s bound: the host's page did not answer. {outcome} \
         {words}",
        host.pid,
        host.port,
        host.profile.display(),
    )
}

#[cfg(test)]
mod stat_note_tests {
    use super::*;
    use std::sync::Mutex;

    /// A logger that keeps the lines the note's site writes. **The emission
    /// itself is the other half of the cut**: a helper that builds the line
    /// and drops it leaves this capture empty.
    struct Capture;

    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    impl log::Log for Capture {
        fn enabled(&self, _metadata: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            if let Ok(mut lines) = LINES.lock() {
                lines.push(record.args().to_string());
            }
        }
        fn flush(&self) {}
    }

    fn seat() -> Seat {
        Seat { org: "Busytools".to_owned(), project: "forge".to_owned(), label: "lead".to_owned() }
    }

    /// **The call's own reason is what the arm returns, and the line is
    /// emitted.** The note is appended where the call is routed, so this
    /// helper must hand the reason back untouched; the `info!` is the half a
    /// recurrence reads, so it is captured here rather than assumed.
    #[test]
    fn the_stat_line_is_logged_and_the_reason_returns_untouched() {
        let installed = log::set_logger(&Capture);
        assert!(installed.is_ok(), "the capture logger installs: {installed:?}");
        // **`set_logger` does not set the filter** - a fresh process starts
        // at Off, and a logger alone would never see the record.
        log::set_max_level(log::LevelFilter::Trace);
        if let Ok(mut lines) = LINES.lock() {
            lines.clear();
        }
        if let Ok(mut lines) = LINES.lock() {
            lines.clear();
        }
        let why = "Error: ENOENT: no such file or directory, stat '/tmp/cv.pdf'".to_owned();
        let returned = note_stat_missing(&seat(), None, "browser_run_code_unsafe", why.clone());
        assert_eq!(returned, why, "the reason is the arm's answer");
        let lines = LINES.lock().expect("the capture is not poisoned").clone();
        assert!(
            lines.iter().any(|line| line.contains("event_name cef_driver_stat_missing")),
            "the stat line is emitted: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("seat Busytools/forge/lead")
                && line.contains("tool browser_run_code_unsafe")
                && line.contains("path /tmp/cv.pdf")),
            "the line carries the seat, tool and path: {lines:?}"
        );

        if let Ok(mut lines) = LINES.lock() {
            lines.clear();
        }
        let plain = "TypeError: page is not defined".to_owned();
        assert_eq!(
            note_stat_missing(&seat(), None, "browser_click", plain.clone()),
            plain,
            "a failure of another class returns untouched"
        );
        let lines = LINES.lock().expect("the capture is not poisoned").clone();
        assert!(lines.is_empty(), "and emits nothing: {lines:?}");
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
    epoch: u64,
) -> Result<String, String> {
    #[cfg(desktop)]
    {
        // **The profile it actually raised**, so the page's Done lowers that
        // very browser: a session's hand-off with no profile raises the
        // seat's own, and the page never has to guess the name.
        host.show(profile.as_deref(), seat.as_ref(), epoch).await
    }
    #[cfg(not(desktop))]
    {
        // The phone's takeover is the native one and its page reports no rect.
        let _ = epoch;
        host.show(profile.as_deref(), seat.as_ref()).await?;
        Ok(profiles::SHARED_PROFILE.to_owned())
    }
}

/// Take the hand-off's window back out of sight; the browser keeps running
/// over the same directory, and the next agent call drives it.
#[tauri::command]
pub async fn browser_hide(
    host: tauri::State<'_, Arc<BrowserHost>>,
    profile: Option<String>,
    seat: Option<Seat>,
) -> Result<bool, String> {
    host.hide(profile.as_deref(), seat.as_ref()).await
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

/// Which way the takeover's arrows move through the page's own history.
#[cfg(desktop)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavDirection {
    Back,
    Forward,
}

/// The takeover bar's arrows: move the page to the neighbouring entry in its
/// history - the same move Cmd+[ and Cmd+] make through the surface.
#[cfg(desktop)]
#[tauri::command]
pub async fn browser_nav(direction: NavDirection) -> Result<(), String> {
    cef::surface::navigate(direction)
}

/// Where the takeover's page area is, in CSS pixels from the viewport's
/// top-left, the device pixel ratio it is drawn at, and the open it belongs
/// to. The surface is placed and sized from this; the page reports it on
/// mount, on every open, on every resize, and when the shell asks for a
/// remeasure after refusing one.
#[cfg(desktop)]
#[tauri::command]
pub async fn browser_surface_rect(
    host: tauri::State<'_, Arc<BrowserHost>>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    epoch: u64,
) -> Result<(), String> {
    host.set_surface_rect(cef::surface::Rect { x, y, width, height, scale }, epoch).await;
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

/// The page's last reported place for its takeover's page area, and the epoch
/// of the mount that measured it.
#[cfg(desktop)]
#[derive(Clone, Copy)]
struct ReportedRect {
    rect: cef::surface::Rect,
    epoch: u64,
}

/// How far a reported page area may sit from the window it claims to fill, in
/// CSS pixels: fractional window sizes and the page's own rounding.
#[cfg(desktop)]
const RECT_SLACK: f64 = 4.0;

/// The window's own chrome between its inner size and the page's viewport: a
/// title bar insets the webview by its height (32 points on macOS). That is
/// furniture, not layout - a page area short by it is still the window under
/// the bar, where a band is hundreds of points short.
#[cfg(desktop)]
const WINDOW_CHROME_SLACK: f64 = 48.0;

/// One open's refused reports: the distinct rects seen and how many refused
/// reports have repeated an already-seen rect, reset by a fresh one. A
/// transition produces a handful of distinct rects before its settled report;
/// a layout that settles wrong keeps reporting rects already seen, and it
/// fails the open rather than asking forever.
#[cfg(desktop)]
#[derive(Default)]
struct Refusals {
    epoch: u64,
    seen: Vec<cef::surface::Rect>,
    repeats: u32,
}

/// How many distinct refused rects one open takes before it fails.
#[cfg(desktop)]
const MAX_REFUSALS: usize = 5;

/// How many times the same refused rect is asked to be measured again before
/// the open fails: a stable wrong rect must reach the sentence, not loop.
#[cfg(desktop)]
const MAX_REPEATS: u32 = 5;

/// What a refusal costs: the page is asked to measure again, until either the
/// distinct-rect or the same-rect bound is reached, when the open fails.
#[cfg(desktop)]
#[derive(Debug, PartialEq)]
enum Refusal {
    Remeasure,
    Fail,
}

/// What the shell tells the page about the surface.
#[cfg(desktop)]
#[derive(Clone, Debug, PartialEq)]
enum SurfaceEvent {
    Remeasure,
    Failed,
}

/// The sentence a surface that can never be placed leaves on the bar.
#[cfg(desktop)]
const FAILED_SENTENCE: &str = "the browser page area could not be placed - try again";

/// Record a refused report against its open and answer what to do about it.
#[cfg(desktop)]
fn refusal(seen: &mut Refusals, open_epoch: u64, rect: cef::surface::Rect) -> Refusal {
    if seen.epoch != open_epoch {
        *seen = Refusals { epoch: open_epoch, ..Refusals::default() };
    }
    if seen.seen.contains(&rect) {
        seen.repeats = seen.repeats.saturating_add(1);
    } else {
        seen.seen.push(rect);
        seen.repeats = 0;
    }
    if seen.seen.len() >= MAX_REFUSALS || seen.repeats >= MAX_REPEATS {
        Refusal::Fail
    } else {
        Refusal::Remeasure
    }
}

/// The main window's page area in CSS pixels, when the window is there.
#[cfg(desktop)]
fn app_window_size(app: &tauri::AppHandle) -> Option<(f64, f64)> {
    let window = app.get_webview_window("main")?;
    let scale = window.scale_factor().unwrap_or(1.0);
    window
        .inner_size()
        .ok()
        .map(|size| (f64::from(size.width) / scale, f64::from(size.height) / scale))
}

/// Whether a report may place the open surface: it must come from THIS open -
/// a report measured for a surface that is gone belongs to another layout -
/// and its rect must be the window under the bar.
#[cfg(desktop)]
fn report_accepted(report: &ReportedRect, open_epoch: u64, window: Option<(f64, f64)>) -> bool {
    report.epoch == open_epoch && window.is_none_or(|size| rect_fits_window(&report.rect, size))
}

/// Whether a reported page area can be the open takeover's page: the area is
/// the window under the bar, so it starts at the window's left edge, spans its
/// width, and its foot is the window's foot. **A report that misses any of
/// those is a measurement of another layout** - a transient viewport, or a
/// mount that is no longer the one on screen - and applying it leaves the page
/// as a sliver of itself (#2470).
#[cfg(desktop)]
fn rect_fits_window(rect: &cef::surface::Rect, window: (f64, f64)) -> bool {
    let (width, height) = window;
    let foot = rect.y + rect.height;
    rect.x.abs() <= RECT_SLACK
        && (rect.width - width).abs() <= RECT_SLACK
        && rect.y >= 0.0
        && rect.height > RECT_SLACK
        && foot <= height + RECT_SLACK
        && foot >= height - WINDOW_CHROME_SLACK
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
    /// The open this surface is: only a report carrying this epoch may place
    /// or resize it.
    pub epoch: u64,
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

    /// **A page area is the window under the bar, and a report that is not is
    /// dropped, and a report from another open is refused whatever its
    /// geometry.** #2470's band was a measurement from a transient, short
    /// viewport: full width, foot far above the window's, drawn as a sliver.
    #[test]
    fn a_report_from_another_open_or_another_layout_is_refused() {
        let window = (2048.0, 1152.0);
        let reported = |epoch: u64, x: f64, y: f64, width: f64, height: f64| ReportedRect {
            rect: cef::surface::Rect { x, y, width, height, scale: 2.0 },
            epoch,
        };
        assert!(
            report_accepted(&reported(7, 0.0, 40.0, 2048.0, 1112.0), 7, Some(window)),
            "this open's page under the bar is the window",
        );
        assert!(
            report_accepted(&reported(7, 0.0, 88.0, 2048.0, 1064.0), 7, Some(window)),
            "a taller bar moves the page's top; its foot is still the window's",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, 40.0, 2048.0, 190.0), 7, Some(window)),
            "a sliver of a viewport is not this window",
        );
        assert!(
            !report_accepted(&reported(7, -5.0, 40.0, 2048.0, 1112.0), 7, Some(window)),
            "a negative left edge is further from the window than the slack allows",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, 40.0, 1200.0, 901.0), 7, Some(window)),
            "another window's measurement is refused",
        );
        assert!(
            !report_accepted(&reported(7, 332.0, 912.0, 1706.0, 228.0), 7, Some(window)),
            "a box that is not the page area is refused",
        );
        assert!(
            !report_accepted(&reported(7, 5.0, 40.0, 2048.0, 1112.0), 7, Some(window)),
            "a page area that does not start at the window's left edge is refused",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, 40.0, 2.0, 1112.0), 7, Some(window)),
            "a zero-width page area is refused",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, 40.0, 0.0, 0.0), 7, Some(window)),
            "a zero by zero page area is refused",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, -1.0, 2048.0, 1153.0), 7, Some(window)),
            "a page area above the window's top is refused",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, 1150.0, 2048.0, 2.0), 7, Some(window)),
            "a two-pixel-tall page area is refused",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, 40.0, 2053.0, 1112.0), 7, Some(window)),
            "a width past the slack is refused",
        );
        assert!(
            report_accepted(&reported(7, 0.0, 40.0, 2052.0, 1112.0), 7, Some(window)),
            "a width inside the slack is the window's",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, 40.0, 2048.0, 1117.0), 7, Some(window)),
            "a foot past the slack is refused",
        );
        assert!(
            report_accepted(&reported(7, 0.0, 40.0, 2048.0, 1116.0), 7, Some(window)),
            "a foot inside the slack is the window's",
        );
        assert!(
            report_accepted(&reported(7, 0.0, 40.0, 2048.0, 1064.0), 7, Some(window)),
            "a foot exactly at the chrome bound is the windowed page's",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, 40.0, 2048.0, 1063.0), 7, Some(window)),
            "a foot past the chrome bound is refused",
        );
        assert!(
            !report_accepted(&reported(6, 0.0, 40.0, 2048.0, 1112.0), 7, Some(window)),
            "a report measured for another open is refused even though it fits",
        );
        assert!(
            !report_accepted(&reported(6, 0.0, 80.0, 2048.0, 1072.0), 7, Some(window)),
            "the previous open's feedback-row rect fits the window and is still refused",
        );
        assert!(
            report_accepted(&reported(7, 0.0, 40.5, 2048.0, 1111.0), 7, Some(window)),
            "fractional layout is not a mismatch",
        );
        assert!(
            report_accepted(&reported(7, 0.0, 40.0, 2048.0, 1082.0), 7, Some(window)),
            "a windowed page is short of the window by its title bar, not a mismatch",
        );
        assert!(
            !report_accepted(&reported(7, 0.0, 40.0, 2048.0, 1000.0), 7, Some(window)),
            "a foot well above the window's is the band",
        );
        assert!(
            report_accepted(&reported(7, 0.0, 40.0, 1440.0, 828.0), 7, Some((1440.0, 900.0))),
            "a 1440x900 window's page area is 828 tall under a 32-point title bar",
        );
        assert!(
            report_accepted(&reported(7, 0.0, 40.0, 2048.0, 1112.0), 7, None),
            "with no window to check against, the open's own report is taken",
        );
    }

    /// **The refusal is the reconcile's, not only the predicate's.** A stored
    /// report from another open is refused where it would have been placed:
    /// nothing attaches, the refusal is counted, and the report is left for
    /// the next one to supersede rather than clearing the store under it.
    #[tokio::test]
    async fn reconcile_refuses_a_report_from_another_open() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let ring =
            Arc::new(cef::osr::Ring::create("/forge-test-refuse-rect", 4, 4).expect("a ring"));
        let dir = PathBuf::from("/tmp/forge-test-refuse");
        *host.surface.lock().await = Some(OpenSurface {
            profile: None,
            dir: dir.clone(),
            host: Arc::new(cef::spawn::ActiveHost { pid: 0, port: 0, ring, profile: dir }),
            epoch: 7,
        });
        let refused =
            cef::surface::Rect { x: 0.0, y: 40.0, width: 2048.0, height: 1112.0, scale: 2.0 };
        *host.surface_rect.lock().await = Some(ReportedRect { rect: refused, epoch: 6 });
        host.reconcile_surface().await.expect("a refused report is not an error");
        assert_eq!(*host.attached.lock().await, None, "nothing attaches on a refused report");
        {
            let refusals = host.refusals.lock().await;
            assert_eq!(refusals.epoch, 7, "the refusal belongs to this open");
            assert_eq!(refusals.seen, vec![refused], "the refused rect is remembered");
        }
        // The same measurement again is asked again, but adds no new rect.
        host.reconcile_surface().await.expect("a repeat is not an error");
        {
            let refusals = host.refusals.lock().await;
            assert_eq!(refusals.seen, vec![refused], "a repeat adds no new rect");
            assert_eq!(refusals.repeats, 1, "and counts toward the repeat bound");
        }
        assert!(
            host.surface_rect.lock().await.is_some(),
            "the stored report is left for the next one to supersede",
        );
        // A report from this open places - the geometry check passes with no
        // window to check against - and the refusal history is behind it.
        *host.surface_rect.lock().await = Some(ReportedRect { rect: refused, epoch: 7 });
        host.reconcile_surface().await.expect("an accepted report is not an error");
        assert!(host.refusals.lock().await.seen.is_empty(), "acceptance clears the refusals",);
        cef::osr::Ring::unlink("/forge-test-refuse-rect");
    }

    /// **An ended host forgets its open's geometry.** A profile closed, deleted
    /// or idle-released while its surface is open must not leave a rect for the
    /// next open to be refused.
    #[tokio::test]
    async fn an_ended_host_forgets_its_open_surface() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let ring = Arc::new(cef::osr::Ring::create("/forge-test-end-host", 4, 4).expect("a ring"));
        let cache = PathBuf::from("/tmp/forge-test-end-host-cache");
        *host.surface.lock().await = Some(OpenSurface {
            profile: None,
            dir: cache.clone(),
            host: Arc::new(cef::spawn::ActiveHost {
                pid: 0,
                port: 0,
                ring,
                profile: cache.clone(),
            }),
            epoch: 7,
        });
        *host.surface_rect.lock().await = Some(ReportedRect {
            rect: cef::surface::Rect { x: 0.0, y: 40.0, width: 2048.0, height: 1112.0, scale: 2.0 },
            epoch: 7,
        });
        *host.refusals.lock().await = Refusals {
            epoch: 7,
            seen: vec![cef::surface::Rect {
                x: 0.0,
                y: 40.0,
                width: 2048.0,
                height: 190.0,
                scale: 2.0,
            }],
            repeats: 2,
        };
        host.end_host_locked(&cache).await;
        assert!(host.surface.lock().await.is_none(), "the ended host's surface is gone");
        assert!(host.surface_rect.lock().await.is_none(), "and its geometry with it");
        assert!(
            host.refusals.lock().await.seen.is_empty(),
            "and no refusal history to fail the next open early",
        );
        cef::osr::Ring::unlink("/forge-test-end-host");
    }

    /// **A page that never places fails its open.** Distinct refused rects are
    /// counted; at the bound the open is failed, so the bar carries the
    /// sentence and the hand-off stays parked instead of a dark page area.
    #[tokio::test]
    async fn a_page_that_never_places_fails_its_open() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let ring =
            Arc::new(cef::osr::Ring::create("/forge-test-refuse-many", 4, 4).expect("a ring"));
        let dir = PathBuf::from("/tmp/forge-test-refuse-many");
        *host.surface.lock().await = Some(OpenSurface {
            profile: None,
            dir: dir.clone(),
            host: Arc::new(cef::spawn::ActiveHost { pid: 0, port: 0, ring, profile: dir }),
            epoch: 7,
        });
        // Four distinct refusals are a transition's worth; the fifth settles
        // it. A bound that moved would leave the surface up one step early or
        // late and fail here.
        for step in 0..4 {
            let rect = cef::surface::Rect {
                x: 0.0,
                y: 40.0 + step as f64,
                width: 2048.0,
                height: 1112.0,
                scale: 2.0,
            };
            *host.surface_rect.lock().await = Some(ReportedRect { rect, epoch: 6 });
            host.reconcile_surface().await.expect("a refusal is not an error");
        }
        assert!(host.surface.lock().await.is_some(), "four refusals do not fail the open");
        let rect =
            cef::surface::Rect { x: 0.0, y: 44.0, width: 2048.0, height: 1112.0, scale: 2.0 };
        *host.surface_rect.lock().await = Some(ReportedRect { rect, epoch: 6 });
        host.reconcile_surface().await.expect("a refusal is not an error");
        assert!(host.surface.lock().await.is_none(), "the open fails at the bound");
        assert!(host.surface_rect.lock().await.is_none(), "and its geometry goes with it");
        cef::osr::Ring::unlink("/forge-test-refuse-many");
    }

    /// **Every refusal asks again, and either bound fails the open.** A fresh
    /// rect counts toward the distinct bound; a rect already seen counts
    /// toward the repeat bound, whether it repeats consecutively or not.
    #[test]
    fn a_refusal_asks_until_a_bound_and_a_stable_rect_fails() {
        let rect =
            |y: f64| cef::surface::Rect { x: 0.0, y, width: 2048.0, height: 1112.0, scale: 2.0 };
        // A stable wrong rect: asked again once per repeat, then failed.
        let mut seen = Refusals::default();
        assert_eq!(refusal(&mut seen, 7, rect(40.0)), Refusal::Remeasure, "a fresh rect asks");
        for _ in 1..MAX_REPEATS {
            assert_eq!(
                refusal(&mut seen, 7, rect(40.0)),
                Refusal::Remeasure,
                "a repeat asks again until the bound",
            );
        }
        assert_eq!(
            refusal(&mut seen, 7, rect(40.0)),
            Refusal::Fail,
            "a stable wrong rect fails the open",
        );
        // Distinct rects fail at their own, separate bound.
        let mut seen = Refusals::default();
        for step in 0..(MAX_REFUSALS - 1) {
            assert_eq!(refusal(&mut seen, 7, rect(40.0 + step as f64)), Refusal::Remeasure,);
        }
        assert_eq!(
            refusal(&mut seen, 7, rect(40.0 + (MAX_REFUSALS - 1) as f64)),
            Refusal::Fail,
            "the fifth distinct rect fails the open",
        );
        // An alternating pair is bounded too: a report of a rect already seen
        // counts toward the repeat bound however often the pair changes.
        let mut seen = Refusals::default();
        assert_eq!(refusal(&mut seen, 7, rect(40.0)), Refusal::Remeasure, "A is fresh");
        assert_eq!(refusal(&mut seen, 7, rect(41.0)), Refusal::Remeasure, "B is fresh");
        let mut failed = false;
        for step in 0..(MAX_REPEATS + 2) {
            let y = if step % 2 == 0 { 40.0 } else { 41.0 };
            if refusal(&mut seen, 7, rect(y)) == Refusal::Fail {
                failed = true;
                break;
            }
        }
        assert!(failed, "an alternating pair reaches the bound instead of oscillating");
        // A new open starts its own history.
        assert_eq!(
            refusal(&mut seen, 8, rect(40.0)),
            Refusal::Remeasure,
            "a new open's first refusal asks again",
        );
    }

    /// **The report's epoch is the one the shell was told at Open.** The wire
    /// path is the thing that matters: a report stored with another epoch is
    /// refused, which is #2470 with no band to see.
    #[tokio::test]
    async fn a_report_carries_the_epoch_its_open_stored() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let ring =
            Arc::new(cef::osr::Ring::create("/forge-test-epoch-wire", 4, 4).expect("a ring"));
        let dir = PathBuf::from("/tmp/forge-test-epoch-wire");
        *host.surface.lock().await = Some(OpenSurface {
            profile: None,
            dir: dir.clone(),
            host: Arc::new(cef::spawn::ActiveHost { pid: 0, port: 0, ring, profile: dir }),
            epoch: 7,
        });
        let rect =
            cef::surface::Rect { x: 0.0, y: 40.0, width: 2048.0, height: 1112.0, scale: 2.0 };
        host.set_surface_rect(rect, 7).await;
        assert_eq!(
            host.surface_rect.lock().await.map(|reported| reported.epoch),
            Some(7),
            "the epoch the page sent is the one stored",
        );
        assert!(host.refusals.lock().await.seen.is_empty(), "a report for this open is accepted",);
        host.set_surface_rect(rect, 8).await;
        {
            let refusals = host.refusals.lock().await;
            assert_eq!(refusals.epoch, 7, "the refusal still belongs to this open");
            assert_eq!(refusals.seen, vec![rect], "another open's report is refused");
        }
        cef::osr::Ring::unlink("/forge-test-epoch-wire");
    }

    /// **The geometry check runs in the reconcile, not only in the predicate.**
    /// A band-shaped report against a pinned window is refused (an epoch-only
    /// check would accept it) and asked to be measured again; once the same
    /// band repeats to the bound the open fails with the sentence. This is
    /// #2470's own refusal, guarded where it is applied.
    #[tokio::test]
    async fn a_band_report_is_refused_and_a_stable_one_fails_the_open() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let ring =
            Arc::new(cef::osr::Ring::create("/forge-test-band-reconcile", 4, 4).expect("a ring"));
        let dir = PathBuf::from("/tmp/forge-test-band");
        *host.surface.lock().await = Some(OpenSurface {
            profile: None,
            dir: dir.clone(),
            host: Arc::new(cef::spawn::ActiveHost { pid: 0, port: 0, ring, profile: dir }),
            epoch: 7,
        });
        *host.window_override.lock().await = Some((2048.0, 1152.0));
        let band = cef::surface::Rect { x: 0.0, y: 40.0, width: 2048.0, height: 190.0, scale: 2.0 };
        *host.surface_rect.lock().await = Some(ReportedRect { rect: band, epoch: 7 });
        host.reconcile_surface().await.expect("a refusal is not an error");
        {
            let refusals = host.refusals.lock().await;
            assert_eq!(refusals.seen, vec![band], "the band is refused by the geometry check");
        }
        assert_eq!(
            *host.emitted.lock().expect("the emitted log"),
            vec![SurfaceEvent::Remeasure],
            "the page is asked to measure again",
        );
        // Four repeats are a slow settle; the fifth settles it wrong and the
        // open fails. A bound that moved would leave the surface up a step
        // early or late and fail the assertions below.
        for _ in 0..4 {
            host.reconcile_surface().await.expect("a refusal is not an error");
        }
        assert!(host.surface.lock().await.is_some(), "four repeats do not fail the open");
        host.reconcile_surface().await.expect("a refusal is not an error");
        assert!(host.surface.lock().await.is_none(), "a stable band fails the open");
        assert_eq!(
            host.emitted.lock().expect("the emitted log").last(),
            Some(&SurfaceEvent::Failed),
            "and the sentence is sent",
        );
        cef::osr::Ring::unlink("/forge-test-band-reconcile");
    }

    /// **The command hop carries the epoch too.** The Tauri command is the
    /// seam between the page's payload and the host; a zero dropped here fails
    /// every report exactly like a band.
    #[tokio::test]
    async fn the_rect_command_carries_its_epoch_into_the_host() {
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("the mock app builds");
        app.manage(Arc::new(BrowserHost::unavailable(
            "the app's data directory cannot be resolved".to_owned(),
        )));
        browser_surface_rect(app.state::<Arc<BrowserHost>>(), 0.0, 40.0, 2048.0, 1112.0, 2.0, 7)
            .await
            .expect("the report is stored");
        assert_eq!(
            app.state::<Arc<BrowserHost>>()
                .surface_rect
                .lock()
                .await
                .map(|reported| reported.epoch),
            Some(7),
            "the command stores the epoch the page sent",
        );
    }

    /// **A raise stores the epoch the page sent.** The show hop is the one the
    /// reviewers could zero without a test: a surface told 0 refuses every
    /// report the page sends under its real epoch.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_raise_stores_the_epoch_the_page_sent() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = Arc::new(BrowserHost::new(paths.clone()));
        let cache = paths.cef_shared();
        std::fs::create_dir_all(&cache).expect("the cache root");
        let stub_host = dir.path().join("stub-host.sh");
        std::fs::write(&stub_host, "#!/bin/sh\nexec sleep 30\n").expect("the stub host");
        std::fs::set_permissions(&stub_host, std::fs::Permissions::from_mode(0o755))
            .expect("the stub host is executable");
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a port");
        let port = listener.local_addr().expect("the address").port();
        listener.set_nonblocking(true).expect("non-blocking");
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            loop {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let mut buffer = [0_u8; 2048];
                        let read = socket.read(&mut buffer).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);
                        let body = if request.starts_with("GET /json/list") {
                            r#"[{"type":"page","url":"https://example.com/raise"}]"#
                        } else {
                            r#"{"Browser":"Chrome/154.0"}"#
                        };
                        let answer = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = socket.write_all(answer.as_bytes());
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        });
        let ring_name = "/forge-cef-show-epoch";
        let ring = Arc::new(cef::osr::Ring::create(ring_name, 64, 64).expect("a ring"));
        let pid = cef::spawn::spawn_with(&stub_host, &cache, port, ring_name, 800, 600)
            .expect("the stub host starts");
        cef::spawn::write_record(
            &cache,
            &cef::spawn::Record { pid, port, ring: ring_name.to_owned() },
        )
        .expect("the record");
        host.hosts.lock().await.insert(
            cache.clone(),
            Arc::new(cef::spawn::ActiveHost { pid, port, ring, profile: cache.clone() }),
        );

        // **Through the command**, which is the page's own door: a zero
        // dropped at this hop leaves every report refused with green tests.
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("the mock app builds");
        app.manage(Arc::clone(&host));
        browser_show(app.state::<Arc<BrowserHost>>(), None, None, 7)
            .await
            .expect("the raise opens");
        assert_eq!(
            host.surface.lock().await.as_ref().map(|open| open.epoch),
            Some(7),
            "the surface is told the epoch the page sent",
        );

        cef::spawn::end(pid).await;
        cef::spawn::remove_record(&cache);
        cef::osr::Ring::unlink(ring_name);
    }

    /// **A lower forgets its geometry.** The next open must wait for its own
    /// mount's report rather than placing at the rect the lowered surface
    /// measured - the stale attach that put a 2048x1064 page into a 1440x900
    /// window.
    #[tokio::test]
    async fn a_lowered_surface_forgets_its_rect() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let ring =
            Arc::new(cef::osr::Ring::create("/forge-test-lower-rect", 4, 4).expect("a ring"));
        let dir = PathBuf::from("/tmp/forge-test-lower");
        *host.surface.lock().await = Some(OpenSurface {
            profile: None,
            dir: dir.clone(),
            host: Arc::new(cef::spawn::ActiveHost { pid: 0, port: 0, ring, profile: dir.clone() }),
            epoch: 7,
        });
        *host.surface_rect.lock().await = Some(ReportedRect {
            rect: cef::surface::Rect { x: 0.0, y: 40.0, width: 2048.0, height: 1112.0, scale: 2.0 },
            epoch: 7,
        });
        *host.refusals.lock().await = Refusals {
            epoch: 7,
            seen: vec![cef::surface::Rect {
                x: 0.0,
                y: 40.0,
                width: 2048.0,
                height: 190.0,
                scale: 2.0,
            }],
            repeats: 1,
        };
        assert!(host.lower_surface(&dir).await, "the open surface lowers");
        assert!(
            host.surface_rect.lock().await.is_none(),
            "the next open starts with no geometry to place",
        );
        assert!(
            host.refusals.lock().await.seen.is_empty(),
            "and no refusal history to fail it early",
        );
        cef::osr::Ring::unlink("/forge-test-lower-rect");
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

    /// **An omitted profile is the seat's own; the shared browser has to be
    /// named.** The shared browser's tabs belong to the person and to other
    /// work - measured 2026-10-23, an omitted profile returned the person's
    /// HiringCafe tab - so the default can never resolve to it, and an empty
    /// name is not a quiet spelling of it either.
    #[test]
    fn an_omitted_profile_is_the_seats_own_and_the_shared_browser_is_named() {
        let mine = seat();
        assert_eq!(profile_target(None, &mine), ProfileTarget::Named("forge-lead".to_owned()));
        assert_eq!(profile_target(Some("hunt"), &mine), ProfileTarget::Named("hunt".to_owned()),);
        assert_eq!(profile_target(Some(profiles::SHARED_PROFILE), &mine), ProfileTarget::Shared);
        assert_eq!(
            profile_target(Some(""), &mine),
            ProfileTarget::Named(String::new()),
            "an empty name is refused by the registry, never read as the shared browser",
        );
        assert_eq!(
            profile_target(None, &Seat { project: "my app".to_owned(), ..mine.clone() }),
            ProfileTarget::Named("my-app-lead".to_owned()),
            "the seat's name is the same fold every auto profile uses",
        );
    }

    /// **`@shared` is the one explicit name for the browser's own profile**,
    /// and a name that cannot be a profile never resolves to a directory -
    /// the resolution `hide` and `windowed` share with the call path.
    #[cfg(desktop)]
    #[test]
    fn the_shared_token_resolves_to_the_shared_cache_root() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        assert_eq!(cache_of(&paths, profiles::SHARED_PROFILE), Ok(paths.cef_shared()));
        assert_eq!(cache_of(&paths, "hunt"), Ok(paths.cef_named("hunt")));
        let refused = cache_of(&paths, "../user-data").expect_err("a traversing name is refused");
        assert!(refused.contains("1 to 64"), "{refused}");
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

    /// **A host with a stub driver behind it.** The driver is the real one:
    /// the stub is an MCP server on stdio in the vendored layout's place, so
    /// `BrowserHost::call` runs its own path and only the answers are the
    /// test's. Each `tools/call` is recorded in `calls.log` beside the stub,
    /// so a test can say how many times one tool was asked for.
    #[cfg(target_os = "macos")]
    struct StubHost {
        dir: tempfile::TempDir,
        host: Arc<BrowserHost>,
        /// The calling seat's own cache root: what an unnamed call drives.
        cache: std::path::PathBuf,
        pid: u32,
        port: u16,
        ring: String,
        ring_value: Arc<cef::osr::Ring>,
        /// The browser's own profile's ring, which an explicit `@shared`
        /// call or the person's own raise drives.
        shared_ring: Arc<cef::osr::Ring>,
        named_ring: Option<Arc<cef::osr::Ring>>,
        /// The host's `/json/list` and `/json/version`, which a test can
        /// script read by read so a created tab appears when it should.
        list: cef::veto::testing::ListStub,
    }

    #[cfg(target_os = "macos")]
    impl StubHost {
        /// `answers` maps a tool name to the MCP result the stub gives it;
        /// any other tool answers `ok`.
        async fn start(ring: &str, answers: &serde_json::Value) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().expect("a temp dir");
            let paths = StackPaths {
                stack: dir.path().join("stack"),
                user_data: dir.path().join("user-data"),
                output: dir.path().join("output"),
                profiles: dir.path().join("profiles"),
            };
            // The vendored layout, with a stub in node's place: the wrapper
            // runs the "cli" with python, which speaks the newline-delimited
            // MCP handshake the driver expects.
            let node = driver::node_path(&paths.stack);
            std::fs::create_dir_all(node.parent().expect("the node dir")).expect("the node dir");
            std::fs::write(&node, "#!/bin/sh\nexec python3 \"$@\"\n").expect("the node wrapper");
            std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755))
                .expect("the wrapper is executable");
            let cli = driver::cli_path(&paths.stack);
            std::fs::create_dir_all(cli.parent().expect("the cli dir")).expect("the cli dir");
            // The answers go in a file of their own: a JSON body in a python
            // string literal would have its escapes read twice.
            let answers_path = dir.path().join("answers.json");
            std::fs::write(&answers_path, answers.to_string()).expect("the stub's answers");
            // The ring the stub reads the cue from, named per test: a named
            // profile's call is served by its own ring.
            let ring_file = dir.path().join("ring");
            std::fs::write(&ring_file, ring).expect("the stub's ring name");
            let body = r#"import sys, json, os, ctypes
ANSWERS = json.load(open("__ANSWERS__"))
CALLS = "__CALLS__"
CALLS_ARGS = "__CALLS_ARGS__"
CUE = "__CUE__"
RING_FILE = "__RING_FILE__"
ONCE = ANSWERS.get("__once__", {})
ONCE_FLAG = "__ONCE__"
STARTS = "__STARTS__"
seen = []
if os.path.exists(STARTS):
    with open(STARTS) as earlier:
        seen = [line.strip() for line in earlier if line.strip()]
with open(STARTS, "a") as started:
    started.write(str(os.getpid()) + "\n")
if ANSWERS.get("__die_second__") and len(seen) + 1 >= 2:
    sys.exit(1)

def cue_state():
    try:
        with open(RING_FILE) as source:
            ring = source.read().strip().encode()
        libc = ctypes.CDLL(None)
        libc.shm_open.restype = ctypes.c_int
        libc.shm_open.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
        fd = libc.shm_open(ring, 0, 0)
        if fd < 0:
            return (0, 0)
        libc.mmap.restype = ctypes.c_void_p
        libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int,
                              ctypes.c_int, ctypes.c_int, ctypes.c_long]
        addr = libc.mmap(None, __HEADER_LEN__, 1, 1, fd, 0)
        if not addr or addr == ctypes.c_void_p(-1).value:
            libc.close(fd)
            return (0, 0)
        sent = ctypes.c_uint64.from_address(addr + __SHOW_SEQ__).value
        applied = ctypes.c_uint64.from_address(addr + __SHOW_APPLIED__).value
        libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
        libc.munmap(ctypes.c_void_p(addr), __HEADER_LEN__)
        libc.close(fd)
        return (sent, applied)
    except Exception:
        return (0, 0)

asked = {}
for line in sys.stdin:
    try:
        message = json.loads(line)
    except Exception:
        continue
    if message.get("id") is None:
        continue
    method = message.get("method")
    if method == "initialize":
        version = message.get("params", {}).get("protocolVersion", "2025-06-18")
        result = {"protocolVersion": version, "capabilities": {"tools": {}},
                  "serverInfo": {"name": "stub", "version": "0"}}
    elif method == "tools/list":
        result = {"tools": []}
    elif method == "tools/call":
        name = message.get("params", {}).get("name", "")
        with open(CALLS, "a") as log:
            log.write(name + "\n")
        with open(CALLS_ARGS, "a") as log:
            log.write(name + " " + json.dumps(message.get("params", {}).get("arguments", {})) + "\n")
        sent, applied = cue_state()
        with open(CUE, "a") as log:
            log.write("%s %d %d\n" % (name, sent, applied))
        answer = ANSWERS.get(name)
        if name in ONCE and not os.path.exists(ONCE_FLAG):
            open(ONCE_FLAG, "w").close()
            answer = ONCE[name]
        if isinstance(answer, list):
            at = min(asked.get(name, 0), len(answer) - 1)
            asked[name] = asked.get(name, 0) + 1
            answer = answer[at]
        result = answer or {"content": [{"type": "text", "text": "ok"}]}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}) + "\n")
    sys.stdout.flush()
"#
            .replace("__ANSWERS__", &answers_path.display().to_string())
            .replace("__CALLS__", &dir.path().join("calls.log").display().to_string())
            .replace(
                "__CALLS_ARGS__",
                &dir.path().join("calls-args.log").display().to_string(),
            )
            .replace("__CUE__", &dir.path().join("cue.log").display().to_string())
            .replace("__RING_FILE__", &ring_file.display().to_string())
            .replace("__ONCE__", &dir.path().join("once.flag").display().to_string())
            .replace("__STARTS__", &dir.path().join("starts.log").display().to_string())
            .replace("__HEADER_LEN__", &std::mem::size_of::<cef::osr::Header>().to_string())
            .replace("__SHOW_SEQ__", &std::mem::offset_of!(cef::osr::Header, show_seq).to_string())
            .replace(
                "__SHOW_APPLIED__",
                &std::mem::offset_of!(cef::osr::Header, show_applied).to_string(),
            );
            std::fs::write(&cli, body).expect("the stub driver");
            std::fs::create_dir_all(&paths.output).expect("the output dir");

            let host = Arc::new(BrowserHost::new(paths.clone()));
            // **The seat's own profile is what an unnamed call drives**, so
            // the fixture's primary host stands behind that cache root; the
            // shared browser gets its own host and ring beside it.
            let cache = profiles::cef_cache(&paths.profiles.join(auto_name(&seat())));
            std::fs::create_dir_all(&cache).expect("the seat's cache root");
            let shared_cache = paths.cef_shared();
            std::fs::create_dir_all(&shared_cache).expect("the shared cache root");
            let stub_host = dir.path().join("stub-host.sh");
            std::fs::write(&stub_host, "#!/bin/sh\nexec sleep 30\n").expect("the stub host");
            std::fs::set_permissions(&stub_host, std::fs::Permissions::from_mode(0o755))
                .expect("the stub host is executable");
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a port");
            let port = listener.local_addr().expect("the address").port();
            drop(listener);
            let list = cef::veto::testing::list_on(port).await.expect("the list endpoint");
            list.set(vec![("T0".to_owned(), "https://example.com/after-call".to_owned())]);
            let ring_name = ring.to_owned();
            let ring_value = Arc::new(cef::osr::Ring::create(&ring_name, 64, 64).expect("a ring"));
            let pid = cef::spawn::spawn_with(&stub_host, &cache, port, &ring_name, 800, 600)
                .expect("the stub host starts");
            cef::spawn::write_record(
                &cache,
                &cef::spawn::Record { pid, port, ring: ring_name.clone() },
            )
            .expect("the record");
            host.hosts.lock().await.insert(
                cache.clone(),
                Arc::new(cef::spawn::ActiveHost {
                    pid,
                    port,
                    ring: Arc::clone(&ring_value),
                    profile: cache.clone(),
                }),
            );
            // **The shared browser stands beside it**, with a ring of its
            // own: an explicit `@shared` call or the person's own raise must
            // cue the shared host's page, not the seat's. Named by the
            // process, because macOS caps a POSIX shm name at 31 characters
            // and two parallel test processes must never map each other's
            // segment.
            let shared_ring_name = format!("/forge-cef-shared-{}", std::process::id());
            let shared_ring =
                Arc::new(cef::osr::Ring::create(&shared_ring_name, 64, 64).expect("a ring"));
            acknowledge_cues(Arc::clone(&shared_ring));
            host.hosts.lock().await.insert(
                shared_cache.clone(),
                Arc::new(cef::spawn::ActiveHost {
                    pid,
                    port,
                    ring: Arc::clone(&shared_ring),
                    profile: shared_cache.clone(),
                }),
            );
            // **A host that applies every cue**, as the host loop does:
            // without it each call's bounded wait burns its full 100 ms.
            acknowledge_cues(Arc::clone(&ring_value));
            Self {
                dir,
                host,
                cache,
                pid,
                port,
                ring: ring_name,
                ring_value,
                shared_ring,
                named_ring: None,
                list,
            }
        }

        /// The same stub host behind a named profile's cache root, so the
        /// named arm of `call` runs the same path as the shared one.
        ///
        /// **A ring of its own**, as a separate host would have: the cue the
        /// named test observes is that profile's, not the shared browser's.
        async fn adopt_named(&mut self, name: &str) {
            let cache = profiles::cef_cache(&self.dir.path().join("profiles").join(name));
            std::fs::create_dir_all(&cache).expect("the named cache root");
            // Named by the process: a fixed name collides across parallel
            // test processes, and macOS caps a POSIX shm name at 31 chars.
            let ring = Arc::new(
                cef::osr::Ring::create(&format!("/forge-cef-named-{}", std::process::id()), 64, 64)
                    .expect("a ring"),
            );
            acknowledge_cues(Arc::clone(&ring));
            std::fs::write(self.dir.path().join("ring"), &ring.name)
                .expect("the named ring name for the stub");
            self.named_ring = Some(Arc::clone(&ring));
            self.host.hosts.lock().await.insert(
                cache.clone(),
                Arc::new(cef::spawn::ActiveHost {
                    pid: self.pid,
                    port: self.port,
                    ring,
                    profile: cache,
                }),
            );
        }

        /// Whether the named profile's own ring acknowledged `seq`.
        fn named_shown(&self, seq: u64) -> bool {
            self.named_ring.as_ref().is_some_and(|ring| ring.shown(seq))
        }

        /// Whether the shared browser's ring acknowledged `seq`.
        fn shared_shown(&self, seq: u64) -> bool {
            self.shared_ring.shown(seq)
        }

        /// Point the stub driver's cue read at the shared browser's ring, so
        /// a `@shared` call's cue is observed on the shared host.
        fn adopt_shared(&self) {
            self.point_at(&self.shared_ring);
        }

        /// Point the stub driver's cue read at `ring`, so the next call's cue
        /// is observed on the host that ring belongs to.
        fn point_at(&self, ring: &cef::osr::Ring) {
            std::fs::write(self.dir.path().join("ring"), &ring.name)
                .expect("the ring name for the stub");
        }

        /// A second seat's own profile, behind the same stub host process
        /// with a ring of its own: the fixture's proof that two seats resolve
        /// to two hosts. Answers that profile's ring.
        async fn adopt_seat(&self, seat: &Seat) -> Arc<cef::osr::Ring> {
            let cache =
                profiles::cef_cache(&self.dir.path().join("profiles").join(auto_name(seat)));
            std::fs::create_dir_all(&cache).expect("the seat's cache root");
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let ring = Arc::new(
                cef::osr::Ring::create(&format!("/forge-cef-s{}-{n}", std::process::id()), 64, 64)
                    .expect("a ring"),
            );
            acknowledge_cues(Arc::clone(&ring));
            self.host.hosts.lock().await.insert(
                cache.clone(),
                Arc::new(cef::spawn::ActiveHost {
                    pid: self.pid,
                    port: self.port,
                    ring: Arc::clone(&ring),
                    profile: cache,
                }),
            );
            ring
        }

        /// Each tool call's name and the ring's cue state at the moment the
        /// driver received it: `(tool, show_seq, show_applied)`.
        fn call_cues(&self) -> Vec<(String, u64, u64)> {
            std::fs::read_to_string(self.dir.path().join("cue.log"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let mut parts = line.rsplitn(3, ' ');
                    let applied = parts.next()?.parse().ok()?;
                    let sent = parts.next()?.parse().ok()?;
                    let tool = parts.next()?.to_owned();
                    Some((tool, sent, applied))
                })
                .collect()
        }

        /// How many driver processes started against this stub, so a test can
        /// tell a rebuilt driver from the stale one.
        fn starts(&self) -> usize {
            std::fs::read_to_string(self.dir.path().join("starts.log"))
                .unwrap_or_default()
                .lines()
                .count()
        }

        /// Each call's tool and the arguments it carried.
        fn call_args(&self) -> Vec<(String, serde_json::Value)> {
            std::fs::read_to_string(self.dir.path().join("calls-args.log"))
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let (tool, args) = line.split_once(' ')?;
                    Some((tool.to_owned(), serde_json::from_str(args).ok()?))
                })
                .collect()
        }

        /// The tool names the stub was asked for, in order.
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        async fn shutdown(self) {
            cef::spawn::end(self.pid).await;
            cef::spawn::remove_record(&self.cache);
            if let Some(ring) = &self.named_ring {
                cef::osr::Ring::unlink(&ring.name);
            }
            cef::osr::Ring::unlink(&self.ring);
            cef::osr::Ring::unlink(&self.shared_ring.name);
        }
    }

    /// The host loop's cue acknowledgment, as a task the fixture runs: every
    /// cue the client writes is applied, not only the first, so a later call
    /// in one test does not wait out the 100 ms bound.
    #[cfg(target_os = "macos")]
    fn acknowledge_cues(ring: Arc<cef::osr::Ring>) {
        tokio::spawn(async move {
            let mut applied = 0_u64;
            loop {
                // SAFETY: the ring outlives this task and holds a header.
                let seq =
                    unsafe { (*ring.header()).show_seq.load(std::sync::atomic::Ordering::Acquire) };
                if seq != applied {
                    applied = seq;
                    // SAFETY: as above.
                    unsafe {
                        (*ring.header())
                            .show_applied
                            .store(seq, std::sync::atomic::Ordering::Release);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        });
    }

    /// **A successful browsing call rewrites the carried page.** The driver is
    /// a stub MCP server on stdio behind the real `Profile::call`, so this
    /// drives `BrowserHost::call` itself: deleting the refresh from the call
    /// path leaves the marker at its old value and this goes red.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_successful_call_rewrites_the_carried_page() {
        let stub = StubHost::start(
            "/forge-cef-call-test",
            &json!({ "browser_navigate": { "content": [{ "type": "text", "text": "ok" }] } }),
        )
        .await;
        write_carried_page(&stub.cache, Some("https://example.com/before"));

        let parts = stub
            .host
            .call(&seat(), "browser_navigate", json!({ "url": "https://example.com/after-call" }))
            .await
            .expect("the stub driver answers");
        assert_eq!(parts.len(), 1, "the stub's one part is the call's answer");
        assert_eq!(
            std::fs::read_to_string(stub.cache.join("page")).expect("the call wrote the page"),
            "https://example.com/after-call",
            "a successful call rewrites the carry"
        );
        // **The cue was applied before the call drove the page.** The stub
        // reads the ring when the driver receives the tool, so a `show` moved
        // after `Profile::call` reads as no cue here and this goes red.
        let cues = stub.call_cues();
        let (tool, sent, applied) =
            cues.first().expect("the driver asked for the tool it was given");
        assert_eq!(tool, "browser_navigate", "the stub saw the call it answered");
        assert!(
            *sent >= 1 && *applied >= *sent,
            "the cue was sent and applied before the call reached the driver: {cues:?}"
        );

        stub.shutdown().await;
    }

    /// **A tab opened at a URL is opened blank first, waits for THAT target's
    /// arm, then navigates.** The stub's `/json/list` grows the created
    /// target after the driver's snapshot, so the wait really runs: pre-armed
    /// it releases at once, unarmed it spends its bound. Deleting either
    /// wait leaves both calls in the same order but makes the unarmed case
    /// fast, which is what this reds on.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_new_tab_with_a_url_opens_blank_first() {
        let reads = vec![
            vec![("T0".to_owned(), "https://example.com/after-call".to_owned())],
            vec![
                ("T0".to_owned(), "https://example.com/after-call".to_owned()),
                ("T1".to_owned(), "about:blank".to_owned()),
            ],
        ];

        // Pre-armed: the wait releases on that id at once, on a warm driver,
        // so the bound here really separates "not waited out" from the 3 s
        // wait; the unarmed case below is the other half.
        let stub = StubHost::start(
            "/forge-cef-blank-first-test",
            &json!({
                "browser_tabs": { "content": [{ "type": "text", "text": "listing" }] },
                "__once__": {
                    "browser_tabs": { "content": [{ "type": "text", "text": "creation" }] },
                },
            }),
        )
        .await;
        stub.list.sequence(reads.clone());
        cef::veto::testing::mark_armed(stub.port, "T1");
        // **The driver starts before the clock**: a cold start is the app's
        // cost, not the wait's, and timing it here would measure the wrong
        // thing (measured cold on a loaded machine: 7.2 s).
        stub.host.call(&seat(), "browser_snapshot", json!({})).await.expect("the driver is warm");
        // **The warm call consumed the scripted list.** Its `refresh_carry`
        // reads `/json/list`, so the sequence is set again for the timed
        // call; without this the snapshot already holds the created target
        // and the wait never runs.
        stub.list.sequence(reads.clone());
        let began = std::time::Instant::now();
        let parts = stub
            .host
            .call(
                &seat(),
                "browser_tabs",
                json!({ "action": "new", "url": "https://example.com/job" }),
            )
            .await
            .expect("the tab opens");
        let armed_elapsed = began.elapsed();
        assert!(
            armed_elapsed < std::time::Duration::from_secs(2),
            "an already-armed tab is not waited out: {armed_elapsed:?}",
        );

        let calls = stub.call_args();
        let (tab_index, tab) = calls
            .iter()
            .enumerate()
            .find(|(_, (tool, _))| tool == "browser_tabs")
            .expect("the tab creation reached the driver");
        let (nav_index, nav) = calls
            .iter()
            .enumerate()
            .find(|(_, (tool, _))| tool == "browser_navigate")
            .expect("the navigation reached the driver");
        assert!(tab_index < nav_index, "the tab opens before it is navigated: {calls:?}");
        assert!(
            tab.1.get("url").is_none(),
            "the tab opens without a url, so the veto registers first: {calls:?}",
        );
        assert_eq!(
            nav.1.get("url").and_then(serde_json::Value::as_str),
            Some("https://example.com/job"),
            "the navigation carries the url the call named",
        );
        // **The reply is the tabs after the navigation**, not the blank
        // creation's own answer: the stub answers the first `browser_tabs`
        // call "creation" and every later one "listing".
        let (last_tool, last_args) = calls.last().expect("the rewrite sends a last call");
        assert_eq!(last_tool, "browser_tabs", "the answer is a tab listing: {calls:?}");
        assert_eq!(
            last_args.get("action").and_then(serde_json::Value::as_str),
            Some("list"),
            "taken after the navigation: {calls:?}",
        );
        assert_eq!(
            parts,
            vec![driver::ReplyPart::Text { text: "listing".to_owned() }],
            "the call answers with the listing, not the blank creation's reply",
        );

        stub.shutdown().await;

        // Unarmed: the wait runs to its bound before the navigation.
        let stub = StubHost::start("/forge-cef-blank-bound-test", &json!({})).await;
        stub.list.sequence(reads);
        let began = std::time::Instant::now();
        stub.host
            .call(
                &seat(),
                "browser_tabs",
                json!({ "action": "new", "url": "https://example.com/job" }),
            )
            .await
            .expect("the tab opens");
        assert!(
            began.elapsed() >= std::time::Duration::from_millis(2_500),
            "the unarmed tab's wait runs to its bound: {:?}",
            began.elapsed(),
        );

        stub.shutdown().await;
    }

    /// **A `new` clears the recorded blank it replaces.** A tab opened blank
    /// records its target; the next `new` - here one with a URL - must drop
    /// that record, or a later navigation waits on a stale, unarmed target.
    /// That stale wait would run its 3 s bound, so the fast navigation is
    /// the signal.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_new_tab_clears_the_blank_it_replaces() {
        // One read per consumer: the blank call's snapshot, its created-target
        // discovery, the refresh after that successful call, then the URL
        // call's snapshot and its discovery - so T2 appears only after its
        // snapshot, which is the branch this test's name claims.
        let reads = vec![
            vec![("T0".to_owned(), "https://example.com/after-call".to_owned())],
            vec![
                ("T0".to_owned(), "https://example.com/after-call".to_owned()),
                ("T1".to_owned(), "about:blank".to_owned()),
            ],
            vec![
                ("T0".to_owned(), "https://example.com/after-call".to_owned()),
                ("T1".to_owned(), "about:blank".to_owned()),
            ],
            vec![
                ("T0".to_owned(), "https://example.com/after-call".to_owned()),
                ("T1".to_owned(), "about:blank".to_owned()),
            ],
            vec![
                ("T0".to_owned(), "https://example.com/after-call".to_owned()),
                ("T1".to_owned(), "about:blank".to_owned()),
                ("T2".to_owned(), "about:blank".to_owned()),
            ],
        ];
        let stub = StubHost::start("/forge-cef-new-clears-test", &json!({})).await;
        stub.list.sequence(reads);
        // T1, the blank the first call records, is deliberately unarmed; T2,
        // the tab the second call creates, is armed so that call stays quick.
        cef::veto::testing::mark_armed(stub.port, "T2");
        stub.host
            .call(&seat(), "browser_tabs", json!({ "action": "new" }))
            .await
            .expect("the blank tab opens");
        stub.host
            .call(
                &seat(),
                "browser_tabs",
                json!({ "action": "new", "url": "https://example.com/job" }),
            )
            .await
            .expect("the tab opens");

        let began = std::time::Instant::now();
        stub.host
            .call(&seat(), "browser_navigate", json!({ "url": "https://example.com/next" }))
            .await
            .expect("the navigation runs");
        assert!(
            began.elapsed() < std::time::Duration::from_secs(2),
            "a stale blank record made the navigation wait: {:?}",
            began.elapsed(),
        );
        stub.shutdown().await;
    }

    /// **A failed post-navigation listing is said, not hidden.** The stub
    /// answers the creation, then refuses the listing; the call still
    /// succeeds (the tab exists) but carries the refusal as its own text and
    /// logs `cef_passkey_tab_list_failed`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_failed_listing_after_a_navigation_is_named() {
        static LOGGED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        struct Capture;
        impl tauri_plugin_log::log::Log for Capture {
            fn enabled(&self, _metadata: &tauri_plugin_log::log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &tauri_plugin_log::log::Record<'_>) {
                LOGGED
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(record.args().to_string());
            }
            fn flush(&self) {}
        }
        static CAPTURE: Capture = Capture;
        if tauri_plugin_log::log::set_logger(&CAPTURE).is_ok() {
            tauri_plugin_log::log::set_max_level(tauri_plugin_log::log::LevelFilter::Trace);
        }

        let stub = StubHost::start(
            "/forge-cef-list-refused-test",
            &json!({
                "browser_tabs": [
                    { "content": [{ "type": "text", "text": "creation" }] },
                    { "isError": true, "content": [{ "type": "text", "text": "list refused" }] },
                ],
            }),
        )
        .await;
        stub.list.sequence(vec![
            vec![("T0".to_owned(), "https://example.com/after-call".to_owned())],
            vec![
                ("T0".to_owned(), "https://example.com/after-call".to_owned()),
                ("T1".to_owned(), "about:blank".to_owned()),
            ],
        ]);
        cef::veto::testing::mark_armed(stub.port, "T1");

        let parts = stub
            .host
            .call(
                &seat(),
                "browser_tabs",
                json!({ "action": "new", "url": "https://example.com/job" }),
            )
            .await
            .expect("the tab exists, so the call answers");
        // **Appended, not replaced**: the creation's own answer is still
        // there, with the refusal as the second part.
        assert_eq!(parts.len(), 2, "the creation answer carries the refusal beside it: {parts:?}");
        assert_eq!(
            parts.first(),
            Some(&driver::ReplyPart::Text { text: "creation".to_owned() }),
            "the creation's answer stays: {parts:?}",
        );
        let last = parts.last().expect("the answer carries a part");
        match last {
            driver::ReplyPart::Text { text } => {
                assert!(text.contains("could not be refreshed"), "the refusal is said: {text}");
                assert!(text.contains("list refused"), "and names its reason: {text}");
            }
            driver::ReplyPart::Image { .. } => panic!("the appended part is text: {parts:?}"),
        }
        let logged = LOGGED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert!(
            logged.iter().any(|line| line.contains("cef_passkey_tab_list_failed")),
            "the refusal is logged: {logged:?}",
        );
        stub.shutdown().await;
    }

    /// **A named profile's call cues its page too.** The shared arm's cue is
    /// pinned above; the `Some(name)` arm carries its own copy, and the
    /// named profile's own ring is read when the driver receives the tool.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_named_profile_call_cues_the_page_before_driving_it() {
        let mut stub = StubHost::start(
            "/forge-cef-named-call-test",
            &json!({ "browser_navigate": { "content": [{ "type": "text", "text": "ok" }] } }),
        )
        .await;
        stub.adopt_named("work").await;

        let parts = stub
            .host
            .call(
                &seat(),
                "browser_navigate",
                json!({ "url": "https://example.com/after-call", "profile": "work" }),
            )
            .await
            .expect("the stub driver answers");
        assert_eq!(parts.len(), 1, "the stub's one part is the call's answer");
        let cues = stub.call_cues();
        let (tool, sent, applied) =
            cues.first().expect("the driver asked for the tool it was given");
        assert_eq!(tool, "browser_navigate", "the stub saw the call it answered");
        assert!(
            *sent >= 1 && *applied >= *sent,
            "the named arm cued its page before the call reached the driver: {cues:?}"
        );
        assert!(stub.named_shown(1), "the named profile's own ring took the cue");

        stub.shutdown().await;
    }

    /// **An unnamed call drives the calling seat's own host; the shared
    /// browser is reachable only by naming it.** This is the tab-leak fix at
    /// the call seam: the seat's ring takes the cue, the shared ring stays
    /// untouched, and `@shared` is what drives the shared browser (#2535).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn an_unnamed_call_drives_the_seats_own_host_and_shared_is_named() {
        let stub = StubHost::start("/forge-cef-route-test", &json!({})).await;

        stub.host
            .call(&seat(), "browser_navigate", json!({ "url": "https://example.com/own" }))
            .await
            .expect("the seat's own browser answers");
        assert!(stub.ring_value.shown(1), "the seat's own profile took the cue");
        assert!(!stub.shared_shown(1), "the default never drives the shared browser");

        // **The shared browser answers only the explicit name.**
        stub.adopt_shared();
        stub.host
            .call(
                &seat(),
                "browser_navigate",
                json!({ "url": "https://example.com/shared", "profile": profiles::SHARED_PROFILE }),
            )
            .await
            .expect("the shared browser answers when named");
        assert!(stub.shared_shown(1), "the named shared browser took the cue");

        // Each driver call cued the page of the browser it was routed to,
        // and the profile argument never crossed into the driver.
        let cues = stub.call_cues();
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert!(
            cues.iter().all(|(_, sent, applied)| *sent >= 1 && applied >= sent),
            "both calls cued the page of their own browser: {cues:?}",
        );
        let args = stub.call_args();
        assert!(
            args.iter().all(|(_, args)| args.get("profile").is_none()),
            "the profile is the host's to consume, not the driver's: {args:?}",
        );
        assert_eq!(stub.calls().len(), 2, "one driver call each");

        stub.shutdown().await;
    }

    /// **Two seats and the shared browser each drive their own host.** The
    /// cue is per profile: each call's cue lands on the ring of the browser
    /// it was routed to, so a default that slipped to the shared browser -
    /// or to the other seat's - shows as a cue on the wrong ring (#2535).
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn two_seats_and_the_shared_browser_each_drive_their_own_host() {
        let stub = StubHost::start("/forge-cef-two-seats-test", &json!({})).await;
        let other = Seat {
            org: "Other".to_owned(),
            project: "elsewhere".to_owned(),
            label: "lead".to_owned(),
        };
        let other_ring = stub.adopt_seat(&other).await;

        // The first seat's own profile takes its call's cue.
        stub.point_at(&stub.ring_value);
        stub.host
            .call(&seat(), "browser_snapshot", json!({}))
            .await
            .expect("the first seat's own browser answers");
        assert!(stub.ring_value.shown(1), "the first seat's own profile took the cue");

        // The second seat's own profile takes its own, not the first's.
        stub.point_at(&other_ring);
        stub.host
            .call(&other, "browser_snapshot", json!({}))
            .await
            .expect("the second seat's own browser answers");
        assert!(other_ring.shown(1), "the second seat's own profile took the cue");
        assert!(!other_ring.shown(2), "no second cue landed on the wrong host");

        // The shared browser takes a cue only when it is named.
        stub.point_at(&stub.shared_ring);
        stub.host
            .call(&seat(), "browser_snapshot", json!({ "profile": profiles::SHARED_PROFILE }))
            .await
            .expect("the shared browser answers when named");
        assert!(stub.shared_shown(1), "the named shared browser took the cue");
        assert!(!stub.shared_shown(2), "and no unnamed call reached it");

        assert_eq!(
            stub.calls().iter().filter(|tool| *tool == "browser_snapshot").count(),
            3,
            "one snapshot per browser, on the browser each call named",
        );
        cef::osr::Ring::unlink(&other_ring.name);
        stub.shutdown().await;
    }

    /// **The log names the host a call drives**, which is what a tab-leak
    /// report reads: the profile, the host's pid and port, and its cache
    /// root. A routing change that sent the call elsewhere moves these and
    /// goes red (#2535, "prove tab identity in the host log").
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_call_names_the_host_it_drives_in_the_log() {
        static LOGGED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        struct Capture;
        impl tauri_plugin_log::log::Log for Capture {
            fn enabled(&self, _metadata: &tauri_plugin_log::log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &tauri_plugin_log::log::Record<'_>) {
                LOGGED
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(record.args().to_string());
            }
            fn flush(&self) {}
        }
        static CAPTURE: Capture = Capture;
        if tauri_plugin_log::log::set_logger(&CAPTURE).is_ok() {
            tauri_plugin_log::log::set_max_level(tauri_plugin_log::log::LevelFilter::Trace);
        }
        if let Ok(mut lines) = LOGGED.lock() {
            lines.clear();
        }

        let stub = StubHost::start("/forge-cef-route-log-test", &json!({})).await;
        stub.host
            .call(&seat(), "browser_snapshot", json!({}))
            .await
            .expect("the seat's own browser answers");
        stub.host
            .call(&seat(), "browser_snapshot", json!({ "profile": profiles::SHARED_PROFILE }))
            .await
            .expect("the shared browser answers when named");

        let lines = LOGGED.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let named = |profile: &str| {
            let wanted = format!("profile {profile}");
            let found = lines
                .iter()
                .find(|line| {
                    line.contains("event_name browser_call_host") && line.contains(&wanted)
                })
                .unwrap_or_else(|| panic!("no host line for {profile}: {lines:?}"));
            found.clone()
        };
        let own = named("forge-lead");
        assert!(own.contains(&format!("pid {}", stub.pid)), "{own}");
        assert!(own.contains(&format!("port {}", stub.port)), "{own}");
        assert!(own.contains("profiles/forge-lead/cef"), "the seat's own cache root: {own}");
        let shared = named(profiles::SHARED_PROFILE);
        assert!(shared.contains("cef-profile"), "the shared browser's cache root: {shared}");

        stub.shutdown().await;
    }

    /// **A driver that never attached is answered with the host's own
    /// identity, and the wedged host is ended** - not handed back as a bare
    /// 30 s `initializeServer` timeout that names nothing (#2534). The next
    /// call then starts a fresh browser instead of serving a wedge.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn an_attach_timeout_names_the_host_and_ends_it() {
        let stub = StubHost::start(
            "/forge-cef-attach-timeout-test",
            &json!({
                "browser_navigate": {
                    "isError": true,
                    "content": [{ "type": "text", "text": "### Error\nTimeoutError: async \
                        initializeServer: Timeout 30000ms exceeded.\nCall log:\n  - <ws \
                        connected> ws://127.0.0.1:9413/devtools/browser/x" }],
                },
            }),
        )
        .await;

        let why = stub
            .host
            .call(&seat(), "browser_navigate", json!({ "url": "https://example.com/" }))
            .await
            .expect_err("the attach never finished");
        assert!(why.contains(&format!("pid {}", stub.pid)), "the host's pid: {why}");
        assert!(why.contains(&format!("port {}", stub.port)), "the host's port: {why}");
        assert!(why.contains(&stub.cache.display().to_string()), "the profile: {why}");
        assert!(why.contains("could not attach within its 30 s bound"), "{why}");
        assert!(
            why.contains("was ended through its SIGTERM path"),
            "the outcome is the one that happened, and the profile is flushed: {why}",
        );
        assert!(why.contains("data intact"), "the profile survives the kill: {why}");
        assert!(why.contains("its log"), "the host's own last words ride the answer: {why}",);
        assert!(
            !why.contains("initializeServer"),
            "the driver's bare timeout is not what a session reads: {why}",
        );
        assert!(!cef::spawn::alive(stub.pid), "the wedged host was ended");
        assert!(
            stub.host.hosts.lock().await.get(&stub.cache).is_none(),
            "and it is not held as this profile's live host",
        );

        stub.shutdown().await;
    }

    /// **The answer says what actually happened to the host.** An end the
    /// client had to refuse (a port that answers with a pid that is not this
    /// app's host) is never reported as a host that was ended.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_attach_failure_names_the_outcome_that_happened() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let ring_name = "/forge-cef-attach-words";
        let ring = Arc::new(cef::osr::Ring::create(ring_name, 4, 4).expect("a ring"));
        let host = cef::spawn::ActiveHost {
            pid: 4242,
            port: 9499,
            ring,
            profile: dir.path().to_path_buf(),
        };

        let ended = attach_failure(&host, true);
        assert!(ended.contains("was ended through its SIGTERM path"), "{ended}");
        assert!(ended.contains("data intact"), "{ended}");
        assert!(ended.contains("pid 4242") && ended.contains("port 9499"), "{ended}");

        let refused = attach_failure(&host, false);
        assert!(refused.contains("could not be ended"), "{refused}");
        assert!(!refused.contains("starts a fresh one"), "an ended promise is not made: {refused}",);
        cef::osr::Ring::unlink(ring_name);
    }

    /// **A raise cues the page.** Open would otherwise draw a takeover over a
    /// page that went hidden while nothing drove it; the fixture's host
    /// applies the cue, so the ring proves it was sent.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_raise_cues_the_page() {
        let stub = StubHost::start("/forge-cef-raise-cue-test", &json!({})).await;

        let raised = stub.host.show(None, None, 1).await;
        assert_eq!(
            raised.as_deref(),
            Ok(profiles::SHARED_PROFILE),
            "the seatless raise is the shared browser, and the page is told its name: {raised:?}",
        );
        assert!(stub.shared_shown(1), "the raise cued the shared browser's page");

        stub.shutdown().await;
    }

    /// **A session's hand-off with no profile raises the seat's own
    /// browser**, answered with the profile it actually raised and cued on
    /// that profile's ring - not the shared browser's. **The name is
    /// re-derived at the lower**: the page's Done passes the ask's null
    /// profile and the asking seat, and the shell resolves that seat to the
    /// same name - so the pair lands on one browser.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_hand_off_with_no_profile_raises_and_names_the_seats_own() {
        let stub = StubHost::start("/forge-cef-raise-seat-test", &json!({})).await;
        let raised = stub.host.show(None, Some(&seat()), 1).await.expect("the raise opens");
        assert_eq!(raised, auto_name(&seat()), "the answer is the seat's own profile");
        assert!(stub.ring_value.shown(1), "the seat's own profile took the cue");
        assert!(!stub.shared_shown(1), "the shared browser was not raised");

        stub.shutdown().await;
    }

    /// **A refused navigation's failure names the page the browser is on.**
    /// The driver's own text names the address it was given; the page's own
    /// title names the host that refused. Removing the failure-path probe, or
    /// the note it appends, leaves the bare reason and this goes red.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_refused_navigation_names_the_page_the_browser_is_on() {
        let refused = "browserBackend.callTool: net::ERR_CONNECTION_REFUSED at \
                       https://job-boards.greenhouse.io/yieldmo/jobs/8243707";
        let error_page = "### Result\n{\n  \"visibility\": \"visible\",\n  \"url\": \
                          \"chrome-error://chromewebdata/\",\n  \"title\": \"yieldmo.com\"\n}\n\n\
                          ### Ran Playwright code\n```js\nawait page.evaluate('...');\n```";
        let stub = StubHost::start(
            "/forge-cef-note-test",
            &json!({
                "browser_navigate": {
                    "content": [{ "type": "text", "text": refused }],
                    "isError": true,
                },
                "browser_evaluate": { "content": [{ "type": "text", "text": error_page }] },
            }),
        )
        .await;

        let why = stub
            .host
            .call(
                &seat(),
                "browser_navigate",
                json!({ "url": "https://job-boards.greenhouse.io/yieldmo/jobs/8243707" }),
            )
            .await
            .expect_err("the stub refuses the navigation");
        assert!(
            why.contains("net::ERR_CONNECTION_REFUSED"),
            "the call's own reason is kept: {why}"
        );
        assert!(
            why.contains("its own error page for \"yieldmo.com\""),
            "the host that refused is named: {why}",
        );

        stub.shutdown().await;
    }

    /// **A page that cannot be read leaves the reason alone, and the probe is
    /// retried once before it gives up.** The first evaluate after a failed
    /// navigation is the one that dies on the destroyed context, so a single
    /// attempt would drop the note in the case it exists for.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_page_that_cannot_be_read_keeps_the_reason_and_the_probe_is_retried() {
        let refused = "browserBackend.callTool: net::ERR_CONNECTION_REFUSED at https://x.test/";
        let stub = StubHost::start(
            "/forge-cef-unreadable-test",
            &json!({
                "browser_navigate": {
                    "content": [{ "type": "text", "text": refused }],
                    "isError": true,
                },
                "browser_evaluate": {
                    "content": [{ "type": "text", "text": "### Error Execution context was destroyed" }],
                    "isError": true,
                },
            }),
        )
        .await;

        let why = stub
            .host
            .call(&seat(), "browser_navigate", json!({ "url": "https://x.test/" }))
            .await
            .expect_err("the stub refuses the navigation");
        assert_eq!(why, refused, "a page that cannot be read adds nothing to the reason");
        let evaluates = stub.calls().into_iter().filter(|tool| tool == "browser_evaluate").count();
        assert_eq!(evaluates, 2, "the probe was retried once, not looped");

        stub.shutdown().await;
    }

    /// **The retry is what reads the page after a refused navigation.** The
    /// first evaluate is the one that dies on the context the navigation
    /// destroyed; a retry that ran both and kept the first answer would drop
    /// exactly the note this path exists for, with every other test green.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn the_second_probe_reads_the_page_the_first_one_lost() {
        let refused = "browserBackend.callTool: net::ERR_CONNECTION_REFUSED at https://x.test/";
        let destroyed =
            "### Error Execution context was destroyed, most likely because of a navigation";
        let error_page = "### Result\n{\n  \"visibility\": \"visible\",\n  \"url\": \
                          \"chrome-error://chromewebdata/\",\n  \"title\": \"yieldmo.com\"\n}\n\n\
                          ### Ran Playwright code\n```js\nawait page.evaluate('...');\n```";
        let stub = StubHost::start(
            "/forge-cef-retry-test",
            &json!({
                "browser_navigate": {
                    "content": [{ "type": "text", "text": refused }],
                    "isError": true,
                },
                "browser_evaluate": [
                    { "content": [{ "type": "text", "text": destroyed }], "isError": true },
                    { "content": [{ "type": "text", "text": error_page }] },
                ],
            }),
        )
        .await;

        let why = stub
            .host
            .call(&seat(), "browser_navigate", json!({ "url": "https://x.test/" }))
            .await
            .expect_err("the stub refuses the navigation");
        assert!(
            why.contains("its own error page for \"yieldmo.com\""),
            "the second attempt's page is the note: {why}",
        );
        let evaluates = stub.calls().into_iter().filter(|tool| tool == "browser_evaluate").count();
        assert_eq!(evaluates, 2, "both attempts ran");

        stub.shutdown().await;
    }

    /// **A dead page context is rebuilt on the call that meets it.** The
    /// driver's first process answers the renderer-death TypeError; a
    /// navigation to an explicit address is safe to repeat, so the call
    /// comes back from the fresh driver rather than failing through.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_dead_page_context_rebuilds_and_repeats_a_navigation() {
        let dead = "### Error TypeError: browserBackend.callTool: Cannot read properties of \
                    undefined (reading '_page')";
        let stub = StubHost::start(
            "/forge-cef-stale-nav-test",
            &json!({
                "browser_navigate": { "content": [{ "type": "text", "text": "ok" }] },
                "__once__": {
                    "browser_navigate": {
                        "content": [{ "type": "text", "text": dead }],
                        "isError": true,
                    },
                },
            }),
        )
        .await;

        let parts = stub
            .host
            .call(&seat(), "browser_navigate", json!({ "url": "https://example.com/after-call" }))
            .await
            .expect("the rebuilt driver answers");
        assert_eq!(parts.len(), 1, "the fresh driver's part is the call's answer");
        let navigates = stub.calls().into_iter().filter(|tool| tool == "browser_navigate").count();
        assert_eq!(navigates, 2, "the call was repeated once, on the rebuilt driver");
        assert_eq!(stub.starts(), 2, "a fresh driver process was started for it");
        // The rebuilt driver is retained: the next call runs on it.
        stub.host
            .call(&seat(), "browser_navigate", json!({ "url": "https://example.com/after-call" }))
            .await
            .expect("the rebuilt driver answers again");
        assert_eq!(stub.starts(), 2, "and no third driver was started for the next call");

        stub.shutdown().await;
    }

    /// **A click is never repeated after the rebuild.** It gets the reason
    /// and a working driver, so the session's next call runs on a live
    /// context instead of a stale page - and no input lands twice.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_dead_page_context_rebuilds_but_never_repeats_a_click() {
        let dead = "### Error TypeError: browserBackend.callTool: Cannot read properties of \
                    undefined (reading '_page')";
        let stub = StubHost::start(
            "/forge-cef-stale-click-test",
            &json!({
                "browser_click": { "content": [{ "type": "text", "text": "ok" }] },
                "__once__": {
                    "browser_click": {
                        "content": [{ "type": "text", "text": dead }],
                        "isError": true,
                    },
                },
            }),
        )
        .await;

        let why = stub
            .host
            .call(&seat(), "browser_click", json!({ "target": "#b" }))
            .await
            .expect_err("a click is not repeated");
        assert!(why.contains("has been rebuilt"), "the rebuild is named: {why}");
        assert!(
            why.contains("the browser's page context died"),
            "and the dead context is named in the client's words: {why}",
        );
        assert!(!why.contains("reading '_page'"), "never the driver's internal reading: {why}");
        let clicks = stub.calls().into_iter().filter(|tool| tool == "browser_click").count();
        assert_eq!(clicks, 1, "the click ran once and was not repeated");
        assert_eq!(stub.starts(), 2, "and the rebuild still started a fresh driver");

        // The rebuild stands: the next call runs against a fresh driver.
        let parts = stub
            .host
            .call(&seat(), "browser_click", json!({ "target": "#b" }))
            .await
            .expect("the fresh driver answers");
        assert_eq!(parts.len(), 1);
        assert_eq!(stub.starts(), 2, "and no further driver was started for it");

        stub.shutdown().await;
    }

    /// **A failed retry is named, and the next call does not pay another
    /// driver start.** The browser stays dead, so the rebuild's own repeat
    /// fails too; the answer says the dead context met the rebuilt driver
    /// again (never the driver's `_page` line), and the cooldown keeps the
    /// next call from starting a third driver against a browser that is not
    /// recovering.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_dead_page_context_is_retried_once_and_then_left_alone() {
        let dead = "### Error TypeError: browserBackend.callTool: Cannot read properties of \
                    undefined (reading '_page')";
        let stub = StubHost::start(
            "/forge-cef-stale-loop-test",
            &json!({
                "browser_navigate": {
                    "content": [{ "type": "text", "text": dead }],
                    "isError": true,
                },
            }),
        )
        .await;

        let first = stub
            .host
            .call(&seat(), "browser_navigate", json!({ "url": "https://x.test/" }))
            .await
            .expect_err("the retried call fails too");
        assert!(
            first.contains("the browser's page context died"),
            "the dead context is named: {first}",
        );
        assert!(first.contains("met the same dead context"), "{first}");
        assert!(!first.contains("reading '_page'"), "not the driver's internal reading: {first}");
        assert_eq!(stub.starts(), 2, "one rebuild was tried");

        let second = stub
            .host
            .call(&seat(), "browser_navigate", json!({ "url": "https://x.test/" }))
            .await
            .expect_err("the browser is still dead");
        assert!(second.contains("not been rebuilt a second time yet"), "{second}");
        assert_eq!(stub.starts(), 2, "the cooldown stopped another driver start");

        stub.shutdown().await;
    }

    /// **A rebuild that cannot start names the dead page and the start's
    /// own failure** - not the driver's `_page` line.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_rebuild_that_cannot_start_names_the_dead_page_and_why() {
        let dead = "### Error TypeError: browserBackend.callTool: Cannot read properties of \
                    undefined (reading '_page')";
        let stub = StubHost::start(
            "/forge-cef-rebuild-fail-test",
            &json!({
                "browser_navigate": { "content": [{ "type": "text", "text": "ok" }] },
                "__once__": {
                    "browser_navigate": {
                        "content": [{ "type": "text", "text": dead }],
                        "isError": true,
                    },
                },
                "__die_second__": true,
            }),
        )
        .await;

        let why = stub
            .host
            .call(&seat(), "browser_navigate", json!({ "url": "https://x.test/" }))
            .await
            .expect_err("the rebuild cannot start");
        assert!(
            why.contains("the browser's page context died"),
            "the dead context is named: {why}",
        );
        assert!(why.contains("could not be rebuilt"), "{why}");
        assert!(!why.contains("reading '_page'"), "not the driver's internal reading: {why}");
        assert_eq!(stub.starts(), 2, "the second start was attempted");

        stub.shutdown().await;
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
            async move { host.hide(None, None).await }
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
        let refused = host.hide(Some("../user-data"), None).await;
        let Err(why) = refused else {
            panic!("a traversing name is refused");
        };
        assert!(why.contains("1 to 64"), "{why}");
    }

    /// **A hand-off that named no profile lowers the seat's own browser**,
    /// the one its raise actually opened. The page passes the seat with its
    /// Done, so the same fold the raise used resolves the hide - without it
    /// the shared surface was asked to come down and the seat's stayed up
    /// over the chat.
    #[tokio::test]
    async fn hide_with_no_profile_and_a_seat_lowers_the_seats_own_browser() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let mine = seat();
        let cache = profiles::cef_cache(&paths.profiles.join(auto_name(&mine)));
        std::fs::create_dir_all(&cache).expect("the seat's cache root");
        let ring_name = "/forge-cef-hide-seat";
        let ring = Arc::new(cef::osr::Ring::create(ring_name, 8, 8).expect("a ring"));
        *host.surface.lock().await = Some(OpenSurface {
            profile: Some(auto_name(&mine)),
            dir: cache.clone(),
            host: Arc::new(cef::spawn::ActiveHost {
                pid: 0,
                port: 0,
                ring,
                profile: cache.clone(),
            }),
            epoch: 1,
        });

        assert_eq!(
            host.hide(None, Some(&mine)).await,
            Ok(true),
            "the seat's own surface comes down",
        );
        assert!(host.surface.lock().await.is_none(), "and nothing is left drawing");
        cef::osr::Ring::unlink(ring_name);
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
    /// that reach drives it.** Afterwards the directory and the record exist
    /// - the name is the seat's from now on - and the launch it attempted is
    /// its own cache root's, never the shared browser's.
    ///
    /// **No browser is launched by this test**: a FILE sits where the auto
    /// profile's cache root goes, so the launch refuses at its lock before
    /// spawning anything, and the refusal names that path - which is the
    /// proof the call was routed to the seat's own profile.
    #[tokio::test]
    async fn the_auto_profile_is_created_on_the_first_unnamed_reach() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let auto = paths.profiles.join("forge-lead");
        std::fs::create_dir_all(&auto).expect("the seat's profile directory");
        // A file where the profile's cache root should be: the launch cannot
        // make its directory or take its lock, so nothing spawns.
        std::fs::write(profiles::cef_cache(&auto), b"").expect("a file where the cache goes");
        let host = BrowserHost::new(paths.clone());
        let who = seat();

        let why = host
            .call(&who, "browser_navigate", json!({ "url": "https://example.com" }))
            .await
            .expect_err("the call cannot launch a cache root that is a file");
        assert!(
            why.contains("cannot be opened") || why.contains("packaged for macOS"),
            "the call failed for some other reason: {why}",
        );
        // **The refusing stage is the machine's own.** On a platform with no
        // engine it refuses at "packaged for macOS" before the cache root is
        // touched; on macOS it refuses at the file this test planted. The
        // routing claim below holds either way.
        #[cfg(target_os = "macos")]
        assert!(
            why.contains(&profiles::cef_cache(&auto).display().to_string()),
            "the refusal names the seat's own cache root, not the shared browser's: {why}",
        );
        assert_eq!(
            profiles::owner_of(&auto),
            Ok(Some(who)),
            "the seat's own name is recorded, its project qualifying its label",
        );
        assert!(
            cef::spawn::read_record(&paths.cef_shared()).is_none(),
            "and nothing was driven through the shared browser",
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
            epoch: 1,
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

        host.named_profile(&one, &auto_name(&one)).await.expect("the first lead's own");
        host.named_profile(&two, &auto_name(&two))
            .await
            .expect("the second lead's own, not a collision");

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

        host.named_profile(&who, &auto_name(&who)).await.expect("the folded name is a profile");
        assert_eq!(
            profiles::owner_of(&paths.profiles.join("my-app-lead")),
            Ok(Some(who)),
            "the space folded to a dash and the profile was recorded",
        );
    }

    /// **A taken auto name is refused with the reason, never suffixed, and
    /// the call never falls back to the shared browser.** The unnamed call
    /// drives the seat's own profile, so a name another seat already holds is
    /// the call's refusal - the shipped fall-through to the shared browser
    /// is exactly the tab leak #2535 names.
    #[tokio::test]
    async fn a_taken_auto_name_refuses_the_call_without_a_suffix_or_a_fallback() {
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

        let why = host
            .call(&who, "browser_snapshot", json!({}))
            .await
            .expect_err("the name belongs to another seat");
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
        assert!(
            !host.used().await.expect("the mark reads"),
            "nothing was driven: a refused name is not the shared browser",
        );
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
            host.show(Some("scout"), Some(&mine), 1).await.expect_err("another seat's profile");
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

    /// **An adopted host is vetoed before it serves.** The record a previous
    /// client left names a live endpoint; the adoption path must arm the
    /// pause on it, and the stub is the proof the pause was asked for.
    #[cfg(all(target_os = "macos", feature = "testing"))]
    #[tokio::test]
    async fn an_adopted_host_is_vetoed_before_it_serves() {
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
        let stub = cef::veto::testing::endpoint().await.expect("the stub endpoint");
        let ring_name = "/forge-cef-adoption-test".to_owned();
        let _ring = cef::osr::Ring::create(&ring_name, 8, 8).expect("a ring");
        cef::spawn::write_record(
            &cache,
            &cef::spawn::Record {
                pid: std::process::id(),
                port: stub.port,
                ring: ring_name.clone(),
            },
        )
        .expect("the record");

        let adopted = host.browser_for_locked(&cache).await.expect("the host is adopted");
        assert_eq!(adopted.port, stub.port, "the record's live host is the one adopted");
        let attach = stub.first("Target.setAutoAttach").expect("the adoption armed the veto");
        assert_eq!(
            attach["params"]["waitForDebuggerOnStart"], true,
            "a target created after adoption stops before its document runs",
        );

        cef::osr::Ring::unlink(&ring_name);
        cef::spawn::remove_record(&cache);
    }

    /// **An adopted host keeps serving when its veto cannot be armed.** The
    /// endpoint answers `/json/version` but refuses every DevTools socket, so
    /// both tries fail; the adoption still answers the host rather than
    /// taking a browser the person may be looking at away.
    #[cfg(all(target_os = "macos", feature = "testing"))]
    #[tokio::test]
    async fn an_adopted_host_serves_even_when_it_cannot_be_vetoed() {
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
        let stub =
            cef::veto::testing::endpoint_without_websocket().await.expect("the stub endpoint");
        let ring_name = "/forge-cef-adoption-unvetoed".to_owned();
        let _ring = cef::osr::Ring::create(&ring_name, 8, 8).expect("a ring");
        cef::spawn::write_record(
            &cache,
            &cef::spawn::Record {
                pid: std::process::id(),
                port: stub.port,
                ring: ring_name.clone(),
            },
        )
        .expect("the record");

        let adopted = host
            .browser_for_locked(&cache)
            .await
            .expect("the host is still served when its veto cannot be armed");
        assert_eq!(adopted.port, stub.port, "the record's live host is the one adopted");
        assert!(stub.calls.lock().expect("the record").is_empty(), "no DevTools call got through");

        cef::osr::Ring::unlink(&ring_name);
        cef::spawn::remove_record(&cache);
    }

    /// **A held carry survives a client restart and a blank read.** The hold
    /// is a file beside the profile, so a NEW host process over the same
    /// cache keeps the marker when the adopted page reads about:blank - the
    /// restart the marker exists for. A real page read releases it.
    #[cfg(all(target_os = "macos", feature = "testing"))]
    #[tokio::test]
    async fn a_held_carry_survives_a_blank_read_and_a_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: dir.path().join("stack"),
            user_data: dir.path().join("user-data"),
            output: dir.path().join("output"),
            profiles: dir.path().join("profiles"),
        };
        let cache = paths.cef_shared();
        std::fs::create_dir_all(&cache).expect("the cache root");
        write_carried_page(&cache, Some("https://example.com/keep"));
        hold_carry(&cache, "https://example.com/keep");

        // A fresh host: nothing held in memory survives a restart.
        let host = BrowserHost::new(paths.clone());
        host.record_carry(&cache, "about:blank").await;
        assert_eq!(
            std::fs::read_to_string(cache.join("page")).expect("the marker"),
            "https://example.com/keep",
            "a blank read keeps a held carry",
        );
        assert!(held_carry(&cache).is_some(), "and the hold stays for the next launch");

        // **The engine's own error page is not a page either.** A launch that
        // was accepted and then failed leaves `chrome-error://`, and
        // releasing on it would erase exactly what the hold is for.
        host.record_carry(&cache, "chrome-error://chromewebdata/").await;
        assert_eq!(
            std::fs::read_to_string(cache.join("page")).expect("the marker"),
            "https://example.com/keep",
            "an error page keeps a held carry",
        );
        assert!(held_carry(&cache).is_some(), "and the hold too");

        host.record_carry(&cache, "https://example.com/next").await;
        assert_eq!(
            std::fs::read_to_string(cache.join("page")).expect("the marker"),
            "https://example.com/next",
            "a real page read rewrites the marker",
        );
        assert!(held_carry(&cache).is_none(), "and releases the hold");
    }

    /// A stand-in host process, for the launch path: `/json/version`, one
    /// DevTools socket, and a line per process and per call in its profile.
    #[cfg(all(target_os = "macos", feature = "testing"))]
    /// `--no-websocket` refuses the upgrade, which is a host whose veto
    /// cannot be armed.
    const STUB_HOST: &str = r#"
const crypto = require('node:crypto');
const http = require('node:http');
const fs = require('node:fs');
const args = process.argv.slice(2);
const port = Number(args[args.indexOf('--port') + 1]);
const profile = args[args.indexOf('--profile') + 1];
const log = profile + '/stub-calls.jsonl';
const silent = args.includes('--no-websocket');
const refuseNavigate = args.includes('--refuse-navigate');
fs.appendFileSync(log, JSON.stringify({ started: port }) + '\n');
function frame(text) {
  const body = Buffer.from(text);
  const head = body.length < 126
    ? Buffer.from([0x81, body.length])
    : Buffer.from([0x81, 126, body.length >> 8, body.length & 0xff]);
  return Buffer.concat([head, body]);
}
const server = http.createServer((request, response) => {
  if (request.url === '/json/version') {
    response.setHeader('content-type', 'application/json');
    response.end(JSON.stringify({
      Browser: 'stub',
      webSocketDebuggerUrl: `ws://127.0.0.1:${port}/devtools/browser/stub`,
    }));
    return;
  }
  response.statusCode = 404;
  response.end();
});
server.on('upgrade', (request, socket) => {
  if (silent) { socket.destroy(); return; }
  const accept = crypto.createHash('sha1')
    .update(request.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11')
    .digest('base64');
  socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
    + `Sec-WebSocket-Accept: ${accept}\r\n\r\n`);
  let buffered = Buffer.alloc(0);
  socket.on('data', (chunk) => {
    buffered = Buffer.concat([buffered, chunk]);
    for (;;) {
      if (buffered.length < 2) return;
      const opcode = buffered[0] & 0x0f;
      const masked = (buffered[1] & 0x80) !== 0;
      let length = buffered[1] & 0x7f;
      let at = 2;
      if (length === 126) {
        if (buffered.length < 4) return;
        length = buffered.readUInt16BE(2);
        at = 4;
      } else if (length === 127) {
        if (buffered.length < 10) return;
        length = Number(buffered.readBigUInt64BE(2));
        at = 10;
      }
      const maskAt = at;
      const bodyAt = at + (masked ? 4 : 0);
      if (buffered.length < bodyAt + length) return;
      const payload = Buffer.from(buffered.subarray(bodyAt, bodyAt + length));
      if (masked) for (let i = 0; i < payload.length; i++) payload[i] ^= buffered[maskAt + (i % 4)];
      buffered = buffered.subarray(bodyAt + length);
      if (opcode === 8) { socket.end(); return; }
      if (opcode !== 1) continue;
      const call = JSON.parse(payload.toString());
      fs.appendFileSync(log, JSON.stringify({ method: call.method, params: call.params }) + '\n');
      if (refuseNavigate && call.method === 'Page.navigate') {
        socket.write(frame(JSON.stringify({
          id: call.id,
          error: { code: -32601, message: 'the navigation is refused' },
        })));
        continue;
      }
      socket.write(frame(JSON.stringify({ id: call.id, result: {} })));
      if (call.method === 'Target.setAutoAttach') {
        socket.write(frame(JSON.stringify({
          method: 'Target.attachedToTarget',
          params: {
            sessionId: 'S1',
            targetInfo: { targetId: 'T1', type: 'page' },
            waitingForDebugger: call.params.waitForDebuggerOnStart === true,
          },
        })));
      }
    }
  });
});
server.listen(port, '127.0.0.1');
"#;

    /// Write the stand-in host and the wrapper `spawn_with` executes, and
    /// point the launch's spawn at it.
    #[cfg(all(target_os = "macos", feature = "testing"))]
    fn stub_host_process(dir: &Path, extra: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        // `.cjs`: the stub is CommonJS, and a `.mjs` there has no `require`.
        let script = dir.join("stub-host.cjs");
        std::fs::write(&script, STUB_HOST).expect("the stand-in host");
        let wrapper = dir.join("stub-host.sh");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec /usr/bin/env node '{}' {extra} \"$@\"\n", script.display()),
        )
        .expect("the wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("the wrapper is executable");
        wrapper
    }

    /// **A launch runs the installer before it holds a host, and refuses one
    /// it cannot arm.** One test, two launches: the two stand-ins fight over
    /// the same free-port range when Nextest runs tests side by side, and a
    /// launch that reached the other's endpoint would prove nothing.
    ///
    /// The launch spawns a real process through the test's stand-in binary,
    /// so the pid-wait and the teardown run for real, and the stub's own log
    /// is the proof. A stub creates no ring, so the first launch ends at the
    /// ring map - past the veto, which is the claim; the adoption test
    /// covers the held host.
    #[cfg(all(target_os = "macos", feature = "testing"))]
    #[tokio::test]
    async fn a_launch_vetoes_its_host_or_serves_nothing() {
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
        // **A carried page the launch accepts keeps its hold** until a real
        // page read: `Opened` is the navigation accepted, not landed.
        let carried = "https://example.com/carried";
        write_carried_page(&cache, Some(carried));
        cef::spawn::set_test_host_binary(Some(stub_host_process(dir.path(), "")));

        let launched = host.browser_for_locked(&cache).await;
        cef::spawn::set_test_host_binary(None);

        let why = launched.expect_err("a stub creates no ring, so the launch ends there");
        assert!(!why.contains("vetoed"), "the veto installed before the ring was mapped: {why}");
        let calls =
            std::fs::read_to_string(cache.join("stub-calls.jsonl")).expect("the stub's log");
        assert!(calls.contains("\"Target.setAutoAttach\""), "the launch armed the pause: {calls}",);
        assert_eq!(
            held_carry(&cache).as_deref(),
            Some(carried),
            "the launch held the carried page it opened: {calls}",
        );
        assert_eq!(
            std::fs::read_to_string(cache.join("page")).expect("the marker"),
            carried,
            "and left the marker alone",
        );
        assert!(
            cef::spawn::read_record(&cache).is_none(),
            "a launch that did not complete left no record behind",
        );
        host.hosts.lock().await.clear();

        // **A host whose pause cannot be armed is not served.** The endpoint
        // answers but accepts no DevTools socket; the launch tears the first
        // host down, relaunches once, and refuses rather than driving an
        // unvetoed profile.
        let refused = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: refused.path().join("stack"),
            user_data: refused.path().join("user-data"),
            output: refused.path().join("output"),
            profiles: refused.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let cache = paths.cef_shared();
        std::fs::create_dir_all(&cache).expect("the cache root");
        cef::spawn::set_test_host_binary(Some(stub_host_process(refused.path(), "--no-websocket")));

        let launched = host.browser_for_locked(&cache).await;
        cef::spawn::set_test_host_binary(None);

        let why = launched.expect_err("an unvetoed host is never served");
        assert!(why.contains("could not be vetoed"), "{why}");
        let calls =
            std::fs::read_to_string(cache.join("stub-calls.jsonl")).expect("the stub's log");
        let launched_count = calls
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line.get("started").is_some())
            .count();
        assert_eq!(launched_count, 2, "one relaunch, then a refusal: {calls}");
        assert!(
            cef::spawn::read_record(&cache).is_none(),
            "the refused launch left no record behind",
        );
        host.hosts.lock().await.clear();

        // **A carried navigation the engine refuses keeps its hold too.** The
        // stub accepts the socket and arms the page, then refuses the
        // navigation; the launch names it and holds the page it could not
        // open, so the next launch retries it.
        let refused_carry = tempfile::tempdir().expect("a temp dir");
        let paths = StackPaths {
            stack: refused_carry.path().join("stack"),
            user_data: refused_carry.path().join("user-data"),
            output: refused_carry.path().join("output"),
            profiles: refused_carry.path().join("profiles"),
        };
        let host = BrowserHost::new(paths.clone());
        let cache = paths.cef_shared();
        std::fs::create_dir_all(&cache).expect("the cache root");
        let carried = "https://example.com/refused";
        write_carried_page(&cache, Some(carried));
        cef::spawn::set_test_host_binary(Some(stub_host_process(
            refused_carry.path(),
            "--refuse-navigate",
        )));

        let launched = host.browser_for_locked(&cache).await;
        cef::spawn::set_test_host_binary(None);

        let why = launched.expect_err("a stub creates no ring, so the launch ends there");
        assert!(!why.contains("vetoed"), "the veto installed: {why}");
        assert_eq!(
            held_carry(&cache).as_deref(),
            Some(carried),
            "a refused carried navigation holds the page",
        );
        assert_eq!(
            std::fs::read_to_string(cache.join("page")).expect("the marker"),
            carried,
            "and keeps the marker",
        );
        host.hosts.lock().await.clear();
    }
}

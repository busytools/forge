//! The server's browser fleet: one host and one driver per open profile.
//!
//! **Every browser is the server's.** A profile is a cache directory and a
//! name; opening one spawns its CEF host (the bundle beside the server) and
//! the vendored Playwright driver attached to that host's endpoint. The
//! frames the host encodes are published as the newest value per profile, so
//! a viewer attaching at any moment is served the latest page rather than a
//! queue, and exactly one viewer holds the controller role that input obeys.

#![allow(unsafe_code, reason = "statvfs reads the disk's free space through libc")]

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use forge_primitives::SessionSlot;
use forge_primitives::browser::{
    BrowserAudio, BrowserDownload, BrowserFileAsk, BrowserFrame, BrowserInputEvent,
    BrowserNavState, BrowserPart, BrowserTabList, BrowserUpload, BrowserViewState,
    MAX_UPLOAD_BYTES, MAX_UPLOAD_FILES, MouseAction, ProfileKey,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, watch};

use crate::control::HostEvent;
use crate::driver::{self, Driver, TabDriver};
use crate::idle::{self, Step, Touch};
use crate::input;
use crate::profiles::Store;
use crate::spawn::{self, ActiveHost};
use crate::tabs::TabRegistry;
use crate::window::{Cdp, EVENT_BACKLOG, Events};
use crate::{channel, engine, osr, veto};

/// What one call knows about the browser it looked up, and the page it drives.
struct Driven {
    generation: u64,
    pid: u32,
    port: u16,
    tab: u16,
    /// The call's remaining budget, so a replacement's own listing cannot
    /// outlive it (an upload is deliberately bounded at 10 s).
    bound: Duration,
}

/// The call a failure is handled for: the driver tool it ran as, the name the
/// caller used, and the arguments a repeat would send. **The two names differ
/// only for a tool forge composes**, so a failure still names what the session
/// asked for.
struct Call<'a> {
    tool: &'a str,
    reported: &'a str,
    args: &'a Value,
}

/// A call as the driver sees it, beside the name the session is told about.
struct DriverCall<'a> {
    tool: &'a str,
    reported: &'a str,
    args: Value,
}

/// The driver tool a call runs as, and the name a failure reports. **They
/// differ only for a tool forge composes**, whose caller never called the
/// evaluate or the snippet underneath.
///
/// `Err` is the call's own shape fault - a forced click with no target, a
/// wait carrying both a time and an expression - decided before a profile is
/// opened or the call lock taken, so the refusal is unconditional.
fn driver_call(tool: &str, args: Value) -> Result<DriverCall<'_>, String> {
    if tool == "browser_challenge_state" {
        return Ok(DriverCall {
            tool: "browser_evaluate",
            reported: tool,
            args: json!({ "function": crate::challenge::state_js() }),
        });
    }
    match crate::custom::route(tool, args)? {
        crate::custom::Routed::Upstream { args } => Ok(DriverCall { tool, reported: tool, args }),
        crate::custom::Routed::Snippet(code) => Ok(DriverCall {
            tool: "browser_run_code_unsafe",
            reported: tool,
            args: json!({ "code": code }),
        }),
    }
}

/// One browser call's tool, budget and the host it drives.
struct Drive<'a> {
    tool: &'a str,
    args: Value,
    bound: Duration,
    port: u16,
    generation: u64,
}

/// The driver a test's replacement installs, one-shot.
#[cfg(test)]
type Successor = Arc<dyn Fn() -> Arc<dyn TabDriver> + Send + Sync>;

/// One profile's clock, as one idle pass reads it.
struct Candidate {
    /// The profile's key, which names it in the calls that follow.
    key: String,
    touch: Touch,
    /// Whether a viewer is attached: **a viewer is a use**.
    viewed: bool,
    /// The host died since the last call read it, so its parks are not a
    /// person's wait and must not hold the profile open.
    restarted: bool,
}

/// The view a profile's browser starts at, in device pixels. A controlling
/// client's surface resizes it while it is open.
pub const VIEW_WIDTH: u32 = 1440;
pub const VIEW_HEIGHT: u32 = 900;

/// How long a creation waits for the host's own `TabCreated` line.
const TAB_EVENT_BOUND: Duration = Duration::from_secs(10);

/// The id a requested tab carries to its birth, so a popup reported beside it
/// cannot be mistaken for it.
static TAB_REQUESTS: AtomicU64 = AtomicU64::new(1);

/// How long a call waits for the profile's turn before it is refused in
/// words. **The driver's own call budget**, shortened for a test so the
/// refusal is exercised without a 150-second wait.
#[cfg(not(test))]
const CALL_QUEUE_BOUND: Duration = driver::CALL_TIMEOUT;
#[cfg(test)]
const CALL_QUEUE_BOUND: Duration = Duration::from_millis(100);

/// How long a creation waits for the page target its tab pairs with.
const TAB_TARGET_BOUND: Duration = Duration::from_secs(5);

/// How long a newly created tab's target is waited for the passkey veto.
const TAB_ARM_BOUND: Duration = Duration::from_secs(3);

/// How long a fresh host is given to report its boot tab. **Bounded**: a host
/// that never reports one still serves, with the miss named.
const BOOT_EVENT_BOUND: Duration = Duration::from_secs(5);

/// How long a profile that just rebuilt its driver waits before another call
/// tries a rebuild: long enough that a run of calls fails fast, short enough
/// that a browser which recovers is picked up by the next fire.
const REBUILD_COOLDOWN: Duration = Duration::from_secs(60);

/// The server's per-profile browser service.
pub struct BrowserService {
    /// `…/forge/browser`: the profiles, the vendored stack and the driver's
    /// output directory all hang off it.
    root: PathBuf,
    /// The vendored node and Playwright package the driver runs from.
    stack: PathBuf,
    store: Store,
    /// The config's profile names, by org and by (org, project). Empty until
    /// the boot installs them; resolution then falls back to `default`.
    profiles_config: Mutex<ProfilesConfig>,
    profiles: AsyncMutex<HashMap<String, Arc<Profile>>>,
    /// One gate per profile key, held across a name's open and its teardown.
    ///
    /// **A same-name call waits on its own profile's run**, not on the whole
    /// fleet: the map lock is released before a host is ended or a directory
    /// removed, so another profile's entry is served while that work runs.
    gates: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    ids: AtomicU16,
    /// The idle policy: how many minutes an untouched profile may sit.
    /// `None` until the boot configures it, and no sweep runs then.
    idle: Mutex<Option<Idle>>,
    /// The rows each profile's strip subscribes to, created on demand so a
    /// strip can attach before any call has opened the host.
    rows: Mutex<HashMap<String, watch::Sender<BrowserTabList>>>,
    /// Tabs a parked hand-off is waiting on the person in, by session.
    holds: Mutex<HashSet<(SessionSlot, u16)>>,
    /// The sessions a sweep last saw alive, so a hand-off resolving after its
    /// owner went can tell the difference.
    live: Mutex<HashSet<SessionSlot>>,
    /// **Seats the workspace has said ended.** A sweep rebuilds `live` from
    /// configuration, which still lists every project's lead, so a closed
    /// lead would come back with it; this set is what the sweep cannot
    /// re-add. A successor clears its slot by warming or by its first call.
    ended_slots: Mutex<HashSet<SessionSlot>>,
    /// Which tab each attached viewer is looking at, so a detach knows what
    /// to leave.
    where_viewing: Mutex<HashMap<u64, (String, u16)>>,
    /// The `[browser]` encoding pair: frames per second and lossy quality.
    encoding: Mutex<(u16, u8)>,
    /// **One launch at a time**: the free-port probe cannot see a launch in
    /// flight, so two concurrently starting hosts would otherwise pick the
    /// same port and one would drive the other's page. Every profile holds
    /// this same lock.
    launch: Arc<AsyncMutex<()>>,
    /// **A stop is final.** `stop_all` drains the map, and an attach that
    /// already held a profile could otherwise start a host after the stop;
    /// every launch path refuses once this is up.
    stopped: Arc<AtomicBool>,
    /// **One sweep at a time, and only forward.** The value is the generation
    /// of the sweep last applied; a capture at or below it is dropped, so a
    /// stale seat set cannot rebuild `live` over a newer one. The generations
    /// are an order because the workspace serialises its captures.
    sweep_lock: AsyncMutex<u64>,
    /// The popup births that asked for a pairing pass, for [`Self::start_pairing`].
    pairing: mpsc::UnboundedSender<PairingRequest>,
    /// The pairing queue, until `start_pairing` takes it.
    pairing_queue: Mutex<Option<mpsc::UnboundedReceiver<PairingRequest>>>,
    /// Whether `start_pairing` has already spawned its consumer.
    pairing_started: AtomicBool,
}

/// The idle release's settings, as the boot configures them.
#[derive(Clone, Copy)]
struct Idle {
    minutes: u64,
}

/// The `browser_profile` names the config resolved, by org and by project.
#[derive(Default)]
struct ProfilesConfig {
    orgs: HashMap<String, String>,
    projects: HashMap<(String, String), String>,
}

impl BrowserService {
    /// A service at `root`, with the vendored driver stack read from `stack`.
    ///
    /// **A popup's pairing pass starts here**, not at a boot call that a later
    /// edit can drop: a birth that nothing pairs is a row the person never
    /// sees, and nothing else in the service is guaranteed to run.
    pub fn at(root: PathBuf, stack: PathBuf) -> Arc<Self> {
        let (pairing, pairing_queue) = mpsc::unbounded_channel();
        let service = Arc::new(Self {
            store: Store::at(&root),
            root,
            stack,
            profiles_config: Mutex::new(ProfilesConfig::default()),
            profiles: AsyncMutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            ids: AtomicU16::new(1),
            idle: Mutex::new(None),
            rows: Mutex::new(HashMap::new()),
            holds: Mutex::new(HashSet::new()),
            live: Mutex::new(HashSet::new()),
            ended_slots: Mutex::new(HashSet::new()),
            where_viewing: Mutex::new(HashMap::new()),
            encoding: Mutex::new((15, 75)),
            launch: Arc::new(AsyncMutex::new(())),
            stopped: Arc::new(AtomicBool::new(false)),
            sweep_lock: AsyncMutex::new(0),
            pairing,
            pairing_queue: Mutex::new(Some(pairing_queue)),
            pairing_started: AtomicBool::new(false),
        });
        service.start_pairing();
        service
    }

    /// Set the `[browser]` encoding pair every host spawned from here reads.
    pub fn configure_encoding(&self, fps: u16, quality: u8) {
        *self.encoding.lock().unwrap_or_else(PoisonError::into_inner) = (fps, quality);
    }

    fn encoding(&self) -> (u16, u8) {
        *self.encoding.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Set the idle policy and, from this point, the sweep's clock.
    pub fn configure_idle(&self, minutes: u64) {
        *self.idle.lock().unwrap_or_else(PoisonError::into_inner) = Some(Idle { minutes });
    }

    /// Run the idle sweep for the process's life, once.
    ///
    /// **Never call it before `configure_idle`**: a sweep with no policy has
    /// nothing to ask through and a tick with no settings does nothing.
    pub fn start_idle(self: &Arc<Self>) {
        static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if STARTED.swap(true, Ordering::AcqRel) {
            return;
        }
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(idle::SWEEP);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                service.idle_pass().await;
            }
        });
    }

    /// Pair a page's popup as it is born, for the process's life, once.
    ///
    /// **A popup has no tool call behind it.** A person clicking a sign-in
    /// button asks this service for nothing, so a birth that waited for the
    /// next call would leave the strip unchanged and the tab undriveable;
    /// this pass is what turns the host's `TabCreated` into a row, a pushed
    /// list and a tab a session can be driven on.
    pub fn start_pairing(self: &Arc<Self>) {
        if self.pairing_started.swap(true, Ordering::AcqRel) {
            return;
        }
        // **A service built outside a runtime cannot spawn its pass**, and
        // leaving the flag down lets the first one inside a runtime start it.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.pairing_started.store(false, Ordering::Release);
            return;
        };
        let taken = self.pairing_queue.lock().unwrap_or_else(PoisonError::into_inner).take();
        let Some(mut queue) = taken else { return };
        // **The task holds a Weak, not the service.** A test builds many
        // services; a pass that owns one would keep every one of them, and
        // its channel, alive for the process's life.
        let weak = Arc::downgrade(self);
        handle.spawn(async move {
            while let Some(request) = queue.recv().await {
                let Some(service) = weak.upgrade() else { break };
                let key = request.key.clone();
                let profile = service.profiles.lock().await.get(&key).cloned();
                let Some(profile) = profile else { continue };
                let (driver, port) = {
                    let state = profile.state.lock().await;
                    (
                        state.driver.as_ref().map(Arc::clone),
                        state.host.as_ref().map(|host| host.port),
                    )
                };
                // **A pass with no driver yet is retried**, or a birth that
                // arrived while the profile was still coming up waits for a
                // call that never comes.
                let (Some(driver), Some(port)) = (driver, port) else {
                    if tokio::time::Instant::now() < request.until {
                        tokio::time::sleep(POPUP_PAIR_EVERY).await;
                        let _ = service.pairing.send(request);
                    }
                    continue;
                };
                // **One turn per profile, like a call's**: a pairing pass and
                // a call never pair against each other's listing.
                let outcome = {
                    let _turn = profile.call_lock.lock().await;
                    tokio::time::timeout(PAIR_BOUND, service.reconcile(&profile, &driver, port))
                        .await
                };
                let retry = if let Ok(listed) = outcome {
                    // **A pass that could not list pairs nothing**, so its
                    // birth is still the receiver's: it is retried exactly as
                    // one still waiting in the deferred list is.
                    !listed || popup_birth_waits(&profile)
                } else {
                    tracing::debug!(
                        event_name = "browser_pair_slow",
                        profile = %key,
                        tab = request.tab,
                        opener = request.opener,
                        url = %request.url,
                        "a popup's pairing pass outlasted its bound"
                    );
                    true
                };
                // **A popup's page appears only once its first document has
                // started.** The birth that asked for this pass may have
                // arrived before the driver could list its page, so a birth
                // still waiting is tried again rather than left for a call
                // that a person clicking a sign-in button never makes.
                if retry && tokio::time::Instant::now() < request.until {
                    tokio::time::sleep(POPUP_PAIR_EVERY).await;
                    let _ = service.pairing.send(request);
                }
            }
        });
    }

    /// The service the server runs: profiles under forge's app-support tree
    /// and the vendored stack installed beside them.
    pub fn installed() -> Result<Arc<Self>, String> {
        let root = crate::host_bundle::app_support()?.join("browser");
        let store = Store::at(&root);
        // **The deletion can be gigabytes, so it does not hold up boot.** It
        // runs once off this thread; the marker keeps a later boot off it,
        // and the one INFO line names every path that went.
        std::thread::spawn(move || match store.delete_legacy() {
            Ok(legacy) if legacy.ran => tracing::info!(
                "the legacy browser profiles are deleted (event_name \
                 browser_profiles_legacy_deleted, removed {:?})",
                legacy.removed,
            ),
            Ok(_) => {}
            Err(why) => tracing::warn!(
                "the legacy browser profiles could not be deleted (event_name \
                 browser_profiles_legacy_delete_failed): {why}"
            ),
        });
        Ok(Self::at(root.clone(), root.join("stack")))
    }

    /// Set the config's profile names, by org and by (org, project).
    pub fn configure_profiles(
        &self,
        orgs: HashMap<String, String>,
        projects: HashMap<(String, String), String>,
    ) {
        *self.profiles_config.lock().unwrap_or_else(PoisonError::into_inner) =
            ProfilesConfig { orgs, projects };
    }

    /// The profile a seat drives: the project's override, else its org's,
    /// else `<org>/default`. **A bad key is a refusal, never a fallback**:
    /// the config validates names at load, so this can only fail for a seat
    /// naming an org that is not a path segment, and that call should say so.
    pub fn profile_for(&self, seat: &SessionSlot) -> Result<ProfileKey, String> {
        let config = self.profiles_config.lock().unwrap_or_else(PoisonError::into_inner);
        let name = config
            .projects
            .get(&(seat.org().to_owned(), seat.project().to_owned()))
            .or_else(|| config.orgs.get(seat.org()))
            .map_or("default", String::as_str);
        ProfileKey::of(seat.org(), name)
    }

    fn target(&self, seat: &SessionSlot) -> Result<ProfileKey, String> {
        self.profile_for(seat)
    }

    /// Where a profile's cache root sits: `<root>/profiles/<org>/<name>/cef`.
    pub fn profile_dir(&self, key: &ProfileKey) -> PathBuf {
        self.store.org_profile(key).join("cef")
    }

    /// Where a profile's takes sit: `<root>/profiles/<org>/<name>/recordings`.
    fn recordings_dir(&self, key: &ProfileKey) -> PathBuf {
        self.store.org_profile(key).join("recordings")
    }

    /// The gate for one profile key, held across its open and its teardown.
    fn gate(&self, key: &str) -> Arc<AsyncMutex<()>> {
        self.gates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Open the asking seat's profile, or answer the one already open. **The
    /// profile's gate is held across the entry and the launch**, so a close
    /// waits for the launch rather than tearing the directory down under it.
    pub async fn open(&self, seat: &SessionSlot) -> Result<Arc<Profile>, String> {
        let key = self.target(seat)?;
        let gate = self.gate(key.as_str());
        let _held = gate.lock().await;
        let profile = self.entry_locked(seat).await?;
        profile.ensure(&self.root, self.stack()).await?;
        // **No identity repair on this path.** Attach opens a profile too, and
        // a just-installed driver that has gone wedged would hold the
        // profile gate for a listing's whole bound; the call and watch paths
        // re-pair lazily by target (`tab_index`, `live_target`).
        // **A host this entry lost took its hand-offs with it.** The park was
        // a number in that host; the same number in the new one belongs to
        // whoever adopted it, and must not be closed by that old answer.
        if profile.restarted.swap(false, Ordering::AcqRel) {
            self.forget_holds(key.as_str());
        }
        self.publish_rows(key.as_str()).await;
        Ok(profile)
    }

    /// The profile's map entry, created on first use and left alone after.
    #[cfg(test)]
    async fn entry(&self, seat: &SessionSlot) -> Result<Arc<Profile>, String> {
        let key = self.target(seat)?;
        let gate = self.gate(key.as_str());
        let _held = gate.lock().await;
        let profile = self.entry_locked(seat).await?;
        self.publish_rows(key.as_str()).await;
        Ok(profile)
    }

    /// A profile whose driver is a fake: the call path runs without a host.
    #[cfg(test)]
    pub(crate) async fn install_fake<D>(&self, seat: &SessionSlot, driver: D)
    where
        D: TabDriver + 'static,
    {
        let driver: Arc<dyn TabDriver> = Arc::new(driver);
        let profile = self.entry(seat).await.expect("the profile entry");
        let mut state = profile.state.lock().await;
        state.driver = Some(driver);
        drop(state);
        // A test's re-install is a new fake host under the same entry, so an
        // end from the previous test step does not refuse the next call.
        profile.ended.store(false, Ordering::Release);
        profile.touch.lock().unwrap_or_else(PoisonError::into_inner).touched();
        profile.up.store(true, Ordering::Release);
    }

    /// The same seam with the driver a replacement installs: **a call that
    /// recovers must really replace the driver**, not retry the wedged one,
    /// and only a successor makes that observable.
    #[cfg(test)]
    pub(crate) async fn install_fake_with_successor<D, F>(
        &self,
        seat: &SessionSlot,
        driver: D,
        successor: F,
    ) where
        D: TabDriver + 'static,
        F: Fn() -> Arc<dyn TabDriver> + Send + Sync + 'static,
    {
        self.install_fake(seat, driver).await;
        let key = self.profile_for(seat).expect("the key");
        let profile =
            self.profiles.lock().await.get(key.as_str()).cloned().expect("the profile entry");
        *profile.successor.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Arc::new(successor));
    }

    /// The same seam with the host's boot tab seeded, for the boot-adoption
    /// and stale-hand-off tests.
    #[cfg(test)]
    pub(crate) async fn install_fake_with_boot<D>(
        &self,
        seat: &SessionSlot,
        driver: D,
        boot: (u16, String),
    ) where
        D: TabDriver + 'static,
    {
        self.install_fake(seat, driver).await;
        let key = self.profile_for(seat).expect("the key");
        let profile =
            self.profiles.lock().await.get(key.as_str()).cloned().expect("the profile entry");
        *profile.boot.lock().unwrap_or_else(PoisonError::into_inner) = Some(boot);
    }

    /// The entry itself, with the key's gate already held.
    async fn entry_locked(&self, seat: &SessionSlot) -> Result<Arc<Profile>, String> {
        let key = self.target(seat)?;
        let dir = self.profile_dir(&key);
        let mut profiles = self.profiles.lock().await;
        if let Some(profile) = profiles.get(key.as_str()) {
            // **Every call and every attach is a use**: the idle clock
            // starts again here.
            profile.touch.lock().unwrap_or_else(PoisonError::into_inner).touched();
            return Ok(Arc::clone(profile));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|why| format!("the profile's directory cannot be made: {why}"))?;
        let profile = Arc::new(Profile {
            key: key.as_str().to_owned(),
            id: self.ids.fetch_add(1, Ordering::Relaxed),
            dir,
            frames: Mutex::new(HashMap::new()),
            frame_seqs: Mutex::new(HashMap::new()),
            nav: Mutex::new(HashMap::new()),
            failures: watch::channel(None).0,
            files: watch::channel(None).0,
            playwright_ask: Mutex::new(None),
            next_file_ask: AtomicU64::new(1 << 32),
            file_decision: AsyncMutex::new(()),
            rebuild: AsyncMutex::new(()),
            chooser_ledger: Mutex::new(ChooserLedger::default()),
            chooser_answer: Mutex::new(None),
            downloads: broadcast::channel(DOWNLOAD_BACKLOG).0,
            audio: broadcast::channel(64).0,
            recorded: watch::channel(None).0,
            recording: Mutex::new(None),
            surface: Mutex::new((VIEW_WIDTH, VIEW_HEIGHT, 1.0)),
            resize: AsyncMutex::new(()),
            encoding: self.encoding(),
            launch: Arc::clone(&self.launch),
            stopped: Arc::clone(&self.stopped),
            tabs: Mutex::new(TabRegistry::default()),
            boot: Mutex::new(None),
            focused: Mutex::new(None),
            controls: Mutex::new(HashMap::new()),
            viewers: Mutex::new(HashMap::new()),
            views: Mutex::new(HashMap::new()),
            call_lock: AsyncMutex::new(()),
            host_tab_events: AsyncMutex::new(None),
            deferred_tab_events: Mutex::new(Vec::new()),
            pairing: self.pairing.clone(),
            state: AsyncMutex::new(State::default()),
            touch: Mutex::new(Touch::now()),
            ended: AtomicBool::new(false),
            up: AtomicBool::new(false),
            restarted: AtomicBool::new(false),
            host_generation: AtomicU64::new(0),
            host_suspect: Mutex::new(None),
            #[cfg(test)]
            successor: Mutex::new(None),
            rows: self.rows_sender(key.as_str()),
        });
        profiles.insert(key.as_str().to_owned(), Arc::clone(&profile));
        Ok(profile)
    }

    /// One idle pass: every open profile's clock.
    ///
    /// **Every touched profile is republished each pass**, so the strip's
    /// "closing in Nm" is never older than one sweep.
    ///
    /// **A viewer attached is a use**, and **a held tab is a person being
    /// asked to act**, so neither profile is closed under them.
    async fn idle_pass(&self) {
        let Some(settings) = self.idle_settings() else { return };
        let idle = Duration::from_secs(settings.minutes.saturating_mul(60));
        let candidates = {
            let profiles = self.profiles.lock().await;
            profiles
                .iter()
                .map(|(key, profile)| Candidate {
                    key: key.clone(),
                    touch: *profile.touch.lock().unwrap_or_else(PoisonError::into_inner),
                    viewed: !profile
                        .viewers
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .values()
                        .all(HashSet::is_empty),
                    restarted: profile.restarted.load(Ordering::Acquire),
                })
                .collect::<Vec<_>>()
        };
        for candidate in candidates {
            self.publish_rows(&candidate.key).await;
            if candidate.restarted {
                self.forget_holds(&candidate.key);
            }
            if candidate.viewed || !self.held_tabs(&candidate.key).is_empty() {
                continue;
            }
            if idle::step(candidate.touch.at, Instant::now(), idle) == Step::Close {
                self.close_idle(&candidate.key).await;
            }
        }
    }

    fn idle_settings(&self) -> Option<Idle> {
        *self.idle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Close one profile's browser under the state lock, re-reading its clock,
    /// its viewers and its held tabs: a call or a raise that landed while the
    /// sweep was deciding wins, and a browser still in use is not closed.
    ///
    /// **The entry leaves the map before its host is ended, and the map lock
    /// is released with it**: a same-name call waits on the key's own gate,
    /// while every other profile keeps being served through the teardown.
    async fn close_idle(&self, key: &str) {
        let Some(settings) = self.idle_settings() else { return };
        let idle = Duration::from_secs(settings.minutes.saturating_mul(60));
        let gate = self.gate(key);
        let _held = gate.lock().await;
        let Some(profile) = self.profiles.lock().await.get(key).map(Arc::clone) else { return };
        let viewed = !profile
            .viewers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .all(HashSet::is_empty);
        if viewed || !self.held_tabs(key).is_empty() {
            return;
        }
        let touch = *profile.touch.lock().unwrap_or_else(PoisonError::into_inner);
        if idle::step(touch.at, Instant::now(), idle) != Step::Close {
            return;
        }
        self.profiles.lock().await.remove(key);
        self.forget_holds(key);
        self.forget_viewing(key);
        // The state lock is taken after the entry is gone, so the teardown's
        // wait is never the map's.
        let mut state = profile.state.lock().await;
        profile.ended.store(true, Ordering::Release);
        profile.teardown(&mut state, Some(HostEnd::Idle), "").await;
        drop(state);
        self.publish_rows(key).await;
        tracing::debug!("an idle browser closed (event_name browser_idle_closed, profile {key})");
    }

    /// End every open profile's browser and driver. **The profiles stay on
    /// disk**: a stop ends the run, never the logins. Answers the profiles
    /// whose host was still alive after its teardown, so a stop that leaves
    /// a process behind cannot report success.
    pub async fn stop_all(&self) -> Vec<String> {
        // **A stop is final**: an attach that already held a profile cannot
        // start a host behind the drain.
        self.stopped.store(true, Ordering::Release);
        let profiles: Vec<(String, Arc<Profile>)> = self.profiles.lock().await.drain().collect();
        self.holds.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.where_viewing.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.live.lock().unwrap_or_else(PoisonError::into_inner).clear();
        let mut left = Vec::new();
        for (key, profile) in profiles {
            let pid = { profile.state.lock().await.host.as_ref().map(|host| host.pid) };
            profile.end(HostEnd::Stopped).await;
            self.publish_rows(&key).await;
            if let Some(pid) = pid
                && spawn::alive(pid)
            {
                left.push(format!("{key} (pid {pid})"));
            }
        }
        left
    }

    /// Reap a host an unclean exit left behind, for every profile the store
    /// holds. **Boot-time**: an orphaned host still holds its profile's
    /// singleton lock, and without this the next boot wedges on it.
    pub async fn reap_stale(&self) {
        // **Serialised with launches**: the boot reap runs detached, and an
        // attach that starts a host while it walks must not be handed to it.
        let _launch = self.launch.lock().await;
        let mut profiles = Vec::new();
        if let Ok(orgs) = std::fs::read_dir(self.store.profiles()) {
            for org in orgs.flatten() {
                if !org.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                if let Ok(entries) = std::fs::read_dir(org.path()) {
                    for entry in entries.flatten() {
                        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                            profiles.push(entry.path().join("cef"));
                        }
                    }
                }
            }
        }
        for profile in profiles {
            spawn::reap_stale_profile(&profile).await;
        }
    }

    /// Close the tabs of every session that is no longer live, except the
    /// ones a parked hand-off is waiting on. **The profile stays**: only the
    /// idle clock ends a host.
    /// **A seat that has touched the browser is live.** The sweep replaces
    /// this set wholesale; a seat that never called but holds a tab reached
    /// the browser through a call, so the two agree from the first call on.
    fn note_live(&self, seat: &SessionSlot) {
        self.live.lock().unwrap_or_else(PoisonError::into_inner).insert(seat.clone());
        // **A call is a live session**: the successor clears the tombstone a
        // replaced occupant left, even before a sweep would see it.
        self.ended_slots.lock().unwrap_or_else(PoisonError::into_inner).remove(seat);
    }

    /// **The workspace says a session ended.** Synchronous, so the tombstone
    /// lands before the close work that follows it; the sweep cannot re-add
    /// the seat from configuration, and a hand-off resolved later answers
    /// from this fact rather than from the last sweep's set.
    pub fn session_ended(&self, seat: &SessionSlot) {
        self.ended_slots.lock().unwrap_or_else(PoisonError::into_inner).insert(seat.clone());
        self.live.lock().unwrap_or_else(PoisonError::into_inner).remove(seat);
    }

    pub async fn sweep_sessions(&self, seats: &[SessionSlot], generation: u64) {
        // **Sweeps serialise and only move forward**: a capture at or below
        // the last applied one is dropped, and loses nothing, because its
        // seat set is the sweep's only input from outside the fleet.
        let mut applied = self.sweep_lock.lock().await;
        if generation <= *applied {
            return;
        }
        *applied = generation;
        let ended = {
            let mut ended = self.ended_slots.lock().unwrap_or_else(PoisonError::into_inner);
            // **A seat the sweep did not see is gone for good** (a worker's
            // label is unique per spawn), so its tombstone is dropped. A
            // configured seat the sweep still lists keeps its tombstone:
            // that is what stops a closed lead coming back.
            ended.retain(|seat| seats.contains(seat));
            ended.clone()
        };
        *self.live.lock().unwrap_or_else(PoisonError::into_inner) =
            seats.iter().filter(|seat| !ended.contains(*seat)).cloned().collect();
        let held = self.holds.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let profiles: Vec<Arc<Profile>> = self.profiles.lock().await.values().cloned().collect();
        for profile in profiles {
            let doomed: Vec<u16> = {
                let tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
                tabs.numbers()
                    .into_iter()
                    .filter(|tab| {
                        tabs.owner(*tab).is_some_and(|owner| {
                            // **A tombstoned owner is gone even when the
                            // configured seat is still listed.**
                            (!seats.contains(owner) || ended.contains(owner))
                                && !held.contains(&(owner.clone(), *tab))
                        })
                    })
                    .collect()
            };
            for tab in doomed {
                self.orphan_tab(&profile, tab).await;
            }
            self.publish_rows(&profile.key).await;
        }
    }

    /// A `/new` or `/resume` keeps the slot and swaps the occupant, so the
    /// inherited tabs close (except a held one) and the slot's current-tab
    /// pointer clears.
    pub async fn replace_seat(&self, seat: &SessionSlot) {
        // **The occupant this replacement retires is gone now.** The
        // tombstone is what a sweep cannot re-add; the successor clears it
        // by warming or by its first call. A held tab survives the swap for
        // a parked hand-off, and its Done must see the old owner gone.
        self.session_ended(seat);
        let Ok(key) = self.profile_for(seat) else { return };
        let Ok(profile) = self.find(key.as_str()).await else { return };
        let doomed: Vec<u16> = {
            let tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.numbers()
                .into_iter()
                .filter(|tab| tabs.owner(*tab) == Some(seat))
                .filter(|tab| {
                    !self
                        .holds
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .contains(&(seat.clone(), *tab))
                })
                .collect()
        };
        for tab in doomed {
            self.orphan_tab(&profile, tab).await;
        }
        profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).forget_current(seat);
        self.publish_rows(key.as_str()).await;
    }

    /// A parked hand-off marks its tab held, so a sweep or the idle clock
    /// leaves it alone until the resolution lands. **A tab that is not this
    /// seat's is refused by name**: an ask parked on a stranger's tab would
    /// send the person to a page the session never opened.
    pub async fn hand_off_started(&self, seat: &SessionSlot, tab: u16) -> Result<(), String> {
        let key = self.profile_for(seat)?;
        let profile = self
            .find(key.as_str())
            .await
            .map_err(|_| format!("tab {tab} is not open on this session's browser"))?;
        profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).owned(seat, tab).map_err(
            |why| {
                tracing::debug!(
                    "a hand-off named a tab the seat does not own (event_name \
                     browser_hand_off_started_refused, seat {}, tab {tab}): {why}",
                    seat.display()
                );
                why
            },
        )?;
        self.holds.lock().unwrap_or_else(PoisonError::into_inner).insert((seat.clone(), tab));
        self.publish_rows(key.as_str()).await;
        Ok(())
    }

    /// A hand-off resolved. **Only a tab still held is acted on**: a
    /// resolution for anything else is stale (its host ended, or it was
    /// already resolved) and touches nothing.
    pub async fn hand_off_finished(&self, seat: &SessionSlot, tab: u16) {
        if !self.holds.lock().unwrap_or_else(PoisonError::into_inner).remove(&(seat.clone(), tab)) {
            return;
        }
        let Ok(key) = self.profile_for(seat) else { return };
        let Ok(profile) = self.find(key.as_str()).await else { return };
        // **The numbers died with the old host**: a new host's tab carrying
        // the same number belongs to someone else, so the pair's ownership is
        // the second check the registry can still answer.
        let owned =
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).owned(seat, tab).is_ok();
        if !owned {
            return;
        }
        let ended = {
            let ended = self.ended_slots.lock().unwrap_or_else(PoisonError::into_inner);
            ended.contains(seat)
        };
        let live = {
            let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            live.contains(seat)
        };
        let gone = ended || !live;
        if gone {
            self.orphan_tab(&profile, tab).await;
        }
        self.publish_rows(key.as_str()).await;
    }

    /// Create the seat's profile entry and start its host, with no tab. The
    /// first call is what opens a tab.
    pub async fn warm(&self, seat: &SessionSlot) {
        // A spawn is the successor arriving: its slot is live again.
        self.note_live(seat);
        if let Err(why) = self.open(seat).await {
            tracing::warn!(
                "a browser profile could not be warmed (event_name browser_warm_failed, seat {}): \
                 {why}",
                seat.display()
            );
        }
    }

    /// Run one browser tool through the tab the calling session drives.
    pub async fn call(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String> {
        self.note_live(seat);
        // The fleet's own doors: these never reach a driver.
        match tool {
            "browser_record_start" => return self.record_start(seat).await,
            "browser_record_stop" => return self.record_stop(seat).await,
            "browser_resize" => return self.resize_window(seat, &args).await,
            _ => {}
        }
        // **An agent's upload is bounded before a browser is opened for it.**
        // The driver reads whatever path it is handed, so forge checks the
        // files the page would take here rather than after the page has
        // loaded them: absolute, readable, regular, and inside the same caps
        // the person's own answer carries.
        let upload =
            if tool == "browser_file_upload" { Some(upload_request(&args)?) } else { None };
        // **The call's own shape is refused before a browser is opened for
        // it**: a malformed call is its argument error, never a host boot or
        // a "the browser is busy" it would not act on. The family's composed
        // tools are built here too, and the driver's name rides beside the
        // caller's, because a failure reported as the evaluate or the snippet
        // underneath would name a tool the session never called.
        let DriverCall { tool: driver_tool, reported, args } = driver_call(tool, args)?;
        let profile = self.open(seat).await?;
        // **A queued call is a call the session is waiting on.** The wait for
        // the profile's turn is bounded by the driver's own call timeout, and
        // a session that spent it in the queue is refused in words instead of
        // silently running long.
        let _serial = tokio::time::timeout(CALL_QUEUE_BOUND, profile.call_lock.lock())
            .await
            .map_err(|_| {
                format!(
                    "the browser is busy: this call waited {}s for its turn on profile '{}'",
                    CALL_QUEUE_BOUND.as_secs(),
                    profile.key,
                )
            })?;
        // **Every `browser_file_upload` is a chooser decision**, including
        // its cancel form with no paths: both answer the MCP's captured
        // chooser, so both take the lock, the refusal and the ledger. The
        // decision is begun here and held through the driver call, so a
        // chooser cannot surface between the refusal and the answer.
        let decision = begin_upload(&profile, upload.as_ref()).await?;
        let args = match upload.as_ref() {
            Some(UploadRequest::Answer(paths)) => {
                let staged = stage_agent_uploads(&profile.dir.join("uploads"), paths)?;
                let mut args = args;
                if let Some(fields) = args.as_object_mut() {
                    fields.insert("paths".to_owned(), json!(staged));
                }
                args
            }
            Some(UploadRequest::Cancel) | None => args,
        };
        // **The state lock is only for the lookup.** A call drives a page for
        // seconds, and holding it across that would block an attach, a
        // teardown or the idle sweep for the whole of it. **The call names
        // the host it drove**, so a failure is diagnosed against the browser
        // that was there rather than whatever replaced it. A test profile has
        // a driver and no host, and answers no host identity.
        let (mut driver, generation, pid, port) = {
            let state = profile.state.lock().await;
            let Some(driver) = state.driver.as_ref() else {
                return Err(format!("the browser for '{}' is not up", profile.key));
            };
            let host = state.host.as_ref();
            (
                Arc::clone(driver),
                captured_generation(&state),
                host.map_or(0, |host| host.pid),
                host.map_or(0, |host| host.port),
            )
        };
        // **The identity a tab-leak report reads**, so a call's browser is
        // answerable after the fact even when the profile name is not enough.
        tracing::info!(
            event_name = "browser_call_host",
            seat = ?seat,
            tool,
            profile = %profile.key,
            pid,
            port,
            cache = %profile.dir.display(),
            "a browser call drives a profile's own host"
        );
        // **An agent's upload is bounded like the person's answer**: a wedged
        // driver must not hold the decision lock for the driver's own 150 s.
        let bound = if upload.is_some() { Duration::from_secs(10) } else { driver::CALL_TIMEOUT };
        let (tab, created) = match tool {
            "browser_tabs" => {
                // **A create's own death is settled here**: this path never
                // reaches `after_failure`, so a host it marked would
                // otherwise stay up for every retry to fail against.
                let outcome = self.tabs_tool(&profile, seat, &mut driver, args).await;
                drop(decision);
                return match outcome {
                    Ok(parts) => {
                        // **A host that answered is not suspect any more**,
                        // the same clear the page path makes.
                        *profile.host_suspect.lock().unwrap_or_else(PoisonError::into_inner) = None;
                        Ok(parts)
                    }
                    Err(why) => Err(self.settle_suspect(&profile, why).await),
                };
            }
            "browser_close" => return self.close_current_tab(&profile, seat).await,
            _ => match self.ensure_tab(&profile, seat, &mut driver, bound, generation).await {
                Ok(tab) => tab,
                Err(why) => {
                    // **A listing that found the host wedged marks it for
                    // ending**; the decision is released first, so an upload
                    // never waits on its own lock through this task.
                    drop(decision);
                    return Err(self.settle_suspect(&profile, why).await);
                }
            },
        };
        // **The challenge probe is one fixed page script**, so it reaches the
        // driver as the evaluate it is; upstream has no tool that answers it.
        // The caller's own name rides beside it, because a failure reported as
        // `browser_evaluate` would name a tool the session never called.
        let challenge = reported == "browser_challenge_state";
        let retry_args = args.clone();
        let (tab, outcome) = self
            .drive(
                &profile,
                seat,
                &driver,
                tab,
                Drive { tool: driver_tool, args, bound, port, generation },
            )
            .await;
        // **The agent answered a chooser the driver had not reported yet.**
        // One event is owed to this answer, so it is consumed rather than
        // asking a person for a chooser that no longer exists.
        if let Some(upload) = upload.as_ref() {
            note_upload_finished(&profile, upload, outcome.is_ok());
        }
        // **The who-answers decision ends with the driver call.** The failure
        // handling below takes `file_decision` itself to clear a dead driver's
        // chooser, so holding the upload's guard across it would deadlock the
        // profile: this lock is not reentrant.
        drop(decision);
        let outcome = self
            .after_failure(
                &profile,
                Driven { generation, pid, port, tab, bound },
                Call { tool: driver_tool, reported, args: &retry_args },
                outcome,
            )
            .await;
        match outcome {
            Ok(mut parts) => {
                // **A host that answered is not suspect any more.**
                *profile.host_suspect.lock().unwrap_or_else(PoisonError::into_inner) = None;
                if challenge {
                    parts = challenge_result(parts);
                }
                if created {
                    parts.push(BrowserPart::Text { text: format!("tab {tab}") });
                }
                Ok(parts)
            }
            Err(why) => Err(reported_error(why, driver_tool, reported)),
        }
    }

    /// The host a driver generation serves right now, as the identity an end
    /// is aimed at. **A rebuild can start a new host**, so this is read after
    /// the install rather than from the call's own lookup.
    async fn live_host(profile: &Arc<Profile>, generation: u64) -> Option<HostIdentity> {
        profile.state.lock().await.host.as_ref().map(|host| HostIdentity {
            pid: host.pid,
            port: host.port,
            generation,
        })
    }

    /// **A host a fresh driver cannot list is marked for the call's own
    /// failure path to end.** It is named by pid and port, so a successor
    /// started in between is not taken for it (#2573).
    async fn mark_host_suspect(profile: &Arc<Profile>, generation: u64) {
        if let Some(host) = Self::live_host(profile, generation).await {
            *profile.host_suspect.lock().unwrap_or_else(PoisonError::into_inner) = Some(host);
        }
    }

    /// End a host a listing already found wedged, if this call marked one.
    /// **Runs with no chooser decision held**: the marker exists so the end
    /// can be reached from here rather than from inside the listing path,
    /// where an upload's own decision lock would be waited on.
    async fn settle_suspect(&self, profile: &Arc<Profile>, why: String) -> String {
        let suspect = profile.host_suspect.lock().unwrap_or_else(PoisonError::into_inner).take();
        let Some(drove) = suspect else { return why };
        let host = self.end_dead_host(profile, drove, &why).await;
        dead_host_answer(profile, host, &why)
    }

    /// The session's tab for a page call: its current one, else the profile's
    /// own boot tab when nobody has adopted it, else a tab created through the
    /// host. Answers the tab and whether it was made for this call.
    async fn ensure_tab(
        &self,
        profile: &Arc<Profile>,
        seat: &SessionSlot,
        driver: &mut Arc<dyn TabDriver>,
        bound: Duration,
        generation: u64,
    ) -> Result<(u16, bool), String> {
        if let Some(tab) = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).current(seat)
        {
            return Ok((tab, false));
        }
        let boot = profile.boot.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some((number, _)) = boot {
            // **The boot tab's target is the driver's, not the endpoint's.**
            // The driver's listing is what a call selects by, so the adoption
            // takes its row rather than a target read before the driver
            // attached.
            let listing = self.tabs_recovering(profile, driver, "boot", bound, generation).await?;
            if let Some(first) = listing.first() {
                let adopted = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).adopt(
                    number,
                    seat.clone(),
                    &first.id,
                    &first.target,
                    &first.title,
                    &first.url,
                );
                return adopted.map(|number| (number, true));
            }
        }
        let number = self.create_tab(profile, seat, driver, bound).await?;
        Ok((number, true))
    }

    /// Create one tab, blank, through the host, and pair it with its page
    /// target. **A URL is a navigation after the tab is armed**, never part of
    /// its creation: the passkey veto must be in front of the first document.
    async fn create_tab(
        &self,
        profile: &Arc<Profile>,
        seat: &SessionSlot,
        driver: &mut Arc<dyn TabDriver>,
        bound: Duration,
    ) -> Result<u16, String> {
        let deadline = tokio::time::Instant::now() + bound;
        if !driver.is_running() {
            return Err(format!("the browser driver for '{}' is not running", profile.key));
        }
        // A test service has no host and no page targets; the fake driver is
        // the tab source, and the test pushes the target it expects before
        // calling.
        #[cfg(test)]
        if profile.state.lock().await.host.is_none() {
            let generation = { profile.state.lock().await.driver_generation };
            let tabs =
                self.tabs_recovering(profile, driver, "create", left(deadline), generation).await?;
            let newest =
                tabs.last().ok_or_else(|| "the fake driver has no tab to adopt".to_owned())?;
            // The fake's listing order is its creation order, so the newest
            // tab's host number is the list's length.
            let number = u16::try_from(tabs.len())
                .map_err(|_| "the fake driver has more tabs than a tab number holds".to_owned())?;
            return profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).adopt(
                number,
                seat.clone(),
                &newest.id,
                &newest.target,
                &newest.title,
                &newest.url,
            );
        }
        let (port, input) = {
            let state = profile.state.lock().await;
            let host = state
                .host
                .as_ref()
                .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
            (host.port, host.input.clone())
        };
        // **A birth already in this queue is not this request's.** A popup
        // the host reported earlier can still be sitting here; draining it
        // first keeps it out of the new-page slots the snapshot below
        // counts. A birth still in the reader thread can land after the
        // snapshot and produce a spurious pairing error, never a wrong
        // pairing. Rule 25: the drained events go to the deferred list.
        let generation = profile.host_generation.load(Ordering::Acquire);
        {
            let mut queue = profile.host_tab_events.lock().await;
            if let Some(receiver) = queue.as_mut() {
                let mut drained = Vec::new();
                while let Ok(notice) = receiver.try_recv() {
                    drained.push(notice);
                }
                if !drained.is_empty() {
                    profile
                        .deferred_tab_events
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .extend(drained);
                }
            }
        }
        // **The host's own target list names the tab it is about to make.**
        // The driver's pages and a host tab are both `about:blank` at birth,
        // so the only exact pairing is the page targets that appear between
        // the snapshot and the creation.
        let before_targets: HashSet<String> =
            spawn::page_targets(port).await.into_iter().map(|(id, _)| id).collect();
        // **The birth is asked for by id, and every birth is kept in order.**
        // A popup the host created beside this request is its own
        // `TabCreated`, and the births and the page targets are both
        // creation-ordered: the request's place among the births is its place
        // among the new pages. Taking the first new page let a popup be
        // adopted as the requested tab.
        let request = TAB_REQUESTS.fetch_add(1, Ordering::Relaxed);
        input
            .send(input::host_line(&input::HostInput::TabNew { url: None, request: Some(request) }))
            .map_err(|_| format!("the browser for '{}' is not taking a tab", profile.key))?;
        let mut events: Vec<(u64, HostEvent)> = Vec::new();
        let mut ours_at: Option<usize> = None;
        let (ours, ours_event) = {
            let mut queue = profile.host_tab_events.lock().await;
            let Some(queue) = queue.as_mut() else {
                return Err(format!("the browser for '{}' is not reporting its tabs", profile.key));
            };
            let event_deadline = tokio::time::Instant::now() + TAB_EVENT_BOUND;
            loop {
                while let Ok(event) = queue.try_recv() {
                    let _ = collect_event(event, generation, request, &mut events, &mut ours_at);
                }
                // **The birth may have arrived through either read.** The
                // recv below sets `ours_at` too, so the loop tests it rather
                // than a per-iteration `found` the next pass would reset.
                if ours_at.is_some() {
                    break;
                }
                if tokio::time::Instant::now() >= event_deadline {
                    defer_events(profile, &mut events, None);
                    return Err(format!(
                        "the browser did not open a tab within {} s",
                        TAB_EVENT_BOUND.as_secs()
                    ));
                }
                match tokio::time::timeout(Duration::from_millis(50), queue.recv()).await {
                    Ok(Some(event)) => {
                        let _ =
                            collect_event(event, generation, request, &mut events, &mut ours_at);
                    }
                    Ok(None) => {
                        defer_events(profile, &mut events, None);
                        return Err(format!(
                            "the browser for '{}' stopped reporting its tabs",
                            profile.key
                        ));
                    }
                    Err(_) => {}
                }
            }
            let ours_event = ours_at.unwrap_or_else(|| events.len().saturating_sub(1));
            let ours = events[..=ours_event]
                .iter()
                .filter(|(event_generation, event)| {
                    *event_generation == generation && matches!(event, HostEvent::TabCreated { .. })
                })
                .count()
                .saturating_sub(1);
            (ours, ours_event)
        };
        let number = match events.get(ours_event) {
            Some((_, HostEvent::TabCreated { tab, .. })) => *tab,
            _ => 0,
        };
        // **A CEF browser the host created after the driver attached is not
        // reported to that driver's context.** A fresh driver enumerates the
        // context's pages, so wait until at least the requested birth's page
        // exists, then let the listing name them in creation order.
        let target_deadline = tokio::time::Instant::now() + TAB_TARGET_BOUND;
        loop {
            let fresh = spawn::page_targets(port)
                .await
                .into_iter()
                .map(|(id, _)| id)
                .filter(|id| !before_targets.contains(id))
                .count();
            if fresh > ours || tokio::time::Instant::now() >= target_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // **This one call needs a driver installed AFTER the tab's birth**: a
        // CEF page created after a driver attached is invisible to it, so the
        // recovery generation guard must not hand back a pre-birth driver
        // here. The rebuild lock is held the same way `recover_driver` holds
        // it.
        let (fresh, fresh_generation) = {
            let _rebuilding = profile.rebuild.lock().await;
            self.install_fresh_driver_locked(profile, left(deadline)).await?
        };
        *driver = Arc::clone(&fresh);
        let listing = match fresh.tabs_within(left(deadline)).await {
            Ok(listing) => listing,
            Err(why) if driver::died(&why) => {
                // **A fresh driver that cannot list means the host's pages
                // are wedged**, and this create never reaches
                // `tabs_recovering`'s recovery: the call's failure path ends
                // the host instead.
                Self::mark_host_suspect(profile, fresh_generation).await;
                return Err(why);
            }
            Err(why) => return Err(why),
        };
        let requested = {
            let mut tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            let known: HashSet<String> = tabs
                .numbers()
                .into_iter()
                .filter_map(|tab| tabs.target(tab).map(str::to_owned))
                .collect();
            let new_rows: Vec<crate::tabs::DriverTab> = listing
                .iter()
                .filter(|row| {
                    !row.target.is_empty()
                        && !before_targets.contains(&row.target)
                        && !known.contains(&row.target)
                })
                .cloned()
                .collect();
            // **Births and pages are both creation-ordered**, so the request's
            // place among the host's births is its place among the new pages.
            new_rows
                .get(ours)
                .map(|row| {
                    tabs.adopt(number, seat.clone(), &row.id, &row.target, &row.title, &row.url)
                })
                .and_then(Result::ok)
        };
        let Some(requested) = requested else {
            defer_events(profile, &mut events, None);
            return Err(format!(
                "the browser's new tab did not appear in its page list (profile '{}')",
                profile.key
            ));
        };
        // **The boot page is only special while nothing else is open.** A
        // create as a profile's first call adopts a tab of its own, and a
        // boot marker left standing would let the next reconcile spare a page
        // nothing owns.
        *profile.boot.lock().unwrap_or_else(PoisonError::into_inner) = None;
        defer_events(profile, &mut events, Some(ours_event));
        let target = profile
            .tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .target(requested)
            .map(str::to_owned);
        if let Some(target) = target {
            let arm_deadline = tokio::time::Instant::now() + left(deadline).min(TAB_ARM_BOUND);
            while !veto::target_armed(port, &target) {
                if tokio::time::Instant::now() >= arm_deadline {
                    tracing::warn!(
                        "a new tab was not armed before its navigation (event_name \
                         cef_passkey_veto_unarmed_tab, port {port}, profile {}, tab {requested})",
                        profile.key,
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
        self.publish_rows(&profile.key).await;
        Ok(requested)
    }

    /// The `browser_tabs` tool, answered from the registry: **the host's
    /// numbers are the tab names**, and no driver call is needed to list,
    /// select or close one.
    async fn tabs_tool(
        &self,
        profile: &Arc<Profile>,
        seat: &SessionSlot,
        driver: &mut Arc<dyn TabDriver>,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String> {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");
        // **The list is reconciled before it is drawn.** A popup the host
        // opened since the last page call is a tab by the time a session
        // lists them, and a page the driver made for itself is closed here.
        let port = {
            let state = profile.state.lock().await;
            state.host.as_ref().map(|host| host.port)
        };
        if let Some(port) = port {
            self.reconcile(profile, driver, port).await;
        }
        match action {
            "list" => {}
            "new" => {
                let number = self.create_tab(profile, seat, driver, driver::CALL_TIMEOUT).await?;
                if let Some(url) =
                    args.get("url").and_then(Value::as_str).filter(|url| !url.is_empty())
                {
                    let deadline = tokio::time::Instant::now() + driver::CALL_TIMEOUT;
                    self.select_tab(profile, driver, number, left(deadline)).await?;
                    driver
                        .call_within("browser_navigate", json!({ "url": url }), left(deadline))
                        .await?;
                    // **The new row is the page's name now**: a navigation
                    // swaps the target and changes the title and url the
                    // next list must show.
                    let page = profile
                        .tabs
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .id(number)
                        .map(str::to_owned);
                    if let (Some(page), Ok(listing)) =
                        (page, driver.tabs_within(left(deadline)).await)
                    {
                        Self::tab_index(profile, number, &page, &listing);
                    }
                }
            }
            "select" => {
                let tab = tool_tab(&args)?;
                profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).select(seat, tab)?;
                self.publish_rows(&profile.key).await;
            }
            "close" => {
                let tab = tool_tab(&args)?;
                self.close_owned_tab(profile, seat, tab).await?;
            }
            other => return Err(format!("browser_tabs does not know the action '{other}'")),
        }
        Ok(vec![BrowserPart::Text { text: self.render_tabs(profile, seat) }])
    }

    /// `browser_close`: the calling session's current tab.
    async fn close_current_tab(
        &self,
        profile: &Arc<Profile>,
        seat: &SessionSlot,
    ) -> Result<Vec<BrowserPart>, String> {
        let current =
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).current(seat).ok_or_else(
                || format!("this session has no current tab on profile '{}'", profile.key),
            )?;
        self.close_owned_tab(profile, seat, current).await?;
        Ok(vec![BrowserPart::Text { text: self.render_tabs(profile, seat) }])
    }

    async fn close_owned_tab(
        &self,
        profile: &Arc<Profile>,
        seat: &SessionSlot,
        tab: u16,
    ) -> Result<(), String> {
        profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).close_owned(seat, tab)?;
        self.orphan_tab(profile, tab).await;
        Ok(())
    }

    /// Close one tab from the host and forget it, whoever owned it. **Every
    /// host-end path comes through here or through `teardown`**, so a number
    /// that dies with its host never keeps a row.
    async fn orphan_tab(&self, profile: &Arc<Profile>, tab: u16) {
        // The host this number belongs to: a replacement restarts at one.
        let generation = profile.host_generation.load(Ordering::Acquire);
        let input = {
            let state = profile.state.lock().await;
            state.host.as_ref().map(|host| host.input.clone())
        };
        if let Some(input) = input {
            let _ = input.send(input::host_line(&input::HostInput::TabClose { tab }));
        }
        profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).close(tab);
        profile.forget_tab(tab);
        // **A closed tab is not the one the history watch follows**: left
        // focused, it would spend a driver round trip every 500 ms on a page
        // that no longer exists.
        profile.unfocus(generation, tab);
        let leaving: Vec<u64> = self
            .where_viewing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, (key, viewing))| key == &profile.key && *viewing == tab)
            .map(|(viewer, _)| *viewer)
            .collect();
        if !leaving.is_empty() {
            let mut where_viewing =
                self.where_viewing.lock().unwrap_or_else(PoisonError::into_inner);
            for viewer in leaving {
                where_viewing.remove(&viewer);
            }
        }
        self.pause_unwatched(profile).await;
        self.publish_rows(&profile.key).await;
    }

    /// The driver's own tab list is the index authority: a session's call
    /// selects its tab first, then drives it.
    async fn drive(
        &self,
        profile: &Arc<Profile>,
        _seat: &SessionSlot,
        driver: &Arc<dyn TabDriver>,
        tab: u16,
        call: Drive<'_>,
    ) -> (u16, Result<Vec<BrowserPart>, String>) {
        let Drive { tool, args, bound, port, generation } = call;
        let deadline = tokio::time::Instant::now() + bound;
        let mut driver = Arc::clone(driver);
        // **Two names, two jobs.** The page guid survives a navigation and is
        // what the driver is asked for; the CDP target is where the page can
        // be driven right now and is refreshed from every listing.
        let (page, target) = {
            let tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            (
                tabs.id(tab).map(str::to_owned).unwrap_or_default(),
                tabs.target(tab).map(str::to_owned).unwrap_or_default(),
            )
        };
        let index = match self
            .tabs_recovering(profile, &mut driver, tool, left(deadline), generation)
            .await
        {
            Ok(listing) => Self::tab_index(profile, tab, &page, &listing),
            Err(why) => return (tab, Err(why)),
        };
        let Some(index) = index else {
            // **A row that cannot be paired is not a closed tab.** Closing it
            // is how a live document is lost: the page's target may predate a
            // navigation, or a listing may not have read it. Log the row and
            // refuse the call; the tab stays open, and the registry holds the
            // orphan prune until it is paired or closed.
            tracing::warn!(
                event_name = "browser_tab_unmatched",
                profile = %profile.key,
                tab,
                target = %target,
                "a tab's page is not in the driver's list; the tab is left open"
            );
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).mark_unresolved(tab);
            return (
                tab,
                Err(format!(
                    "tab {tab}'s page is not in the browser's list, so the tab was left open. \
                     Call browser_tabs to see it, or open a new tab."
                )),
            );
        };
        if let Err(why) = driver.select(index, left(deadline)).await {
            return (tab, Err(why));
        }
        self.focus(profile, tab).await;
        profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).mark_busy(tab, true);
        self.publish_rows(&profile.key).await;
        let outcome = driver.call_within(tool, args, left(deadline)).await;
        // A test service has no host to probe and no page targets to
        // reconcile: the fake driver's answer is the whole of it.
        #[cfg(test)]
        if profile.state.lock().await.host.is_none() {
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).mark_busy(tab, false);
            self.publish_rows(&profile.key).await;
            return (tab, outcome);
        }
        // **The driver's row is the target of record after the call.** A
        // navigation swaps CEF's page target id while the driver's Tab
        // survives it; without this the reconcile below would take the live
        // page for an orphan and close it.
        if outcome.is_ok()
            && let Ok(listing) = driver.tabs_within(left(deadline)).await
            && let Some(row) = listing.iter().find(|row| row.current)
        {
            let mut tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.retarget(tab, &row.target);
            tabs.update(tab, &row.title, &row.url);
        }
        self.reconcile(profile, &driver, port).await;
        profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).mark_busy(tab, false);
        self.publish_rows(&profile.key).await;
        // **Failure handling is the caller's, after the chooser decision is
        // over.** Its paths take `file_decision` to clear a dead driver's
        // chooser; an upload holds that lock across this call, and a second
        // acquisition here would deadlock the profile.
        (tab, outcome)
    }

    /// The driver's tabs, replacing a driver that is dead. **The backend
    /// holds Tab -> Page**, and a dead page or a driver that stopped
    /// answering never resets itself upstream (#2463, #2534); a fresh driver
    /// re-reads the context's pages and works again. **The call that finds a
    /// dead driver replaces it on that same call**, so nothing waits for a
    /// restart.
    async fn tabs_recovering(
        &self,
        profile: &Arc<Profile>,
        driver: &mut Arc<dyn TabDriver>,
        tool: &str,
        bound: Duration,
        generation: u64,
    ) -> Result<Vec<crate::tabs::DriverTab>, String> {
        match driver.tabs_within(bound).await {
            Err(why) if driver::died(&why) => {
                tracing::warn!(
                    event_name = "browser_driver_replaced",
                    profile = %profile.key,
                    tool,
                    stale = driver::stale_page(&why),
                    "the driver is dead; a fresh one is attached: {why}"
                );
                let (fresh, fresh_generation) =
                    self.recover_driver(profile, generation, bound).await?;
                *driver = fresh;
                match driver.tabs_within(bound).await {
                    Ok(listing) => Ok(listing),
                    Err(again) if driver::died(&again) => {
                        // **A fresh driver that still cannot list means the
                        // host's pages are wedged.** It is marked rather than
                        // ended: an upload's call holds `file_decision`
                        // across this listing, so taking it now would wait on
                        // the task that is waiting on us. The call's own
                        // failure path ends the host once the decision is
                        // released.
                        Self::mark_host_suspect(profile, fresh_generation).await;
                        // **The driver's own words go back unchanged**, so the
                        // call's failure path can still classify them.
                        Err(again)
                    }
                    other => other,
                }
            }
            other => other,
        }
    }

    /// Drop the profile's driver and attach a fresh one to the same host.
    ///
    /// **The generation is checked under the rebuild lock**: a driver
    /// installed since the caller's lookup is not this call's to clear, and
    /// the newer one is handed back for the retry instead.
    async fn recover_driver(
        &self,
        profile: &Arc<Profile>,
        generation: u64,
        bound: Duration,
    ) -> Result<(Arc<dyn TabDriver>, u64), String> {
        let _rebuilding = profile.rebuild.lock().await;
        {
            let state = profile.state.lock().await;
            if state.driver_generation != generation {
                // **A newer driver is handed back, never cleared**: it is not
                // this call's, and the caller's retry can use it.
                let current = state.driver.as_ref().map(Arc::clone).ok_or_else(|| {
                    format!("a newer browser driver for '{}' is not running", profile.key)
                })?;
                return Ok((current, state.driver_generation));
            }
        }
        // A test service has no host to start a driver against; a successor
        // stands in for the replacement, and without one the fake stays.
        #[cfg(test)]
        {
            let successor = profile.successor.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(successor) = successor {
                let fresh = successor();
                let generation = {
                    let mut state = profile.state.lock().await;
                    state.driver_generation = installed_generation(&state);
                    state.driver = Some(Arc::clone(&fresh));
                    state.driver_generation
                };
                self.repair_tab_identity(profile, &fresh, bound).await;
                return Ok((fresh, generation));
            }
            let state = profile.state.lock().await;
            if state.host.is_none()
                && let Some(driver) = state.driver.as_ref()
            {
                return Ok((Arc::clone(driver), state.driver_generation));
            }
        }
        self.install_fresh_driver_locked(profile, bound).await
    }

    /// The replacement itself; **the caller holds `profile.rebuild`**. Answers
    /// the driver and the generation it was installed under, so a caller can
    /// aim its later clear at exactly this one.
    async fn install_fresh_driver_locked(
        &self,
        profile: &Arc<Profile>,
        bound: Duration,
    ) -> Result<(Arc<dyn TabDriver>, u64), String> {
        {
            let mut state = profile.state.lock().await;
            state.driver = None;
        }
        // A test service has no host to start a driver against; a successor
        // stands in for the install when a test leaves one.
        #[cfg(test)]
        {
            let successor = profile.successor.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(successor) = successor {
                let fresh = successor();
                let generation = {
                    let mut state = profile.state.lock().await;
                    state.driver = Some(Arc::clone(&fresh));
                    state.driver_generation = installed_generation(&state);
                    state.driver_generation
                };
                return Ok((fresh, generation));
            }
        }
        profile.ensure(&self.root, self.stack()).await?;
        // The host can die between a stale page and its rebuild; that rebuild
        // starts a new host, and the old one's hand-offs must not outlive it.
        if profile.restarted.swap(false, Ordering::AcqRel) {
            self.forget_holds(&profile.key);
        }
        let (driver, generation) = {
            let state = profile.state.lock().await;
            let driver = state.driver.as_ref().map(Arc::clone).ok_or_else(|| {
                format!("the browser driver for '{}' did not come back", profile.key)
            })?;
            (driver, state.driver_generation)
        };
        self.repair_tab_identity(profile, &driver, bound).await;
        Ok((driver, generation))
    }

    /// **A fresh driver names every page again.** The registry's rows are
    /// re-paired by the CDP targets the new driver lists, so a session's tab
    /// survives a driver replacement instead of being re-created as
    /// about:blank; a row the listing does not name keeps its old name and is
    /// answered as gone by the next call. **The caller's bound is the
    /// listing's**: a replacement made under an upload's 10 s budget does not
    /// lend its driver 150 s.
    async fn repair_tab_identity(
        &self,
        profile: &Arc<Profile>,
        driver: &Arc<dyn TabDriver>,
        bound: Duration,
    ) {
        let Ok(listing) = driver.tabs_within(bound).await else {
            tracing::debug!(
                event_name = "browser_tab_identity_unlisted",
                profile = %profile.key,
                "a fresh driver could not list its tabs, so no row was re-paired"
            );
            return;
        };
        let repaired =
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).reidentify_from(&listing);
        if repaired > 0 {
            tracing::debug!(
                event_name = "browser_tab_identity_repaired",
                profile = %profile.key,
                repaired,
                "a fresh driver renamed pages; their tabs follow the targets"
            );
        }
    }

    /// The Playwright index of `tab` in one listing, updating the row's title
    /// and url from what the driver sees.
    ///
    /// **A driver replacement renames every page**, so a guid miss is not a
    /// gone tab: the CDP target is the browser's name for the same page and
    /// survives the replacement, so the row is re-paired by it and the call
    /// drives the page the session already had.
    fn tab_index(
        profile: &Arc<Profile>,
        tab: u16,
        page: &str,
        listing: &[crate::tabs::DriverTab],
    ) -> Option<usize> {
        let mut tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let index = crate::tabs::index_of(listing, page).or_else(|| {
            let target = tabs.target(tab).map(str::to_owned)?;
            let at = crate::tabs::index_of_target(listing, &target)?;
            let row = listing.get(at)?;
            tabs.reidentify(tab, &row.id);
            Some(at)
        });
        if let Some(entry) = index.and_then(|index| listing.get(index)) {
            tabs.update(tab, &entry.title, &entry.url);
            tabs.retarget(tab, &entry.target);
        }
        index
    }

    /// Adopt every popup the host opened during a call, then close the page
    /// targets neither the registry nor the boot tab knows: **a page the
    /// driver created on its own is not a host tab and must not shift the
    /// indices**.
    /// **Answers whether it got a listing.** A pass that could not list paired
    /// nothing, so a caller retrying on a waiting birth knows to try again.
    async fn reconcile(
        &self,
        profile: &Arc<Profile>,
        driver: &Arc<dyn TabDriver>,
        port: u16,
    ) -> bool {
        // **A reconcile in flight across a host death must not write its
        // events back over the empty list** the death just made. The
        // generation names the host this pass is pairing for.
        let generation = profile.host_generation.load(Ordering::Acquire);
        let Ok(mut listing) = driver.tabs_within(driver::CALL_TIMEOUT).await else {
            tracing::debug!(
                event_name = "browser_reconcile_unlisted",
                profile = %profile.key,
                "reconcile could not list the driver's tabs"
            );
            self.publish_rows(&profile.key).await;
            return false;
        };
        // **The events this pass takes are held, not owned.** The pairing pass
        // that runs here is bounded by a timeout, and a listing that never
        // returns cancels the future with the births in hand; dropping them
        // loses a popup for good, and the next pass then prunes its page as an
        // orphan.
        let taken: Vec<(u64, HostEvent)> = {
            let mut deferred =
                profile.deferred_tab_events.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::take(&mut *deferred)
        };
        let mut held = TakenEvents { profile, events: Some(taken) };
        {
            let mut queue = profile.host_tab_events.lock().await;
            if let Some(receiver) = queue.as_mut() {
                while let Ok(event) = receiver.try_recv() {
                    if let Some(events) = held.events.as_mut() {
                        events.push(event);
                    }
                }
            }
        }
        let events = held.events.clone().unwrap_or_default();
        let mut unmatched = {
            let mut tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            pair_tab_events(&mut tabs, &listing, events, generation)
        };
        if unmatched.iter().any(|(_, event)| matches!(event, HostEvent::TabCreated { .. })) {
            // **A CEF browser the host created after the driver attached is
            // not reported to that driver's context**, so a popup is only a
            // tab to a driver that attached after it existed. A fresh one
            // enumerates the context's pages.
            let driver_generation = { profile.state.lock().await.driver_generation };
            if let Ok((fresh, _generation)) =
                self.recover_driver(profile, driver_generation, driver::CALL_TIMEOUT).await
                && let Ok(fresh_listing) = fresh.tabs_within(driver::CALL_TIMEOUT).await
            {
                unmatched = {
                    let mut tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
                    pair_tab_events(&mut tabs, &fresh_listing, unmatched, generation)
                };
                listing = fresh_listing;
            }
        }
        // **An event read from a receiver that has since been replaced goes
        // back for the next pass rather than away** (rule 25); that pass
        // pairs it with its own generation. Only a death with no successor
        // leaves the list as this pass found it.
        let moved = profile.host_generation.load(Ordering::Acquire) != generation;
        if moved {
            profile
                .deferred_tab_events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(unmatched);
        } else {
            *profile.deferred_tab_events.lock().unwrap_or_else(PoisonError::into_inner) = unmatched;
        }
        // **The taken events are the deferred list's now**, so a cancellation
        // from here on has nothing to put back.
        held.events = None;
        let orphaned: Vec<String> = {
            let tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            // **A fresh host's current page is its boot tab, not an
            // orphan**, and only while nothing else is open. Once any tab is
            // adopted the marker is gone, and a registry with tabs never
            // spares an unowned page.
            let boot_pending = tabs.numbers().is_empty()
                && profile.boot.lock().unwrap_or_else(PoisonError::into_inner).is_some();
            let unresolved = tabs.unresolved_tabs();
            if !unresolved.is_empty() {
                tracing::debug!(
                    event_name = "browser_reconcile_held",
                    profile = %profile.key,
                    tabs = ?unresolved,
                    "a tab's page could not be named, so no page is closed as an orphan"
                );
            }
            closed_as_orphans(&tabs, &listing, boot_pending)
        };
        for target in orphaned {
            // **A host replaced during this pass is not this pass's to
            // prune**: its successor's pages are what the fresh listing above
            // holds, and the births they belong to were just re-deferred. A
            // successor normally reuses the freed port, so the generation is
            // re-read before every close rather than once for the pass.
            if profile.host_generation.load(Ordering::Acquire) != generation {
                tracing::debug!(
                    event_name = "browser_reconcile_generation_moved",
                    profile = %profile.key,
                    generation,
                    "the host was replaced during reconcile, so no page is closed as an orphan"
                );
                break;
            }
            spawn::close_target(port, &target).await;
        }
        tracing::debug!(
            event_name = "browser_reconcile",
            profile = %profile.key,
            rows = listing.len(),
            "tabs reconciled"
        );
        self.publish_rows(&profile.key).await;
        true
    }

    /// One tab's row list, in the words the tool returns.
    fn render_tabs(&self, profile: &Profile, seat: &SessionSlot) -> String {
        let held = self.held_tabs(&profile.key);
        let controlled: HashSet<u16> = profile
            .controls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        let tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        let current = tabs.current(seat);
        let rows = tabs.rows(&held, &controlled);
        if rows.is_empty() {
            return "no tabs are open".to_owned();
        }
        rows.iter()
            .map(|row| {
                let marker = if current == Some(row.number) { "(current) " } else { "" };
                let owner = if row.owner == *seat { "you".to_owned() } else { row.owner.display() };
                format!("- {}: {}[{}]({}) - {owner}", row.number, marker, row.title, row.url)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Select one tab in the driver by its target id.
    async fn select_tab(
        &self,
        profile: &Arc<Profile>,
        driver: &Arc<dyn TabDriver>,
        tab: u16,
        bound: Duration,
    ) -> Result<(), String> {
        let page = profile
            .tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .id(tab)
            .map(str::to_owned)
            .ok_or_else(|| format!("tab {tab} is not open"))?;
        let listing = driver.tabs_within(bound).await?;
        let index = Self::tab_index(profile, tab, &page, &listing)
            .ok_or_else(|| format!("tab {tab}'s page is not in the driver's list any more"))?;
        driver.select(index, bound).await?;
        self.focus(profile, tab).await;
        Ok(())
    }

    /// The profile's current page follows the tab the last call drove. **A
    /// moved page restarts the history watch**, which is bound to the old
    /// target's connection.
    async fn focus(&self, profile: &Arc<Profile>, tab: u16) {
        let changed = {
            let mut focused = profile.focused.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = *focused != Some(tab);
            *focused = Some(tab);
            changed
        };
        if changed {
            self.restart_nav(profile).await;
        }
    }

    async fn restart_nav(&self, profile: &Arc<Profile>) {
        let mut state = profile.state.lock().await;
        if let Some(watch) = state.nav_watch.take() {
            watch.abort();
        }
        if let Some(port) = state.host.as_ref().map(|host| host.port) {
            state.nav_watch = Some(start_nav_watch(Arc::clone(profile), port));
        }
    }

    /// The host encodes only while some tab has a viewer.
    async fn pause_unwatched(&self, profile: &Arc<Profile>) {
        let watched = !profile
            .viewers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .all(HashSet::is_empty);
        if watched {
            return;
        }
        let state = profile.state.lock().await;
        if let Some(host) = state.host.as_ref() {
            host.ring.pause(true);
        }
    }

    /// What a call that came back leaves behind, and what the session is
    /// told.
    ///
    /// **The driver is the suspect before the tool is.** A page context that
    /// died is rebuilt, and only a call safe to repeat is repeated; a host
    /// that never let the driver attach is ended and named with its own
    /// identity; a plain tool failure leaves the driver where it is. Every
    /// clear is aimed at the generation this call drove, so a newer driver
    /// started in between keeps its handle.
    async fn after_failure(
        &self,
        profile: &Arc<Profile>,
        driven: Driven,
        call: Call<'_>,
        outcome: Result<Vec<BrowserPart>, String>,
    ) -> Result<Vec<BrowserPart>, String> {
        let Driven { generation, pid, port, tab, bound, .. } = driven;
        let Call { tool, reported, args } = call;
        let drove = HostIdentity { pid, port, generation };
        let why = match outcome {
            Ok(parts) => {
                let mut state = profile.state.lock().await;
                if state.driver_generation == generation {
                    state.stale_since = None;
                }
                return Ok(parts);
            }
            Err(why) => why,
        };
        // **A host a listing found wedged is ended here**, on the path that
        // can take the chooser decision without waiting on this same call.
        // The marker names the host; a successor started since is not taken
        // for it (#2573).
        {
            let suspect =
                profile.host_suspect.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(suspect) = suspect {
                let host = self.end_dead_host(profile, suspect, &why).await;
                return Err(dead_host_answer(profile, host, &why));
            }
        }
        let again = Call { tool, reported, args };
        if driver::stale_page(&why) {
            return self
                .rebuild_after_dead_page(profile, generation, drove, tab, again, bound, &why)
                .await;
        }
        if driver::died(&why) {
            // **A driver that never answered is dead, not a page.** It is
            // replaced on this call rather than waiting for a restart.
            return self
                .replace_dead_driver(profile, generation, drove, tab, again, bound, &why)
                .await;
        }
        // **A driver built against a browser that is gone is dropped.** The
        // probe is the browser's own liveness, not the driver's; a host that
        // is not answering is torn down so the next call starts a fresh one.
        let host_up = spawn::probe(port).await;
        let _decision = profile.file_decision.lock().await;
        // **A test profile has no host to probe**, so the fake driver is not
        // a dead one to clear. The decision lock is still taken above, which
        // is what keeps a caller that holds it (an upload) under test.
        #[cfg(test)]
        if profile.state.lock().await.host.is_none() {
            return Err(why);
        }
        let mut state = profile.state.lock().await;
        let alive = state.driver.as_ref().is_some_and(|driver| driver.is_running());
        if clear_if_dead(&mut state, generation, alive && host_up, profile) && !host_up {
            // **The cause is published before the tab senders close.** A
            // call that finds the host gone may be the first to notice it;
            // without this a viewer reads "tab N was closed" and never
            // hears that the browser went.
            profile.failures.send_replace(Some(format!(
                "the browser host stopped answering (pid {pid}); the next call brings it back"
            )));
            profile.teardown(&mut state, Some(HostEnd::Unresponsive), &why).await;
        }
        Err(why)
    }

    /// **End a host whose pages never answered a fresh driver.** A host kept
    /// only on process liveness that no driver can list is the wedge that
    /// used to cost an hour: the call that finds it, after replacing the
    /// driver, ends it so the next call starts a fresh browser with the
    /// profile's data intact.
    async fn end_dead_host(
        &self,
        profile: &Arc<Profile>,
        drove: HostIdentity,
        why: &str,
    ) -> DeadHost {
        let _decision = profile.file_decision.lock().await;
        let mut tail = String::new();
        let host = {
            let mut state = profile.state.lock().await;
            // **Only the host this call drove is ended**, and only while the
            // driver it drove is still the one installed: a successor host is
            // not this call's, and neither is a newer driver on the same host
            // (#2573).
            match state.host.as_ref().map(|host| (host.pid, host.port)) {
                Some((pid, port))
                    if pid == drove.pid
                        && port == drove.port
                        && state.driver_generation == drove.generation => {}
                Some(_) => return DeadHost::NotOurs,
                None => return DeadHost::AlreadyGone,
            }
            state.driver = None;
            state.cdp.clear();
            state.cdp_events.clear();
            if let Some(watch) = state.nav_watch.take() {
                watch.abort();
            }
            profile.clear_chooser_state();
            let host = state.host.take();
            if let Some(host) = host.as_ref() {
                profile.up.store(false, Ordering::Release);
                // **The sentence goes out before the tab senders close**, so
                // a viewer reads why the browser went rather than "tab N was
                // closed"; the identity is what a recurrence is diagnosed by.
                tail = spawn::log_tail(&spawn::host_log_path(&profile.dir));
                let words = if tail.is_empty() {
                    "its log is empty".to_owned()
                } else {
                    format!("its log ends: {tail}")
                };
                profile.failures.send_replace(Some(format!(
                    "the browser host (pid {}, port {}, profile {}) stopped answering: {why}. \
                     {words}. The next call starts a fresh browser.",
                    host.pid,
                    host.port,
                    profile.dir.display(),
                )));
                // **A wedged host takes its numbers with it.**
                profile.host_gone();
                *profile.host_tab_events.lock().await = None;
            }
            host
        };
        let Some(host) = host else { return DeadHost::AlreadyGone };
        // **A host that had already gone is not an end.** The monitor is left
        // to write its death, and `gone` leaves the status for it; only a
        // live host is ended here and recorded. Two narrow windows remain: a
        // crash between this check and the abort can still write both
        // records, and a monitor aborted on a host that outlived its end
        // leaves a later crash unwritten.
        let was_up = !spawn::gone(host.pid);
        let reader = host.reader;
        if !was_up {
            if let Some(reader) = reader {
                reader.stop();
            }
            // **A crashed host's staged files go with it**: the monitor
            // cannot clear them once the state no longer names the host.
            let _ = std::fs::remove_dir_all(profile.dir.join("uploads"));
            return DeadHost::AlreadyGone;
        }
        if let Some(monitor) = host.monitor {
            monitor.abort();
        }
        let ended = spawn::end_verified(host.pid, host.port, &profile.dir).await;
        if let Some(reader) = reader {
            reader.stop();
        }
        let outcome = if ended { "ended" } else { "refused" };
        log_host_ended(profile, host.pid, host.port, HostEnd::Undrivable, outcome, why, &tail);
        if ended {
            DeadHost::Ended { pid: host.pid, port: host.port }
        } else {
            DeadHost::NotEnded { pid: host.pid, port: host.port }
        }
    }

    /// End the host standing after a rebuild whose fresh driver cannot list.
    /// **A rebuild starts a new host when the old one died**, so the identity
    /// is read now rather than taken from the call's own lookup.
    async fn end_rebuilt_host(
        &self,
        profile: &Arc<Profile>,
        drove: HostIdentity,
        fresh_generation: u64,
        why: String,
    ) -> String {
        let rebuilt = Self::live_host(profile, fresh_generation).await.unwrap_or(drove);
        let host = self.end_dead_host(profile, rebuilt, &why).await;
        dead_host_answer(profile, host, &why)
    }

    /// Rebuild a driver whose page context died, and repeat the call only
    /// where repeating it cannot land twice.
    async fn rebuild_after_dead_page(
        &self,
        profile: &Arc<Profile>,
        generation: u64,
        drove: HostIdentity,
        tab: u16,
        call: Call<'_>,
        bound: Duration,
        why: &str,
    ) -> Result<Vec<BrowserPart>, String> {
        let Call { tool, reported, args } = call;
        // **One rebuild at a time, and the cooldown lives inside it.** Two
        // calls that both read an old `stale_since` would otherwise both
        // rebuild, and the second would clear the driver the first installed.
        let _rebuilding = profile.rebuild.lock().await;
        // **One decision -> state section for the check and the clear**, the
        // order every other chooser transition takes. The generation is read
        // after both locks, so a driver installed while this call waited is
        // seen; a separate earlier check would be a second guard that no test
        // can tell from this one.
        {
            let _decision = profile.file_decision.lock().await;
            let mut state = profile.state.lock().await;
            // **A newer driver is not this call's to clear.** The call drove
            // the generation it captured at its lookup; a driver installed
            // since then is serving the profile now.
            if state.driver_generation != generation {
                log_rebuild(profile, reported, "not this call's generation", why);
                return Err(dead_driver_answer(DeadDriverOutcome::NotRepeated {
                    tool: reported,
                    why,
                }));
            }
            if let Some(at) = state.stale_since
                && at.elapsed() < REBUILD_COOLDOWN
            {
                log_rebuild(profile, reported, "not rebuilt again yet", why);
                return Err(dead_page_answer(DeadPageOutcome::Cooling));
            }
            // **The stale driver's ask and handle go with it**, and the slot
            // is let go before the fresh one starts: clearing the held slot
            // alone leaves this call's own `Arc` holding the child and its
            // CDP connection open while the new driver attaches beside it.
            state.driver = None;
            profile.clear_chooser_state();
        }
        if let Err(cannot) = profile.ensure(&self.root, self.stack()).await {
            profile.state.lock().await.stale_since = Some(Instant::now());
            log_rebuild(profile, reported, "rebuild failed", why);
            return Err(dead_page_answer(DeadPageOutcome::RebuildFailed { why: &cannot }));
        }
        let (fresh, fresh_generation) = {
            let state = profile.state.lock().await;
            (state.driver.as_ref().map(Arc::clone), state.driver_generation)
        };
        let Some(fresh) = fresh else {
            profile.state.lock().await.stale_since = Some(Instant::now());
            log_rebuild(profile, reported, "rebuild left no driver", why);
            return Err(dead_page_answer(DeadPageOutcome::RebuildFailed {
                why: "the rebuilt driver did not come up",
            }));
        };
        self.repair_tab_identity(profile, &fresh, bound).await;
        if repeatable(tool, reported, args) {
            // **The page is found by the guid, with the target as the
            // fallback a fresh driver's renaming needs.** A retry without a
            // page would drive whatever the fresh driver calls current -
            // another session's tab - so a miss makes no call at all.
            let page = profile
                .tabs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .id(tab)
                .map(str::to_owned);
            let index = match fresh.tabs_within(bound).await {
                Ok(listing) => {
                    page.as_deref().and_then(|page| Self::tab_index(profile, tab, page, &listing))
                }
                Err(cannot) if driver::died(&cannot) => {
                    return Err(self
                        .end_rebuilt_host(profile, drove, fresh_generation, cannot)
                        .await);
                }
                // **A listing that failed for any other reason was never
                // read**: nothing is known about the page, so it is not
                // gone.
                Err(cannot) => {
                    log_rebuild(profile, reported, "rebuilt, the listing failed", why);
                    return Err(dead_page_answer(DeadPageOutcome::ListFailed { why: &cannot }));
                }
            };
            let Some(index) = index else {
                log_rebuild(profile, reported, "rebuilt, the tab is not listed", why);
                return Err(dead_page_answer(DeadPageOutcome::TabGone { tab }));
            };
            if let Err(cannot) = fresh.select(index, bound).await {
                profile.state.lock().await.stale_since = Some(Instant::now());
                log_rebuild(profile, reported, "rebuilt, selection failed", why);
                return Err(dead_page_answer(DeadPageOutcome::RetryFailed { why: &cannot }));
            }
            return match fresh.call_within(tool, args.clone(), bound).await {
                Ok(parts) => {
                    profile.state.lock().await.stale_since = None;
                    log_rebuild(profile, reported, "rebuilt, call retried and answered", why);
                    Ok(parts)
                }
                Err(retry_why) => {
                    profile.state.lock().await.stale_since = Some(Instant::now());
                    log_rebuild(profile, reported, "rebuilt, retried call failed", why);
                    // **No second rebuild in this call**: the repeated
                    // failure is handed back, not met with another driver
                    // start.
                    Err(if driver::stale_page(&retry_why) {
                        dead_page_answer(DeadPageOutcome::StillDead)
                    } else {
                        dead_page_answer(DeadPageOutcome::RetryFailed { why: &retry_why })
                    })
                }
            };
        }
        log_rebuild(profile, reported, "rebuilt, call handed back", why);
        Err(dead_page_answer(DeadPageOutcome::NotRepeated { tool: reported }))
    }

    /// **A driver that stopped answering is replaced on this call.** The
    /// replacement is the one `tabs_recovering` uses; the call is repeated
    /// only where repeating cannot land twice.
    async fn replace_dead_driver(
        &self,
        profile: &Arc<Profile>,
        generation: u64,
        drove: HostIdentity,
        tab: u16,
        call: Call<'_>,
        bound: Duration,
        why: &str,
    ) -> Result<Vec<BrowserPart>, String> {
        let Call { tool, reported, args } = call;
        {
            let state = profile.state.lock().await;
            // **A newer driver is not this call's to clear**: the call drove
            // the generation it captured at its lookup.
            if state.driver_generation != generation {
                return Err(why.to_owned());
            }
        }
        tracing::warn!(
            event_name = "browser_driver_replaced",
            profile = %profile.key,
            tool = reported,
            "the call found its driver dead; a fresh one is attached: {why}"
        );
        let (fresh, fresh_generation) = match self.recover_driver(profile, generation, bound).await
        {
            Ok(fresh) => fresh,
            Err(cannot) => {
                // **A replacement that cannot start leaves the host suspect**:
                // end the host this call drove, if it is still the one there.
                self.end_dead_host(profile, drove, &cannot).await;
                return Err(dead_driver_answer(DeadDriverOutcome::ReplaceFailed { why: &cannot }));
            }
        };
        if !repeatable_after_timeout(tool, reported, args) {
            return Err(dead_driver_answer(DeadDriverOutcome::NotRepeated { tool: reported, why }));
        }
        let page =
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).id(tab).map(str::to_owned);
        let index = match fresh.tabs_within(bound).await {
            Ok(listing) => {
                page.as_deref().and_then(|page| Self::tab_index(profile, tab, page, &listing))
            }
            Err(cannot) if driver::died(&cannot) => {
                return Err(self.end_rebuilt_host(profile, drove, fresh_generation, cannot).await);
            }
            // **A listing that failed for any other reason was never read**:
            // nothing is known about the page, so it is not gone.
            Err(cannot) => {
                return Err(dead_driver_answer(DeadDriverOutcome::ListFailed { why: &cannot }));
            }
        };
        let Some(index) = index else {
            return Err(dead_driver_answer(DeadDriverOutcome::TabGone));
        };
        if let Err(cannot) = fresh.select(index, bound).await {
            return Err(dead_driver_answer(DeadDriverOutcome::RetryFailed { why: &cannot }));
        }
        match fresh.call_within(tool, args.clone(), bound).await {
            Ok(parts) => {
                profile.state.lock().await.stale_since = None;
                Ok(parts)
            }
            Err(retry_why) => {
                Err(dead_driver_answer(DeadDriverOutcome::RetryFailed { why: &retry_why }))
            }
        }
    }

    /// Attach a viewer to one tab: its newest frame, the tab's viewer state,
    /// and the profile's key and tab for the calls that follow.
    ///
    /// `tab` of `None` is the asking seat's current tab, else the profile's
    /// first. Attaching also asks the host for a full-quality frame, so a
    /// viewer joining mid-run draws a crisp page first.
    pub async fn attach(
        &self,
        viewer: u64,
        seat: &SessionSlot,
        tab: Option<u16>,
    ) -> Result<Attachment, String> {
        self.note_live(seat);
        let key = self.target(seat)?;
        let was =
            self.where_viewing.lock().unwrap_or_else(PoisonError::into_inner).get(&viewer).cloned();
        // **A move to another profile lets the old one go before it opens.**
        // `open` can start a cold host, and the old tab would otherwise keep
        // this viewer, and the role it held there, warm for the whole wait.
        if was.as_ref().is_some_and(|(was_key, _)| was_key != key.as_str()) {
            self.detach(viewer).await;
        }
        let profile = self.open(seat).await?;
        let tab = if let Some(tab) = tab {
            tab
        } else {
            let tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
            tabs.current(seat).or_else(|| tabs.numbers().first().copied()).ok_or_else(|| {
                format!("the browser profile '{}' has no tab to show yet", profile.key)
            })?
        };
        let exists =
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).target(tab).is_some();
        if !exists {
            return Err(format!("tab {tab} is not open"));
        }
        // **One tab at a time per viewer**: a surface that moved leaves the
        // tab it was on, and a role it held there goes with it. **The same
        // tab is not a move**: an open that lands twice, or a re-attach to
        // the tab already held, is one viewer on one page, so the role it
        // took there survives.
        let staying =
            was.as_ref().is_some_and(|(key, viewing)| key == &profile.key && *viewing == tab);
        if !staying {
            self.detach(viewer).await;
        }
        // **A re-attach is where a role went missing without a line.** The
        // entry names the profile and tab the viewer was on and whether this
        // attach is the same surface - the same profile AND the same tab -
        // so a surface driving a tab it does not control, or no longer
        // holding a role it had, is answerable from the log.
        tracing::debug!(
            event_name = "browser_attach",
            profile = %profile.key,
            tab,
            viewer,
            was_profile = was.as_ref().map(|(was_key, _)| was_key.as_str()),
            was_tab = was.as_ref().map(|(_, viewing)| *viewing),
            same_surface = staying,
            "a viewer attached to a tab",
        );
        let frames = profile.frame_receiver(tab);
        let view = profile.view_for(tab).subscribe();
        let nav = profile.nav_receiver(tab);
        let failures = profile.failures.subscribe();
        let files = profile.files.subscribe();
        let downloads = profile.downloads.subscribe();
        let audio = profile.audio.subscribe();
        {
            let mut viewers = profile.viewers.lock().unwrap_or_else(PoisonError::into_inner);
            viewers.entry(tab).or_default().insert(viewer);
        }
        self.where_viewing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(viewer, (profile.key.clone(), tab));
        profile.publish_view(tab);
        {
            let state = profile.state.lock().await;
            if let Some(host) = state.host.as_ref() {
                // **A viewer is watching now**: resume encoding, ask for the
                // crisp frame its first draw is owed, and re-assert the page
                // visible so a page that went hidden while unwatched paints
                // rather than drawing its last stale frame.
                host.ring.pause(false);
                host.ring.key();
                host.ring.show();
            }
        }
        self.publish_rows(&profile.key).await;
        let tabs = self.rows_sender(&profile.key).subscribe();
        Ok(Attachment {
            key: profile.key.clone(),
            id: profile.id,
            tab,
            frames,
            view,
            nav,
            failures,
            files,
            downloads,
            audio,
            tabs,
        })
    }

    /// A viewer is gone: its count comes off the tab it was on, and a role it
    /// held is released with it.
    pub async fn detach(&self, viewer: u64) {
        self.where_viewing.lock().unwrap_or_else(PoisonError::into_inner).remove(&viewer);
        let profiles: Vec<Arc<Profile>> = self.profiles.lock().await.values().cloned().collect();
        for profile in profiles {
            // **The departure and the chooser it cancels are one decision**,
            // so a chooser event cannot publish an ask for a controller who
            // has already gone.
            let _decision = profile.file_decision.lock().await;
            let mut left: HashSet<u16> = HashSet::new();
            let mut removed = false;
            {
                let mut viewers = profile.viewers.lock().unwrap_or_else(PoisonError::into_inner);
                for (tab, watchers) in viewers.iter_mut() {
                    if watchers.remove(&viewer) {
                        removed = true;
                        left.insert(*tab);
                    }
                }
                viewers.retain(|_, watchers| !watchers.is_empty());
            }
            let mut released = false;
            {
                let mut controls = profile.controls.lock().unwrap_or_else(PoisonError::into_inner);
                let held: Vec<u16> = controls
                    .iter()
                    .filter(|(_, (holder, _))| *holder == viewer)
                    .map(|(tab, _)| *tab)
                    .collect();
                for tab in held {
                    controls.remove(&tab);
                    left.insert(tab);
                    released = true;
                    tracing::debug!(
                        event_name = "browser_role_released",
                        profile = %profile.key,
                        tab,
                        viewer,
                        "a viewer gave the controller role up",
                    );
                }
            }
            if released {
                // **The person driving it is gone**: a chooser they were
                // being asked is not the next viewer's to answer.
                profile.cancel_files().await;
            }
            for tab in left {
                profile.publish_view(tab);
            }
            if removed || released {
                self.publish_rows(&profile.key).await;
            }
            if removed {
                self.pause_unwatched(&profile).await;
            }
        }
    }

    /// End the seat's browser and forget it. The next call relaunches it over
    /// the same directory, so a close ends the run and never the logins.
    pub async fn close(&self, seat: &SessionSlot) -> Result<(), String> {
        // **The session stays live when its browser closes.** Only the
        // profile ends here; the next call opens it again, so this must not
        // tombstone the seat as session-ended.
        let key = self.target(seat)?;
        // **The entry leaves the map before its host is ended, and the
        // map lock is released with it**: a same-key call waits on the
        // key's own gate, while every other profile keeps being served
        // through the whole teardown.
        let gate = self.gate(key.as_str());
        let _held = gate.lock().await;
        let profile = self.profiles.lock().await.remove(key.as_str());
        let Some(profile) = profile else {
            return Err(format!("no browser profile is open under '{}'", key.as_str()));
        };
        self.forget_holds(key.as_str());
        self.forget_viewing(key.as_str());
        profile.end(HostEnd::Closed).await;
        self.publish_rows(key.as_str()).await;
        Ok(())
    }

    /// Give `viewer` the controller role for one tab.
    ///
    /// **Exactly one controller per tab.** A take moves the role to the
    /// caller and the previous holder becomes a watcher; there is no default
    /// holder and no override machinery - a surface starts controller-less.
    pub async fn take_control(&self, profile: &str, tab: u16, viewer: u64) -> Result<(), String> {
        let profile = self.find(profile).await?;
        if profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).target(tab).is_none() {
            return Err(format!("tab {tab} is not open"));
        }
        // **A role change and the chooser it cancels are one decision**: a
        // chooser event cannot read the old controller and publish after the
        // role has moved.
        let _decision = profile.file_decision.lock().await;
        profile.controls.lock().unwrap_or_else(PoisonError::into_inner).insert(tab, (viewer, 0));
        profile.cancel_files().await;
        profile.publish_view(tab);
        // **A person driving a tab is using it**, so the strip says so.
        self.publish_rows(&profile.key).await;
        self.focus(&profile, tab).await;
        Ok(())
    }

    /// Release the role, when this viewer still holds it on that tab.
    pub async fn release_control(&self, profile: &str, tab: u16, viewer: u64) {
        if let Ok(profile) = self.find(profile).await {
            let _decision = profile.file_decision.lock().await;
            let released = {
                let mut controls = profile.controls.lock().unwrap_or_else(PoisonError::into_inner);
                let released = controls.get(&tab).is_some_and(|(holder, _)| *holder == viewer);
                if released {
                    controls.remove(&tab);
                }
                released
            };
            if released {
                // **The one role change a person makes by hand.** A bar's
                // release is the person's own act, and it is recorded here
                // with the same fields a departure's release carries.
                tracing::debug!(
                    event_name = "browser_role_released",
                    profile = %profile.key,
                    tab,
                    viewer,
                    "a viewer gave the controller role up",
                );
                profile.cancel_files().await;
                profile.publish_view(tab);
                // The tab is nobody's to drive now, so it is idle again.
                self.publish_rows(&profile.key).await;
            }
        }
    }

    /// Lay one tab out to a viewer's surface.
    ///
    /// **A held controller owns the layout** - the page must not move under
    /// the person driving it - and with nobody holding the role it belongs to
    /// the tab's longest-watching viewer, never to whoever asks. A size the
    /// page already has is not a request: a re-attach or a re-measure that
    /// lands on the same box must not force the host through another layout.
    pub async fn resize(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        width: u32,
        height: u32,
        scale: f64,
    ) {
        let Ok(profile) = self.find(profile).await else { return };
        let controller = profile
            .controls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tab)
            .map(|(holder, _)| *holder);
        let watchers = profile
            .viewers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tab)
            .cloned()
            .unwrap_or_default();
        let Some((width, height, scale)) =
            owner_size(controller, &watchers, viewer, width, height, scale)
        else {
            return;
        };
        // **The surface and the request are one step**: two resizes
        // interleaving would leave the ring painting one size while the
        // stored surface named the other, which is what a take restores.
        let _serial = profile.resize.lock().await;
        // **A take owns the layout**: the controller's resize is remembered
        // and applied again when the recording ends.
        {
            let mut surface = profile.surface.lock().unwrap_or_else(PoisonError::into_inner);
            if *surface == (width, height, scale) {
                return;
            }
            *surface = (width, height, scale);
        }
        if profile.recording.lock().unwrap_or_else(PoisonError::into_inner).is_some() {
            return;
        }
        let state = profile.state.lock().await;
        if let Some(host) = state.host.as_ref() {
            host.ring.request(width, height, scale);
        }
    }

    /// Resize the browser's painted surface from a session's own tool call.
    ///
    /// **The tool means the window, not a viewport override.** Upstream's
    /// `browser_resize` is Playwright's `page.setViewportSize`, which tells
    /// the page one size while the host paints another and leaves the device
    /// ratio at 1 for the rest of the host's life. Here the window is the
    /// profile's ring, so the viewport, the ratio and the painted size all
    /// follow it - the path a viewer's own `BrowserResize` already takes.
    async fn resize_window(
        &self,
        seat: &SessionSlot,
        args: &Value,
    ) -> Result<Vec<BrowserPart>, String> {
        let profile = self.open(seat).await?;
        // **One critical section for the read, the request and the write**, so
        // a viewer's resize cannot land between them and leave the stored
        // surface naming a size the ring never painted.
        let _serial = profile.resize.lock().await;
        // **The take's size is the take's to promise**: a recording start
        // takes this same lock before it lays its tab out, so no take can
        // begin between this check and the request below.
        let running = profile.recording.lock().unwrap_or_else(PoisonError::into_inner).is_some();
        if running {
            return Err(format!(
                "a recording is running for '{}', so the page stays at {}x{} until \
                 browser_record_stop",
                profile.key,
                crate::record::WIDTH,
                crate::record::HEIGHT,
            ));
        }
        let scale = profile.surface.lock().unwrap_or_else(PoisonError::into_inner).2;
        let width = window_dimension(args, "width", scale, crate::osr::MAX_WIDTH)?;
        let height = window_dimension(args, "height", scale, crate::osr::MAX_HEIGHT)?;
        let (device_width, device_height) =
            (device_pixels(width, scale), device_pixels(height, scale));
        {
            let state = profile.state.lock().await;
            let host = state
                .host
                .as_ref()
                .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
            host.ring.request(device_width, device_height, scale);
        }
        // **The surface is written back, as a viewer's own resize writes
        // it**: a take restores this size when it ends, and the next resize
        // from either side replaces it.
        *profile.surface.lock().unwrap_or_else(PoisonError::into_inner) =
            (device_width, device_height, scale);
        Ok(vec![BrowserPart::Text { text: format!("The browser window is now {width}x{height}.") }])
    }

    /// Inject one input event into one tab, and only for the viewer that holds
    /// its role.
    ///
    /// **The role is checked before the event is mapped or sent**, so input
    /// never reaches a page from a viewer that does not control it. An event
    /// at or below the last sequence the role carried was superseded before
    /// it arrived, and is dropped.
    pub async fn input(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        seq: u64,
        event: &BrowserInputEvent,
    ) -> Result<(), String> {
        let profile = self.find(profile).await?;
        {
            let mut controls = profile.controls.lock().unwrap_or_else(PoisonError::into_inner);
            match controls.get_mut(&tab) {
                Some((holder, last)) if *holder == viewer => {
                    if seq <= *last {
                        return Ok(());
                    }
                    *last = seq;
                }
                _ => return Err("this viewer is not the browser's controller".to_owned()),
            }
        }
        if profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).target(tab).is_none() {
            return Err(format!("tab {tab} is not open"));
        }
        // **The person's own input re-focuses their tab.** Another session's
        // call focuses its own, and the history watch follows the focused
        // tab; without this the arrows freeze on a tab the person is not
        // looking at.
        self.focus(&profile, tab).await;
        let (input, port) = {
            let state = profile.state.lock().await;
            let host = state
                .host
                .as_ref()
                .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
            (host.input.clone(), host.port)
        };
        match input::route(event, tab) {
            // **A pointer gesture is the host's**, whose own mouse calls are
            // what CEF routes into a windowless popup.
            input::Route::Host(line) => {
                input.send(line).map_err(|_| {
                    format!("the browser for '{}' is not taking input", profile.key)
                })?;
                // **A click can navigate, and CEF swaps the page's target.**
                // The next history read or Back would otherwise drive the
                // target the page had before the click; refresh it from the
                // driver now rather than waiting for an agent call.
                if matches!(
                    event,
                    BrowserInputEvent::Mouse {
                        action: MouseAction::Press | MouseAction::Release,
                        ..
                    }
                ) {
                    let profile = Arc::clone(&profile);
                    tokio::spawn(async move {
                        let _ = live_target(&profile, tab).await;
                    });
                }
                Ok(())
            }
            input::Route::Page(method, params) => {
                // **Typing must not pay a driver round trip per key.** The
                // registry's target is what the nav watch and the mouse
                // refresh keep current; a page that moved under a navigation
                // refuses the send once, and only then is the driver asked.
                let target = profile
                    .tabs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .target(tab)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("tab {tab} is not open"))?;
                let mut state = profile.state.lock().await;
                match send_page(&mut state, port, &target, method, params.clone()).await {
                    Ok(()) => Ok(()),
                    Err(first) => {
                        drop(state);
                        let LiveTarget::Found(fresh) = live_target(&profile, tab).await else {
                            return Err(first);
                        };
                        if fresh == target {
                            return Err(first);
                        }
                        let mut state = profile.state.lock().await;
                        send_page(&mut state, port, &fresh, method, params).await
                    }
                }
            }
        }
    }

    /// Answer a page's file chooser: the files a person chose, or none.
    ///
    /// **The bytes are written before the host is told**, because CEF reads
    /// a chosen file's content when the callback returns. A dialog past the
    /// caps, or an answer for a profile that is not up, is a cancel - the
    /// page gets no file rather than a path to something it cannot read.
    pub async fn answer_files(
        &self,
        profile: &str,
        tab: u16,
        id: u64,
        files: Vec<BrowserUpload>,
    ) -> Result<(), String> {
        let profile = self.find(profile).await?;
        if profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).target(tab).is_none() {
            return Err(format!("tab {tab} is not open"));
        }
        // **The answer is one chooser decision**, so a role change or an
        // agent's upload cannot interleave between the check and the driver
        // call that answers it.
        let _decision = profile.file_decision.lock().await;
        // **A stale answer touches nothing.** The outstanding ask is checked
        // before the uploads directory is cleared or a byte is written, so a
        // superseded answer cannot wipe the files a live dialog is handing
        // over.
        let outstanding = profile.files.borrow().as_ref().copied();
        match &outstanding {
            Some(ask) if ask.id == id && ask.tab == tab => {}
            // **A chooser belongs to its tab's viewer.** A watcher on another
            // tab answering the id it saw on the wire would hand files to a
            // page it is not looking at.
            Some(ask) if ask.id == id => {
                return Err(format!(
                    "the file chooser for id {id} is on tab {}, not tab {tab}",
                    ask.tab
                ));
            }
            _ => {
                return Err(format!(
                    "the file chooser for id {id} is not outstanding, so the answer is refused"
                ));
            }
        }
        let total: usize = files.iter().map(|file| file.bytes.len()).sum();
        let cap = usize::try_from(MAX_UPLOAD_BYTES).unwrap_or(usize::MAX);
        let paths = if files.is_empty() || files.len() > MAX_UPLOAD_FILES || total > cap {
            if !files.is_empty() {
                tracing::warn!(
                    "a file answer was past the caps and is cancelled (event_name \
                     browser_file_over_cap, profile {}, files {}, bytes {total})",
                    profile.key,
                    files.len(),
                );
            }
            Vec::new()
        } else {
            let dir = profile.dir.join("uploads");
            let _ = std::fs::remove_dir_all(&dir);
            write_uploads(&dir, &files)?
        };
        // **The driver's chooser is answered through the tool that owns it**;
        // the CEF host's dialog is the fallback for a browser with no driver.
        let playwright = profile
            .playwright_ask
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some_and(|held| held == id);
        if playwright {
            let calls =
                { profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).clone() };
            let Some(calls) = calls else {
                return Err(format!("the browser driver for '{}' is not up", profile.key));
            };
            let args = if paths.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::json!({ "paths": paths })
            };
            // **The person's answer is one chooser decision under the lock**,
            // so its MCP call is bounded tightly rather than at the driver's
            // own 150 s: a wedged driver must not hold a role change out.
            let outcome = calls.call(args, Duration::from_secs(10)).await;
            if outcome.is_ok() {
                note_surfaced_consumed_by(&profile, 1);
            }
            if let Err(why) = &outcome {
                tracing::warn!(
                    "the driver could not answer the person's file chooser (event_name \
                     browser_file_answer_failed, profile {}, id {id}): {why}",
                    profile.key,
                );
                // **A failed or timed-out answer can leave the captured
                // chooser pending**: dismiss it, so the page does not hang
                // behind a chooser nobody can answer and a retry can raise a
                // new one.
                let _ = calls.call(serde_json::json!({}), Duration::from_secs(5)).await;
            }
            {
                let mut held =
                    profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner);
                if *held == Some(id) {
                    *held = None;
                }
            }
            if profile.files.borrow().as_ref().map(|ask| ask.id) == Some(id) {
                profile.files.send_replace(None);
            }
            return outcome.map(|_| ());
        }
        let state = profile.state.lock().await;
        let host = state
            .host
            .as_ref()
            .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
        host.input
            .send(input::host_line(&crate::input::HostInput::File { id, paths }))
            .map_err(|_| format!("the browser for '{}' is not taking files", profile.key))?;
        drop(state);
        if profile.files.borrow().as_ref().map(|ask| ask.id) == Some(id) {
            profile.files.send_replace(None);
        }
        Ok(())
    }

    /// Begin recording the page's viewport, after laying it out at the file's
    /// size.
    ///
    /// **The page is re-laid at [`crate::record::WIDTH`] by
    /// [`crate::record::HEIGHT`]**, so the file holds the layout the page had
    /// at that size rather than a scaled copy of the controller's surface.
    /// Stopping puts the controller's own view back.
    pub async fn record_start(&self, seat: &SessionSlot) -> Result<Vec<BrowserPart>, String> {
        /// Recordings kept beside the takes: the newest ten, this one
        /// included.
        const KEEP: usize = 10;
        /// A take refuses to start below this much free space.
        const FLOOR: u64 = 512 * 1024 * 1024;
        let profile = self.open(seat).await?;
        if profile.recording.lock().unwrap_or_else(PoisonError::into_inner).is_some() {
            return Err(format!(
                "a recording is already running for '{}'; stop it first",
                profile.key
            ));
        }
        let tab = profile
            .tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .current(seat)
            .ok_or_else(|| "this session has no tab to record".to_owned())?;
        let key = self.target(seat)?;
        let dir = self.recordings_dir(&key);
        std::fs::create_dir_all(&dir)
            .map_err(|why| format!("the recordings directory cannot be made: {why}"))?;
        if let Some(free) = free_bytes(&dir)
            && free < FLOOR
        {
            return Err(format!(
                "the disk has {} MiB free, and a recording needs 512 MiB",
                free / (1024 * 1024)
            ));
        }
        let input = {
            let state = profile.state.lock().await;
            let host = state
                .host
                .as_ref()
                .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
            host.input.clone()
        };
        prune_recordings(&dir, KEEP);
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let path = dir.join(format!("{stamp}.mp4"));
        // **A take's layout is a resize's business**: the lock a resize holds
        // across its request spans this start, so a window request cannot land
        // after the take laid its tab out.
        let serial = profile.resize.lock().await;
        let running = profile.recording.lock().unwrap_or_else(PoisonError::into_inner).is_some();
        if running {
            return Err(format!(
                "a recording is already running for '{}'; stop it first",
                profile.key
            ));
        }
        profile.recorded.send_replace(None);
        let mut recorded = profile.recorded.subscribe();
        *profile.recording.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((tab, path.clone()));
        // **The layout follows the take, on the recorded tab alone.** The
        // profile's request would resize every tab; a per-tab resize leaves
        // the others as their viewers laid them out.
        let resize = input::host_line(&crate::input::HostInput::TabResize {
            tab,
            width: crate::record::WIDTH,
            height: crate::record::HEIGHT,
            scale_milli: 1_000,
        });
        let line = input::host_line(&crate::input::HostInput::RecordStart {
            tab,
            path: path.to_string_lossy().into_owned(),
        });
        if let Err(why) = input.send(resize).and_then(|()| input.send(line)) {
            *profile.recording.lock().unwrap_or_else(PoisonError::into_inner) = None;
            restore_tab_surface(&profile, &input, tab);
            return Err(format!(
                "the browser for '{}' is not taking a recording: {why}",
                profile.key
            ));
        }
        // **The lock spans the start, not the take**: a resize during it reads
        // `recording` and refuses.
        drop(serial);
        match recording_answer(&mut recorded, None, Duration::from_secs(10)).await {
            Some(state) if state.started => Ok(vec![BrowserPart::Text {
                text: format!(
                    "Recording the page into {}. It is a silent 1280x720 MP4 at 30 frames a \
                     second; call browser_record_stop to finish it, and it stops itself at \
                     180 seconds.",
                    path.display()
                ),
            }]),
            Some(state) => {
                *profile.recording.lock().unwrap_or_else(PoisonError::into_inner) = None;
                restore_tab_surface(&profile, &input, tab);
                Err(state.error.unwrap_or_else(|| "the recording writer did not start".to_owned()))
            }
            // The host never answered; a writer that did open is recording,
            // so the take is left running and the stop is its answer.
            None => Ok(vec![BrowserPart::Text {
                text: format!(
                    "Recording into {}; the host has not confirmed the writer opened yet. Call \
                     browser_record_stop to finish the take.",
                    path.display()
                ),
            }]),
        }
    }

    /// Finish the running recording, answer what was written, and put the
    /// controller's view back.
    pub async fn record_stop(&self, seat: &SessionSlot) -> Result<Vec<BrowserPart>, String> {
        // **A stop never starts a browser.** The take lives in the host that
        // was recording, so a profile that is not open has no take to end.
        let key = self.target(seat)?;
        let Ok(profile) = self.find(key.as_str()).await else {
            return Err(format!("no recording is running for '{}'", key.as_str()));
        };
        let taken = profile.recording.lock().unwrap_or_else(PoisonError::into_inner).take();
        let Some((tab, path)) = taken else {
            return Err(format!("no recording is running for '{}'", profile.key));
        };
        let input = {
            let state = profile.state.lock().await;
            let host = state
                .host
                .as_ref()
                .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
            host.input.clone()
        };
        let mut recorded = profile.recorded.subscribe();
        let delivered = input.send(input::host_line(&crate::input::HostInput::RecordStop));
        let answer = if delivered.is_ok() {
            recording_answer(&mut recorded, Some(false), Duration::from_secs(30)).await
        } else {
            None
        };
        profile.recorded.send_replace(None);
        restore_tab_surface(&profile, &input, tab);
        if delivered.is_err() {
            return Err(format!("the browser for '{}' is not taking a stop", profile.key));
        }
        let Some(state) = answer else {
            return Err(format!(
                "the recording did not finish in time; a partial file may be at {}",
                path.display()
            ));
        };
        if let Some(why) = state.error {
            return Err(format!(
                "the recording did not finish ({why}); a partial file may be at {}",
                path.display()
            ));
        }
        let size = state.bytes / (1024 * 1024);
        let capped =
            if state.capped { " It reached the 180 second cap and stopped itself." } else { "" };
        Ok(vec![BrowserPart::Text {
            text: format!(
                "The recording is written: {} ({}s, {} MiB).{capped}",
                path.display(),
                state.seconds,
                size,
            ),
        }])
    }

    /// Where one tab's history sits, or nothing when it cannot be read.
    pub async fn nav_state(&self, profile: &str, tab: u16) -> Option<BrowserNavState> {
        let profile = self.find(profile).await.ok()?;
        // **The attach path reads the cached target.** A driver listing per
        // attach is a Node round trip in front of the first arrows; only a
        // page that refuses the cached target is asked about.
        let target = profile
            .tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .target(tab)
            .map(str::to_owned)?;
        let mut state = profile.state.lock().await;
        let port = state.host.as_ref().map(|host| host.port)?;
        let reply = if let Ok(reply) =
            call_page(&mut state, port, &target, "Page.getNavigationHistory", json!({})).await
        {
            reply
        } else {
            drop(state);
            let LiveTarget::Found(target) = live_target(&profile, tab).await else {
                return None;
            };
            let mut state = profile.state.lock().await;
            call_page(&mut state, port, &target, "Page.getNavigationHistory", json!({}))
                .await
                .ok()?
        };
        let nav = input::history_state(&reply)?;
        profile.publish_nav(tab, nav);
        Some(nav)
    }

    /// Move one tab through its history, answering where it now sits.
    ///
    /// **Only that tab's controller moves the page.** The arrows are the same
    /// page a watcher is looking at, so a watcher's move would move the person
    /// driving it; the role is checked before the history is read.
    pub async fn nav(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        back: bool,
    ) -> Result<BrowserNavState, String> {
        let profile = self.find(profile).await?;
        let holding = profile
            .controls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tab)
            .is_some_and(|(holder, _)| *holder == viewer);
        if !holding {
            return Err("this viewer is not the browser's controller".to_owned());
        }
        let LiveTarget::Found(target) = live_target(&profile, tab).await else {
            return Err(format!("tab {tab} is not open"));
        };
        let mut state = profile.state.lock().await;
        let port = state
            .host
            .as_ref()
            .map(|host| host.port)
            .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
        let reply =
            call_page(&mut state, port, &target, "Page.getNavigationHistory", json!({})).await?;
        let (index, entries) = input::history_position(&reply)
            .ok_or_else(|| "the page's history could not be read".to_owned())?;
        let target_entry = if back {
            index.checked_sub(1)
        } else {
            (index + 1 < entries.len()).then_some(index + 1)
        };
        if let Some(entry) = target_entry
            && let Some(id) =
                entries.get(entry).and_then(|entry| entry.get("id")).and_then(Value::as_i64)
        {
            call_page(
                &mut state,
                port,
                &target,
                "Page.navigateToHistoryEntry",
                json!({ "entryId": id }),
            )
            .await?;
        }
        let reply =
            call_page(&mut state, port, &target, "Page.getNavigationHistory", json!({})).await?;
        let nav = input::history_state(&reply)
            .ok_or_else(|| "the page's history could not be read".to_owned())?;
        profile.publish_nav(tab, nav);
        Ok(nav)
    }

    fn stack(&self) -> &Path {
        &self.stack
    }

    async fn find(&self, key: &str) -> Result<Arc<Profile>, String> {
        self.profiles
            .lock()
            .await
            .get(key)
            .map(Arc::clone)
            .ok_or_else(|| format!("no browser profile is open under '{key}'"))
    }

    /// A seat's tabs as its strip draws them.
    pub async fn tabs(&self, seat: &SessionSlot) -> Result<BrowserTabList, String> {
        let key = self.profile_for(seat)?;
        self.publish_rows(key.as_str()).await;
        Ok(self.rows_sender(key.as_str()).borrow().clone())
    }

    /// A stream of a seat's tab rows, for a strip that keeps it open.
    pub async fn tab_stream(
        &self,
        seat: &SessionSlot,
    ) -> Result<watch::Receiver<BrowserTabList>, String> {
        let key = self.profile_for(seat)?;
        self.publish_rows(key.as_str()).await;
        Ok(self.rows_sender(key.as_str()).subscribe())
    }

    fn rows_sender(&self, key: &str) -> watch::Sender<BrowserTabList> {
        self.rows
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.to_owned())
            .or_insert_with(|| {
                watch::channel(BrowserTabList {
                    profile: key.to_owned(),
                    id: 0,
                    up: false,
                    connected: false,
                    closing_at_ms: None,
                    rows: Vec::new(),
                })
                .0
            })
            .clone()
    }

    /// Republish one profile's rows: its tabs, its connection and the idle
    /// deadline. **A closed profile answers an empty list with no deadline**,
    /// so a strip never draws a browser that is gone as one still running.
    async fn publish_rows(&self, key: &str) {
        let profile = self.profiles.lock().await.get(key).cloned();
        let held = self.held_tabs(key);
        let idle = self.idle_settings().map(|settings| Duration::from_secs(settings.minutes * 60));
        let (id, up, connected, closing_at_ms, rows) = match &profile {
            Some(profile) => {
                let connected = {
                    let state = profile.state.lock().await;
                    state.driver.as_ref().is_some_and(|driver| driver.is_running())
                };
                let up = profile.up.load(Ordering::Acquire);
                let touch = *profile.touch.lock().unwrap_or_else(PoisonError::into_inner);
                let controlled: HashSet<u16> = profile
                    .controls
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .keys()
                    .copied()
                    .collect();
                let rows = profile
                    .tabs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .rows(&held, &controlled);
                let closing_at_ms = idle.filter(|_| up).map(|idle| {
                    let remaining = (touch.at + idle).saturating_duration_since(Instant::now());
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX));
                    now.saturating_add(u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX))
                });
                (profile.id, up, connected, closing_at_ms, rows)
            }
            None => (0, false, false, None, Vec::new()),
        };
        self.rows_sender(key).send_replace(BrowserTabList {
            profile: key.to_owned(),
            id,
            up,
            connected,
            closing_at_ms,
            rows,
        });
    }

    /// The tab numbers of one profile a parked hand-off holds.
    fn held_tabs(&self, key: &str) -> HashSet<u16> {
        let holds = self.holds.lock().unwrap_or_else(PoisonError::into_inner).clone();
        holds
            .into_iter()
            .filter_map(|(seat, tab)| {
                let profile = self.profile_for(&seat).ok()?;
                (profile.as_str() == key).then_some(tab)
            })
            .collect()
    }

    fn forget_holds(&self, key: &str) {
        self.holds.lock().unwrap_or_else(PoisonError::into_inner).retain(|(seat, _)| {
            self.profile_for(seat).is_ok_and(|profile| profile.as_str() != key)
        });
    }

    fn forget_viewing(&self, key: &str) {
        let leaving: Vec<u64> = self
            .where_viewing
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, (profile, _))| profile == key)
            .map(|(viewer, _)| *viewer)
            .collect();
        let mut where_viewing = self.where_viewing.lock().unwrap_or_else(PoisonError::into_inner);
        for viewer in leaving {
            where_viewing.remove(&viewer);
        }
    }
}

/// A viewer's handle on one tab, as the transport keeps it.
pub struct Attachment {
    /// The profile's key, for the calls that follow (input, control, nav).
    pub key: String,
    /// The id the binary frames carry.
    pub id: u16,
    /// The tab this viewer is on, for every call that follows.
    pub tab: u16,
    /// The newest frame, and a way to follow it.
    pub frames: watch::Receiver<Option<BrowserFrame>>,
    /// The tab's controller and the viewer count, as they change.
    pub view: watch::Receiver<BrowserViewState>,
    /// The profile's tabs, as they change.
    pub tabs: watch::Receiver<BrowserTabList>,
    /// Where the page's history sits, as it moves.
    pub nav: watch::Receiver<Option<BrowserNavState>>,
    /// Why the browser stopped, when it stopped.
    pub failures: watch::Receiver<Option<String>>,
    /// A page's file chooser, while a person has to answer one.
    pub files: watch::Receiver<Option<BrowserFileAsk>>,
    /// Finished downloads, each once, for the viewer that takes it.
    pub downloads: broadcast::Receiver<BrowserDownload>,
    /// The page's own audio, streamed while it plays.
    pub audio: broadcast::Receiver<BrowserAudio>,
}

/// Choosers the driver surfaced that no answer has consumed, and the ones an
/// agent answered before their event was read. Both are guarded by
/// `Profile::file_decision`.
#[derive(Default)]
struct ChooserLedger {
    surfaced: usize,
    pre_answered: usize,
}

type ChooserFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<BrowserPart>, String>> + Send + 'a>>;

/// The MCP call one chooser decision makes. **A seam for the race tests**:
/// the real driver answers through `browser_file_upload`, and a test gates
/// that call to prove the decision lock spans it.
trait ChooserAnswer: Send + Sync {
    fn call(&self, args: Value, bound: Duration) -> ChooserFuture<'_>;
}

impl ChooserAnswer for Driver {
    fn call(&self, args: Value, bound: Duration) -> ChooserFuture<'_> {
        Box::pin(async move { self.call_within("browser_file_upload", args, bound).await })
    }
}

/// One open profile: its identity, its hot state and its frames.
pub struct Profile {
    key: String,
    /// The id the frames and the wire carry for this profile.
    id: u16,
    /// The profile's CEF cache root: cookies and storage.
    dir: PathBuf,
    /// The `[browser]` encoding pair, fixed at the profile's creation.
    encoding: (u16, u8),
    /// The service's launch lock, shared by every profile.
    launch: Arc<AsyncMutex<()>>,
    /// The service's stop flag, shared by every profile.
    stopped: Arc<AtomicBool>,
    /// **Each tab's newest frame is its own watch.** One profile-wide slot
    /// let a background tab's paint replace the viewed tab's, and an attach
    /// burst - every tab asked for a key frame - could blank the surface.
    frames: Mutex<HashMap<u16, watch::Sender<Option<BrowserFrame>>>>,
    /// The sequence the relayed frames carry for each tab. **A host restarts
    /// its own count at zero**, so the wire's sequence is kept here and never
    /// goes backwards across a host death.
    frame_seqs: Mutex<HashMap<u16, u32>>,
    /// Where the page's history sits, published when it moves: a link click
    /// moves the bar's arrows without anyone asking.
    /// **Each tab's history is its own**: a watch per tab, so a viewer's
    /// arrows never show another tab's page.
    nav: Mutex<HashMap<u16, watch::Sender<Option<BrowserNavState>>>>,
    /// Why the browser stopped, when it stopped under a viewer.
    failures: watch::Sender<Option<String>>,
    /// A page's file chooser waiting on the person, as it is raised.
    files: watch::Sender<Option<BrowserFileAsk>>,
    /// The ask the driver's Playwright chooser owns, when one is open.
    /// **Its ids start above the CEF dialog's small counter**, so an answer
    /// routes to the path that raised it.
    playwright_ask: Mutex<Option<u64>>,
    next_file_ask: AtomicU64,
    /// **One chooser decision at a time.** A role change, a person's answer
    /// and an agent's upload all decide who answers the same chooser; this
    /// lock spans each check and its driver call, so no two can interleave.
    file_decision: AsyncMutex<()>,
    /// **One rebuild at a time.** A dead page's rebuild reads the cooldown,
    /// clears the driver and starts a fresh one; two calls doing that at once
    /// would each pass a stale check, and the second would clear the fresh
    /// driver the first installed.
    rebuild: AsyncMutex<()>,
    /// Choosers the driver surfaced, and the ones an agent answered before
    /// their event was read. **An agent's upload answers the oldest
    /// surfaced chooser**, so one success consumes one surface or, with none
    /// known yet, is owed one event.
    chooser_ledger: Mutex<ChooserLedger>,
    /// The driver call a chooser answer makes, when a driver is up.
    chooser_answer: Mutex<Option<Arc<dyn ChooserAnswer>>>,
    /// Finished downloads, each announced once to whichever viewer takes it.
    downloads: broadcast::Sender<BrowserDownload>,
    /// The page's own audio, streamed while it plays.
    audio: broadcast::Sender<BrowserAudio>,
    /// What the host last said about a recording.
    recorded: watch::Sender<Option<RecordingState>>,
    /// The running take's tab and file, when one runs.
    recording: Mutex<Option<(u16, PathBuf)>>,
    /// The controller's last requested view, restored when a take ends.
    surface: Mutex<(u32, u32, f64)>,
    /// **A resize's request and the surface it stores are one step.** Two
    /// resizes and a take's own layout all move the same page; this lock
    /// spans each one's read, request and write, so the stored surface always
    /// belongs to the request the ring holds last.
    resize: AsyncMutex<()>,
    /// The tab this profile's tabs are, keyed by the host's own numbers.
    tabs: Mutex<TabRegistry>,
    /// The host's boot tab, until a session's first page call adopts it.
    boot: Mutex<Option<(u16, String)>>,
    /// The tab the driver's current page is, so a chooser and the history
    /// watch read the right one.
    focused: Mutex<Option<u16>>,
    /// Each tab's controller: its viewer id, and the last input sequence it
    /// sent.
    controls: Mutex<HashMap<u16, (u64, u64)>>,
    /// The viewer ids attached to each tab right now.
    viewers: Mutex<HashMap<u16, HashSet<u64>>>,
    /// Each tab's viewer state, published when a role or the count moves.
    views: Mutex<HashMap<u16, watch::Sender<BrowserViewState>>>,
    /// **One call at a time per profile**, in arrival order: two calls that
    /// both selected and drove would swap the driver's current page under
    /// each other.
    call_lock: AsyncMutex<()>,
    /// The host's own tab births and ends, for the call path that pairs them
    /// with page targets. `None` until a host starts.
    host_tab_events: AsyncMutex<Option<mpsc::UnboundedReceiver<(u64, HostEvent)>>>,
    /// Host tab events a reconcile could not pair yet: **the host reports a
    /// popup before the driver lists its page**, so an unmatched event waits
    /// for the next pass rather than being lost.
    deferred_tab_events: Mutex<Vec<(u64, HostEvent)>>,
    /// Where a page's popup asks for a pairing pass. **A birth nothing asked
    /// for must still become a row**: a person clicking "Continue with
    /// Google" makes no tool call, so the service cannot wait for one.
    pairing: mpsc::UnboundedSender<PairingRequest>,
    state: AsyncMutex<State>,
    /// When a seat last used this profile.
    touch: Mutex<Touch>,
    /// Whether this entry has been closed: **a caller that kept the handle
    /// from before the close is refused**, rather than starting a second
    /// host on a directory the map no longer names.
    ended: AtomicBool,
    /// Whether the host is up, for the strip's read: a lock-free flag so the
    /// list never waits on a call in flight.
    up: AtomicBool,
    /// A host this entry lost since the service last looked, so the service
    /// can forget the hand-offs that host held before its numbers come back.
    restarted: AtomicBool,
    /// **The identity of the host this profile is on.** Bumped on every
    /// successful launch and again when a host goes, so a tab birth or close
    /// carries the host that reported it; an event from a receiver that was
    /// replaced is refused even when the tab numbers collide.
    host_generation: AtomicU64,
    /// A fresh driver still could not list this profile's pages: the host it
    /// was listed from is wedged, and is ended on the first path that can
    /// take `file_decision` without waiting on the call that found it.
    host_suspect: Mutex<Option<HostIdentity>>,
    /// The driver a replacement installs under test, so a test can tell a
    /// replacement from a retry of the same fake. One-shot.
    #[cfg(test)]
    successor: Mutex<Option<Successor>>,
    /// **The same rows channel the service publishes on**, so a host's death
    /// can say so at once instead of waiting for a sweep.
    rows: watch::Sender<BrowserTabList>,
}

/// The most a finished download may carry to a viewer.
const MAX_DOWNLOAD_BYTES: u64 = 32 * 1024 * 1024;

/// How many finished downloads one profile may hold for a lagging viewer.
///
/// **The backlog is memory, and a download is bytes.** Every reader that is
/// behind keeps up to this many `BrowserDownload`s, each up to
/// [`MAX_DOWNLOAD_BYTES`], so the ceiling is this times 32 MiB per profile -
/// 128 MiB here, against 512 at the sixteen this used to keep. A viewer that
/// lags past it loses the oldest offers, which is already what a broadcast
/// does; what it must not do is hold half a gigabyte per open profile for a
/// page that saved sixteen files while nobody looked.
const DOWNLOAD_BACKLOG: usize = 4;

/// What the host last said about a recording: whether the writer opened, and -
/// once it stopped - how the take ended.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordingState {
    started: bool,
    seconds: u64,
    bytes: u64,
    capped: bool,
    error: Option<String>,
}

/// One finished download, read for the viewer or refused with its reason.
///
/// **The bytes come from the host's own directory or not at all**: the path
/// crossed a pipe from another process, and a path outside the profile's
/// uploads is not this server's to read.
fn read_download(dir: &Path, tab: u16, name: &str, path: &str, size: u64) -> BrowserDownload {
    let downloads = dir.join("downloads");
    let path = Path::new(path);
    if !path.starts_with(&downloads) {
        tracing::warn!(
            "a download named a path outside its profile and is refused (event_name \
             browser_download_outside, tab {tab}, name {name}, path {})",
            path.display()
        );
        return BrowserDownload {
            tab,
            name: name.to_owned(),
            bytes: Vec::new(),
            error: Some("the downloaded file could not be read on the server".to_owned()),
        };
    }
    if size > MAX_DOWNLOAD_BYTES {
        let _ = std::fs::remove_file(path);
        return BrowserDownload {
            tab,
            name: name.to_owned(),
            bytes: Vec::new(),
            error: Some("this file is larger than 32 MiB, so the viewer cannot save it".to_owned()),
        };
    }
    let bytes = std::fs::read(path);
    let _ = std::fs::remove_file(path);
    match bytes {
        Ok(bytes) => BrowserDownload { tab, name: name.to_owned(), bytes, error: None },
        Err(why) => {
            tracing::warn!(
                "a finished download could not be read (event_name browser_download_unreadable, \
                 tab {tab}, name {name}): {why}"
            );
            BrowserDownload {
                tab,
                name: name.to_owned(),
                bytes: Vec::new(),
                error: Some("the downloaded file could not be read on the server".to_owned()),
            }
        }
    }
}

/// One chosen file's name on disk: its basename made safe, **the name the
/// page must see**.
///
/// **Every file lands in a directory of its own** (`uploads/<place>/`), so
/// two files that share a name both keep it; decorating the basename with
/// its place instead made the page's `input.files` read `0-resume.txt`.
fn file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(
            |ch| if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') { ch } else { '_' },
        )
        .take(120)
        .collect();
    let cleaned = cleaned.trim_matches('.').to_owned();
    if cleaned.is_empty() { "file".to_owned() } else { cleaned }
}

/// Write one answer's files under `dir`, each in its own place directory, and
/// answer the paths in the order they were chosen.
fn write_uploads(dir: &Path, files: &[BrowserUpload]) -> Result<Vec<String>, String> {
    std::fs::create_dir_all(dir)
        .map_err(|why| format!("the upload directory cannot be made: {why}"))?;
    let mut paths = Vec::with_capacity(files.len());
    for (place, file) in files.iter().enumerate() {
        let sub = dir.join(place.to_string());
        std::fs::create_dir_all(&sub)
            .map_err(|why| format!("an upload directory cannot be made: {why}"))?;
        let path = sub.join(file_name(&file.name));
        std::fs::write(&path, &file.bytes)
            .map_err(|why| format!("a chosen file cannot be written: {why}"))?;
        paths.push(path.to_string_lossy().into_owned());
    }
    Ok(paths)
}

/// The paths one agent upload asks to hand over, checked before a browser is
/// opened for it: absolute, readable regular files, within the same caps the
/// person's answer takes. `None` is the tool's own "cancel the chooser".
fn agent_upload_paths(args: &serde_json::Value) -> Result<Option<Vec<PathBuf>>, String> {
    let Some(listed) = args.get("paths").and_then(serde_json::Value::as_array) else {
        return Ok(None);
    };
    if listed.len() > MAX_UPLOAD_FILES {
        return Err(format!(
            "an upload takes at most {MAX_UPLOAD_FILES} files, and this one names {}",
            listed.len()
        ));
    }
    let cap = MAX_UPLOAD_BYTES;
    let mut total = 0_u64;
    let mut paths = Vec::with_capacity(listed.len());
    for listed in listed {
        let Some(path) = listed.as_str().map(PathBuf::from) else {
            return Err(format!("an upload path is a string, not {listed}"));
        };
        if !path.is_absolute() {
            return Err(format!(
                "an upload path is absolute on the server, and {} is not",
                path.display()
            ));
        }
        let meta = std::fs::metadata(&path)
            .map_err(|why| format!("the file {} cannot be read: {why}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        total = total.saturating_add(meta.len());
        if total > cap {
            return Err(format!(
                "the files are larger than {} MiB, so the page cannot take them",
                cap / (1024 * 1024)
            ));
        }
        paths.push(path);
    }
    Ok(Some(paths))
}

/// One `browser_file_upload` call: the paths it named, or a cancel. **Every
/// such call answers the captured chooser**, so the lock, the refusal and the
/// ledger cover both forms.
enum UploadRequest {
    Answer(Vec<PathBuf>),
    Cancel,
}

/// Classify one `browser_file_upload` call. The tool's own schema makes
/// `paths` optional, and omitting it is its documented cancel.
fn upload_request(args: &serde_json::Value) -> Result<UploadRequest, String> {
    match agent_upload_paths(args)? {
        Some(paths) => Ok(UploadRequest::Answer(paths)),
        None => Ok(UploadRequest::Cancel),
    }
}

/// Stage an agent's files beside the profile and answer the paths the page
/// reads: **the same bound and the same place-per-file layout as the person's
/// answer**, so the page sees the file's own name and a file that changed
/// size after the check is still refused rather than handed over whole.
fn stage_agent_uploads(dir: &Path, paths: &[PathBuf]) -> Result<Vec<String>, String> {
    use std::io::Read as _;

    let cap = MAX_UPLOAD_BYTES;
    let mut total = 0_u64;
    let mut uploads = Vec::with_capacity(paths.len());
    for path in paths {
        let mut bytes = Vec::new();
        let file = std::fs::File::open(path)
            .map_err(|why| format!("the file {} cannot be read: {why}", path.display()))?;
        // **One byte past what the answer may carry**, because the check
        // above raced whoever wrote the file.
        std::io::Read::take(file, cap.saturating_add(1) - total.min(cap))
            .read_to_end(&mut bytes)
            .map_err(|why| format!("the file {} cannot be read: {why}", path.display()))?;
        total = total.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if total > cap {
            return Err(format!(
                "the files are larger than {} MiB, so the page cannot take them",
                cap / (1024 * 1024)
            ));
        }
        let name = path
            .file_name()
            .map_or_else(|| "file".to_owned(), |name| name.to_string_lossy().into_owned());
        uploads.push(BrowserUpload { name, bytes });
    }
    let _ = std::fs::remove_dir_all(dir);
    write_uploads(dir, &uploads)
}

/// Wait for the host's recording answer, skipping one that does not match
/// `started` (a start's confirmation still sitting in the watch).
async fn recording_answer(
    recorded: &mut watch::Receiver<Option<RecordingState>>,
    started: Option<bool>,
    bound: Duration,
) -> Option<RecordingState> {
    let wait = async {
        loop {
            if let Some(state) = recorded.borrow().clone()
                && started.is_none_or(|want| state.started == want)
            {
                return Some(state);
            }
            if recorded.changed().await.is_err() {
                return None;
            }
        }
    };
    tokio::time::timeout(bound, wait).await.ok().flatten()
}

/// Put the controller's last requested view back after a take.
/// Put one tab back on the profile's surface after a take ends.
fn restore_tab_surface(profile: &Profile, input: &spawn::InputSender, tab: u16) {
    let (width, height, scale) = *profile.surface.lock().unwrap_or_else(PoisonError::into_inner);
    let line = input::host_line(&crate::input::HostInput::TabResize {
        tab,
        width,
        height,
        scale_milli: crate::osr::scale_milli(scale),
    });
    let _ = input.send(line);
}

/// The bytes free under `path`, when the kernel reports them.
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `path` is a NUL-terminated path and `stat` is written on
    // success.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded, so it initialized the struct.
    let stat = unsafe { stat.assume_init() };
    Some(u64::from(stat.f_bavail).saturating_mul(stat.f_frsize))
}

/// The newest `keep - 1` takes, so the one about to start makes `keep`.
fn prune_recordings(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut takes: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "mp4") {
                return None;
            }
            let at = entry.metadata().ok()?.modified().ok()?;
            Some((at, path))
        })
        .collect();
    takes.sort_by_key(|take| std::cmp::Reverse(take.0));
    for (_, path) in takes.into_iter().skip(keep.saturating_sub(1)) {
        let _ = std::fs::remove_file(path);
    }
}

/// Why a host the service is ending is going, and the level that states it
/// honestly. **An end forge caused by a fault is a warning; an idle release
/// or a profile close is not** (rule 20).
#[derive(Debug, Clone, Copy)]
enum HostEnd {
    /// No driver could drive it: a fresh one could not list or attach.
    Undrivable,
    /// It never became usable: its endpoint or its ring failed.
    NeverReady,
    /// It stopped answering.
    Unresponsive,
    /// The idle clock released it.
    Idle,
    /// Its profile was closed.
    Closed,
    /// Forge stopped.
    Stopped,
}

impl HostEnd {
    /// The level the record takes.
    fn level(self) -> tracing::Level {
        match self {
            Self::Undrivable | Self::NeverReady | Self::Unresponsive => tracing::Level::WARN,
            Self::Idle | Self::Closed | Self::Stopped => tracing::Level::INFO,
        }
    }

    /// What the record says the host was going for.
    fn cause(self, why: &str) -> String {
        match self {
            Self::Undrivable => format!("no driver could drive it: {why}"),
            Self::NeverReady => format!("it never became usable: {why}"),
            Self::Unresponsive => format!("it stopped answering: {why}"),
            Self::Idle => "the idle clock released it".to_owned(),
            Self::Closed => "its profile was closed".to_owned(),
            Self::Stopped => "forge stopped".to_owned(),
        }
    }
}

/// The host's own last words, read off the async worker: the log is
/// append-only and an end holds a profile lock. `teardown` calls this with
/// the state lock held and `start_host` with the launch lock; the read is
/// bounded and does not block the worker.
async fn host_tail(profile: &Profile) -> String {
    let dir = profile.dir.clone();
    tokio::task::spawn_blocking(move || spawn::log_tail(&spawn::host_log_path(&dir)))
        .await
        .unwrap_or_default()
}

/// **A host the service deliberately ends is said in the log**, with the
/// identity, the cause and the host's own last words. A host that had
/// already gone is not recorded here.
fn log_host_ended(
    profile: &Profile,
    pid: u32,
    port: u16,
    end: HostEnd,
    outcome: &str,
    why: &str,
    tail: &str,
) {
    let cause = end.cause(why);
    macro_rules! record {
        ($level:expr) => {
            tracing::event!(
                $level,
                event_name = "browser_host_ended",
                profile = %profile.key,
                pid,
                port,
                cause = %cause,
                outcome = %outcome,
                log_tail = %tail,
                "the service ended a browser host",
            )
        };
    }
    if end.level() == tracing::Level::WARN {
        record!(tracing::Level::WARN);
    } else {
        record!(tracing::Level::INFO);
    }
}

/// The launch error a session reads: the reason, with the host's own last
/// words when it left any.
fn launch_error(why: String, tail: &str) -> String {
    if tail.is_empty() { why } else { format!("{why}: {tail}") }
}

/// How a host's death reads: a signal, an exit code, or a status this
/// process could not read. **The client's sentence carries the same words**,
/// so a crash is diagnosable from the surface and the log alike.
fn death_cause(status: Option<std::process::ExitStatus>) -> String {
    let Some(status) = status else {
        return "exit status not ours to read".to_owned();
    };
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }
    "an unreadable status".to_owned()
}

impl Profile {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn id(&self) -> u16 {
        self.id
    }

    /// One tab's controller, when a viewer holds its role.
    fn controller(&self, tab: u16) -> Option<u64> {
        self.controls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tab)
            .map(|(holder, _)| *holder)
    }

    /// The controller a chooser is put to: the focused tab's when one holds
    /// it, else the first tab whose role someone took.
    fn chooser_controller(&self) -> Option<u64> {
        let focused = *self.focused.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(controller) = focused.and_then(|tab| self.controller(tab)) {
            return Some(controller);
        }
        let controls = self.controls.lock().unwrap_or_else(PoisonError::into_inner);
        controls.iter().min_by_key(|(tab, _)| **tab).map(|(_, (holder, _))| *holder)
    }

    fn view_for(&self, tab: u16) -> watch::Sender<BrowserViewState> {
        self.views
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(tab)
            .or_insert_with(|| watch::channel(BrowserViewState::default()).0)
            .clone()
    }

    /// Forget everything one tab owned, **without touching the host or the
    /// registry**: a closed tab's role, watchers and view stream go with it.
    fn forget_tab(&self, tab: u16) {
        self.controls.lock().unwrap_or_else(PoisonError::into_inner).remove(&tab);
        self.viewers.lock().unwrap_or_else(PoisonError::into_inner).remove(&tab);
        self.views.lock().unwrap_or_else(PoisonError::into_inner).remove(&tab);
        self.nav.lock().unwrap_or_else(PoisonError::into_inner).remove(&tab);
        self.frames.lock().unwrap_or_else(PoisonError::into_inner).remove(&tab);
    }

    /// **A chooser outliving the person who could answer it is cancelled.**
    /// The ask leaves the watch and the chooser is dismissed - the host's CEF
    /// dialog when no driver is attached, the driver's own captured chooser
    /// when it is - so a client that went away, or a role that moved, cannot
    /// leave the browser holding a chooser nobody is looking at.
    ///
    /// **The caller holds `file_decision`**: the role change, the answer and
    /// the cancel are one chooser decision.
    async fn cancel_files(&self) {
        let playwright = self.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(id) = playwright {
            if self.files.borrow().as_ref().map(|ask| ask.id) == Some(id) {
                self.files.send_replace(None);
            }
            // **A drain reconciles what it actually cancelled**, so a ledger
            // surface cannot survive the chooser and feed a later upload a
            // stale credit; the open ask itself is at least one.
            let cancelled = self.cancel_driver_chooser(true).await;
            note_surfaced_consumed_by(self, cancelled.max(1));
            return;
        }
        let ask = self.files.send_replace(None);
        let Some(ask) = ask else { return };
        let state = self.state.lock().await;
        if let Some(host) = state.host.as_ref()
            && let Err(why) = host.input.send(input::host_line(&crate::input::HostInput::File {
                id: ask.id,
                paths: Vec::new(),
            }))
        {
            tracing::debug!(
                "a file chooser could not be cancelled (event_name browser_file_cancel_failed, \
                 profile {}, id {}): {why}",
                self.key,
                ask.id
            );
        }
    }

    /// Answer the driver's captured chooser with no paths: the MCP's cancel.
    ///
    /// **The MCP answers its choosers one at a time**, so a detach drains
    /// them all - a client that crashed with two choosers open used to leave
    /// one pending, and the page then raised no further chooser. A supersede
    /// cancels only the one the ask belonged to, because the chooser that
    /// just surfaced is the one being asked about.
    async fn cancel_driver_chooser(&self, drain: bool) -> usize {
        let calls = { self.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).clone() };
        let Some(calls) = calls else { return 0 };
        let rounds = if drain { 8 } else { 1 };
        let mut cancelled = 0;
        for _ in 0..rounds {
            match calls.call(serde_json::json!({}), Duration::from_secs(5)).await {
                Ok(_) => cancelled += 1,
                Err(why) => {
                    // **No chooser left is the drain's success**, whatever
                    // case or punctuation the tool's own sentence carries.
                    if !why.to_lowercase().contains("no file chooser visible") {
                        tracing::debug!(
                            "the driver's file chooser could not be cancelled (event_name \
                             browser_file_cancel_failed, profile {}): {why}",
                            self.key
                        );
                    }
                    break;
                }
            }
        }
        cancelled
    }

    /// **A dead driver's chooser goes with it.** The caller holds
    /// `file_decision` and the state lock, so the clear cannot wipe a newer
    /// driver's handle installed in between: the next call starts a fresh
    /// driver, and no ask may point at the old one.
    fn clear_chooser_state(&self) {
        self.files.send_replace(None);
        *self.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger::default();
        *self.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// **A lagged reader still counts its skipped choosers**: they were
    /// surfaced by the driver, so an agent's answer consumes one instead of
    /// leaving a stale credit that eats the next real ask.
    async fn chooser_lagged(self: &Arc<Self>, skipped: u64) {
        let _decision = self.file_decision.lock().await;
        if !self.ended.load(Ordering::Acquire) && self.up.load(Ordering::Acquire) {
            note_surfaced_opened(self, usize::try_from(skipped).unwrap_or(usize::MAX));
        }
    }

    /// One chooser the driver surfaced.
    ///
    /// **The controlling person is asked when one holds the role**; a
    /// watcher is never asked and never cancels it. With no controller the
    /// chooser waits for an agent's own `browser_file_upload`, which is the
    /// same MCP tool that owns the captured chooser.
    async fn file_chooser(
        self: &Arc<Self>,
        chooser: driver::FileChooser,
        generation: u64,
        tab: u16,
    ) {
        // **The whole decision is one critical section**: the previous ask's
        // clear and cancel, the role read, and the new ask's publication.
        let _decision = self.file_decision.lock().await;
        if self.ended.load(Ordering::Acquire) || !self.up.load(Ordering::Acquire) {
            return;
        }
        // **A chooser from a replaced driver is not published.** Its ask's id
        // belongs to a driver that no longer exists, and answering it would
        // cancel the next real chooser as that stale id.
        if self.state.lock().await.driver_generation != generation {
            tracing::debug!(
                "a chooser from a replaced driver is dropped (event_name \
                 browser_file_chooser_stale, profile {})",
                self.key
            );
            return;
        }
        if !chooser.current {
            // **`current` is the driver's snapshot at event time.** The MCP's
            // upload answers whichever tab is current when the answer runs,
            // so a tab switch between the event and the answer is the one
            // window this check cannot close; the tool itself is the
            // authority on what it answers, and a person is never sent to a
            // tab they were not looking at when the chooser opened.
            //
            // **An owed event is still owed**: the agent may have answered
            // this chooser before the tab lost currentness, so the credit is
            // consumed here rather than left to eat the next real chooser.
            {
                let mut ledger = self.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
                if ledger.pre_answered > 0 {
                    ledger.pre_answered -= 1;
                }
            }
            tracing::debug!(
                "a chooser on a background tab is not asked (event_name \
                 browser_file_chooser_background, profile {}, url {})",
                self.key,
                chooser.url
            );
            return;
        }
        // **An event the agent already answered is not a person's to see.**
        {
            let mut ledger = self.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            if ledger.pre_answered > 0 {
                ledger.pre_answered -= 1;
                tracing::debug!(
                    "a chooser the agent answered had its event read (event_name \
                     browser_file_chooser_agent, profile {})",
                    self.key
                );
                return;
            }
            ledger.surfaced += 1;
        }
        let previous = self.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(previous) = previous {
            // **A newer chooser supersedes the older one**: the MCP answers
            // its choosers in order, so the one the panel was opened for is
            // cancelled before the new ask takes over.
            if self.files.borrow().as_ref().map(|ask| ask.id) == Some(previous) {
                self.files.send_replace(None);
            }
            let cancelled = self.cancel_driver_chooser(false).await;
            note_surfaced_consumed_by(self, cancelled.max(1));
            // **The cancel can park while the profile goes down**: teardown
            // sets `up` false before it clears, so a chooser that waited
            // here is not published for a host that is gone.
            if self.ended.load(Ordering::Acquire) || !self.up.load(Ordering::Acquire) {
                return;
            }
        }
        let Some(controller) = self.chooser_controller() else {
            tracing::debug!(
                "a page's file chooser is held for the agent (event_name \
                 browser_file_chooser_held, profile {})",
                self.key
            );
            return;
        };
        let id = self.next_file_ask.fetch_add(1, Ordering::Relaxed);
        *self.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(id);
        // **The chooser belongs to the tab the driver was on when it
        // surfaced**, captured by the reader rather than read here: a queued
        // event can otherwise name whichever tab a later call focused.
        self.files.send_replace(Some(BrowserFileAsk { id, tab, multiple: chooser.multiple }));
        tracing::info!(
            "a page's file chooser went to the controller (event_name browser_file_chooser, \
             profile {}, tab {tab}, id {id}, viewer {controller}, url {})",
            self.key,
            chooser.url,
        );
    }

    /// The sequence the next relayed frame for `tab` carries.
    ///
    /// **The host's own count restarts at zero with every host**, and a
    /// viewer drops a frame whose sequence is not newer than the one it
    /// painted; across a host death that rule would reject every new frame
    /// and leave the dead host's picture up.
    fn next_frame_seq(&self, tab: u16) -> u32 {
        let mut seqs = self.frame_seqs.lock().unwrap_or_else(PoisonError::into_inner);
        let next = seqs.entry(tab).or_insert(0);
        *next = next.saturating_add(1);
        *next
    }

    /// One tab's frame watch, made on demand.
    fn frame_sender(&self, tab: u16) -> watch::Sender<Option<BrowserFrame>> {
        self.frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(tab)
            .or_insert_with(|| watch::channel(None).0)
            .clone()
    }

    /// The frame watch a viewer on `tab` follows.
    fn frame_receiver(&self, tab: u16) -> watch::Receiver<Option<BrowserFrame>> {
        self.frame_sender(tab).subscribe()
    }

    /// One tab's history watch, made on demand.
    fn nav_sender(&self, tab: u16) -> watch::Sender<Option<BrowserNavState>> {
        self.nav
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(tab)
            .or_insert_with(|| watch::channel(None).0)
            .clone()
    }

    /// The watch a viewer on `tab` follows.
    fn nav_receiver(&self, tab: u16) -> watch::Receiver<Option<BrowserNavState>> {
        self.nav_sender(tab).subscribe()
    }

    /// Publish where a tab's history sits, when it is news.
    fn publish_nav(&self, tab: u16, nav: BrowserNavState) {
        self.nav_sender(tab).send_if_modified(|current| {
            if *current == Some(nav) {
                false
            } else {
                *current = Some(nav);
                true
            }
        });
    }

    /// Let the focus go when it names `tab`: the history watch follows only a
    /// page that is there. **The generation is part of the name**: a host
    /// that was replaced restarts tab numbers at one, so a late close from
    /// the old host must not unfocus its successor's tab one.
    fn unfocus(&self, generation: u64, tab: u16) {
        if self.host_generation.load(Ordering::Acquire) != generation {
            return;
        }
        let mut focused = self.focused.lock().unwrap_or_else(PoisonError::into_inner);
        if *focused == Some(tab) {
            *focused = None;
        }
    }

    /// Republish one tab's viewer state: its controller and how many are
    /// attached to it.
    fn publish_view(&self, tab: u16) {
        let controller = self.controller(tab);
        let viewers = u32::try_from(
            self.viewers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&tab)
                .map_or(0, HashSet::len),
        )
        .unwrap_or(u32::MAX);
        self.view_for(tab).send_replace(BrowserViewState { controller, viewers });
    }

    /// Bring the profile's host and driver up, or keep the ones already up.
    async fn ensure(self: &Arc<Self>, root: &Path, stack: &Path) -> Result<(), String> {
        let mut state = self.state.lock().await;
        self.ensure_locked(&mut state, root, stack).await
    }

    async fn ensure_locked(
        self: &Arc<Self>,
        state: &mut State,
        root: &Path,
        stack: &Path,
    ) -> Result<(), String> {
        // **A closed entry is never brought back under a caller that kept
        // it**: the next call goes through the map and gets a fresh profile.
        if self.ended.load(Ordering::Acquire) {
            return Err(format!("the browser profile '{}' was closed; open it again", self.key));
        }
        // **A stop wins over any caller that kept a handle**: no host starts
        // behind the drain.
        if self.stopped.load(Ordering::Acquire) {
            return Err("the server is shutting down".to_owned());
        }
        // A test profile's driver is the whole browser: a fake host never
        // exists, and a real one is never started for it.
        #[cfg(test)]
        if state.host.is_none() && state.driver.is_some() {
            return Ok(());
        }
        // A host that died is replaced; a live one keeps its ring.
        if let Some(host) = state.host.as_ref()
            && spawn::gone(host.pid)
        {
            self.teardown(state, None, "").await;
        }
        if state.host.is_none() {
            let host = self.start_host().await?;
            state.host = Some(host);
            self.up.store(true, Ordering::Release);
        }
        if let Some(driver) = state.driver.as_ref()
            && driver.is_running()
        {
            self.watch_nav(state);
            return Ok(());
        }
        state.driver = None;
        // **A replaced driver's chooser goes with it.** Its ask's id belongs
        // to a driver that no longer exists, and the next real chooser would
        // be cancelled as that stale id. The clear is synchronous, before
        // the new driver's reader starts, so it cannot wipe a fresh ask.
        self.clear_chooser_state();
        let Some(host) = state.host.as_ref() else {
            return Err(format!("the browser for '{}' is not up", self.key));
        };
        let node = driver::node_path(stack);
        let cli = driver::cli_path(stack);
        let endpoint = engine::endpoint(host.port);
        let chooser_socket = chooser_socket_path(&self.dir, self.id)?;
        let driver = Driver::start(&node, &cli, &endpoint, &root.join("output"), &chooser_socket)
            .await
            .map_err(|why| {
                tracing::warn!(
                    "the browser driver did not start (event_name browser_driver_start_failed, \
                     profile {}, port {}): {why}",
                    self.key,
                    host.port,
                );
                why
            })?;
        let driver = Arc::new(driver);
        // **A chooser answer rides this driver's MCP call**, and the race
        // tests replace this handle with a gated one.
        let calls: Arc<dyn ChooserAnswer> = driver.clone();
        *self.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        // **Every chooser the driver surfaces is the profile's to answer.**
        // The task ends when the driver is replaced: its sender is dropped
        // with the Arc, and `recv` then closes.
        let mut choosers = driver.file_choosers();
        let this = Arc::clone(self);
        let generation = installed_generation(state);
        state.driver_generation = generation;
        tokio::spawn(async move {
            loop {
                match choosers.recv().await {
                    Ok(chooser) => {
                        // **The tab and the driver are captured when the
                        // chooser arrives**: by the time a queued event runs,
                        // `focused` may belong to another tab and this reader
                        // may belong to a replaced driver.
                        let tab = this
                            .focused
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .unwrap_or(0);
                        this.file_chooser(chooser, generation, tab).await;
                    }
                    // **A lagged reader keeps reading**: the newest chooser
                    // still matters, and a dropped event is not a closed
                    // channel.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            "a browser profile's chooser reader fell behind (event_name \
                             browser_chooser_lagged, profile {}, skipped {skipped})",
                            this.key,
                        );
                        this.chooser_lagged(skipped).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        state.driver = Some(driver);
        self.watch_nav(state);
        Ok(())
    }

    /// Start the page's navigation watch for this host, once.
    fn watch_nav(self: &Arc<Self>, state: &mut State) {
        if state.nav_watch.as_ref().is_some_and(|watch| !watch.is_finished()) {
            return;
        }
        if let Some(port) = state.host.as_ref().map(|host| host.port) {
            state.nav_watch = Some(start_nav_watch(Arc::clone(self), port));
        }
    }

    /// Watch the host's pid for its life: a death is said once, and the
    /// profile's state is cleared only while this host is still the one
    /// installed.
    ///
    /// **A monitor left alive for a host that had already gone can tick after
    /// its successor is installed** (`ensure_locked` starts one without
    /// waiting on the monitor). Clearing a successor's tabs, roles, tab
    /// events and staged uploads would be the old host's death visited on the
    /// new one, so the state under the lock is what says whose death this is.
    fn watch_host(self: &Arc<Self>, pid: u32, port: u16) -> tokio::task::JoinHandle<()> {
        let failures = self.failures.clone();
        let files = self.files.clone();
        let dir = self.dir.clone();
        let key = self.key.clone();
        let profile = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let Some(status) = spawn::exit(pid) else { continue };
                // **The cause is the point of the line**: a signal and an
                // exit code mean different things to whoever reads it, and
                // the surface's sentence carries the same words.
                let cause = death_cause(status);
                // **The host's own last words are the diagnosis here too.**
                // The read is bounded, and the monitor holds no profile lock.
                let tail = profile.upgrade().map_or_else(String::new, |profile| {
                    spawn::log_tail(&spawn::host_log_path(&profile.dir))
                });
                tracing::warn!(
                    event_name = "browser_host_died",
                    profile = %key,
                    pid,
                    port,
                    cause = %cause,
                    log_tail = %tail,
                    "a browser host exited under its profile"
                );
                // **A dead host's chooser and its staged files go with it**:
                // the dialog was its own, and nothing is left waiting on a
                // callback that cannot run. The ask is cleared under the
                // same lock an answer or a role change takes, and the
                // profile is marked down so a queued chooser event cannot
                // publish against the dead host.
                if let Some(profile) = profile.upgrade() {
                    let _decision = profile.file_decision.lock().await;
                    // **A successor is not this host's to clear.** The state
                    // lock is held across the cleanup, so a successor cannot
                    // install between the check and the clear.
                    let state = profile.state.lock().await;
                    if !state.host.as_ref().is_some_and(|host| host.pid == pid && host.port == port)
                    {
                        return;
                    }
                    *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = None;
                    *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
                        ChooserLedger::default();
                    profile.up.store(false, Ordering::Release);
                    // **A dead host's chooser and its staged files go with
                    // it** before anything is said about the death.
                    files.send_replace(None);
                    let _ = std::fs::remove_dir_all(dir.join("uploads"));
                    // **The cause is published before the tab senders
                    // close.** A viewer woken by a dead tab otherwise reads
                    // "tab N was closed" first and never hears why.
                    failures.send_replace(Some(format!(
                        "the browser host exited ({cause}; pid {pid}); the next call brings it back"
                    )));
                    // **A dead host's numbers die with it**, before a call
                    // arrives to start its successor: a strip must not point
                    // at tabs nothing runs, and a parked hand-off on one of
                    // them must not close whichever tab takes the number.
                    profile.host_gone();
                    *profile.host_tab_events.lock().await = None;
                }
                return;
            }
        })
    }

    /// Spawn the host, wait for its endpoint, adopt its ring and install the
    /// passkey veto.
    async fn start_host(self: &Arc<Self>) -> Result<ActiveHost, String> {
        let _launch = self.launch.lock().await;
        // **The frame socket is a short path outside the profile**: a key
        // under app-support exceeds a Unix socket's address, and an org
        // profile is two segments deep.
        let socket = channel::frame_socket_path(&self.dir, self.id)?;
        let profile = Arc::downgrade(self);
        let audio = self.audio.clone();
        let reader = channel::serve(&socket, move |message| match message {
            channel::ChannelMessage::Frame(frame) => {
                // **The newest frame wins per tab, and it is KEPT even with
                // no viewer attached.** `send` drops a value nobody is
                // watching, and the first paint happens before the first
                // viewer attaches - the page would then sit blank until
                // something repainted it. One profile-wide slot let another
                // tab's paint replace the viewed tab's.
                let encoded = frame.encode();
                if let Some(profile) = profile.upgrade() {
                    let seq = profile.next_frame_seq(frame.tab);
                    profile.frame_sender(frame.tab).send_replace(Some(BrowserFrame {
                        profile: frame.profile,
                        tab: frame.tab,
                        seq,
                        encoded,
                    }));
                }
            }
            // **Audio is a stream, not a newest-wins value**: a viewer that
            // falls behind drops a packet and hears a glitch rather than
            // carrying everything from the start of playback.
            channel::ChannelMessage::Audio(packet) => {
                let _ = audio.send(BrowserAudio {
                    profile: packet.profile,
                    tab: packet.tab,
                    seq: packet.seq,
                    sample_rate: packet.sample_rate,
                    channels: packet.channels,
                    samples: packet.samples,
                });
            }
        })?;
        let port = spawn::free_port().ok_or_else(|| {
            tracing::warn!(
                "no browser port was free (event_name browser_port_exhausted, profile {})",
                self.key
            );
            "no free browser port".to_owned()
        })?;
        let ring_name = spawn::ring_name(port);
        let (pid, input, mut events) = match spawn::spawn(
            &self.dir,
            port,
            &ring_name,
            VIEW_WIDTH,
            VIEW_HEIGHT,
            self.id,
            self.encoding,
            &socket,
        ) {
            Ok(spawned) => spawned,
            Err(why) => {
                tracing::warn!(
                    "a browser host did not spawn (event_name browser_host_spawn_failed, \
                     profile {}, port {port}): {why}",
                    self.key,
                );
                reader.stop();
                return Err(why);
            }
        };
        if let Err(why) = spawn::wait_ready_for(pid, port, &ring_name).await {
            let tail = host_tail(self).await;
            let ended = spawn::end_verified(pid, port, &self.dir).await;
            reader.stop();
            let outcome = if ended { "ended" } else { "refused" };
            log_host_ended(self, pid, port, HostEnd::NeverReady, outcome, &why, &tail);
            return Err(launch_error(why, &tail));
        }
        let ring = match osr::Ring::open(&ring_name) {
            Ok(ring) => Arc::new(ring),
            Err(why) => {
                let tail = host_tail(self).await;
                let ended = spawn::end_verified(pid, port, &self.dir).await;
                reader.stop();
                let outcome = if ended { "ended" } else { "refused" };
                log_host_ended(self, pid, port, HostEnd::NeverReady, outcome, &why, &tail);
                return Err(launch_error(why, &tail));
            }
        };
        // **The record is written once the host is READY**: it is what a
        // later boot reaps an unclean exit by, so it must never name a pid
        // that had not come up yet.
        if let Err(why) =
            spawn::write_record(&self.dir, &spawn::Record { pid, port, ring: ring_name.clone() })
        {
            tracing::warn!(
                "the host record could not be written for {} (event_name \
                 browser_host_record_failed): {why}",
                self.key
            );
        }
        // **A page is never armed by a driver alone.** The veto is installed
        // before the profile serves any call, so every document a tab starts
        // opens behind it.
        if let Err(why) = veto::install(port, &self.dir).await {
            tracing::warn!(
                "the passkey veto did not install for {} (event_name browser_veto_failed): {why}",
                self.key
            );
        }
        // **The host opens one blank boot tab**, and the first page call
        // adopts it. Its number is the host's first, and its page target is
        // the only one listed before the driver starts.
        let (tab_sender, tab_receiver) = mpsc::unbounded_channel();
        *self.host_tab_events.lock().await = Some(tab_receiver);
        let boot_deadline = tokio::time::Instant::now() + BOOT_EVENT_BOUND;
        let mut pending: Vec<HostEvent> = Vec::new();
        let mut booted = false;
        loop {
            match tokio::time::timeout_at(boot_deadline, events.recv()).await {
                Ok(Some(HostEvent::TabCreated { tab: 1, opener: None, .. })) => {
                    booted = true;
                    break;
                }
                Ok(Some(event)) => pending.push(event),
                _ => break,
            }
        }
        if booted {
            // **The target is the driver's to name.** A target read from the
            // endpoint before the driver attaches is not necessarily the page
            // the driver lists, so the adoption takes its row at the first
            // page call instead.
            *self.boot.lock().unwrap_or_else(PoisonError::into_inner) = Some((1, String::new()));
        } else {
            tracing::warn!(
                "a browser host reported no boot tab to adopt (event_name \
                 browser_boot_tab_missing, profile {}, port {port})",
                self.key
            );
        }
        tracing::info!(
            "the browser host is up (event_name browser_host_up, profile {}, pid {pid}, port {port})",
            self.key
        );
        // **This launch is the profile's host identity.** Every tab event the
        // host reports carries this value, so a birth or a close from a
        // receiver that has since been replaced cannot touch the successor's
        // tabs even when the numbers collide.
        let generation = self.host_generation.fetch_add(1, Ordering::AcqRel) + 1;
        // **A successor starts with no failure**: the watch describes the
        // host that is up, not the one before it. Left standing, a dead
        // host's sentence would be read as the cause of the successor's own
        // tab close.
        self.failures.send_replace(None);
        // **A host that dies under a viewer is said, not left as a frozen
        // frame.** A deliberate close aborts this first, so it never reads
        // as a crash.
        let monitor = self.watch_host(pid, port);
        // **A page's file chooser goes where a person is.** The host writes
        // the request on its stdout; the transport offers it to the viewer
        // that owns the role, and `answer_files` hands the host the paths.
        //
        // **The host's tab births and ends are the call path's**, queued for
        // whichever creation or call is pairing them with page targets; a
        // close is applied to the registry here, because a page can close
        // its own tab with no call in flight.
        let profile = Arc::downgrade(self);
        let downloads = self.downloads.clone();
        let recorded = self.recorded.clone();
        let dir = self.dir.clone();
        let key = self.key.clone();
        // **A popup's birth pairs itself.** Nothing a person clicks is a call
        // this service can wait for, so the birth asks for a pass here.
        let pairing = self.pairing.clone();
        tokio::spawn(async move {
            let mut pending: std::collections::VecDeque<HostEvent> = pending.into();
            loop {
                let event = match pending.pop_front() {
                    Some(event) => event,
                    None => match events.recv().await {
                        Some(event) => event,
                        None => break,
                    },
                };
                match event {
                    HostEvent::TabCreated { tab, opener, url, request } => {
                        tracing::debug!(
                            "a host tab opened (event_name browser_tab_created, profile {key}, \
                             tab {tab}, opener {opener:?}, request {request:?}, url {url})"
                        );
                        let _ = tab_sender.send((
                            generation,
                            HostEvent::TabCreated { tab, opener, url: url.clone(), request },
                        ));
                        // **A popup is nobody's call**: its row has to appear
                        // on its own, or the person who opened it sees the
                        // screen unchanged.
                        if let Some(opener) = opener {
                            let _ = pairing.send(PairingRequest {
                                key: key.clone(),
                                tab,
                                opener,
                                url: url.clone(),
                                until: tokio::time::Instant::now() + POPUP_PAIR_UNTIL,
                            });
                        }
                    }
                    HostEvent::TabClosed { tab } => {
                        tracing::debug!(
                            "a host tab closed (event_name browser_tab_closed, profile {key}, \
                             tab {tab})"
                        );
                        if let Some(profile) = profile.upgrade() {
                            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).close(tab);
                            profile.forget_tab(tab);
                            // **The strip hears it now**, not at the next
                            // call: a popup that closes itself, or a tab the
                            // host gave up, has no call behind it, and a row
                            // left drawn points at a tab nothing runs.
                            profile.rows.send_modify(|list| {
                                list.rows.retain(|row| row.number != tab);
                            });
                        }
                        let _ = tab_sender.send((generation, HostEvent::TabClosed { tab }));
                    }
                    HostEvent::FileDialog { id, tab, multiple } => {
                        // **The CEF fallback is a chooser decision too**: it
                        // takes the lock and checks the profile is up, so a
                        // dialog that arrives while the host goes is not
                        // published to anyone.
                        let Some(profile) = profile.upgrade() else { continue };
                        let _decision = profile.file_decision.lock().await;
                        if profile.ended.load(Ordering::Acquire)
                            || !profile.up.load(Ordering::Acquire)
                        {
                            continue;
                        }
                        tracing::info!(
                            "a page asked for a file (event_name browser_file_dialog, \
                             profile {key}, tab {tab}, id {id}, multiple {multiple})"
                        );
                        profile.files.send_replace(Some(BrowserFileAsk { id, tab, multiple }));
                    }
                    HostEvent::Download { tab, name, path, size } => {
                        let download = read_download(&dir, tab, &name, &path, size);
                        if downloads.send(download).is_err() {
                            tracing::debug!(
                                "a download had no viewer to take it (event_name \
                                 browser_download_unclaimed, profile {key}, name {name}, \
                                 size {size})"
                            );
                        }
                    }
                    HostEvent::Recording { started, seconds, bytes, capped, error } => {
                        tracing::debug!(
                            "a recording answered (event_name browser_recording, profile {key}, \
                             started {started}, seconds {seconds}, bytes {bytes}, capped \
                             {capped})"
                        );
                        recorded.send_replace(Some(RecordingState {
                            started,
                            seconds,
                            bytes,
                            capped,
                            error,
                        }));
                    }
                }
            }
        });
        Ok(ActiveHost {
            pid,
            port,
            ring,
            profile: self.dir.clone(),
            input,
            reader: Some(reader),
            monitor: Some(monitor),
        })
    }

    /// Take everything a host owned when it goes: its tab numbers, the boot
    /// tab and every viewer's place on them. **A new host starts again at tab
    /// 1**, so nothing may point at the old numbering - and a park a viewer
    /// left on one of those numbers must not survive to close its successor,
    /// which is why the service is told to forget this host's holds.
    fn host_gone(&self) {
        *self.host_suspect.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.tabs.lock().unwrap_or_else(PoisonError::into_inner) = TabRegistry::default();
        *self.boot.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.focused.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.controls.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.viewers.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.views.lock().unwrap_or_else(PoisonError::into_inner).clear();
        // **A stale popup event must not pair against the successor.**
        self.deferred_tab_events.lock().unwrap_or_else(PoisonError::into_inner).clear();
        // **A birth or reconcile already in flight must not write its events
        // back** over the empty list this death just made.
        self.host_generation.fetch_add(1, Ordering::AcqRel);
        // **The dead host's frames and history go with its numbers.** A kept
        // frame would paint the old page onto the successor's tab, and a kept
        // history would draw arrows for a page that no longer exists.
        self.frames.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.nav.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.restarted.store(true, Ordering::Release);
        // **The strip hears the death now**, not at the next sweep: a
        // subscribed strip with no takeover otherwise keeps dead tabs and a
        // browser that reads as up.
        self.rows.send_replace(BrowserTabList {
            profile: self.key.clone(),
            id: self.id,
            up: false,
            connected: false,
            closing_at_ms: None,
            rows: Vec::new(),
        });
    }

    /// End whatever is up: the driver, then the host, then the frames.
    ///
    /// `end` is why the host is going, and `why` is the fault behind it for
    /// the paths that have one. `None` for a host already known to be gone.
    async fn teardown(&self, state: &mut State, end: Option<HostEnd>, why: &str) {
        // **The profile is down before its chooser is cleared.** A chooser
        // event parked inside the driver's cancel resumes after this and must
        // see the profile down, or it would publish an ask for a dead host.
        self.up.store(false, Ordering::Release);
        self.host_gone();
        *self.host_tab_events.lock().await = None;
        // **A driver or host going takes its chooser with it.** These are
        // cleared without the decision lock because teardown runs under the
        // state lock and the order is decision then state; a queued chooser
        // event is refused by `up`/`ended` before it can publish.
        self.files.send_replace(None);
        *self.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger::default();
        *self.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = None;
        state.driver = None;
        state.cdp.clear();
        state.cdp_events.clear();
        if let Some(watch) = state.nav_watch.take() {
            watch.abort();
        }
        if let Some(host) = state.host.take() {
            let reader = host.reader;
            // **A host that had already gone is not an end**, and its status
            // is the monitor's to read: `gone` leaves it in place. The two
            // windows `end_dead_host` names stay here too.
            let was_up = !spawn::gone(host.pid);
            if !was_up {
                if let Some(reader) = reader {
                    reader.stop();
                }
                // **A crashed host's staged files go with it**: the monitor
                // cannot clear them once the state no longer names the host.
                let _ = std::fs::remove_dir_all(self.dir.join("uploads"));
                self.clear_host_trail(host.port);
                return;
            }
            if let Some(monitor) = host.monitor {
                monitor.abort();
            }
            let ended = spawn::end_verified(host.pid, host.port, &self.dir).await;
            if let Some(reader) = reader {
                reader.stop();
            }
            if let Some(end) = end {
                let tail = host_tail(self).await;
                let outcome = if ended { "ended" } else { "refused" };
                log_host_ended(self, host.pid, host.port, end, outcome, why, &tail);
            }
            if spawn::alive(host.pid) {
                // **A host that outlived its end keeps its trail.** The next
                // boot's reaper needs the record to find it, and clearing it
                // here would strand the process with nothing naming it.
                tracing::warn!(
                    "a browser host outlived its teardown (event_name \
                     browser_host_survived_end, profile {}, pid {}, port {})",
                    self.key,
                    host.pid,
                    host.port
                );
            } else {
                self.clear_host_trail(host.port);
            }
        }
    }

    /// **A clean end takes its own trail with it**: the record, the singleton
    /// lock and the ring are the reaper's, and none is left for the next boot
    /// to trip over.
    fn clear_host_trail(&self, port: u16) {
        spawn::remove_record(&self.dir);
        spawn::clear_singleton(&self.dir);
        osr::Ring::unlink(&spawn::ring_name(port));
    }

    /// End the profile for good, as a close or a delete does.
    async fn end(&self, end: HostEnd) {
        self.ended.store(true, Ordering::Release);
        // **Ending is one chooser decision too**: no answer or chooser event
        // can publish against a profile that is gone.
        let _decision = self.file_decision.lock().await;
        let mut state = self.state.lock().await;
        self.teardown(&mut state, Some(end), "").await;
        self.frames.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.nav.lock().unwrap_or_else(PoisonError::into_inner).clear();
        // **Nothing of a gone browser's chooser or its staged files stays.**
        self.files.send_replace(None);
        *self.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger::default();
        let _ = std::fs::remove_dir_all(self.dir.join("uploads"));
    }
}

/// The longest a history read in the watch is given: a wedged renderer must
/// not stall the watch for ever.
const NAV_BOUND: Duration = Duration::from_secs(2);

/// What the driver says about one tab's page.
enum LiveTarget {
    /// A target to drive: the driver's fresh one, else the registry's last.
    Found(String),
    /// The driver listed its tabs and the page was not among them: the tab
    /// was closed or its page is gone.
    Gone,
    /// Nothing to drive yet.
    Unknown,
}

/// The tab's live CDP target, refreshed from the driver when it can answer.
///
/// **CEF swaps a page's target id on a navigation**, a person's click
/// included; the registry's copy is only as fresh as the last call, so a
/// history read, a Back and the nav watch ask the driver before they use it.
/// The page guid is the identity that survives the swap.
async fn live_target(profile: &Arc<Profile>, tab: u16) -> LiveTarget {
    let driver = {
        let state = profile.state.lock().await;
        state.driver.as_ref().filter(|driver| driver.is_running()).map(Arc::clone)
    };
    if let Some(driver) = driver
        && let Ok(listing) = driver.tabs_within(NAV_BOUND).await
    {
        let page =
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).id(tab).map(str::to_owned);
        let mut row = page
            .as_deref()
            .and_then(|page| crate::tabs::index_of(&listing, page))
            .and_then(|at| listing.get(at));
        // **A fresh driver renames every page; the CDP target survives it.**
        // The same fallback the call path has, so a person's arrows keep
        // working after a driver transport crash.
        if row.is_none() {
            // **The lock is scoped to the read**: a let-chain's guard lives
            // to the end of the `if`, and the body needs the same lock.
            let stored = {
                let tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
                tabs.target(tab).map(str::to_owned)
            };
            if let Some(target) = stored
                && let Some(found) =
                    crate::tabs::index_of_target(&listing, &target).and_then(|at| listing.get(at))
            {
                let mut tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
                tabs.reidentify(tab, &found.id);
                tabs.retarget(tab, &found.target);
                row = Some(found);
            }
        }
        match row {
            Some(row) if !row.target.is_empty() => {
                profile
                    .tabs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .retarget(tab, &row.target);
                return LiveTarget::Found(row.target.clone());
            }
            // **Only a listing with no row for the page is a gone tab.** A
            // listed page whose target is empty is live: the export writes ""
            // when its own CDP read throws, so the registry's last target is
            // the one to try.
            None if page.is_some() => return LiveTarget::Gone,
            _ => {}
        }
    }
    match profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).target(tab).map(str::to_owned)
    {
        Some(target) => LiveTarget::Found(target),
        None => LiveTarget::Unknown,
    }
}

/// How long the watch waits for a page target that is not there yet.
const NAV_RETRY: Duration = Duration::from_millis(500);

/// How long one birth's pairing pass is given. **A pass that outlasts this is
/// named rather than left to the driver's own 150 s**: the person is waiting
/// on a popup's row, and a listing that never returns is the wedge a stuck
/// popup target used to cause.
const PAIR_BOUND: Duration = Duration::from_secs(30);

/// How often a pairing pass is retried while a popup's birth still waits for
/// its page, and how long the retries last. **The page appears only after
/// the popup's own first document starts**, so the birth is older than the
/// listing that can name it.
const POPUP_PAIR_EVERY: Duration = Duration::from_millis(250);
/// A little past the host's own give-up, so a final pass reads the close that
/// answers the birth instead of leaving it deferred until someone calls.
const POPUP_PAIR_UNTIL: Duration = Duration::from_secs(12);

/// One popup birth's pairing pass was asked for: the profile to pass over,
/// and what the slow-pass line names.
#[derive(Clone)]
struct PairingRequest {
    key: String,
    tab: u16,
    opener: u16,
    url: String,
    until: tokio::time::Instant,
}

/// Watch the focused tab's own navigations and publish the bar's arrows.
///
/// **The page navigates without anyone asking**: a link click, a redirect
/// and a fragment change all move the history, so the bar reads this watch
/// rather than the next input's answer. The events that count are
/// [`input::history_event`]'s; a read that fails is no news, because the
/// frame may not be attached yet and greyed arrows are worse than the last
/// pair standing.
fn start_nav_watch(profile: Arc<Profile>, port: u16) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The host this watch belongs to: a replacement restarts at tab one.
        let generation = profile.host_generation.load(Ordering::Acquire);
        'watch: loop {
            if profile.ended.load(Ordering::Acquire) {
                return;
            }
            let focused = *profile.focused.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(tab) = focused else {
                tokio::time::sleep(NAV_RETRY).await;
                continue;
            };
            let target = match live_target(&profile, tab).await {
                LiveTarget::Found(target) => target,
                // **A page the driver no longer lists is not retried**: each
                // retry is a driver round trip every 500 ms against a dead
                // tab, and on an unpatched stack that listing also creates a
                // page per tick (#2581).
                LiveTarget::Gone => {
                    tracing::debug!(
                        event_name = "browser_nav_watch_stopped",
                        profile = %profile.key,
                        tab,
                        "the history watch's page is gone; it stops rather than retrying"
                    );
                    profile.unfocus(generation, tab);
                    return;
                }
                LiveTarget::Unknown => {
                    tokio::time::sleep(NAV_RETRY).await;
                    continue;
                }
            };
            let Some(url) = spawn::page_websocket_of(port, &target).await else {
                tokio::time::sleep(NAV_RETRY).await;
                continue;
            };
            let Some(path) = url.strip_prefix(&spawn::ws_prefix(port)).map(str::to_owned) else {
                return;
            };
            let Ok((mut cdp, mut events)) = Cdp::connect_watch(port, &path).await else {
                tokio::time::sleep(NAV_RETRY).await;
                continue;
            };
            // The events below arrive only with the page domain on.
            if cdp.call("Page.enable", json!({}), None).await.is_err() {
                tokio::time::sleep(NAV_RETRY).await;
                continue;
            }
            // **Only the main frame speaks for the page.** A same-document
            // event names the frame it happened in, and an iframe's URL is
            // not the page the bar describes; the tree names the main frame
            // before any navigation event arrives.
            let mut main_frame: Option<String> = cdp
                .call("Page.getFrameTree", json!({}), None)
                .await
                .ok()
                .and_then(|reply| input::main_frame_id(&reply).map(str::to_owned));
            // A fresh connection's first move is news: the bar cleared its
            // arrows on the attach that led here.
            let mut last: Option<BrowserNavState> = None;
            while let Some(event) = events.recv().await {
                // **The focused tab may have moved** under this connection;
                // the watch follows it rather than publishing another tab's
                // history.
                if *profile.focused.lock().unwrap_or_else(PoisonError::into_inner) != Some(tab) {
                    continue 'watch;
                }
                let method = event.get("method").and_then(Value::as_str).unwrap_or_default();
                if let Some(frame) = input::navigated_frame_id(&event) {
                    main_frame = Some(frame.to_owned());
                }
                if !input::history_event(method, &event, main_frame.as_deref()) {
                    continue;
                }
                let reply = tokio::time::timeout(
                    NAV_BOUND,
                    cdp.call("Page.getNavigationHistory", json!({}), None),
                )
                .await;
                let Some(nav) =
                    reply.ok().and_then(Result::ok).as_ref().and_then(input::history_state)
                else {
                    continue;
                };
                if last == Some(nav) {
                    continue;
                }
                last = Some(nav);
                profile.publish_nav(tab, nav);
            }
            // The page's connection closed (a tab change, a dying renderer):
            // follow the focused page target again.
            if profile.ended.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(NAV_RETRY).await;
        }
    })
}

/// One page's input connection, opened on demand by its target id.
///
/// **One target, one connection.** Each tab's page is its own target, and
/// every input and history call follows the tab it names.
async fn page_cdp<'a>(
    state: &'a mut State,
    port: u16,
    target: &str,
) -> Result<&'a mut Cdp, String> {
    if !state.cdp.contains_key(target) {
        let path = spawn::page_websocket_of(port, target)
            .await
            .ok_or_else(|| format!("the page target {target} is not on the host"))?;
        let Some(path) = path.strip_prefix(&spawn::ws_prefix(port)).map(str::to_owned) else {
            return Err("the page's websocket is not on the host's port".to_owned());
        };
        let (cdp, events) = Cdp::connect_watch_bounded(port, &path, EVENT_BACKLOG).await?;
        state.cdp.insert(target.to_owned(), cdp);
        state.cdp_events.insert(target.to_owned(), events);
    }
    // **The page's events are drained and dropped.** Input and history are
    // send-and-reply paths; the channel exists because the reader task needs
    // a live one, and an unread backlog on a page that reports many events is
    // growth nobody would see.
    if let Some(events) = state.cdp_events.get_mut(target) {
        events.drain();
    }
    state.cdp.get_mut(target).ok_or_else(|| "the page's input connection is not open".to_owned())
}

/// Send one CDP call to one page without waiting for its reply.
async fn send_page(
    state: &mut State,
    port: u16,
    target: &str,
    method: &'static str,
    params: Value,
) -> Result<(), String> {
    page_cdp(state, port, target).await?.send(method, params).await
}

/// One CDP call to one page, answered.
async fn call_page(
    state: &mut State,
    port: u16,
    target: &str,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    page_cdp(state, port, target).await?.call(method, params, None).await
}

/// What a profile holds once it is up.
#[derive(Default)]
struct State {
    host: Option<ActiveHost>,
    driver: Option<Arc<dyn TabDriver>>,
    /// Bumped whenever a driver is installed, so a call's dead-driver clear
    /// cannot wipe a newer driver's handle started in between.
    driver_generation: u64,
    /// When this profile last rebuilt its driver after a dead page context;
    /// the cooldown stops a browser that stays dead costing a driver start
    /// on every call.
    stale_since: Option<Instant>,
    /// The input connection to each page target, keyed by the target id so a
    /// tab switch opens the new target's connection rather than reusing the
    /// old one.
    cdp: HashMap<String, Cdp>,
    cdp_events: HashMap<String, Events>,
    /// The focused tab's navigation watch: the bar's arrows after a link
    /// click.
    nav_watch: Option<tokio::task::JoinHandle<()>>,
}

/// Whether an agent's upload is refused because the person's picker is open.
/// Called under `file_decision`, so the decision cannot move under it.
fn agent_upload_refused(profile: &Profile) -> bool {
    profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_some()
}

/// The refusal an agent's upload meets while the person's picker is open.
/// **The form does not matter**: an answer and the schema's cancel both
/// answer the captured chooser. Called under `file_decision`.
fn upload_gate(upload: Option<&UploadRequest>, profile: &Profile) -> Result<(), String> {
    if upload.is_none() || !agent_upload_refused(profile) {
        return Ok(());
    }
    Err("the client's picker is open for this page's file chooser, so the agent's upload is \
         refused"
        .to_owned())
}

/// Begin one upload's decision: take the lock and apply the person's picker
/// refusal. The guard is returned so the driver call runs inside it.
async fn begin_upload<'a>(
    profile: &'a Profile,
    upload: Option<&UploadRequest>,
) -> Result<Option<tokio::sync::MutexGuard<'a, ()>>, String> {
    let Some(upload) = upload else { return Ok(None) };
    let decision = profile.file_decision.lock().await;
    upload_gate(Some(upload), profile)?;
    Ok(Some(decision))
}

/// The ledger side of a finished upload: an answer or a cancel that reached
/// the chooser consumes one surface, or owes one event. The form does not
/// matter; only a call that reached the MCP counts. Called under
/// `file_decision`.
fn note_upload_finished(profile: &Profile, _upload: &UploadRequest, ok: bool) {
    if ok {
        note_agent_answered(profile);
    }
}

/// **A dead driver's clear only when it is still the installed generation.**
/// The generation is bumped on every install, so a clear aimed at a dead
/// driver cannot wipe a newer driver's handle started in between.
fn clear_if_dead(state: &mut State, generation: u64, alive: bool, profile: &Profile) -> bool {
    if alive || state.driver_generation != generation {
        return false;
    }
    state.driver = None;
    profile.clear_chooser_state();
    true
}

/// The generation a call drives: the one installed when it looked the driver
/// up. A driver installed after this is not the one the call saw, so a clear
/// or rebuild aimed at this call must not touch it.
fn captured_generation(state: &State) -> u64 {
    state.driver_generation
}

/// The generation a fresh install takes: one past the current, so a clear
/// aimed at the old one can never match the new driver.
fn installed_generation(state: &State) -> u64 {
    state.driver_generation.wrapping_add(1)
}

/// The report an evaluate answer carries, without the evaluate's own
/// dressing: the `### Result` block is the report, and the code it ran is a
/// script this tool fixes rather than something the caller sent.
fn challenge_result(parts: Vec<BrowserPart>) -> Vec<BrowserPart> {
    parts
        .into_iter()
        .map(|part| match part {
            BrowserPart::Text { text } => {
                let result =
                    text.split_once("### Result\n").map_or(text.as_str(), |(_, rest)| rest);
                let result = result.split("\n### ").next().unwrap_or(result);
                BrowserPart::Text { text: result.trim().to_owned() }
            }
            part @ BrowserPart::Image { .. } => part,
        })
        .collect()
}

/// A failure's text names the tool the session called, not the evaluate a
/// composed call runs as. **Only the composed call differs**, so a plain
/// tool's own words ride through untouched.
fn reported_error(why: String, driver_tool: &str, reported: &str) -> String {
    if driver_tool == reported { why } else { why.replace(driver_tool, reported) }
}

/// Whether repeating a tool call cannot land twice on the page.
///
/// A dead page context is rebuilt and the call repeated only here; a click,
/// a submit or a type is handed back with the rebuild named instead.
fn repeatable(tool: &str, reported: &str, args: &Value) -> bool {
    // The challenge probe and a form read are reads: repeating either cannot
    // land twice.
    if matches!(reported, "browser_challenge_state" | "browser_form_state") {
        return true;
    }
    match tool {
        "browser_navigate" => {
            args.get("url").and_then(Value::as_str).is_some_and(|url| !url.is_empty())
        }
        "browser_snapshot" => true,
        _ => false,
    }
}

/// Whether a call that was already in flight is safe to repeat after its
/// driver stopped answering. **A timed-out call is not a call that did not
/// run**: the browser keeps working after our request is cancelled, so a
/// navigate, a click or a type may already have landed. Only reads are
/// repeated; everything else is handed back with that warning. (A dead PAGE
/// is different - its context died and took the action with it - so
/// [`repeatable`] is the broader rule there.)
fn repeatable_after_timeout(tool: &str, reported: &str, _args: &Value) -> bool {
    matches!(reported, "browser_challenge_state" | "browser_form_state")
        || matches!(tool, "browser_snapshot")
}

/// What became of a call whose page context died, as the answer's own words.
#[derive(Debug, Clone, Copy)]
enum DeadPageOutcome<'a> {
    /// The call cannot be repeated safely, so it was handed back.
    NotRepeated { tool: &'a str },
    /// The repeated call failed too, with a reason of its own.
    RetryFailed { why: &'a str },
    /// The repeated call hit the same dead context again.
    StillDead,
    /// No rebuild was possible.
    RebuildFailed { why: &'a str },
    /// A rebuild failed inside the cooldown, so none was attempted.
    Cooling,
    /// The fresh driver does not list the page, so no retry can land on it.
    TabGone { tab: u16 },
    /// The fresh driver's tab list could not be read, so nothing is known
    /// about the page: **a listing that failed was never read**, and its page
    /// is not gone because of it.
    ListFailed { why: &'a str },
}

/// **The host a call drove, as the identity a late failure may end.** A host
/// is a process, not a counter: `driver_generation` bumps on a driver install
/// and says nothing about a host started beside it, so a guard on it alone
/// could take a successor's host and leak the process (#2573). The driver
/// generation is part of the identity too: a driver installed on the same
/// host after the call looked it up is not the one that call drove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostIdentity {
    pid: u32,
    port: u16,
    generation: u64,
}

/// What became of a host a call tried to end.
#[derive(Debug, Clone, Copy)]
enum DeadHost {
    /// This call ended it, with the identity a recurrence is diagnosed by.
    Ended { pid: u32, port: u16 },
    /// The host this call recorded was not there to end: another process
    /// answers on its port, so the end was refused.
    NotEnded { pid: u32, port: u16 },
    /// The host this call drove was already gone.
    AlreadyGone,
    /// Another host serves the profile now; this call ends nothing.
    NotOurs,
}

/// **The answer never leads with the driver's `_page` TypeError**: it names
/// an internal Playwright object, and a session can act only on what died
/// and what to do now (#2534). The driver's line stays in the rebuild log.
fn dead_page_answer(outcome: DeadPageOutcome<'_>) -> String {
    let died = "the browser's page context died (its renderer went away)";
    match outcome {
        DeadPageOutcome::NotRepeated { tool } => format!(
            "{died} and the driver has been rebuilt. {tool} may already have landed, so it was \
             not repeated; check the page before running it again."
        ),
        DeadPageOutcome::RetryFailed { why } => {
            format!("{died} and the driver was rebuilt, but the repeated call failed too: {why}")
        }
        DeadPageOutcome::StillDead => format!(
            "{died} and the driver was rebuilt, but the repeated call met the same dead context; \
             try again shortly."
        ),
        DeadPageOutcome::RebuildFailed { why } => {
            format!("{died} and the driver could not be rebuilt: {why}")
        }
        DeadPageOutcome::Cooling => format!(
            "{died} again after a rebuild, and the driver has not been rebuilt a second time yet; \
             try again shortly."
        ),
        DeadPageOutcome::TabGone { tab } => format!(
            "{died} and the driver was rebuilt, but tab {tab}'s page is not in the fresh driver's \
             list; the tab was left open. Call browser_tabs to see it, or open a new tab."
        ),
        DeadPageOutcome::ListFailed { why } => format!(
            "{died} and the driver was rebuilt, but its tab list could not be read ({why}), so \
             nothing is known about the page; the tab was left open. Try again."
        ),
    }
}

/// **A host a fresh driver still could not list is named by its pid, port and
/// profile**: a bare timeout is not a diagnosis, and the next call's state is
/// what the session needs to act on.
fn dead_host_answer(profile: &Profile, host: DeadHost, why: &str) -> String {
    match host {
        DeadHost::Ended { pid, port } => {
            // **The host's own last words are the diagnosis**: a bare
            // initializeServer timeout names nothing (#2534).
            let tail = spawn::log_tail(&spawn::host_log_path(&profile.dir));
            let words = if tail.is_empty() {
                "its log is empty".to_owned()
            } else {
                format!("its log ends: {tail}")
            };
            format!(
                "the browser host (pid {pid}, port {port}, profile {}) stopped answering and was \
                 ended: {why}. {words}. The next call starts a fresh browser with the profile's \
                 data intact.",
                profile.dir.display(),
            )
        }
        DeadHost::NotEnded { pid, port } => format!(
            "the browser host (pid {pid}, port {port}, profile {}) stopped answering but was not \
             ended: another process answers on that port. {why}. The next call starts a fresh \
             browser.",
            profile.dir.display(),
        ),
        DeadHost::AlreadyGone => format!(
            "the browser host (profile {}) stopped answering: {why}. The next call starts a fresh \
             browser.",
            profile.dir.display(),
        ),
        DeadHost::NotOurs => format!("the browser driver stopped answering: {why}"),
    }
}

/// What became of a call whose driver stopped answering.
#[derive(Debug, Clone, Copy)]
enum DeadDriverOutcome<'a> {
    /// The call cannot be repeated safely, so it was handed back. `why` says
    /// whether it ever reached the page.
    NotRepeated { tool: &'a str, why: &'a str },
    /// The repeated call failed too, with a reason of its own.
    RetryFailed { why: &'a str },
    /// The replacement driver could not start.
    ReplaceFailed { why: &'a str },
    /// The fresh driver does not list the tab's page, so no retry can land.
    TabGone,
    /// The fresh driver's tab list could not be read, so nothing is known
    /// about the page: a listing that failed was never read.
    ListFailed { why: &'a str },
}

/// **The driver is named as replaced, never as a page that died**: the
/// session's next move is to call again, not to restart anything.
fn dead_driver_answer(outcome: DeadDriverOutcome<'_>) -> String {
    let replaced = "the browser driver stopped answering and was replaced on this call";
    match outcome {
        // **An attach timeout never reached the page**: the driver could not
        // finish its handshake, so the tool was not delivered and repeating
        // it cannot double. Every other timeout may have landed.
        // **An attach timeout and a `browser_tabs` timeout never reached the
        // page**: the first never finished its handshake and the second is a
        // listing or a selection that never answered, so nothing was
        // delivered however the outer tool is named. Repeating either cannot
        // double.
        DeadDriverOutcome::NotRepeated { tool, why }
            if driver::attach_timed_out(why)
                || why.contains("the driver did not answer browser_tabs") =>
        {
            format!(
                "{replaced}. The call never reached the page (the driver never answered it), so \
                 {tool} can be run again now."
            )
        }
        DeadDriverOutcome::NotRepeated { tool, .. } => format!(
            "{replaced}. {tool} may already have landed, so it was not repeated; check the page \
             before running it again."
        ),
        DeadDriverOutcome::RetryFailed { why } => {
            format!("{replaced}, but the repeated call failed too: {why}")
        }
        DeadDriverOutcome::ReplaceFailed { why } => {
            format!("the browser driver stopped answering and could not be replaced: {why}")
        }
        DeadDriverOutcome::TabGone => format!(
            "{replaced}, but this session's page is not in the fresh driver's list; the tab is \
             gone. Open a tab and run the call again."
        ),
        DeadDriverOutcome::ListFailed { why } => format!(
            "{replaced}, but its tab list could not be read ({why}), so nothing is known about \
             this session's page; the tab was left open. Try again."
        ),
    }
}

/// The line a rebuild logs: the profile, the tool and the outcome, so a
/// recurrence is diagnosed from the log rather than from the driver's own
/// `_page` TypeError on the wire.
fn log_rebuild(profile: &Profile, tool: &str, outcome: &str, why: &str) {
    tracing::debug!(
        event_name = "browser_driver_rebuild",
        profile = %profile.key,
        tool,
        outcome,
        "a dead page context rebuilt the driver: {why}"
    );
}

/// The agent's upload answered the oldest surfaced chooser: consume its
/// surface, or owe one event when no chooser has surfaced yet.
/// Called under `file_decision`.
fn note_agent_answered(profile: &Profile) {
    let mut ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
    if ledger.surfaced > 0 {
        ledger.surfaced -= 1;
    } else {
        ledger.pre_answered += 1;
    }
}

/// `count` choosers were surfaced, or a lagged reader skipped that many.
/// Called under `file_decision`.
fn note_surfaced_opened(profile: &Profile, count: usize) {
    let mut ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
    ledger.surfaced = ledger.surfaced.saturating_add(count);
}

/// `count` surfaced choosers are consumed, by a person's answer or a drain.
/// Called under `file_decision`.
fn note_surfaced_consumed_by(profile: &Profile, count: usize) {
    let mut ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
    ledger.surfaced = ledger.surfaced.saturating_sub(count);
}

/// The driver's chooser socket: **a short path in a private directory**,
/// because a Unix socket's address is limited to about 104 bytes and the
/// profile's own directory is deep under app-support; the pid keeps two
/// forge instances apart.
fn chooser_socket_path(profile_dir: &Path, profile: u16) -> Result<PathBuf, String> {
    Ok(channel::private_dir(profile_dir)?
        .join(format!("chooser-{}-{profile}.sock", std::process::id())))
}

/// What is left of a call's budget - at least enough to send the call, so a
/// spent budget answers a timeout rather than a silent skip.
fn left(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now()).max(Duration::from_millis(1))
}

/// The pages reconcile closes: every page target the registry does not know,
/// except a fresh host's boot page while no session has adopted it yet.
fn closed_as_orphans(
    tabs: &crate::tabs::TabRegistry,
    listing: &[crate::tabs::DriverTab],
    boot_pending: bool,
) -> Vec<String> {
    // **A row whose page could not be named is not an orphan.** Nothing is
    // pruned while one is unresolved: an unclaimed page may be that live
    // document, and closing it is how a filled form is lost.
    if !tabs.unresolved_tabs().is_empty() {
        return Vec::new();
    }
    let mut orphaned = crate::tabs::orphan_targets(tabs, listing);
    if boot_pending
        && let Some(boot) = listing.iter().find(|row| row.current).or_else(|| listing.first())
    {
        orphaned.retain(|target| target != &boot.target);
    }
    orphaned
}

/// Record one host tab event in arrival order, answering whether it is this
/// request's own birth. **The host generation is part of the event's
/// identity**: a birth or close from a receiver that has been replaced is
/// kept for the next pass, but never taken for the current host's.
fn collect_event(
    (event_generation, event): (u64, HostEvent),
    generation: u64,
    request: u64,
    events: &mut Vec<(u64, HostEvent)>,
    ours_at: &mut Option<usize>,
) -> bool {
    let ours = event_generation == generation
        && matches!(
            &event,
            HostEvent::TabCreated { request: Some(asked), .. } if *asked == request
        );
    if ours {
        *ours_at = Some(events.len());
    }
    events.push((event_generation, event));
    ours
}

/// Keep this attempt's events for the next reconcile. **Rule 25**: an event
/// the call read is re-injected rather than dropped when the call cannot use
/// it. An event from a receiver that was replaced is kept too; a later pass
/// drops it by its older generation, or pairs it when it is the successor's.
fn defer_events(profile: &Profile, events: &mut Vec<(u64, HostEvent)>, skip: Option<usize>) {
    let kept: Vec<(u64, HostEvent)> = std::mem::take(events)
        .into_iter()
        .enumerate()
        .filter(|(at, _)| Some(*at) != skip)
        .map(|(_, event)| event)
        .collect();
    if kept.is_empty() {
        return;
    }
    profile.deferred_tab_events.lock().unwrap_or_else(PoisonError::into_inner).extend(kept);
}

/// Whether a popup's birth is still waiting for its page.
fn popup_birth_waits(profile: &Profile) -> bool {
    profile
        .deferred_tab_events
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .any(|(_, event)| matches!(event, HostEvent::TabCreated { opener: Some(_), .. }))
}

/// **The events one reconcile took, put back if that pass is cancelled.**
/// The pairing pass is bounded by a timeout, and a listing that never returns
/// cancels the future where it stands; without this the births held in the
/// pass's own vector go with it, the popup never pairs, and the next pass
/// prunes its page as an orphan.
struct TakenEvents<'a> {
    profile: &'a Profile,
    events: Option<Vec<(u64, HostEvent)>>,
}

impl Drop for TakenEvents<'_> {
    fn drop(&mut self) {
        let Some(events) = self.events.take() else { return };
        if events.is_empty() {
            return;
        }
        self.profile
            .deferred_tab_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(events);
    }
}

/// Pair host tab events with the driver's pages: a `TabClosed` removes the
/// tab, and a `TabCreated` takes the first page no tab claims. **A popup's
/// page can appear in the driver's list a moment after the host reports it**,
/// so an event with no page is answered back for the next pass rather than
/// dropped.
fn pair_tab_events(
    tabs: &mut TabRegistry,
    listing: &[crate::tabs::DriverTab],
    events: Vec<(u64, HostEvent)>,
    generation: u64,
) -> Vec<(u64, HostEvent)> {
    let mut unmatched = Vec::new();
    for (event_generation, event) in events {
        // **An older receiver speaks for a dead host.** Its tab numbers
        // belong to a registry this profile no longer holds, so it is
        // dropped. **A newer one is the successor's**: this pass pairs with
        // the old listing and must not touch it, so it goes back for the
        // next pass, which will carry its generation.
        if event_generation < generation {
            tracing::debug!(
                event_name = "browser_tab_event_stale_generation",
                event_generation,
                generation,
                "a tab event from a replaced host is dropped"
            );
            continue;
        }
        if event_generation > generation {
            unmatched.push((event_generation, event));
            continue;
        }
        match event {
            HostEvent::TabClosed { tab } => {
                tabs.close(tab);
            }
            HostEvent::TabCreated { tab, opener, url, request } => {
                if tabs.target(tab).is_some() {
                    continue;
                }
                let known: HashSet<String> = tabs
                    .numbers()
                    .into_iter()
                    .filter_map(|number| tabs.target(number).map(str::to_owned))
                    .collect();
                // **The popup's address is the best name it has.** An
                // `about:blank` popup falls back to the first unclaimed page,
                // the order the host reported it in.
                let unclaimed = |row: &&crate::tabs::DriverTab| {
                    !row.target.is_empty() && !known.contains(&row.target)
                };
                let Some(row) = listing
                    .iter()
                    .find(|row| unclaimed(row) && (url.is_empty() || row.url == url))
                    .or_else(|| listing.iter().find(unclaimed))
                else {
                    unmatched.push((
                        event_generation,
                        HostEvent::TabCreated { tab, opener, url, request },
                    ));
                    continue;
                };
                if let Some(opener) = opener {
                    let _ =
                        tabs.create_under(opener, tab, &row.id, &row.target, &row.title, &row.url);
                }
            }
            other => unmatched.push((event_generation, other)),
        }
    }
    // **A close and a birth can arrive in one pass**, the birth first. The
    // close answers the birth, so it is dropped here rather than kept for a
    // page the tab will never have.
    unmatched.retain(|(event_generation, event)| {
        !(*event_generation == generation
            && matches!(event, HostEvent::TabCreated { tab, .. } if tabs.was_closed(*tab)))
    });
    unmatched
}

/// The tab's number from a `browser_tabs` call: the field keeps the upstream
/// name `index`, and it is a forge tab number.
fn tool_tab(args: &Value) -> Result<u16, String> {
    let index = args
        .get("index")
        .ok_or_else(|| "browser_tabs needs the tab's number in `index`".to_owned())?;
    let number =
        index.as_u64().ok_or_else(|| "`index` is a tab number, not what was passed".to_owned())?;
    u16::try_from(number).map_err(|_| format!("{number} is not a tab number"))
}

/// The view a resize asks for, when the caller owns the tab's layout: its
/// controller while the role is held, else its longest-watching viewer.
///
/// **One owner, never whoever asks**: two viewers sizing the same unheld tab
/// would move the page under each other, and a page with no shell still lays
/// its own view out while it is the only one watching.
fn owner_size(
    controller: Option<u64>,
    watchers: &HashSet<u64>,
    viewer: u64,
    width: u32,
    height: u32,
    scale: f64,
) -> Option<(u32, u32, f64)> {
    let owner = controller.or_else(|| watchers.iter().copied().min());
    if owner != Some(viewer) {
        return None;
    }
    let scale = if scale.is_finite() && scale > 0.0 { scale.clamp(0.1, 8.0) } else { 1.0 };
    Some((width.max(1), height.max(1), scale))
}

/// One window dimension in CSS pixels, bounded to what the ring can hold at
/// `scale`. **Bounded here rather than left to the ring's silent clamp**, so
/// the answer names the size the page will report.
fn window_dimension(args: &Value, name: &str, scale: f64, device_max: u32) -> Result<u32, String> {
    let asked = args
        .get(name)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("browser_resize needs `{name}` as a number"))?;
    if !asked.is_finite() || asked < 1.0 {
        return Err(format!("browser_resize needs `{name}` as a positive number"));
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked finite and at least 1, then bounded by the ring's own device size"
    )]
    let css = asked.round().min(f64::from(device_max) / scale) as u32;
    Ok(css.max(1))
}

/// The device pixels a CSS dimension paints at, rounded to the nearest. The
/// caller has already bounded it by the ring's device size.
fn device_pixels(css: u32, scale: f64) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounded by the ring's own device bound before the cast"
    )]
    let pixels = (f64::from(css) * scale).round() as u32;
    pixels.max(1)
}

#[cfg(test)]
mod tests {
    use super::{
        BrowserService, Call, ChooserAnswer, ChooserFuture, ChooserLedger, DeadDriverOutcome,
        DeadHost, DeadPageOutcome, Driven, HostEnd, Profile, State, UploadRequest,
        agent_upload_paths, agent_upload_refused, begin_upload, captured_generation, clear_if_dead,
        dead_driver_answer, dead_host_answer, dead_page_answer, death_cause, device_pixels,
        file_name, installed_generation, note_agent_answered, note_surfaced_opened,
        note_upload_finished, owner_size, pair_tab_events, stage_agent_uploads, upload_request,
        window_dimension, write_uploads,
    };
    use forge_primitives::SessionSlot;
    use forge_primitives::browser::ProfileKey;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;
    use tokio::sync::Notify;
    use tracing_subscriber::layer::SubscriberExt as _;

    /// A driver call a test can hold open: it says it started, then waits to
    /// be released. The decision lock must span this wait.
    struct GatedAnswer {
        entered: Notify,
        release: Notify,
    }

    impl ChooserAnswer for GatedAnswer {
        fn call(&self, _args: serde_json::Value, _bound: Duration) -> ChooserFuture<'_> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                Ok(Vec::new())
            })
        }
    }

    /// A driver call that answers at once, for the drain's accounting.
    struct InstantAnswer;

    impl ChooserAnswer for InstantAnswer {
        fn call(&self, _args: serde_json::Value, _bound: Duration) -> ChooserFuture<'_> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    /// Every record a log site emitted while the capture was installed.
    #[derive(Clone, Default, Debug)]
    pub(crate) struct Caught(Arc<Mutex<Vec<(tracing::Level, String)>>>);

    /// Every field a record carried, as `name=value`, so a test reads the
    /// `event_name` and the identity off the site that emitted it.
    #[derive(Default)]
    struct Fields(String);

    impl tracing::field::Visit for Fields {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Caught {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((*event.metadata().level(), fields.0));
        }
    }

    /// The `browser_host_ended` records a capture caught, in emission order.
    pub(crate) fn ended(caught: &Caught) -> Vec<(tracing::Level, String)> {
        caught
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, fields)| fields.contains("event_name=\"browser_host_ended\""))
            .cloned()
            .collect()
    }

    /// Every record a capture caught, in emission order.
    pub(crate) fn records(caught: &Caught) -> Vec<(tracing::Level, String)> {
        caught.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Whether the capture caught `event_name=<event>`.
    pub(crate) fn saw(caught: &Caught, event: &str) -> bool {
        let needle = format!("event_name=\"{event}\"");
        caught
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|(_, fields)| fields.contains(&needle))
    }

    /// **A download is read once, from its own directory, or refused with a
    /// reason.** The happy path hands the bytes on and removes the server's
    /// copy; a file past the cap and a path outside the profile are both
    /// refusals the client draws.
    #[test]
    fn a_download_is_read_or_refused_with_its_reason() {
        use super::{MAX_DOWNLOAD_BYTES, read_download};
        let dir = tempfile::tempdir().expect("a temp dir");
        let downloads = dir.path().join("downloads");
        std::fs::create_dir_all(&downloads).expect("the downloads dir");

        let small = downloads.join("1-small.txt");
        std::fs::write(&small, b"hello").expect("a small file");
        let read = read_download(dir.path(), 2, "small.txt", small.to_str().expect("a path"), 5);
        assert_eq!(read.tab, 2, "the tab comes with the file");
        assert_eq!(read.bytes, b"hello", "the bytes are handed on");
        assert!(read.error.is_none());
        assert!(!small.exists(), "the server's copy goes once it is read");

        let big = downloads.join("2-big.bin");
        let read = read_download(
            dir.path(),
            2,
            "big.bin",
            big.to_str().expect("a path"),
            MAX_DOWNLOAD_BYTES + 1,
        );
        assert!(read.bytes.is_empty());
        assert!(
            read.error.as_deref().unwrap_or_default().contains("larger than 32 MiB"),
            "an oversized file is refused in words: {:?}",
            read.error
        );

        let outside = dir.path().join("elsewhere.txt");
        std::fs::write(&outside, b"x").expect("a file outside downloads");
        let read =
            read_download(dir.path(), 2, "elsewhere.txt", outside.to_str().expect("a path"), 1);
        assert!(read.error.is_some(), "a path outside downloads is refused");
        assert!(outside.exists(), "and a file outside is not deleted");
    }

    /// **A chooser does not outlive the person driving it.** A take, a
    /// release or the controller detaching cancels an outstanding ask, so a
    /// role that moved cannot leave the browser holding a dialog nobody is
    /// looking at.
    #[tokio::test]
    async fn a_control_change_cancels_an_outstanding_file_ask() {
        use forge_primitives::browser::BrowserFileAsk;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.files.send_replace(Some(BrowserFileAsk { id: 4, tab: 1, multiple: false }));
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        assert!(profile.files.borrow().is_none(), "the ask is cancelled with the take");

        profile.files.send_replace(Some(BrowserFileAsk { id: 5, tab: 1, multiple: false }));
        service.release_control("TestOrg/default", tab, 7).await;
        assert!(profile.files.borrow().is_none(), "and with the release");

        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves back");
        profile.files.send_replace(Some(BrowserFileAsk { id: 6, tab: 1, multiple: false }));
        service.detach(7).await;
        assert!(profile.files.borrow().is_none(), "and when the controller detaches");
    }

    /// **An answer from another tab is refused.** The ask names its tab; a
    /// viewer on another tab answering its id would hand files to a page it
    /// is not looking at.
    #[tokio::test]
    async fn a_file_answer_from_another_tab_is_refused() {
        use forge_primitives::browser::BrowserFileAsk;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, tab: 2, multiple: false }));

        let refused =
            service.answer_files("TestOrg/default", tab, 5, Vec::new()).await.expect_err("refused");
        assert!(refused.contains("is on tab 2, not tab 1"), "{refused}");
    }

    /// **A navigation swaps the page's target, and the driver is asked
    /// afresh.** A person's click leaves the registry's target stale until an
    /// agent call; the history read, Back and the nav watch ask the driver by
    /// the page guid that survives the swap.
    #[tokio::test]
    async fn a_person_navigation_refreshes_the_target_from_the_driver() {
        use super::{LiveTarget, live_target};

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        let seen = fake.clone();
        service.install_fake(&seat, fake).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");
        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).target(1),
            Some("T1"),
        );

        // The page navigated under a person's click: same guid, new target.
        {
            let mut tabs = seen.tabs.lock().expect("the fake's tabs");
            tabs[0].target = "T2".to_owned();
        }
        assert!(
            matches!(live_target(&profile, 1).await, LiveTarget::Found(target) if target == "T2"),
            "the driver's fresh target is the one to drive",
        );
        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).target(1),
            Some("T2"),
            "the registry follows the driver's listing",
        );
    }

    /// Wait until the fake has answered `want` listings, on the test's own
    /// clock rather than a wall-clock sleep.
    async fn wait_for_listings(fake: &FakeDriver, want: usize) {
        while fake.listings() < want {
            fake.changed.notified().await;
        }
    }

    /// **A watch whose focused page is gone stops asking.** The driver's tab
    /// listing creates a page when it has none (upstream's `ensureTab`), so a
    /// retry loop against a closed tab manufactures a page and a renderer on
    /// every tick (#2581).
    #[tokio::test(start_paused = true)]
    async fn a_nav_watch_whose_focused_page_is_gone_stops_listing() {
        use super::start_nav_watch;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        let seen = fake.clone();
        service.install_fake(&seat, fake).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");
        assert_eq!(
            *profile.focused.lock().unwrap_or_else(PoisonError::into_inner),
            Some(1),
            "the call focused its tab",
        );
        // The call listed too; the watch's own listing is the next one.
        let base = seen.listings();

        // Nothing answers on this port, so the watch lists with the page
        // still there, finds it, and comes back around on its retry clock.
        let watch = start_nav_watch(Arc::clone(&profile), 9);
        wait_for_listings(&seen, base + 1).await;

        // The page closed: the driver lists and it is not there.
        seen.close_all();
        wait_for_listings(&seen, base + 2).await;
        let settled = seen.listings();
        tokio::time::sleep(super::NAV_RETRY * 8).await;
        assert_eq!(
            seen.listings(),
            settled,
            "a watch whose focused page is gone stops listing; every further listing is a page \
             the driver would create",
        );
        assert_eq!(
            *profile.focused.lock().unwrap_or_else(PoisonError::into_inner),
            None,
            "and it lets the focus go, so the next call refocuses",
        );
        watch.abort();
    }

    /// **A listing that cannot name a target is not a gone tab.** An empty
    /// target is a live page whose own CDP read failed, and a stale-page error
    /// is another driver context's dead page; both fall back to the registry.
    #[tokio::test]
    async fn a_listing_without_a_target_falls_back_to_the_registry() {
        use super::{LiveTarget, live_target};

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        let seen = fake.clone();
        service.install_fake(&seat, fake).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");

        seen.clear_target();
        assert!(
            matches!(live_target(&profile, 1).await, LiveTarget::Found(target) if target == "T1"),
            "a listed page with no target falls back to the registry target",
        );

        seen.stale_once();
        assert!(
            matches!(live_target(&profile, 1).await, LiveTarget::Found(target) if target == "T1"),
            "a stale-page listing error falls back to the registry target",
        );
    }

    /// **A renamed page is found by its target.** The person's arrows and the
    /// history watch use the same fallback the call path has, so a driver
    /// replacement does not freeze them on a page that is still there.
    #[tokio::test]
    async fn a_renamed_page_is_found_by_its_target() {
        use super::{LiveTarget, live_target};

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        let seen = fake.clone();
        service.install_fake(&seat, fake).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");

        seen.reidentify("P2");

        assert!(
            matches!(live_target(&profile, 1).await, LiveTarget::Found(target) if target == "T1"),
            "the target names the page a fresh driver renamed",
        );
        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).id(1),
            Some("P2"),
            "and the row follows the fresh driver's name",
        );
    }

    /// **A fresh driver that still times out ends the host.** A host kept on
    /// process liveness alone with pages no driver can list is the wedge that
    /// used to cost an hour; the call that finds it takes it down so the next
    /// call starts a fresh browser.
    #[tokio::test]
    async fn a_fresh_driver_that_still_times_out_ends_the_host() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let wedged = FakeDriver::with_blank("T1");
        let successor = FakeDriver::with_blank("T1");
        let replaced = successor.clone();
        service
            .install_fake_with_successor(&seat, wedged.clone(), move || {
                Arc::new(replaced.clone()) as Arc<dyn crate::driver::TabDriver>
            })
            .await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        // Every listing times out from here, on the driver and on its
        // replacement.
        wedged.timeout_every_listing();
        successor.timeout_every_listing();
        let profile = service.find("O/default").await.expect("the profile");
        let ring_name = format!("/forge-host-end-{}", std::process::id());
        let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
        let (input, _input_rx) = std::sync::mpsc::channel();
        profile.state.lock().await.host = Some(crate::spawn::ActiveHost {
            pid: std::process::id(),
            port: 9,
            ring: Arc::new(ring),
            profile: dir.path().join("host-end"),
            input,
            reader: None,
            monitor: None,
        });

        let refused = service
            .call(&seat, "browser_snapshot", json!({}))
            .await
            .expect_err("a host no driver can list is ended, not retried for ever");
        assert!(refused.contains("stopped answering"), "{refused}");
        assert!(refused.contains("pid"), "the host's identity is named: {refused}");
        assert!(profile.state.lock().await.host.is_none(), "the wedged host was taken down");
        assert!(
            successor.listings() > 0,
            "the replacement driver listed before the host was ended"
        );
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **A rebuild that replaces the host ends the host standing after it.**
    /// A rebuild starts a new host when the old one died, so the fresh
    /// driver serves a host the call never looked up; a listing that dies
    /// there must end that host rather than answer for the one that is gone
    /// (#2573's shape).
    #[tokio::test]
    async fn a_rebuild_that_replaces_the_host_ends_the_standing_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let installed = FakeDriver::with_blank("T1");
        let successor = FakeDriver::with_blank("T1");
        let fresh = successor.clone();
        successor.timeout_every_listing();
        service
            .install_fake_with_successor(&seat, installed, move || {
                Arc::new(successor.clone()) as Arc<dyn crate::driver::TabDriver>
            })
            .await;
        let profile = service.find("O/default").await.expect("the profile");
        let ring_name = format!("/forge-replaced-host-{}", std::process::id());
        let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
        let (input, _input_rx) = std::sync::mpsc::channel();
        let generation = {
            let mut state = profile.state.lock().await;
            state.host = Some(crate::spawn::ActiveHost {
                pid: std::process::id(),
                port: 9,
                ring: Arc::new(ring),
                profile: dir.path().join("replacement"),
                input,
                reader: None,
                monitor: None,
            });
            state.driver_generation
        };
        // The host the call looked up is not the one standing after the
        // rebuild, the shape a start behind a dead host leaves.
        let drove = super::HostIdentity { pid: std::process::id(), port: 9411, generation };

        let refused = service
            .replace_dead_driver(
                &profile,
                generation,
                drove,
                1,
                Call { tool: "browser_snapshot", reported: "browser_snapshot", args: &json!({}) },
                Duration::from_secs(1),
                "the driver did not answer browser_tabs within 9 s",
            )
            .await
            .expect_err("a fresh driver that cannot list ends the host");

        assert!(refused.contains("was ended"), "the standing host is named as ended: {refused}");
        assert!(profile.state.lock().await.host.is_none(), "the standing host was ended");
        assert!(fresh.listings() > 0, "the replacement listed before the host was ended");
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **A host with no boot tab is ended from the create it wedges.** A
    /// create installs its own driver and lists with it; a listing that dies
    /// there is the same wedge `tabs_recovering` ends, and this path never
    /// reaches that recovery.
    #[tokio::test]
    async fn a_create_whose_fresh_driver_cannot_list_ends_the_host() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let wedged = FakeDriver::with_blank("T1");
        wedged.timeout_every_listing();
        let successor = FakeDriver::with_blank("T1");
        successor.timeout_every_listing();
        service
            .install_fake_with_successor(&seat, wedged, move || {
                Arc::new(successor.clone()) as Arc<dyn crate::driver::TabDriver>
            })
            .await;
        let profile = service.find("O/default").await.expect("the profile");
        let ring_name = format!("/forge-create-end-{}", std::process::id());
        let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
        let (input, requests) = std::sync::mpsc::channel();
        let (events, births) = tokio::sync::mpsc::unbounded_channel();
        *profile.host_tab_events.lock().await = Some(births);
        profile.state.lock().await.host = Some(crate::spawn::ActiveHost {
            pid: std::process::id(),
            port: 9,
            ring: Arc::new(ring),
            profile: dir.path().join("host-create"),
            input,
            reader: None,
            monitor: None,
        });
        let generation = profile.host_generation.load(Ordering::Acquire);
        let host = tokio::task::spawn_blocking(move || {
            if let Ok(line) = requests.recv_timeout(Duration::from_secs(10))
                && let Some(crate::input::HostInput::TabNew { request, .. }) =
                    crate::input::parse_host_line(&line)
            {
                events
                    .send((
                        generation,
                        crate::control::HostEvent::TabCreated {
                            tab: 1,
                            opener: None,
                            url: String::new(),
                            request,
                        },
                    ))
                    .expect("the birth is reported");
            }
        });

        let refused = service
            .call(&seat, "browser_tabs", json!({ "action": "new" }))
            .await
            .expect_err("a host no fresh driver can list is ended, not retried for ever");
        host.await.expect("the host's birth is answered");

        assert!(refused.contains("stopped answering"), "{refused}");
        assert!(profile.state.lock().await.host.is_none(), "the wedged host was taken down");
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **A late close from the old host leaves its successor's tab one
    /// alone.** A host death clears the focus and restarts tab numbers, so a
    /// close aimed at the old host must not unfocus the new one's tab.
    #[tokio::test]
    async fn a_close_from_the_old_host_leaves_the_successors_focus() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");
        let old_host = profile.host_generation.load(Ordering::Acquire);
        profile.host_generation.fetch_add(1, Ordering::AcqRel);

        profile.unfocus(old_host, 1);

        assert_eq!(
            *profile.focused.lock().unwrap_or_else(PoisonError::into_inner),
            Some(1),
            "a close from the old host does not unfocus the successor's tab one",
        );
    }

    /// **A finished watch is replaced by the next start.** The Gone path ends
    /// its task and leaves the handle behind; a later driver start must not
    /// read that handle as a watch still running.
    #[tokio::test(start_paused = true)]
    async fn a_finished_watch_is_replaced_by_the_next_start() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        let seen = fake.clone();
        service.install_fake(&seat, fake).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");

        // A host for watch_nav's port, and the finished handle the Gone path
        // leaves in the slot.
        let ring_name = format!("/forge-watch-{}", std::process::id());
        let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
        let (input, _input_rx) = std::sync::mpsc::channel();
        let finished = tokio::spawn(async {});
        while !finished.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut state = profile.state.lock().await;
        state.host = Some(crate::spawn::ActiveHost {
            pid: std::process::id(),
            port: 9,
            ring: Arc::new(ring),
            profile: dir.path().join("profile"),
            input,
            reader: None,
            monitor: None,
        });
        state.nav_watch = Some(finished);

        let before = seen.listings();
        profile.watch_nav(&mut state);
        assert!(
            state.nav_watch.as_ref().is_some_and(|watch| !watch.is_finished()),
            "a finished watch is replaced by a fresh one",
        );
        drop(state);

        let mut listed = false;
        for _ in 0..20 {
            if seen.listings() > before {
                listed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(listed, "the fresh watch lists where the finished one stopped");
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **A renamed page keeps the session's tab.** A fresh Playwright backend
    /// gives every page a new guid; the tab is re-paired by its CDP target
    /// rather than re-created as about:blank, so the number a tool printed is
    /// the number a hand-off accepts. (The fake's ids change; no driver is
    /// replaced in this test.)
    #[tokio::test]
    async fn a_renamed_page_keeps_the_sessions_tab() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        let seen = fake.clone();
        service.install_fake(&seat, fake).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");

        // A fresh driver against the same host: the same page, a new guid.
        seen.reidentify("P2");

        let parts = service
            .call(&seat, "browser_navigate", json!({ "url": "http://a/" }))
            .await
            .expect("the call drives the page the session already had");
        assert!(
            !parts.iter().any(|part| matches!(
                part,
                forge_primitives::browser::BrowserPart::Text { text } if text.starts_with("tab ")
            )),
            "no tab was created for this call: {parts:?}",
        );
        let rows = service.tabs(&seat).await.expect("rows").rows;
        assert_eq!(rows.len(), 1, "one tab, not a new one: {rows:?}");
        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).id(1),
            Some("P2"),
            "the registry follows the fresh driver's name for the same page",
        );
        service
            .hand_off_started(&seat, 1)
            .await
            .expect("the number a tool printed is the number a hand-off accepts");
        service.hand_off_finished(&seat, 1).await;
    }

    /// **The registry follows a fresh driver's names for the pages it still
    /// has.** The install-time repair is what keeps a listing or a hand-off
    /// answered between two page calls.
    #[tokio::test]
    async fn a_fresh_driver_re_pairs_the_registry_by_target() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        let seen = fake.clone();
        service.install_fake(&seat, fake).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");
        seen.reidentify("P2");

        let driver: Arc<dyn crate::driver::TabDriver> = Arc::new(seen);
        service.repair_tab_identity(&profile, &driver, crate::driver::CALL_TIMEOUT).await;

        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).id(1),
            Some("P2"),
            "the row is re-paired with the fresh driver's name",
        );
    }

    /// **A listing that times out is a dead driver, replaced on this call.**
    /// The first fake times out on EVERY listing, so a retry of it cannot
    /// pass: only the successor answering proves the driver was replaced.
    /// **A live wedge is not constructible in CI**; this path is pinned by
    /// the fake alone.
    #[tokio::test]
    async fn a_listing_timeout_replaces_the_driver_on_the_call() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let wedged = FakeDriver::with_blank("T1");
        let seen = wedged.clone();
        seen.timeout_every_listing();
        let successor = FakeDriver::with_blank("T1");
        let fresh = successor.clone();
        service
            .install_fake_with_successor(&seat, wedged, move || {
                Arc::new(successor.clone()) as Arc<dyn crate::driver::TabDriver>
            })
            .await;

        service
            .call(&seat, "browser_snapshot", json!({}))
            .await
            .expect("a timed-out listing is answered by a replacement driver on the same call");
        assert!(fresh.listings() > 0, "the successor answered the listing");
    }

    /// **A driver whose initialise times out is replaced on this call**, not
    /// left for a person or another agent to restart the browser. **A
    /// navigate is not repeated after the attach timeout** - though it was
    /// never delivered, the rule is per tool - and is handed back saying the
    /// call never reached the page and can be run again. **A live wedge is
    /// not constructible in CI**; the fake pins this path alone.
    #[tokio::test]
    async fn an_initialise_timeout_replaces_the_driver_and_hands_a_navigate_back() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let wedged = FakeDriver::with_blank("T1");
        let seen = wedged.clone();
        let successor = FakeDriver::with_blank("T1");
        let fresh = successor.clone();
        service
            .install_fake_with_successor(&seat, wedged, move || {
                Arc::new(successor.clone()) as Arc<dyn crate::driver::TabDriver>
            })
            .await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        seen.timeout_every_call();

        let handed = service
            .call(&seat, "browser_navigate", json!({ "url": "http://a/" }))
            .await
            .expect_err("a timed-out navigate is handed back, not repeated");
        // **An attach timeout never reached the page**, so the answer says
        // the call can be run again rather than warning it may have landed.
        assert!(handed.contains("never reached the page"), "{handed}");
        assert!(handed.contains("can be run again now"), "{handed}");
        assert!(fresh.listings() > 0, "the successor replaced the wedged driver");
    }

    /// **A row that cannot be paired is left open and the call refused.** A
    /// page whose guid and target both moved under the registry is not a
    /// closed tab; closing it is how a filled form is lost, and the orphan
    /// prune is held while the row is unresolved.
    #[tokio::test]
    async fn an_unpairable_tab_is_left_open_and_the_call_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        let seen = fake.clone();
        service.install_fake(&seat, fake).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");
        // The page navigated and the driver was replaced: neither name the
        // registry holds matches the listing.
        seen.rename("P2", "T2");
        let listing = seen.tabs.lock().expect("the tabs").clone();
        let drives = seen.selected().len();

        let refused = service
            .call(&seat, "browser_snapshot", json!({}))
            .await
            .expect_err("the call is refused rather than driving another page");
        assert!(refused.contains("left open"), "{refused}");
        assert_eq!(seen.selected().len(), drives, "the refused call drove nothing");
        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).target(1),
            Some("T1"),
            "the tab is still open with its own row",
        );
        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).unresolved_tabs(),
            vec![1],
            "the row is marked unresolved",
        );
        let tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            super::closed_as_orphans(&tabs, &listing, false).is_empty(),
            "an unresolved row holds the orphan prune",
        );
    }

    /// **A host a call ended is named by pid, port and profile**, and one a
    /// newer driver owns is not claimed.
    #[tokio::test]
    async fn a_dead_host_answer_names_the_host_and_the_next_call() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let profile = service.entry(&seat).await.expect("a profile entry");

        // **The host's own last words reach the agent**, not just the banner:
        // a bare initializeServer timeout names nothing (#2534).
        let empty = dead_host_answer(
            &profile,
            DeadHost::Ended { pid: 1234, port: 9411 },
            "a listing timed out",
        );
        assert!(empty.contains("pid 1234"), "{empty}");
        assert!(empty.contains("port 9411"), "{empty}");
        assert!(empty.contains("/cef"), "{empty}");
        assert!(empty.contains("The next call starts a fresh browser"), "{empty}");
        assert!(empty.contains("its log is empty"), "{empty}");

        std::fs::write(
            profile.dir.join("cef-host.log"),
            "cef host: live (pid 1234, port 9411)\nthe page never answered\n",
        )
        .expect("a host log");
        let ended = dead_host_answer(
            &profile,
            DeadHost::Ended { pid: 1234, port: 9411 },
            "a listing timed out",
        );
        assert!(ended.contains("the page never answered"), "{ended}");
        assert!(!ended.contains("its log is empty"), "{ended}");

        let gone = dead_host_answer(&profile, DeadHost::AlreadyGone, "a listing timed out");
        assert!(gone.contains("The next call starts a fresh browser"), "{gone}");
        let ours = dead_host_answer(&profile, DeadHost::NotOurs, "a listing timed out");
        assert!(ours.contains("stopped answering"), "{ours}");
        assert!(!ours.contains("pid"), "a host this call does not own is not claimed: {ours}");
    }

    /// **A host that was not ended is not reported as ended.** The record's
    /// pid is a number, and when another process answers on its port the end
    /// is refused; the answer must not tell the session the host was taken
    /// down.
    #[tokio::test]
    async fn a_host_that_was_not_ended_is_not_reported_as_ended() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        let profile = service.find("O/default").await.expect("the profile");
        // A process that answers as a browser on the recorded host's port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("the address").port();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let body = r#"{"Browser":"other"}"#;
                let _ = std::io::Write::write_all(
                    &mut stream,
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        let ring_name = format!("/forge-not-ended-{}", std::process::id());
        let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
        let (input, _input_rx) = std::sync::mpsc::channel();
        let generation = {
            let mut state = profile.state.lock().await;
            state.host = Some(crate::spawn::ActiveHost {
                pid: std::process::id(),
                port,
                ring: Arc::new(ring),
                profile: dir.path().join("host-not-ended"),
                input,
                reader: None,
                monitor: None,
            });
            state.driver_generation
        };
        let drove = super::HostIdentity { pid: std::process::id(), port, generation };

        let why = "a listing timed out";
        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let host = service.end_dead_host(&profile, drove, why).await;
        drop(guard);
        let answer = dead_host_answer(&profile, host, why);

        assert!(!answer.contains("was ended"), "the refused end is not claimed: {answer}");
        assert!(answer.contains("was not ended"), "{answer}");
        assert!(answer.contains(&format!("port {port}")), "the host is named: {answer}");
        let records = ended(&caught);
        assert_eq!(records.len(), 1, "one end, one record: {records:?}");
        let (_, fields) = &records[0];
        assert!(
            fields.contains("outcome=refused"),
            "the record says the signal was refused: {fields}"
        );
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **A delayed failure does not take a successor's host.** A host is
    /// matched by its pid and port: one started after the call looked its
    /// browser up is not the host that call drove, whoever installed it.
    /// Matching on `driver_generation` let a successor be taken and its
    /// process leaked (#2573).
    #[tokio::test]
    async fn a_late_failure_does_not_take_a_successors_host() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");
        // The successor host serving the profile now.
        let ring_name = format!("/forge-successor-{}", std::process::id());
        let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
        let (input, _input_rx) = std::sync::mpsc::channel();
        let successor = super::HostIdentity { pid: std::process::id(), port: 9, generation: 0 };
        profile.state.lock().await.host = Some(crate::spawn::ActiveHost {
            pid: successor.pid,
            port: successor.port,
            ring: Arc::new(ring),
            profile: dir.path().join("successor"),
            input,
            reader: None,
            monitor: None,
        });

        // **Its boot marker is the successor's too**, not this call's to take.
        *profile.boot.lock().unwrap_or_else(PoisonError::into_inner) = Some((1, "T1".to_owned()));
        let stale =
            super::HostIdentity { pid: successor.pid + 1, port: successor.port + 1, generation: 0 };
        let host = service.end_dead_host(&profile, stale, "a delayed timeout").await;

        assert!(matches!(host, DeadHost::NotOurs), "the successor is not this call's to end");
        assert!(profile.state.lock().await.host.is_some(), "the successor host is still there");
        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).numbers(),
            vec![1],
            "and its tab registry was not wiped",
        );
        assert_eq!(
            *profile.boot.lock().unwrap_or_else(PoisonError::into_inner),
            Some((1, "T1".to_owned())),
            "and its boot marker still waits for its first call",
        );

        // **A successor usually reuses the freed port and differs only by
        // pid**: the pid alone must be enough to refuse it.
        let same_port =
            super::HostIdentity { pid: successor.pid + 1, port: successor.port, generation: 0 };
        let host = service.end_dead_host(&profile, same_port, "a delayed timeout").await;
        assert!(matches!(host, DeadHost::NotOurs), "a same-port successor is not this call's");
        assert!(profile.state.lock().await.host.is_some(), "and it is still serving");
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **A newer driver on the same host is not this call's to end either.**
    /// The identity carries the driver generation the call drove: an install
    /// that landed after it makes the host NotOurs, and the newer driver is
    /// left in place (#2573's first half).
    #[tokio::test]
    async fn a_late_failure_with_a_newer_driver_keeps_the_host() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");
        let ring_name = format!("/forge-newer-driver-{}", std::process::id());
        let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
        let (input, _input_rx) = std::sync::mpsc::channel();
        profile.state.lock().await.host = Some(crate::spawn::ActiveHost {
            pid: std::process::id(),
            port: 9,
            ring: Arc::new(ring),
            profile: dir.path().join("newer-driver"),
            input,
            reader: None,
            monitor: None,
        });
        // The call looked the browser up at generation 0; a successor driver
        // is installed on the same host since.
        let drove = super::HostIdentity { pid: std::process::id(), port: 9, generation: 0 };
        profile.state.lock().await.driver_generation = 7;

        let host = service.end_dead_host(&profile, drove, "a delayed timeout").await;

        assert!(matches!(host, DeadHost::NotOurs), "a newer driver's host is not this call's");
        assert!(profile.state.lock().await.host.is_some(), "the host is still serving");
        assert!(profile.state.lock().await.driver.is_some(), "and its newer driver is untouched");
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **The repair's re-pair clears the unresolved flag, so the prune runs
    /// again.** A transient listing miss holds the orphan prune; re-pairing
    /// the row by its target releases it.
    #[tokio::test]
    async fn a_re_paired_row_lets_the_prune_run() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        let seen = fake.clone();
        service.install_fake(&seat, fake).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");
        seen.rename("P2", "T2");
        service
            .call(&seat, "browser_snapshot", json!({}))
            .await
            .expect_err("an unpairable row refuses the call");
        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).unresolved_tabs(),
            vec![1],
            "the row is unresolved and holds the prune",
        );

        // The target names it again, and a page no tab claims is listed.
        seen.rename("P3", "T1");
        seen.push_target("T9");
        let driver: Arc<dyn crate::driver::TabDriver> = Arc::new(seen.clone());
        service.repair_tab_identity(&profile, &driver, crate::driver::CALL_TIMEOUT).await;

        let tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(tabs.id(1), Some("P3"), "the repair re-paired the row by target");
        assert!(tabs.unresolved_tabs().is_empty(), "and cleared its flag");
        let listing = seen.tabs.lock().expect("the tabs").clone();
        assert_eq!(
            super::closed_as_orphans(&tabs, &listing, false),
            vec!["T9".to_owned()],
            "the prune runs again",
        );
    }

    /// **A driver installed since this call's lookup is not this call's to
    /// clear.** The guard is inside the rebuild lock, so the newer driver is
    /// handed back untouched and its successor stands untouched too.
    #[tokio::test]
    async fn a_replacement_for_an_old_generation_keeps_the_newer_driver() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let current = FakeDriver::with_blank("T1");
        let successor = FakeDriver::with_blank("P9");
        let replacement = successor.clone();
        service
            .install_fake_with_successor(&seat, current, move || {
                Arc::new(successor.clone()) as Arc<dyn crate::driver::TabDriver>
            })
            .await;
        let profile = service.find("O/default").await.expect("the profile");
        profile.state.lock().await.driver_generation = 5;

        let (kept, _generation) = service
            .recover_driver(&profile, 4, crate::driver::CALL_TIMEOUT)
            .await
            .expect("the current driver is handed back");

        let rows = kept.tabs_within(Duration::from_secs(1)).await.expect("the fake lists");
        assert_eq!(rows[0].id, "T1", "the current driver, not the successor");
        assert_eq!(replacement.listings(), 0, "the successor was never installed");
    }

    /// **Closing the focused tab lets the focus go.** A focus left on it
    /// would keep the history watch driving a page that no longer exists.
    #[tokio::test]
    async fn closing_the_focused_tab_clears_the_focus() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("O/default").await.expect("the profile");
        assert_eq!(
            *profile.focused.lock().unwrap_or_else(PoisonError::into_inner),
            Some(1),
            "the call focused its tab",
        );

        service.call(&seat, "browser_close", json!({})).await.expect("the tab closes");

        assert_eq!(
            *profile.focused.lock().unwrap_or_else(PoisonError::into_inner),
            None,
            "a closed tab is not the one the history watch follows",
        );
    }

    /// **Each tab's arrows are its own.** A history published for tab 1 is
    /// not the state a viewer on tab 2 follows: one profile-wide watch showed
    /// the focused tab's history on every viewer's bar.
    #[tokio::test]
    async fn a_tabs_history_is_its_own_watch() {
        use forge_primitives::browser::BrowserNavState;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let one = seed_tab(&profile, &seat);
        profile
            .tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .adopt(2, seat.clone(), "p2", "t2", "two", "http://b/")
            .expect("a second tab");
        let first = profile.nav_receiver(one);
        let second = profile.nav_receiver(2);
        let moved = BrowserNavState { can_back: true, can_forward: false };
        profile.publish_nav(one, moved);

        assert_eq!(*first.borrow(), Some(moved), "tab 1's bar follows tab 1");
        assert_eq!(*second.borrow(), None, "tab 2's bar is not tab 1's");
    }

    /// **A tab a person is driving reads in use in the strip.** The spec's
    /// in-use is a call or the person's control; the release makes it idle
    /// again.
    #[tokio::test]
    async fn a_controlled_tab_reads_in_use() {
        use forge_primitives::browser::BrowserTabState;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        let rows = service.tabs(&seat).await.expect("rows").rows;
        assert_eq!(rows[0].state, BrowserTabState::Idle, "idle before a person takes it");

        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        let rows = service.tabs(&seat).await.expect("rows").rows;
        assert_eq!(rows[0].state, BrowserTabState::InUse, "the person's control is a use");

        service.release_control("TestOrg/default", tab, 7).await;
        let rows = service.tabs(&seat).await.expect("rows").rows;
        assert_eq!(rows[0].state, BrowserTabState::Idle, "the release leaves it idle");
    }

    /// **The driver's chooser goes to the controller, and waits for the
    /// agent when nobody holds the role.** Its ids never share CEF's small
    /// counter, and a role that moves takes the ask with it.
    #[tokio::test]
    async fn a_driver_chooser_goes_to_the_controller_and_is_held_without_one() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);

        profile
            .file_chooser(
                FileChooser { multiple: true, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(
            profile.files.borrow().is_none(),
            "no controller: the chooser is held for the agent's upload",
        );
        assert!(
            profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "and no person is asked",
        );

        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        let ask = profile
            .files
            .borrow()
            .as_ref()
            .copied()
            .expect("the controller is asked for the chooser");
        assert!(!ask.multiple, "the page's own mode");
        assert!(ask.id >= 1 << 32, "a driver ask never shares CEF's ids: {}", ask.id);
        assert_eq!(
            *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner),
            Some(ask.id),
        );

        service.release_control("TestOrg/default", tab, 7).await;
        assert!(profile.files.borrow().is_none(), "the ask leaves with the role it was asked for");
        assert!(
            profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "and the driver's chooser is not answered again",
        );
    }

    /// **A chooser from a replaced driver is not published.** Its ask's id
    /// belongs to a driver that no longer exists, and answering it would
    /// cancel the next real chooser as that stale id. The generation is the
    /// one the reader captured when the event arrived, checked under the
    /// decision lock.
    #[tokio::test]
    async fn a_chooser_from_a_replaced_driver_is_not_published() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role");
        // The profile's installed driver is generation 2; the chooser below
        // was read from the driver generation 1 replaced.
        profile.state.lock().await.driver_generation = 2;

        let chooser =
            || FileChooser { multiple: false, current: true, url: "http://x/".to_owned() };
        profile.file_chooser(chooser(), 1, tab).await;
        assert!(profile.files.borrow().is_none(), "the replaced driver's chooser is not asked");
        profile.file_chooser(chooser(), 2, tab).await;
        let ask = profile
            .files
            .borrow()
            .as_ref()
            .copied()
            .expect("the installed driver's chooser is asked");
        assert_eq!(ask.tab, tab, "and it names the tab captured with it");
    }

    /// **An answer for the chooser the newer ask replaced is refused**, and
    /// the newer ask stands. The answer's check and its driver call are one
    /// critical section, so the in-flight answer cannot upload the older
    /// picker's files to the newer chooser.
    #[tokio::test]
    async fn a_person_answer_for_a_superseded_ask_is_refused_and_leaves_the_newer() {
        use crate::driver::FileChooser;
        use forge_primitives::browser::BrowserUpload;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");

        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        let first = profile.files.borrow().as_ref().copied().expect("the first ask");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        let second = profile.files.borrow().as_ref().copied().expect("the second ask");
        assert_ne!(first.id, second.id, "the second chooser is a new ask");

        let refused = service
            .answer_files(
                "TestOrg/default",
                tab,
                first.id,
                vec![BrowserUpload { name: "old.txt".to_owned(), bytes: b"old".to_vec() }],
            )
            .await
            .expect_err("an answer for the superseded ask is refused");
        assert!(refused.contains("not outstanding"), "{refused}");
        assert_eq!(
            profile.files.borrow().as_ref().map(|ask| ask.id),
            Some(second.id),
            "the newer ask stands",
        );
        assert_eq!(
            *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner),
            Some(second.id),
            "and it is still the driver's chooser",
        );
    }

    /// **An agent's upload is refused while the person's picker is open**,
    /// and the release closes that refusal. The refusal check and the
    /// driver call are one critical section, so a chooser cannot surface
    /// between them and be answered by the agent.
    #[tokio::test]
    async fn an_agent_upload_is_refused_while_the_person_is_choosing() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;

        assert!(agent_upload_refused(&profile), "the person's picker is open");
        service.release_control("TestOrg/default", tab, 7).await;
        assert!(!agent_upload_refused(&profile), "the release closes the refusal");
    }

    /// **A chooser the agent already answered is not put to the person.**
    /// Its driver event can arrive after the upload; one event is consumed
    /// for that answer instead of publishing an ask for a chooser that is
    /// already gone.
    #[tokio::test]
    async fn a_chooser_the_agent_answered_before_its_event_is_not_put_to_the_person() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");

        note_agent_answered(&profile);
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(profile.files.borrow().is_none(), "the owed event is consumed, not asked");
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0));
        }

        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(profile.files.borrow().is_some(), "the next chooser is the person's");
    }

    /// **An agent's answer for a chooser whose event was already read leaves
    /// no credit behind**: the surface it consumed is gone, so the next
    /// chooser is still asked rather than eaten by a stale credit.
    #[tokio::test]
    async fn an_agent_answer_for_a_surfaced_chooser_leaves_no_credit() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);

        // No controller: the chooser is surfaced and held for the agent.
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!(ledger.surfaced, 1, "the event was read and is the agent's to answer");
        }
        note_agent_answered(&profile);
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0), "the surface is consumed");
        }

        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(
            profile.files.borrow().is_some(),
            "the next chooser is asked, not eaten by a stale credit",
        );
    }

    /// **Every `browser_file_upload` is a chooser decision**, including the
    /// schema's cancel form with no paths, so the lock, the refusal and the
    /// ledger cover it.
    #[test]
    fn every_browser_file_upload_is_a_chooser_decision() {
        assert!(matches!(upload_request(&json!({})), Ok(UploadRequest::Cancel)));
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("cv.pdf");
        std::fs::write(&path, b"hi").expect("a file");
        assert!(matches!(
            upload_request(&json!({ "paths": [path.to_str().expect("a path")] })),
            Ok(UploadRequest::Answer(_)),
        ));
    }

    /// **The cancel form is gated like an answer**: it answers the captured
    /// chooser, so the person's open picker refuses it too.
    #[tokio::test]
    async fn a_cancel_is_refused_while_the_person_is_choosing() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;

        let refused = begin_upload(&profile, Some(&UploadRequest::Cancel))
            .await
            .expect_err("the cancel form is refused too");
        assert!(refused.contains("picker"), "{refused}");
        service.release_control("TestOrg/default", tab, 7).await;
        let guard = begin_upload(&profile, Some(&UploadRequest::Cancel))
            .await
            .expect("the cancel form is allowed")
            .expect("and it holds the decision lock");
        assert!(
            profile.file_decision.try_lock().is_err(),
            "the cancel form's driver call runs inside the decision lock",
        );
        drop(guard);
        assert!(
            begin_upload(&profile, None).await.expect("not an upload").is_none(),
            "a call that is not an upload is not gated",
        );
    }

    /// **The decision lock spans the driver call.** A second task cannot
    /// answer while the first is parked inside the gated driver call; when
    /// the call finishes, the answer runs and is refused as superseded.
    #[tokio::test]
    async fn the_decision_lock_spans_the_driver_call() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        let gate = Arc::new(GatedAnswer { entered: Notify::new(), release: Notify::new() });
        let calls: Arc<dyn ChooserAnswer> = gate.clone();
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        let first = profile.files.borrow().as_ref().copied().expect("the first ask");

        let superseding = {
            let profile = Arc::clone(&profile);
            tokio::spawn(async move {
                profile
                    .file_chooser(
                        FileChooser { multiple: false, current: true, url: "http://y/".to_owned() },
                        0,
                        0,
                    )
                    .await;
            })
        };
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .expect("the gated driver call starts");

        let mut answering = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service.answer_files("TestOrg/default", tab, first.id, Vec::new()).await
            })
        };
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut answering).await.is_err(),
            "the answer waits on the decision lock while the driver call runs",
        );

        gate.release.notify_waiters();
        superseding.await.expect("the superseding chooser joins");
        let refused = answering.await.expect("the answer joins");
        assert!(refused.is_err(), "the superseded answer is refused once the lock frees");
        assert_ne!(
            profile.files.borrow().as_ref().map(|ask| ask.id),
            Some(first.id),
            "the first ask is gone",
        );
    }

    /// **A teardown while a chooser waits leaves no ask.** The teardown
    /// marks the profile down and clears the chooser without the decision
    /// lock; the waiting chooser, released afterwards, must not publish.
    #[tokio::test]
    async fn a_teardown_while_a_chooser_waits_leaves_no_ask() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        let gate = Arc::new(GatedAnswer { entered: Notify::new(), release: Notify::new() });
        let calls: Arc<dyn ChooserAnswer> = gate.clone();
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;

        let waiting = {
            let profile = Arc::clone(&profile);
            tokio::spawn(async move {
                profile
                    .file_chooser(
                        FileChooser { multiple: false, current: true, url: "http://y/".to_owned() },
                        0,
                        0,
                    )
                    .await;
            })
        };
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .expect("the gated driver call starts");

        let mut state = State::default();
        profile.teardown(&mut state, Some(HostEnd::Closed), "").await;
        gate.release.notify_waiters();
        waiting.await.expect("the waiting chooser joins");

        assert!(
            profile.files.borrow().is_none(),
            "the waiting chooser does not publish after the teardown",
        );
        assert!(profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),);
        let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0));
    }

    /// **A drain reconciles the ledger it consumed**, so a stale surface
    /// cannot feed a later upload a phantom credit.
    #[tokio::test]
    async fn a_drain_reconciles_the_ledger() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(profile.files.borrow().is_some());
        // Two more surfaces than the open ask: a drain that consumed a
        // constant one would leave them behind and fail the assertion below.
        note_surfaced_opened(&profile, 2);

        service.release_control("TestOrg/default", tab, 7).await;
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0), "the drain consumed it");
        }

        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves back");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://y/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(
            profile.files.borrow().is_some(),
            "a later chooser is asked, not eaten by a stale credit",
        );
    }

    /// **A lagged reader counts its skipped choosers**, so an agent's answer
    /// consumes one instead of leaving a credit that eats the next ask.
    #[tokio::test]
    async fn a_lagged_reader_counts_its_skipped_choosers() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);

        // The reader is one behind: a capacity-one channel, two sends and one
        // read, which is exactly the lag the listener task sees.
        let (sender, mut receiver) = tokio::sync::broadcast::channel::<FileChooser>(1);
        let _ = sender.send(FileChooser {
            multiple: false,
            current: true,
            url: "http://a/".to_owned(),
        });
        let _ = sender.send(FileChooser {
            multiple: false,
            current: true,
            url: "http://b/".to_owned(),
        });
        let skipped = match receiver.recv().await {
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => skipped,
            other => panic!("a full channel lags, got {other:?}"),
        };
        profile.chooser_lagged(skipped).await;

        let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(ledger.surfaced, 1, "the skipped chooser is counted");
        assert_eq!(ledger.pre_answered, 0);
    }

    /// **A successful answer consumes its surface** in the ledger, and the
    /// answer's ask and driver id are cleared.
    #[tokio::test]
    async fn a_successful_answer_consumes_its_surface() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                tab,
            )
            .await;
        let ask = profile.files.borrow().as_ref().copied().expect("the ask");

        service
            .answer_files("TestOrg/default", tab, ask.id, Vec::new())
            .await
            .expect("the answer lands");

        let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0), "the answer consumed it");
        drop(ledger);
        assert!(profile.files.borrow().is_none());
        assert!(profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none());
    }

    /// **An agent's cancel consumes a surface like an answer**, and a failed
    /// call consumes nothing.
    #[tokio::test]
    async fn an_agent_cancel_consumes_a_surface() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");

        note_surfaced_opened(&profile, 1);
        note_upload_finished(&profile, &UploadRequest::Cancel, false);
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!(ledger.surfaced, 1, "a failed call does not consume");
        }
        note_upload_finished(&profile, &UploadRequest::Cancel, true);
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!(ledger.surfaced, 0, "the cancel consumes its surface");
        }
    }

    /// **A dead driver's clear empties the chooser state**, under the locks
    /// its caller holds.
    #[tokio::test]
    async fn a_clear_chooser_state_empties_the_ask_and_the_ledger() {
        use forge_primitives::browser::BrowserFileAsk;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, tab: 1, multiple: false }));
        *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(5);
        *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger { surfaced: 2, pre_answered: 1 };
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);

        {
            let _decision = profile.file_decision.lock().await;
            let _state = profile.state.lock().await;
            profile.clear_chooser_state();
        }

        assert!(profile.files.borrow().is_none(), "the ask is cleared");
        assert!(
            profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "the driver's id is cleared",
        );
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0));
        }
        assert!(
            profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "the dead driver's handle is cleared",
        );
    }

    /// **A dead driver's clear spares a newer driver.** The generation is
    /// bumped on install, so a clear aimed at the old one leaves the newer
    /// generation's ask and handle alone.
    #[tokio::test]
    async fn a_dead_driver_s_clear_spares_a_newer_driver() {
        use forge_primitives::browser::BrowserFileAsk;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, tab: 1, multiple: false }));
        *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(5);
        *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger { surfaced: 1, pre_answered: 0 };
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);

        // A newer driver installed after the call's lookup: generation 2.
        let mut state = State { driver_generation: 2, ..Default::default() };
        assert!(
            !clear_if_dead(&mut state, 1, false, &profile),
            "a newer generation is not the dead one",
        );
        assert!(profile.files.borrow().is_some(), "the newer driver's ask survives");
        assert!(
            profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).is_some(),
            "and its handle survives",
        );

        // The same generation, dead: cleared. A live driver is left alone.
        let mut state = State { driver_generation: 1, ..Default::default() };
        assert!(clear_if_dead(&mut state, 1, false, &profile), "the dead generation is cleared");
        assert!(profile.files.borrow().is_none());
        assert!(profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).is_none());
        assert!(!clear_if_dead(&mut state, 1, true, &profile), "a live driver is not cleared");
    }

    /// **The generation a call captures is the one installed at its lookup**,
    /// and an install takes the next one, so a clear aimed at the old
    /// generation can never match the driver that replaced it.
    #[test]
    fn a_capture_and_an_install_are_adjacent_generations() {
        let mut state = State { driver_generation: 7, ..Default::default() };
        assert_eq!(captured_generation(&state), 7, "a call carries the installed generation");
        assert_eq!(installed_generation(&state), 8, "an install moves one past it");
        state.driver_generation = u64::MAX;
        assert_eq!(installed_generation(&state), 0, "the counter wraps rather than panicking");
    }

    /// **A dead page's rebuild is guarded by the generation the call drove.**
    /// A newer driver's ask and handle survive an old call's rebuild; the
    /// same generation's rebuild clears them through the chooser decision,
    /// and a failed rebuild starts the cooldown rather than another attempt.
    #[tokio::test]
    async fn a_rebuild_is_guarded_by_the_generation_the_call_drove() {
        use forge_primitives::browser::BrowserFileAsk;

        let dead = "browserBackend.callTool: Cannot read properties of undefined (reading '_page')";
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, tab: 1, multiple: false }));
        *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(5);
        *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger { surfaced: 1, pre_answered: 0 };
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        // A newer driver is installed: generation 2.
        profile.state.lock().await.driver_generation = 2;

        let why = service
            .after_failure(
                &profile,
                Driven { generation: 1, pid: 1, port: 9, tab: 1, bound: Duration::from_secs(150) },
                Call {
                    tool: "browser_evaluate",
                    reported: "browser_challenge_state",
                    args: &json!({}),
                },
                Err(dead.to_owned()),
            )
            .await
            .expect_err("a stale failure is not this call's to rebuild");
        assert!(
            why.contains("browser_challenge_state") && !why.contains("browser_evaluate"),
            "a stale call is handed back as not repeated, under the caller's name: {why}",
        );
        assert!(profile.files.borrow().is_some(), "the newer driver's ask survives");
        assert!(
            profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).is_some(),
            "and its handle survives",
        );

        // The same generation: the rebuild clears the dead driver's chooser
        // state through the decision lock, then cannot start a driver because
        // the profile is closed; the cooldown holds the next attempt off.
        profile.ended.store(true, Ordering::Release);
        let why = service
            .after_failure(
                &profile,
                Driven { generation: 2, pid: 1, port: 9, tab: 1, bound: Duration::from_secs(150) },
                Call { tool: "browser_click", reported: "browser_click", args: &json!({}) },
                Err(dead.to_owned()),
            )
            .await
            .expect_err("a closed profile cannot rebuild");
        assert!(why.contains("could not be rebuilt"), "{why}");
        assert!(profile.files.borrow().is_none(), "the dead driver's ask is cleared");
        assert!(
            profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "and its handle with it",
        );
        let again = service
            .after_failure(
                &profile,
                Driven { generation: 2, pid: 1, port: 9, tab: 1, bound: Duration::from_secs(150) },
                Call { tool: "browser_click", reported: "browser_click", args: &json!({}) },
                Err(dead.to_owned()),
            )
            .await
            .expect_err("the cooldown refuses a second rebuild");
        assert!(again.contains("not been rebuilt a second time yet"), "{again}");
    }

    /// **A composed tool drives as the evaluate the driver has, and its
    /// failures still name what the session called.** A report reading
    /// "browser_evaluate may already have landed" would name a tool the
    /// session never used, so the two names are pinned here.
    #[test]
    fn a_composed_tool_drives_as_an_evaluate_and_reports_its_caller() {
        let composed = super::driver_call("browser_challenge_state", json!({}))
            .expect("the challenge probe composes");
        assert_eq!(composed.tool, "browser_evaluate");
        assert_eq!(composed.reported, "browser_challenge_state");
        assert!(
            composed.args["function"]
                .as_str()
                .is_some_and(|script| script.contains("challengePresent")),
            "the driver is handed the challenge script: {}",
            composed.args,
        );

        let plain = super::driver_call("browser_click", json!({ "target": "e1" }))
            .expect("a plain call composes");
        assert_eq!(plain.tool, "browser_click");
        assert_eq!(plain.reported, "browser_click");
        assert_eq!(
            plain.args,
            json!({ "target": "e1" }),
            "a plain call's arguments ride untouched"
        );

        // **A host-answered tool is a snippet over the driver's own run_code**,
        // and the caller's name rides beside it so a failure names the tool a
        // session called rather than the one underneath.
        let snippet = super::driver_call("browser_form_state", json!({}))
            .expect("a host-answered tool composes");
        assert_eq!(snippet.tool, "browser_run_code_unsafe");
        assert_eq!(snippet.reported, "browser_form_state");
        assert!(
            snippet.args["code"].as_str().is_some_and(|code| code.contains("elementHandles")),
            "the driver is handed the form snippet: {}",
            snippet.args,
        );

        // The call's own shape fault is decided here, before any page.
        assert!(
            super::driver_call("browser_click", json!({ "force": true })).is_err(),
            "a forced click with no target is refused before a tab is consulted",
        );

        // A read is safe to repeat after a dead page and after a timeout.
        assert!(super::repeatable("browser_evaluate", "browser_challenge_state", &json!({})));
        assert!(super::repeatable_after_timeout(
            "browser_evaluate",
            "browser_challenge_state",
            &json!({})
        ));
        assert!(
            !super::repeatable("browser_evaluate", "browser_evaluate", &json!({})),
            "an evaluate a session sent is not assumed repeatable",
        );

        // **A form read cannot land twice either**, and the click-and-capture
        // tool is a click underneath, so it stays out of both lists.
        assert!(super::repeatable("browser_run_code_unsafe", "browser_form_state", &json!({})));
        assert!(super::repeatable_after_timeout(
            "browser_run_code_unsafe",
            "browser_form_state",
            &json!({})
        ));
        assert!(
            !super::repeatable("browser_run_code_unsafe", "browser_click_and_capture", &json!({})),
            "a capture clicks, so it is not repeated after a dead page",
        );
        assert!(!super::repeatable_after_timeout(
            "browser_run_code_unsafe",
            "browser_click_and_capture",
            &json!({})
        ));

        // The driver's own words name the caller's tool in what the session reads.
        let scrubbed = super::reported_error(
            "the driver did not answer browser_evaluate within 150 s".to_owned(),
            "browser_evaluate",
            "browser_challenge_state",
        );
        assert!(scrubbed.contains("browser_challenge_state"), "{scrubbed}");
        assert!(!scrubbed.contains("browser_evaluate"), "{scrubbed}");
    }

    /// **The rebuild reads the generation after both locks**, so a driver
    /// installed while it waited on the file decision is seen and its ask and
    /// handle are left alone. This is the case the second lock order exists
    /// for; a check taken before the wait cannot pass it.
    #[tokio::test]
    async fn a_rebuild_reads_the_generation_after_it_holds_both_locks() {
        use forge_primitives::browser::BrowserFileAsk;

        let dead = "browserBackend.callTool: Cannot read properties of undefined (reading '_page')";
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, tab: 1, multiple: false }));
        *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(5);
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        profile.state.lock().await.driver_generation = 1;

        // The rebuild parks on the file decision this test holds; while it
        // waits, a newer driver installs.
        let held = profile.file_decision.lock().await;
        let bump = async {
            tokio::task::yield_now().await;
            profile.state.lock().await.driver_generation = 2;
            drop(held);
        };
        let no_args = json!({});
        let (outcome, ()) = tokio::join!(
            service.after_failure(
                &profile,
                Driven { generation: 1, pid: 1, port: 9, tab: 1, bound: Duration::from_secs(150) },
                Call { tool: "browser_click", reported: "browser_click", args: &no_args },
                Err(dead.to_owned()),
            ),
            bump,
        );
        let why = outcome.expect_err("the rebuild waited and then saw the newer generation");
        assert!(
            why.contains("browser_click") && why.contains("not repeated"),
            "the newer generation hands the call back as not repeated: {why}",
        );
        assert!(profile.files.borrow().is_some(), "the newer driver's ask survives");
        assert!(
            profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).is_some(),
            "and its handle survives",
        );
    }

    /// **A background chooser consumes an owed event.** The agent may have
    /// answered the chooser before its tab lost currentness; the credit is
    /// consumed there so the next real chooser is still asked.
    #[tokio::test]
    async fn a_background_chooser_consumes_an_owed_event() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger { surfaced: 0, pre_answered: 1 };

        profile
            .file_chooser(
                FileChooser {
                    multiple: false,
                    current: false,
                    url: "http://background/".to_owned(),
                },
                0,
                0,
            )
            .await;
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!(
                (ledger.surfaced, ledger.pre_answered),
                (0, 0),
                "the owed event is consumed"
            );
        }
        assert!(profile.files.borrow().is_none(), "the background chooser is not asked");

        profile
            .file_chooser(
                FileChooser {
                    multiple: false,
                    current: true,
                    url: "http://foreground/".to_owned(),
                },
                0,
                0,
            )
            .await;
        assert!(
            profile.files.borrow().is_some(),
            "the next real chooser is asked, not eaten by the stale credit",
        );
    }

    /// **A detach takes the open chooser with it**, and a chooser raised
    /// after the role is gone is held for the agent rather than published
    /// against a controller who left.
    #[tokio::test]
    async fn a_detach_takes_the_open_chooser_with_it() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(profile.files.borrow().is_some());

        service.detach(7).await;
        assert!(profile.files.borrow().is_none(), "the open ask leaves with the controller");
        assert!(
            profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "and the driver's chooser is not answered again",
        );

        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(
            profile.files.borrow().is_none(),
            "a chooser after the detach is held for the agent",
        );
    }

    /// **A background tab's chooser is not put to the person**: the upload
    /// answers the current tab, so asking for another tab's chooser would
    /// send the person to a picker that cannot be answered.
    #[tokio::test]
    async fn a_background_tabs_chooser_is_not_asked() {
        use crate::driver::FileChooser;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");

        profile
            .file_chooser(
                FileChooser { multiple: false, current: false, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(profile.files.borrow().is_none(), "the background tab is not asked");
        assert!(profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),);
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0), "and it is not counted");
        }

        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                0,
            )
            .await;
        assert!(profile.files.borrow().is_some(), "the current tab's chooser is asked");
    }

    /// **The driver's ask is answered through the driver**, not the CEF
    /// dialog: with no driver up the refusal names the driver, which is the
    /// branch the answer took.
    #[tokio::test]
    async fn a_driver_ask_is_answered_through_the_driver() {
        use crate::driver::FileChooser;
        use forge_primitives::browser::BrowserUpload;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        profile.up.store(true, Ordering::Release);
        service.take_control("TestOrg/default", tab, 7).await.expect("the role moves");
        profile
            .file_chooser(
                FileChooser { multiple: false, current: true, url: "http://x/".to_owned() },
                0,
                tab,
            )
            .await;
        let ask = profile.files.borrow().as_ref().copied().expect("the ask");

        let refused = service
            .answer_files(
                "TestOrg/default",
                tab,
                ask.id,
                vec![BrowserUpload { name: "cv.pdf".to_owned(), bytes: b"hi".to_vec() }],
            )
            .await
            .expect_err("no driver is up to answer it");
        assert!(
            refused.contains("browser driver"),
            "the driver's chooser is routed to the driver: {refused}",
        );
    }

    /// **A teardown clears a stale chooser**: a host or driver that goes
    /// cannot leave an ask or a ledger entry for the next browser to answer.
    #[tokio::test]
    async fn a_teardown_clears_a_stale_chooser() {
        use forge_primitives::browser::BrowserFileAsk;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, tab: 1, multiple: false }));
        *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(5);
        *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger { surfaced: 1, pre_answered: 0 };

        let mut state = State::default();
        profile.teardown(&mut state, Some(HostEnd::Closed), "").await;

        assert!(profile.files.borrow().is_none(), "the ask is cleared");
        assert!(
            profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "the driver's chooser is cleared",
        );
        let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0));
    }

    /// **A first take on a fresh profile opens its own file.** The
    /// recordings directory is the profile's, so a fresh profile has none
    /// until its first take; without making it, AVAssetWriter refuses with
    /// "Cannot create file" on every real profile.
    #[tokio::test]
    async fn a_first_take_writes_into_its_profiles_own_recordings_directory() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().join("browser"), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        let profile = service.entry(&seat).await.expect("a profile entry");
        seed_tab(&profile, &seat);

        // A host that runs the real writer: the answer the take waits on is
        // AVFoundation's own.
        let (input, lines) = std::sync::mpsc::channel();
        let answers = profile.recorded.clone();
        let pixels = vec![
            0_u8;
            usize::try_from(crate::record::WIDTH * crate::record::HEIGHT * 4)
                .expect("a frame")
        ];
        let slot: crate::record::FrameSlot = Arc::new(Mutex::new(Some(Arc::new(pixels))));
        let ring_name = format!("/forge-first-take-{}", std::process::id());
        let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
        profile.state.lock().await.host = Some(crate::spawn::ActiveHost {
            pid: std::process::id(),
            port: 9,
            ring: Arc::new(ring),
            profile: profile.dir.clone(),
            input,
            reader: None,
            monitor: None,
        });
        let host = std::thread::spawn(move || {
            let mut recorder = None;
            while let Ok(line) = lines.recv() {
                match crate::input::parse_host_line(&line) {
                    Some(crate::input::HostInput::RecordStart { path, .. }) => {
                        let state =
                            match crate::record::Recorder::start(path.into(), Arc::clone(&slot)) {
                                Ok(started) => {
                                    recorder = Some(started);
                                    super::RecordingState {
                                        started: true,
                                        seconds: 0,
                                        bytes: 0,
                                        capped: false,
                                        error: None,
                                    }
                                }
                                Err(why) => super::RecordingState {
                                    started: false,
                                    seconds: 0,
                                    bytes: 0,
                                    capped: false,
                                    error: Some(why),
                                },
                            };
                        answers.send_replace(Some(state));
                    }
                    Some(crate::input::HostInput::RecordStop) => {
                        let stopped = recorder.take().map(crate::record::Recorder::stop);
                        let outcome = stopped.unwrap_or(crate::record::Outcome {
                            seconds: 0,
                            bytes: 0,
                            capped: false,
                            error: None,
                        });
                        answers.send_replace(Some(super::RecordingState {
                            started: false,
                            seconds: outcome.seconds,
                            bytes: outcome.bytes,
                            capped: outcome.capped,
                            error: outcome.error,
                        }));
                        break;
                    }
                    _ => {}
                }
            }
        });

        let started = service
            .call(&seat, "browser_record_start", json!({}))
            .await
            .expect("the first take starts");
        assert!(
            matches!(
                started.as_slice(),
                [forge_primitives::browser::BrowserPart::Text { text }]
                    if text.starts_with("Recording the page into")
            ),
            "the take reports that it started: {started:?}"
        );
        let path = profile
            .recording
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .expect("a live take")
            .1;
        let own = service.store.org_profile(&service.profile_for(&seat).expect("the seat's key"));
        assert!(
            path.starts_with(&own),
            "the take lands under the profile's own directory: {}",
            path.display()
        );
        assert!(path.is_file(), "the writer opened the file at {}", path.display());

        service.call(&seat, "browser_record_stop", json!({})).await.expect("the take stops");
        host.join().expect("the fake host ends");
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **A stop refuses by name when there is no take**, and never starts a
    /// browser to look for one.
    #[tokio::test]
    async fn a_stop_with_no_take_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let refused = service
            .call(&seat, "browser_record_stop", json!({}))
            .await
            .expect_err("no take is running");
        assert!(
            refused.contains("no recording is running"),
            "the stop names the missing take: {refused}"
        );
    }

    /// **A malformed call is refused before a browser opens for it.** A cold
    /// profile must not boot a host to answer a call that cannot run, and a
    /// busy one must not answer "the browser is busy" to an argument error.
    #[tokio::test]
    async fn a_malformed_call_is_refused_before_a_profile_opens() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let refused = service
            .call(&seat, "browser_click", json!({ "force": true }))
            .await
            .expect_err("a forced click with no target is refused");
        assert!(refused.contains("`target`"), "the call's own argument error: {refused}");
        assert!(
            !dir.path().join("profiles").exists(),
            "no profile was opened for it: {:?}",
            dir.path().join("profiles"),
        );
    }

    /// **Old takes go before a new one starts.** The recordings directory
    /// keeps ten, so a take that fills it does not fill the disk, and files
    /// that are not takes are left alone.
    #[test]
    fn a_prune_keeps_the_newest_takes_and_nothing_else() {
        use super::prune_recordings;
        let dir = tempfile::tempdir().expect("a temp dir");
        for name in ["one.mp4", "two.mp4", "three.mp4"] {
            std::fs::write(dir.path().join(name), b"take").expect("a take");
        }
        std::fs::write(dir.path().join("notes.txt"), b"mine").expect("a file that is not a take");
        // Keep two, as the start does with ten: the prune leaves room for
        // one more.
        prune_recordings(dir.path(), 2);
        let takes: Vec<String> = std::fs::read_dir(dir.path())
            .expect("the directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| std::path::Path::new(name).extension().is_some_and(|ext| ext == "mp4"))
            .collect();
        assert_eq!(takes.len(), 1, "one take was kept: {takes:?}");
        assert!(dir.path().join("notes.txt").exists(), "a file that is not a take stays");
    }

    /// **A chosen name never picks the directory it lands in, and never
    /// loses its own spelling.** The name comes from another machine: a
    /// traversal or an empty stem still writes under the profile's uploads,
    /// and two files that share a name both keep it (their place directories
    /// are what tells them apart).
    #[test]
    fn a_chosen_name_is_flattened_and_kept() {
        assert_eq!(file_name("../../etc/passwd"), "passwd");
        assert_eq!(file_name("cv.pdf"), "cv.pdf");
        assert_eq!(file_name(".."), "file");
        assert_eq!(file_name(""), "file");

        let dir = tempfile::tempdir().expect("a temp dir");
        let uploads = dir.path().join("uploads");
        let files = vec![
            forge_primitives::browser::BrowserUpload {
                name: "cv.pdf".to_owned(),
                bytes: b"one".to_vec(),
            },
            forge_primitives::browser::BrowserUpload {
                name: "cv.pdf".to_owned(),
                bytes: b"two".to_vec(),
            },
        ];
        let paths = write_uploads(&uploads, &files).expect("the files land");
        assert_eq!(paths.len(), 2, "two files of one name both land: {paths:?}");
        for path in &paths {
            let path = std::path::Path::new(path);
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some("cv.pdf"),
                "the page sees the file's own name, not its place",
            );
            assert!(path.starts_with(&uploads), "and it lands under the profile");
        }
        assert_ne!(paths[0], paths[1], "each file has its own place");
    }

    /// **A stale answer touches nothing.** An id that is not the profile's
    /// outstanding ask is refused before the uploads directory is cleared,
    /// so a superseded answer cannot wipe the files a live dialog is handing
    /// over.
    #[tokio::test]
    async fn a_stale_file_answer_is_refused_before_it_writes() {
        use forge_primitives::browser::{BrowserFileAsk, BrowserUpload};

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let tab = seed_tab(&profile, &seat);
        let uploads = profile.dir.join("uploads");
        std::fs::create_dir_all(&uploads).expect("the uploads dir");
        let kept = uploads.join("kept.txt");
        std::fs::write(&kept, b"a live dialog's file").expect("a live file");
        profile.files.send_replace(Some(BrowserFileAsk { id: 7, tab: 1, multiple: false }));

        let refused = service
            .answer_files(
                "TestOrg/default",
                tab,
                8,
                vec![BrowserUpload { name: "other.txt".to_owned(), bytes: b"x".to_vec() }],
            )
            .await
            .expect_err("an id that is not outstanding is refused");
        assert!(refused.contains("not outstanding"), "{refused}");
        assert!(kept.exists(), "the live dialog's file was not wiped");
        assert!(
            profile.files.borrow().as_ref().is_some_and(|ask| ask.id == 7),
            "and the outstanding ask is still the live one",
        );
    }

    /// **The agent's upload tool is bounded by the service itself**, not only
    /// by the driver it forwards to: a refusal happens before a browser is
    /// opened for the call, so the tool answers in words and touches nothing.
    #[tokio::test]
    async fn a_browser_file_upload_past_the_cap_is_refused_before_a_browser() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let big = dir.path().join("big.bin");
        std::fs::File::create(&big)
            .expect("a big file")
            .set_len(forge_primitives::browser::MAX_UPLOAD_BYTES + 1)
            .expect("its length");

        let refused = service
            .call(&seat, "browser_file_upload", json!({ "paths": [big.to_str().expect("a path")] }))
            .await
            .expect_err("past the cap is refused");
        assert!(refused.contains("8 MiB"), "{refused}");
    }

    /// **A closed profile leaves no chooser and no staged files behind.**
    #[tokio::test]
    async fn closing_clears_the_chooser_and_its_uploads() {
        use forge_primitives::browser::BrowserFileAsk;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let uploads = profile.dir.join("uploads");
        std::fs::create_dir_all(&uploads).expect("the uploads dir");
        std::fs::write(uploads.join("stale.txt"), b"stale").expect("a staged file");
        profile.files.send_replace(Some(BrowserFileAsk { id: 3, tab: 1, multiple: false }));

        service.close(&seat).await.expect("the profile closes");
        assert!(profile.files.borrow().is_none(), "the chooser is gone with the profile");
        assert!(!uploads.exists(), "and so are its staged files");
    }

    /// **An agent's upload is bounded before a browser is opened.** A path
    /// past the cap, a directory, a relative path and an unreadable path are
    /// refused in words; the staging copies a good file beside the profile
    /// under its own name.
    #[test]
    fn an_agent_upload_is_bounded_and_staged() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let small = dir.path().join("cv.pdf");
        std::fs::write(&small, b"a resume").expect("a small file");
        let good = serde_json::json!({ "paths": [small.to_str().expect("a path")] });
        let paths = agent_upload_paths(&good).expect("a good upload").expect("paths");
        assert_eq!(paths, vec![small.clone()], "the agent's path is what it named");
        let staged = stage_agent_uploads(&dir.path().join("uploads"), &paths).expect("staged");
        assert_eq!(staged.len(), 1);
        assert_eq!(
            std::path::Path::new(&staged[0]).file_name().and_then(|name| name.to_str()),
            Some("cv.pdf"),
            "the page sees the file's own name",
        );
        assert_eq!(std::fs::read(&staged[0]).expect("the staged bytes"), b"a resume");

        let big = dir.path().join("big.bin");
        let file = std::fs::File::create(&big).expect("a big file");
        file.set_len(forge_primitives::browser::MAX_UPLOAD_BYTES + 1).expect("its length");
        let over = serde_json::json!({ "paths": [big.to_str().expect("a path")] });
        let refused = agent_upload_paths(&over).expect_err("past the cap is refused");
        assert!(refused.contains("8 MiB"), "{refused}");

        let folder = serde_json::json!({ "paths": [dir.path().to_str().expect("a path")] });
        let refused = agent_upload_paths(&folder).expect_err("a directory is refused");
        assert!(refused.contains("not a file"), "{refused}");

        let relative = serde_json::json!({ "paths": ["cv.pdf"] });
        let refused = agent_upload_paths(&relative).expect_err("a relative path is refused");
        assert!(refused.contains("absolute"), "{refused}");

        let missing = serde_json::json!({ "paths": ["/nonexistent/forge/resume.pdf"] });
        let refused = agent_upload_paths(&missing).expect_err("an unreadable path is refused");
        assert!(refused.contains("cannot be read"), "{refused}");

        assert!(
            agent_upload_paths(&serde_json::json!({})).expect("a cancelling call").is_none(),
            "no paths is the tool's own cancel, not a refusal",
        );
    }

    /// **A death names its cause**: a signal and an exit code mean different
    /// things, and the surface's sentence carries the same words as the log.
    #[cfg(unix)]
    #[test]
    fn a_hosts_death_names_its_cause() {
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(death_cause(Some(std::process::ExitStatus::from_raw(15 << 8))), "exit code 15",);
        assert_eq!(death_cause(Some(std::process::ExitStatus::from_raw(9))), "signal 9");
        assert_eq!(death_cause(None), "exit status not ours to read");
    }

    /// **A killed host is said, and the next call relaunches it.** The
    /// server outlives its browser: the failure watch names the signal, and
    /// the next open starts a fresh host over the same profile - usually on
    /// the port the killed host freed.
    ///
    /// **The driver stack is deliberately absent**: the host starts before
    /// the driver does, and the open's own error is the driver's, not this
    /// test's subject.
    ///
    /// **It needs the installed bundle at this crate's version** - the
    /// service refuses a bundle another version staged - so a plain nextest
    /// on a checkout that has not installed this build skips it; the macOS
    /// CI job stages the bundle and runs the ignored set.
    #[ignore = "needs the installed Forge Browser.app at this crate's version"]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_killed_host_is_said_and_the_next_call_relaunches_it() {
        let exe = crate::host_bundle::executable().expect("the host bundle resolves");
        assert!(
            exe.is_file(),
            "the host bundle is not installed at {}; stage it with scripts/stage-browser-host.sh",
            exe.display(),
        );
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("no-stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let open = service.open(&seat).await;
        let profile = service.entry(&seat).await.expect("the profile entry");
        let mut failures = profile.failures.subscribe();
        // **A dead host takes its chooser and its staged uploads with it.**
        let uploads = profile.dir.join("uploads");
        std::fs::create_dir_all(&uploads).expect("the uploads dir");
        std::fs::write(uploads.join("staged.txt"), b"a chosen file").expect("a staged file");
        profile.files.send_replace(Some(forge_primitives::browser::BrowserFileAsk {
            id: 5,
            tab: 1,
            multiple: false,
        }));
        let (pid, _port) = {
            let state = profile.state.lock().await;
            let host = state.host.as_ref().unwrap_or_else(|| {
                panic!("the host started before the driver (open: {:?})", open.err())
            });
            (host.pid, host.port)
        };

        std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status()
            .expect("the kill runs");
        tokio::time::timeout(std::time::Duration::from_secs(8), failures.changed())
            .await
            .expect("the death is said rather than left as a frozen frame")
            .expect("the failure watch feeds");
        let said = failures.borrow_and_update().clone().unwrap_or_default();
        assert!(said.contains("signal 9"), "the signal is named in the surface's sentence: {said}");
        assert!(!uploads.exists(), "a dead host's staged uploads are gone");
        assert!(
            profile.files.borrow().is_none(),
            "and its chooser is cancelled rather than left waiting on a callback that cannot run",
        );

        let _ = service.open(&seat).await;
        let (next_pid, next_port) = {
            let state = profile.state.lock().await;
            let host = state.host.as_ref().expect("a fresh host is up");
            (host.pid, host.port)
        };
        assert_ne!(next_pid, pid, "the next call starts a new host");
        // **The port may be the same**: the killed host freed it, and
        // `free_port` scans from the start of its range, so the relaunch
        // normally takes it back. A new pid answering on its endpoint is the
        // relaunch's own proof.
        assert!(crate::spawn::alive(next_pid), "the fresh host is running");
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", next_port)).await.is_ok(),
            "and listening on its port",
        );
        let left = service.stop_all().await;
        assert!(left.is_empty(), "the test's host is ended with the service: {left:?}");
    }

    /// **A dead page is answered in the client's words, never the driver's
    /// `_page` TypeError.** Each outcome names what died and what to do now;
    /// the raw line stays in the rebuild log (#2534).
    #[test]
    fn a_dead_page_answer_never_leads_with_the_drivers_type_error() {
        let answers = [
            dead_page_answer(DeadPageOutcome::NotRepeated { tool: "browser_click" }),
            dead_page_answer(DeadPageOutcome::RetryFailed { why: "no element matches" }),
            dead_page_answer(DeadPageOutcome::StillDead),
            dead_page_answer(DeadPageOutcome::RebuildFailed { why: "the start timed out" }),
            dead_page_answer(DeadPageOutcome::Cooling),
            dead_page_answer(DeadPageOutcome::TabGone { tab: 2 }),
            dead_page_answer(DeadPageOutcome::ListFailed {
                why: "the driver's tab list carried no FORGE-TABS export",
            }),
        ];
        for answer in answers {
            assert!(answer.contains("the browser's page context died"), "{answer}");
            assert!(answer.contains("renderer went away"), "{answer}");
            assert!(!answer.contains("_page"), "the internal reading never surfaces: {answer}");
            assert!(
                !answer.contains("browserBackend.callTool"),
                "nor the driver's wrapper: {answer}",
            );
        }
        // **A handed-back call says it may already have landed**: "run it
        // again now" invites the doubling the refusal existed to prevent.
        let handed = dead_page_answer(DeadPageOutcome::NotRepeated { tool: "browser_tabs" });
        assert!(handed.contains("browser_tabs may already have landed"), "{handed}");
        assert!(handed.contains("check the page before running it again"), "{handed}");
        let unread = dead_page_answer(DeadPageOutcome::ListFailed {
            why: "the driver's tab list carried no FORGE-TABS export",
        });
        assert!(unread.contains("could not be read"), "{unread}");
        assert!(unread.contains("nothing is known about the page"), "{unread}");
    }

    /// **Every dead-driver outcome names the replacement and the next move**,
    /// and a handed-back call is never invited to repeat blind.
    #[test]
    fn a_dead_driver_answer_names_the_replacement_and_the_next_move() {
        let answers = [
            dead_driver_answer(DeadDriverOutcome::NotRepeated {
                tool: "browser_click",
                why: "the driver did not answer browser_click within 149 s",
            }),
            dead_driver_answer(DeadDriverOutcome::RetryFailed { why: "no element matches" }),
            dead_driver_answer(DeadDriverOutcome::ReplaceFailed { why: "the start timed out" }),
            dead_driver_answer(DeadDriverOutcome::TabGone),
            dead_driver_answer(DeadDriverOutcome::ListFailed { why: "no FORGE-TABS export" }),
        ];
        for answer in answers {
            assert!(
                answer.contains("stopped answering") || answer.contains("could not be replaced"),
                "{answer}",
            );
            assert!(!answer.contains("_page"), "the internal reading never surfaces: {answer}");
        }
        let handed = dead_driver_answer(DeadDriverOutcome::NotRepeated {
            tool: "browser_click",
            why: "the driver did not answer browser_click within 149 s",
        });
        assert!(handed.contains("browser_click may already have landed"), "{handed}");
        assert!(handed.contains("check the page before running it again"), "{handed}");
        // **An attach timeout never reached the page**, so it is safe to
        // repeat and the answer says so.
        let never = dead_driver_answer(DeadDriverOutcome::NotRepeated {
            tool: "browser_click",
            why: "### Error\nTimeoutError: async initializeServer: Timeout 30000ms exceeded.",
        });
        assert!(never.contains("never reached the page"), "{never}");
        assert!(never.contains("can be run again now"), "{never}");
        // **A `browser_tabs` timeout delivered nothing either**, even when the
        // outer tool is a click: its tab selection never answered, so the
        // sentence is true for it too.
        let tabs = dead_driver_answer(DeadDriverOutcome::NotRepeated {
            tool: "browser_click",
            why: "the driver did not answer browser_tabs within 149 s",
        });
        assert!(tabs.contains("never reached the page"), "{tabs}");
        assert!(tabs.contains("can be run again now"), "{tabs}");
        let gone = dead_driver_answer(DeadDriverOutcome::TabGone);
        assert!(gone.contains("not in the fresh driver's list"), "{gone}");
        let unread = dead_driver_answer(DeadDriverOutcome::ListFailed {
            why: "the driver's tab list carried no FORGE-TABS export",
        });
        assert!(unread.contains("could not be read"), "{unread}");
        assert!(unread.contains("nothing is known about this session's page"), "{unread}");
    }

    /// **A seat's entry is its org's profile**: two seats of one org share a
    /// browser, and another org's seat gets one of its own.
    #[tokio::test]
    async fn a_seat_enters_its_org_profile_and_two_orgs_do_not_share_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let one = SessionSlot::lead("TestOrg", "proj");
        let same_org = SessionSlot::worker("TestOrg", "proj", "runner");
        let other_org = SessionSlot::lead("OtherOrg", "proj");

        let mine = service.entry(&one).await.expect("the seat's own");
        assert_eq!(mine.key, "TestOrg/default", "the seat's org profile");
        let shared = service.entry(&same_org).await.expect("the same org's");
        assert_eq!(shared.id, mine.id, "one org, one browser");
        let other = service.entry(&other_org).await.expect("another org's");
        assert_ne!(other.id, mine.id, "two orgs, two browsers");
    }

    /// **A held role owns the page's layout; an unheld one belongs to the tab's
    /// longest-watching viewer.** A second unheld viewer's window never moves
    /// the page under the first, a page with no shell still lays its own view
    /// out while it is the only one watching, and a nonsense ratio draws at 1
    /// rather than poisoning CEF.
    #[test]
    fn a_resize_follows_the_controller_else_the_longest_watcher() {
        let alone = HashSet::from([7]);
        assert_eq!(
            owner_size(None, &alone, 7, 800, 600, 2.0),
            Some((800, 600, 2.0)),
            "the only viewer sizes its own view"
        );
        let two = HashSet::from([7, 9]);
        assert_eq!(
            owner_size(None, &two, 9, 800, 600, 2.0),
            None,
            "a second viewer never moves the page under the first"
        );
        assert_eq!(
            owner_size(None, &two, 7, 640, 480, 1.0),
            Some((640, 480, 1.0)),
            "the longest-watching viewer owns the unheld layout"
        );
        assert_eq!(
            owner_size(Some(8), &two, 7, 800, 600, 2.0),
            None,
            "a watcher's window never moves the page under the holder"
        );
        assert_eq!(owner_size(Some(7), &two, 7, 800, 600, 2.0), Some((800, 600, 2.0)));
        assert_eq!(
            owner_size(Some(7), &two, 7, 800, 600, f64::NAN),
            Some((800, 600, 1.0)),
            "a nonsense ratio draws at 1"
        );
        assert_eq!(
            owner_size(Some(7), &two, 7, 0, 0, 99.0),
            Some((1, 1, 8.0)),
            "bounded both ways"
        );
    }

    /// **A window size is the page's own CSS pixels, and the answer names the
    /// size the ring will paint.** A request past the ring's device bound is
    /// cut here rather than left to the ring's silent clamp, and a size that
    /// is not a positive number is refused rather than clamped to 1.
    #[test]
    fn a_window_size_is_css_pixels_bounded_by_the_ring() {
        let width = |value: serde_json::Value| {
            window_dimension(&json!({ "width": value }), "width", 2.0, crate::osr::MAX_WIDTH)
        };
        assert_eq!(width(json!(1120)), Ok(1120), "a CSS size passes through");
        assert_eq!(width(json!(1120.6)), Ok(1121), "and rounds to the nearest pixel");
        assert_eq!(
            width(json!(3000)),
            Ok(1920),
            "3000 CSS pixels would paint past 3840 device pixels at 2x"
        );
        assert_eq!(
            width(json!(0)),
            Err("browser_resize needs `width` as a positive number".into())
        );
        assert_eq!(width(json!("wide")), Err("browser_resize needs `width` as a number".into()));
        assert_eq!(device_pixels(1120, 2.0), 2240, "CSS pixels times the ratio it paints at");
        assert_eq!(device_pixels(1920, 2.0), crate::osr::MAX_WIDTH, "the bound is reachable");
    }

    /// **A resize is one step: the ring is asked for the size and that size is
    /// stored.** A take lays its tab out at its own size and restores the
    /// profile's surface when it ends, so a resize that moved the ring and
    /// left the stored surface alone would be undone by the next take. **Both
    /// arms are the same step**: the tool's window and the viewer's own.
    #[tokio::test]
    async fn a_resize_stores_the_surface_it_asks_the_ring_for() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        let profile = service.find("O/default").await.expect("the profile");
        let ring_name = format!("/forge-resize-surface-{}", std::process::id());
        let ring = Arc::new(
            crate::osr::Ring::create(
                &ring_name,
                crate::service::VIEW_WIDTH,
                crate::service::VIEW_HEIGHT,
            )
            .expect("a ring"),
        );
        let (input, _input_rx) = std::sync::mpsc::channel();
        profile.state.lock().await.host = Some(crate::spawn::ActiveHost {
            pid: std::process::id(),
            port: 9,
            ring: Arc::clone(&ring),
            profile: dir.path().join("host"),
            input,
            reader: None,
            monitor: None,
        });
        *profile.surface.lock().unwrap_or_else(PoisonError::into_inner) = (2880, 2128, 2.0);

        service
            .call(&seat, "browser_resize", json!({ "width": 1120, "height": 760 }))
            .await
            .expect("the window resizes");

        assert_eq!(
            ring.requested(),
            (2240, 1520, 2_000, 1),
            "the ring is asked once, for the device size the page paints at",
        );
        assert_eq!(
            *profile.surface.lock().unwrap_or_else(PoisonError::into_inner),
            (2240, 1520, 2.0),
            "the tool's window replaces the surface, at the ratio the surface paints at",
        );

        // **The viewer's arm is the same step**: its own surface report asks
        // the ring and stores the surface together, so a take restores the
        // window the viewer last asked for.
        profile.controls.lock().unwrap_or_else(PoisonError::into_inner).insert(1, (7, 0));
        service.resize("O/default", 1, 7, 2880, 2128, 2.0).await;
        assert_eq!(
            ring.requested(),
            (2880, 2128, 2_000, 2),
            "the viewer's window is the request the ring holds last",
        );
        assert_eq!(
            *profile.surface.lock().unwrap_or_else(PoisonError::into_inner),
            (2880, 2128, 2.0),
            "and the surface a take restores is that same request",
        );
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **Two orgs share nothing**: their wire ids differ (what a frame is
    /// keyed by), their cache roots differ, and one profile's frame never
    /// reaches the other's stream.
    #[tokio::test]
    async fn two_profiles_share_no_id_root_or_frame_stream() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let one = SessionSlot::lead("TestOrg", "proj");
        let two = SessionSlot::lead("OtherOrg", "proj");
        let first = service.entry(&one).await.expect("the first profile");
        let second = service.entry(&two).await.expect("the second profile");

        assert_ne!(first.id(), second.id(), "a frame is keyed by the profile's id");
        assert_ne!(
            service.profile_dir(&ProfileKey::parse(&first.key).expect("a key")),
            service.profile_dir(&ProfileKey::parse(&second.key).expect("a key")),
            "and each has its own root",
        );
        first.frame_sender(1).send_replace(Some(forge_primitives::browser::BrowserFrame {
            profile: first.id(),
            tab: 1,
            seq: 1,
            encoded: vec![1],
        }));
        assert!(
            first.frame_receiver(1).borrow().is_some(),
            "the first profile's stream carries its frame"
        );
        assert!(second.frame_receiver(1).borrow().is_none(), "and the second's does not");
    }

    /// **A tab's newest frame is not replaced by another tab's paint.** One
    /// profile-wide slot let a background tab overwrite the viewed tab's
    /// frame - an attach burst asks every tab for a key frame - and the
    /// surface could blank.
    #[tokio::test]
    async fn a_tabs_frame_is_not_replaced_by_another_tabs_paint() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let viewer = profile.frame_receiver(1);
        let frame = |tab, seq| {
            Some(forge_primitives::browser::BrowserFrame {
                profile: profile.id(),
                tab,
                seq,
                encoded: vec![1],
            })
        };

        profile.frame_sender(1).send_replace(frame(1, 1));
        profile.frame_sender(2).send_replace(frame(2, 9));
        assert_eq!(
            viewer.borrow().as_ref().map(|frame| (frame.tab, frame.seq)),
            Some((1, 1)),
            "tab 2's paint does not replace tab 1's frame",
        );
        profile.frame_sender(1).send_replace(frame(1, 2));
        assert_eq!(
            viewer.borrow().as_ref().map(|frame| (frame.tab, frame.seq)),
            Some((1, 2)),
            "the viewed tab's own newest paint arrives",
        );
    }

    /// **Only the controller moves the page, checked in the service before
    /// any host lookup.** A watcher's move is refused even with no host up,
    /// so the rule cannot be supplied by a stub or by the transport.
    #[tokio::test]
    async fn only_the_controller_moves_the_page() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("the profile");
        let tab = seed_tab(&profile, &seat);
        service.take_control(&profile.key, tab, 7).await.expect("the take");

        let why = service.nav(&profile.key, tab, 8, true).await.expect_err("a watcher is refused");
        assert!(why.contains("not the browser's controller"), "{why}");
    }

    /// **A re-attach of the same viewer to the same tab keeps its role, and
    /// a different viewer's take still moves it.** A surface that opens twice
    /// (or asks again for the tab it is already on) is one viewer on one
    /// page; dropping the role there left the person driving a tab whose
    /// every gesture was refused. Keeping it must not make the role the
    /// first taker's for good: a take is still what moves it, and the holder
    /// it moved from is a watcher. A move to another tab leaves the role
    /// behind with the tab.
    #[tokio::test]
    async fn a_reattach_of_the_same_viewer_keeps_its_role() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("TestOrg/default").await.expect("the profile");
        let tab = 1;

        service.attach(7, &seat, Some(tab)).await.expect("the first attach");
        service.take_control(&profile.key, tab, 7).await.expect("the take");
        assert_eq!(profile.controller(tab), Some(7), "the take lands");

        service.attach(7, &seat, Some(tab)).await.expect("the re-attach");
        assert_eq!(profile.controller(tab), Some(7), "the re-attach keeps the role");

        // **A take still moves the role**, so keeping it across a re-attach
        // did not make it the first taker's for good.
        service.take_control(&profile.key, tab, 8).await.expect("another viewer's take");
        assert_eq!(profile.controller(tab), Some(8), "a take moves the role");
        let why = service
            .input(
                &profile.key,
                tab,
                7,
                1,
                &forge_primitives::browser::BrowserInputEvent::Text { text: "x".to_owned() },
            )
            .await
            .expect_err("the holder it moved from is a watcher now");
        assert!(why.contains("not the browser's controller"), "{why}");
        service.take_control(&profile.key, tab, 7).await.expect("the role back");

        profile
            .tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .adopt(2, seat.clone(), "T2", "T2", "", "about:blank")
            .expect("tab 2");
        service.attach(7, &seat, Some(2)).await.expect("the move");
        assert_eq!(profile.controller(tab), None, "a move to another tab gives the role up");
        assert!(
            !profile
                .viewers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&tab)
                .is_some_and(|watchers| watchers.contains(&7)),
            "and the tab it left keeps no viewer of it",
        );
        assert!(
            profile
                .viewers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&2)
                .is_some_and(|watchers| watchers.contains(&7)),
            "the tab it moved to holds the viewer",
        );
        assert_eq!(profile.controller(2), None, "and nobody took its role yet");
        assert_eq!(
            service.where_viewing.lock().unwrap_or_else(PoisonError::into_inner).get(&7).cloned(),
            Some((profile.key.clone(), 2)),
            "and the viewer is known to be on the tab it moved to",
        );

        // **The same tab number in another profile is not the same tab.**
        let other = SessionSlot::lead("OtherOrg", "proj");
        service.install_fake(&other, FakeDriver::with_blank("T1")).await;
        service
            .call(&other, "browser_snapshot", json!({}))
            .await
            .expect("the other profile's tab is adopted");
        let other_profile = service.find("OtherOrg/default").await.expect("the other profile");
        service.attach(9, &seat, Some(tab)).await.expect("a viewer on the first profile");
        service.take_control(&profile.key, tab, 9).await.expect("its take");
        service.attach(9, &other, Some(1)).await.expect("the move across profiles");
        assert_eq!(profile.controller(tab), None, "the first profile's role is released");
        assert!(
            !profile
                .viewers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&tab)
                .is_some_and(|watchers| watchers.contains(&9)),
            "and the viewer left the first profile's tab",
        );
        assert_eq!(other_profile.controller(1), None, "the second profile holds no role yet");
        assert!(
            other_profile
                .viewers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&1)
                .is_some_and(|watchers| watchers.contains(&9)),
            "and the viewer is on the second profile's tab 1",
        );
    }

    /// **A release is recorded once, for the role it actually held.** The
    /// bar's release control is the one role change a person makes by hand,
    /// and the record is the service's own: a release that held nothing is
    /// not an event and writes nothing.
    #[tokio::test]
    async fn a_release_is_recorded_once_by_the_service() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");
        let profile = service.find("TestOrg/default").await.expect("the profile");
        let tab = 1;

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let with = |caught: &Caught, event: &str| -> Vec<(tracing::Level, String)> {
            let needle = format!("event_name=\"{event}\"");
            records(caught).into_iter().filter(|(_, fields)| fields.contains(&needle)).collect()
        };

        service.attach(7, &seat, Some(tab)).await.expect("the attach");
        let attaches = with(&caught, "browser_attach");
        assert_eq!(attaches.len(), 1, "the attach is recorded: {attaches:?}");
        assert!(
            attaches[0].0 == tracing::Level::DEBUG,
            "at the level the shipped directive keeps: {:?}",
            attaches[0].0,
        );
        assert!(
            attaches[0].1.contains("same_surface=false"),
            "a first attach keeps no role: {}",
            attaches[0].1,
        );
        // The same tab again is the same surface, and the record says so.
        service.attach(7, &seat, Some(tab)).await.expect("the re-attach");
        let reattached = with(&caught, "browser_attach");
        assert!(
            reattached[1].1.contains("same_surface=true") && reattached[1].1.contains("was_tab=1"),
            "the re-attach names the surface it kept: {}",
            reattached[1].1,
        );

        service.take_control(&profile.key, tab, 7).await.expect("the take");
        assert!(!saw(&caught, "browser_role_released"), "a take is not a release");

        let released = |caught: &Caught| -> Vec<(tracing::Level, String)> {
            with(caught, "browser_role_released")
        };
        service.release_control(&profile.key, tab, 7).await;
        let caught_release = released(&caught);
        assert_eq!(caught_release.len(), 1, "the release is recorded once: {caught_release:?}");
        assert!(
            caught_release[0].0 == tracing::Level::DEBUG,
            "at the level the shipped directive keeps: {:?}",
            caught_release[0].0,
        );
        for needle in ["profile=TestOrg/default", "tab=1", "viewer=7"] {
            assert!(
                caught_release[0].1.contains(needle),
                "the record carries `{needle}`: {}",
                caught_release[0].1,
            );
        }

        service.release_control(&profile.key, tab, 7).await;
        assert_eq!(
            released(&caught).len(),
            1,
            "a release that held nothing writes nothing: {:?}",
            released(&caught),
        );

        // **A departure releases the role through the same record.** A
        // viewer that goes takes the role with it, and the line is the
        // service's, not the connection's.
        service.attach(8, &seat, Some(tab)).await.expect("another viewer");
        service.take_control(&profile.key, tab, 8).await.expect("its take");
        service.detach(8).await;
        let departed = released(&caught);
        assert_eq!(departed.len(), 2, "the departure's release is recorded too: {departed:?}");
        assert!(
            departed[1].1.contains("viewer=8"),
            "named by the viewer that left: {}",
            departed[1].1,
        );
        drop(guard);
    }

    /// **A close ends the seat's own profile**, and the entry leaves the map.
    #[tokio::test]
    async fn a_close_ends_the_seats_profile() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        service.entry(&seat).await.expect("the seat's profile");

        service.close(&seat).await.expect("the close");
        assert!(
            !service.profiles.lock().await.contains_key("TestOrg/default"),
            "and the entry is gone",
        );
    }

    /// **A seat resolves the project's override, else its org's, else
    /// `default`.** An empty map is every org's `default`.
    #[test]
    fn a_seat_resolves_the_project_override_then_the_org_then_default() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        service.configure_profiles(
            HashMap::from([("Busytools".to_owned(), "work".to_owned())]),
            HashMap::from([(("Busytools".to_owned(), "apply".to_owned()), "job".to_owned())]),
        );
        let key = |org: &str, project: &str| {
            service
                .profile_for(&SessionSlot::lead(org, project))
                .expect("the key resolves")
                .as_str()
                .to_owned()
        };
        assert_eq!(key("Busytools", "forge"), "Busytools/work", "the org's profile");
        assert_eq!(key("Busytools", "apply"), "Busytools/job", "the project's override");
        assert_eq!(key("Personal", "forge"), "Personal/default", "no config is default");
    }

    /// A service whose idle bound has already passed: a profile an idle pass
    /// reads is one the clock says closes.
    fn idle_service(root: &std::path::Path) -> Arc<BrowserService> {
        let service = BrowserService::at(root.to_owned(), root.join("stack"));
        service.configure_idle(0);
        service
    }

    /// **The clock closes the profile and the logins stay.** A profile nobody
    /// touched is gone from the map on the first pass, and its directory is
    /// still there for the next call.
    #[tokio::test]
    async fn an_idle_profile_closes_at_the_bound_and_keeps_its_files() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = idle_service(&root);
        let seat = SessionSlot::lead("TestOrg", "proj");
        service.entry(&seat).await.expect("the profile opens");

        service.idle_pass().await;
        assert!(
            !service.profiles.lock().await.contains_key("TestOrg/default"),
            "the browser closes at its bound",
        );
        assert!(
            root.join("profiles/TestOrg/default").exists(),
            "the profile's logins stay on disk",
        );
        service.idle_pass().await;
        assert!(
            !service.profiles.lock().await.contains_key("TestOrg/default"),
            "a closed profile is not reopened by the next pass",
        );
    }

    /// **A close takes the entry out from under any caller that kept it.**
    /// The handle from before the close cannot start a second host on the
    /// profile the map no longer names; the next call opens a fresh entry.
    #[tokio::test]
    async fn a_closed_profile_is_not_reopened_under_a_caller_that_kept_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat).await.expect("the entry");
        service.close(&seat).await.expect("the close");

        let why =
            profile.ensure(&service.root, service.stack()).await.expect_err("the ended entry");
        assert!(why.contains("closed"), "the caller is told to open it again: {why}");
        let again = service.entry(&seat).await.expect("a fresh entry");
        assert!(!Arc::ptr_eq(&profile, &again), "the next call answers a new entry");
    }

    /// **A parked hand-off holds the close.** A person being asked to act in
    /// the page must not have it closed under them; once the hold lifts, the
    /// next pass closes it.
    #[tokio::test]
    async fn a_held_profile_is_not_closed_until_the_hold_lifts() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = idle_service(&root);
        let seat = SessionSlot::lead("TestOrg", "proj");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab");
        let tab = service.tabs(&seat).await.expect("rows").rows[0].number;
        service.sweep_sessions(std::slice::from_ref(&seat), 1).await;
        service.hand_off_started(&seat, tab).await.expect("the tab is held");

        service.idle_pass().await;
        assert!(
            service.profiles.lock().await.contains_key("TestOrg/default"),
            "a held profile is not closed",
        );

        service.hand_off_finished(&seat, tab).await;
        assert_eq!(
            service.tabs(&seat).await.expect("rows").rows.len(),
            1,
            "a live owner keeps its tab after the hand-off",
        );
        service.idle_pass().await;
        assert!(
            !service.profiles.lock().await.contains_key("TestOrg/default"),
            "the pass after the hold lifts closes it",
        );
    }

    /// **A viewer attached is a use**: the surface the person is looking at
    /// is never closed, however long it stays up.
    #[tokio::test]
    async fn a_viewed_profile_is_never_idle() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = idle_service(&root);
        let profile =
            service.entry(&SessionSlot::lead("TestOrg", "proj")).await.expect("the profile opens");
        profile
            .viewers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(1)
            .or_default()
            .insert(9);

        service.idle_pass().await;

        assert!(
            service.profiles.lock().await.contains_key("TestOrg/default"),
            "a viewed browser is not closed",
        );
    }

    /// **Another profile is served while one is torn down.** The map lock is
    /// released before the end (up to the kill bound), so every other
    /// profile's entry keeps being served.
    #[tokio::test]
    async fn another_profile_is_served_while_one_is_torn_down() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let one = SessionSlot::lead("TestOrg", "proj");
        let two = SessionSlot::lead("OtherOrg", "proj");
        let a = service.entry(&one).await.expect("the first profile");
        service.entry(&two).await.expect("the second profile");

        // **Hold A's state lock**: the close takes A out of the map, then
        // its end parks here - the teardown window the other profile must
        // not wait on.
        let blocked = a.state.lock().await;
        let closing = tokio::spawn({
            let service = Arc::clone(&service);
            let one = one.clone();
            async move { service.close(&one).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !service.profiles.lock().await.contains_key("TestOrg/default") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the close took its entry before the teardown");

        let b = tokio::time::timeout(std::time::Duration::from_secs(2), service.entry(&two))
            .await
            .expect("another profile is served during the teardown")
            .expect("the entry answers");
        assert_eq!(b.key, "OtherOrg/default", "the untouched profile");

        drop(blocked);
        closing.await.expect("the close task joins").expect("the close completes");
    }

    /// **An idle close releases the fleet's map before it tears its host
    /// down**, the same rule the close follows: another profile's entry is
    /// served while the teardown runs.
    #[tokio::test]
    async fn another_profile_is_served_while_an_idle_close_tears_one_down() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = idle_service(&root);
        let one = SessionSlot::lead("TestOrg", "proj");
        let two = SessionSlot::lead("OtherOrg", "proj");
        let a = service.entry(&one).await.expect("the first profile");
        service.entry(&two).await.expect("the second profile");

        // **Hold A's state lock**: the idle close takes A out of the map and
        // then parks in its teardown, which the other profile must not wait
        // on.
        let blocked = a.state.lock().await;
        let closing = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.close_idle("TestOrg/default").await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !service.profiles.lock().await.contains_key("TestOrg/default") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the idle close took its entry before the teardown");

        let b = tokio::time::timeout(std::time::Duration::from_secs(2), service.entry(&two))
            .await
            .expect("another profile is served during the teardown")
            .expect("the entry answers");
        assert_eq!(b.key, "OtherOrg/default", "the untouched profile");

        drop(blocked);
        closing.await.expect("the close task joins");
    }

    /// **A stop forgets every open profile and keeps its files.** This test
    /// holds no real host, so what it pins is the map draining and the logins
    /// staying for the next boot; the live stop is the crash/relaunch test's,
    /// where a real host is ended by `stop_all`.
    #[tokio::test]
    async fn a_stop_forgets_every_open_profile_and_keeps_its_files() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let one = SessionSlot::lead("TestOrg", "proj");
        let two = SessionSlot::lead("OtherOrg", "proj");
        service.entry(&one).await.expect("the first profile");
        service.entry(&two).await.expect("the second profile");

        let left = service.stop_all().await;

        assert!(left.is_empty(), "no host outlived its teardown: {left:?}");
        assert!(service.profiles.lock().await.is_empty(), "nothing is left open");
        assert!(root.join("profiles/TestOrg/default").exists(), "the profile's logins stay");
        assert!(root.join("profiles/OtherOrg/default").exists(), "both of them");
    }

    /// The profile's cache root is under the service's root, never a launch
    /// directory, and its key is the org and the name.
    #[test]
    fn a_profile_dir_hangs_off_the_service_root() {
        let service = BrowserService::at("/browser".into(), "/browser/stack".into());
        assert_eq!(
            service.profile_dir(&ProfileKey::of("Busytools", "work").expect("a key")),
            std::path::Path::new("/browser/profiles/Busytools/work/cef"),
        );
    }

    /// One tab on a profile a test made by hand, so the viewer and chooser
    /// tests can name the tab their roles and asks belong to.
    pub(crate) fn seed_tab(profile: &Arc<Profile>, seat: &SessionSlot) -> u16 {
        let mut tabs = profile.tabs.lock().unwrap_or_else(PoisonError::into_inner);
        if tabs.target(1).is_some() {
            return 1;
        }
        tabs.adopt(1, seat.clone(), "T1", "T1", "", "about:blank").expect("the test's tab")
    }

    /// A driver the call path can be tested against: it records the indices
    /// it selects and answers every tool with one text part.
    #[derive(Clone, Default)]
    pub(crate) struct FakeDriver {
        tabs: Arc<Mutex<Vec<crate::tabs::DriverTab>>>,
        selected: Arc<Mutex<Vec<usize>>>,
        /// One listing that fails like a driver whose page outlived its
        /// renderer, so the recovery path has a real caller.
        stale_once: Arc<std::sync::atomic::AtomicBool>,
        /// An upload the driver refuses, so the failure path has a real
        /// caller that also holds the chooser decision.
        fail_upload: Arc<std::sync::atomic::AtomicBool>,
        /// How many tab listings this fake has answered, so a test can see a
        /// watch stop asking.
        listings: Arc<AtomicUsize>,
        /// Poked on every listing, for a test that waits on the count.
        changed: Arc<Notify>,
        /// **Every** listing answers the driver's own timeout.
        slow_listing: Arc<std::sync::atomic::AtomicBool>,
        /// **Every** page call answers an initialise timeout, as a driver
        /// that could not attach to its host does.
        attach_timeout: Arc<std::sync::atomic::AtomicBool>,
        /// Every page call answers the driver's plain failure.
        fail_calls: Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeDriver {
        pub(crate) fn with_blank(target: &str) -> Self {
            let fake = Self::default();
            fake.tabs.lock().expect("the tabs").push(crate::tabs::DriverTab {
                index: 0,
                id: target.to_owned(),
                target: target.to_owned(),
                title: String::new(),
                url: "about:blank".to_owned(),
                current: true,
            });
            fake
        }

        pub(crate) fn push_target(&self, target: &str) {
            let mut tabs = self.tabs.lock().expect("the tabs");
            let index = tabs.len();
            tabs.push(crate::tabs::DriverTab {
                index,
                id: target.to_owned(),
                target: target.to_owned(),
                title: String::new(),
                url: "about:blank".to_owned(),
                current: false,
            });
        }

        fn selected(&self) -> Vec<usize> {
            self.selected.lock().expect("the selections").clone()
        }

        /// How many listings this fake has answered so far.
        fn listings(&self) -> usize {
            self.listings.load(Ordering::Acquire)
        }

        /// The page is gone: the next listing names none.
        fn close_all(&self) {
            self.tabs.lock().expect("the tabs").clear();
        }

        /// The page is listed but its target is empty, as the export writes it
        /// when its own CDP read throws.
        fn clear_target(&self) {
            let mut tabs = self.tabs.lock().expect("the tabs");
            if let Some(tab) = tabs.first_mut() {
                tab.target.clear();
            }
        }

        /// The fake's next listing answers the stale-page error once.
        pub(crate) fn stale_once(&self) {
            self.stale_once.store(true, Ordering::Release);
        }

        /// **Every** listing answers the driver's own timeout: a retry on
        /// this same fake cannot pass, so only a real replacement can.
        pub(crate) fn timeout_every_listing(&self) {
            self.slow_listing.store(true, Ordering::Release);
        }

        /// **Every** page call answers an initialise timeout, for the same
        /// reason: a retry of this fake cannot pass either.
        fn timeout_every_call(&self) {
            self.attach_timeout.store(true, Ordering::Release);
        }

        /// A fresh driver attached to the same host names the same page
        /// again: every Playwright guid changes, the target does not.
        fn reidentify(&self, id: &str) {
            let mut tabs = self.tabs.lock().expect("the tabs");
            if let Some(tab) = tabs.first_mut() {
                id.clone_into(&mut tab.id);
            }
        }

        /// The page navigated and the driver was replaced: both names the
        /// registry holds are gone.
        fn rename(&self, id: &str, target: &str) {
            let mut tabs = self.tabs.lock().expect("the tabs");
            if let Some(tab) = tabs.first_mut() {
                id.clone_into(&mut tab.id);
                target.clone_into(&mut tab.target);
            }
        }

        /// Every upload answers the driver's plain failure.
        fn fail_uploads(&self) {
            self.fail_upload.store(true, Ordering::Release);
        }

        /// Every page call answers the driver's plain failure.
        fn fail_calls(&self) {
            self.fail_calls.store(true, Ordering::Release);
        }
    }

    /// **A popup queued beside a requested tab is not taken for it.** The
    /// birth carries the request's id, and every event is kept in arrival
    /// order so its place among the births can pair it with its page.
    #[test]
    fn a_popup_beside_a_requested_birth_is_not_taken_for_it() {
        use super::collect_event;
        use crate::control::HostEvent;

        let popup = HostEvent::TabCreated {
            tab: 5,
            opener: Some(1),
            url: "http://pop/".to_owned(),
            request: None,
        };
        let ours = HostEvent::TabCreated {
            tab: 6,
            opener: None,
            url: "about:blank".to_owned(),
            request: Some(42),
        };
        let closed = HostEvent::TabClosed { tab: 4 };
        let mut events = Vec::new();
        let mut ours_at = None;
        assert!(
            !collect_event((7, popup), 7, 42, &mut events, &mut ours_at),
            "a popup is not ours",
        );
        assert!(
            !collect_event((7, closed), 7, 42, &mut events, &mut ours_at),
            "a close is not ours",
        );
        assert!(
            collect_event((7, ours), 7, 42, &mut events, &mut ours_at),
            "the id says which birth",
        );
        assert_eq!(ours_at, Some(2), "the request's birth is the third event, in order");
        assert_eq!(events.len(), 3, "the others are kept, not dropped");
        assert!(matches!(
            events.get(2),
            Some((7, HostEvent::TabCreated { tab: 6, request: Some(42), .. }))
        ));
        // **A birth from a receiver that was replaced is not this one's**
        // even when its request id agrees: the host generation is part of
        // the identity, so a stale birth can never be taken for the current
        // host's tab.
        let mut events = Vec::new();
        let mut ours_at = None;
        let stale =
            HostEvent::TabCreated { tab: 8, opener: None, url: String::new(), request: Some(42) };
        assert!(
            !collect_event((6, stale), 7, 42, &mut events, &mut ours_at),
            "another generation's birth is not ours",
        );
        assert_eq!(ours_at, None);
    }

    /// **A fresh host's boot page is not closed as an orphan.** The first
    /// `browser_tabs` call reconciles before any session has adopted it;
    /// closing the current page would leave the next call with nothing to
    /// adopt, and the host would come back with an extra page.
    #[test]
    fn a_fresh_hosts_boot_page_is_not_closed_as_an_orphan() {
        use super::closed_as_orphans;
        use crate::tabs::DriverTab;

        let listing = [
            DriverTab {
                index: 0,
                id: "p1".to_owned(),
                target: "boot".to_owned(),
                title: String::new(),
                url: "about:blank".to_owned(),
                current: true,
            },
            DriverTab {
                index: 1,
                id: "p2".to_owned(),
                target: "made".to_owned(),
                title: String::new(),
                url: "about:blank".to_owned(),
                current: false,
            },
        ];
        let tabs = crate::tabs::TabRegistry::default();
        assert_eq!(
            closed_as_orphans(&tabs, &listing, true),
            vec!["made".to_owned()],
            "the boot page waits for its session's first call",
        );
        assert_eq!(
            closed_as_orphans(&tabs, &listing, false),
            vec!["boot".to_owned(), "made".to_owned()],
            "once adopted, an unclaimed page is an orphan like any other",
        );
    }

    /// **A host tab the driver has not listed waits for the next pass.** The
    /// popup is adopted when its page appears, never guessed at by position.
    #[test]
    fn a_host_tab_not_yet_listed_waits_pending() {
        let seat = SessionSlot::lead("O", "p");
        let mut tabs = crate::tabs::TabRegistry::default();
        tabs.adopt(1, seat.clone(), "p1", "T1", "", "about:blank").expect("the opener");
        let event = crate::control::HostEvent::TabCreated {
            tab: 2,
            opener: Some(1),
            url: "u".to_owned(),
            request: None,
        };
        let waiting = pair_tab_events(&mut tabs, &[], vec![(1, event.clone())], 1);
        assert_eq!(waiting, vec![(1, event)], "no page, no adoption");
        let listing = [crate::tabs::DriverTab {
            index: 0,
            id: "p2".to_owned(),
            target: "T2".to_owned(),
            title: String::new(),
            url: "u".to_owned(),
            current: true,
        }];
        let waiting = pair_tab_events(&mut tabs, &listing, waiting, 1);
        assert!(waiting.is_empty(), "the next pass adopts it");
        assert_eq!(tabs.number_of("p2"), Some(2));
        assert_eq!(tabs.owner(2), Some(&seat), "the opener's owner");

        // **An older receiver's close cannot close a live tab.** The close
        // names tab 2, which generation 1 holds; a generation-0 close is
        // refused, so the guard is what keeps it.
        let stale = crate::control::HostEvent::TabClosed { tab: 2 };
        let waiting = pair_tab_events(&mut tabs, &listing, vec![(0, stale)], 1);
        assert!(waiting.is_empty(), "a stale close is dropped, not re-deferred");
        assert_eq!(tabs.number_of("p2"), Some(2), "the live tab survives the stale close");

        // **A NEWER receiver is the successor's**, not a stale one: its
        // events go back for the next pass, which will carry its generation,
        // rather than being dropped here or paired with this pass's listing.
        let successor = crate::control::HostEvent::TabClosed { tab: 2 };
        let waiting = pair_tab_events(&mut tabs, &listing, vec![(2, successor.clone())], 1);
        assert_eq!(
            waiting,
            vec![(2, successor)],
            "a newer generation is re-deferred, not paired or dropped"
        );
        assert_eq!(tabs.number_of("p2"), Some(2), "and this pass leaves the tab alone");
    }

    /// **A stale listing is a stale driver.** The `_page` error drops the
    /// driver and the retried listing serves the call.
    #[tokio::test]
    async fn a_stale_listing_recovers_the_driver() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let driver = FakeDriver::with_blank("T1");
        driver.stale_once();
        service.install_fake(&seat, driver).await;

        let parts = service
            .call(&seat, "browser_snapshot", json!({}))
            .await
            .expect("the call recovers from the stale listing");
        assert!(
            matches!(parts.first(), Some(forge_primitives::browser::BrowserPart::Text { text }) if text == "ok"),
            "the retried call answers: {parts:?}"
        );
    }

    #[async_trait::async_trait]
    impl crate::driver::TabDriver for FakeDriver {
        async fn tabs_within(
            &self,
            bound: Duration,
        ) -> Result<Vec<crate::tabs::DriverTab>, String> {
            self.listings.fetch_add(1, Ordering::AcqRel);
            self.changed.notify_one();
            if self.slow_listing.load(Ordering::Acquire) {
                return Err(crate::driver::did_not_answer("browser_tabs", bound));
            }
            if self.stale_once.swap(false, Ordering::AcqRel) {
                return Err("browserBackend.callTool: Cannot read properties of undefined \
                            (reading '_page')"
                    .to_owned());
            }
            Ok(self.tabs.lock().expect("the tabs").clone())
        }

        async fn select(&self, index: usize, _bound: Duration) -> Result<(), String> {
            self.selected.lock().expect("the selections").push(index);
            Ok(())
        }

        async fn call_within(
            &self,
            tool: &str,
            _args: serde_json::Value,
            _bound: Duration,
        ) -> Result<Vec<forge_primitives::browser::BrowserPart>, String> {
            if tool == "browser_file_upload" && self.fail_upload.load(Ordering::Acquire) {
                return Err("the driver refused the upload".to_owned());
            }
            if self.attach_timeout.load(Ordering::Acquire) {
                return Err(
                    "### Error\nTimeoutError: async initializeServer: Timeout 30000ms exceeded."
                        .to_owned(),
                );
            }
            if self.fail_calls.load(Ordering::Acquire) {
                return Err(format!("{tool} failed"));
            }
            Ok(vec![forge_primitives::browser::BrowserPart::Text { text: "ok".to_owned() }])
        }

        fn is_running(&self) -> bool {
            true
        }
    }

    /// **A session's first page call creates its tab, and the answer names the
    /// number.** The call then drives that tab and no other.
    #[tokio::test]
    async fn the_first_page_call_creates_a_named_tab() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let driver = FakeDriver::default();
        driver.tabs.lock().expect("the tabs").push(crate::tabs::DriverTab {
            index: 0,
            id: "T1".into(),
            target: "T1".into(),
            title: String::new(),
            url: "about:blank".into(),
            current: true,
        });
        service.install_fake(&seat, driver).await;

        let parts = service
            .call(&seat, "browser_navigate", json!({"url": "http://a/"}))
            .await
            .expect("the call runs");
        assert!(
            parts
                .iter()
                .any(|part| matches!(part, forge_primitives::browser::BrowserPart::Text { text } if text.contains("tab 1"))),
        );
        let tabs = service.tabs(&seat).await.expect("the rows");
        assert_eq!(tabs.rows.len(), 1);
        assert_eq!(tabs.rows[0].owner, seat);
    }

    /// **A composed call's failure, through the whole path, names the tool the
    /// session called.** The scrub sits on the one boundary every error leaves
    /// [`BrowserService::call`] through, so this drives it end to end: the
    /// fake driver answers the synthetic "browser_evaluate failed" a real one
    /// does, and the session must not read that name.
    #[tokio::test]
    async fn a_failed_challenge_call_names_the_caller_not_the_evaluate() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let driver = FakeDriver::with_blank("T1");
        driver.fail_calls();
        service.install_fake(&seat, driver).await;

        let why = service
            .call(&seat, "browser_challenge_state", json!({}))
            .await
            .expect_err("the driver's failure is the call's");
        assert!(why.contains("browser_challenge_state"), "the caller's tool is named: {why}");
        assert!(!why.contains("browser_evaluate"), "the evaluate it ran as is not: {why}");
    }

    /// **Two sessions never drive each other's tab.** The second session's
    /// call creates tab 2 and selects it; the first session's tab stays
    /// current for its own next call.
    #[tokio::test]
    async fn two_sessions_drive_their_own_tabs() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let lead = SessionSlot::lead("O", "p");
        let worker = SessionSlot::worker("O", "p", "w");
        let driver = FakeDriver::with_blank("T1");
        let seen = driver.clone();
        service.install_fake(&lead, driver).await;

        service.call(&lead, "browser_snapshot", json!({})).await.expect("the lead's call");
        seen.push_target("T2");
        service.call(&worker, "browser_snapshot", json!({})).await.expect("the worker's call");
        service.call(&lead, "browser_snapshot", json!({})).await.expect("the lead's second call");

        assert_eq!(seen.selected(), vec![0, 1, 0], "each call selected its caller's tab");
        let rows = service.tabs(&lead).await.expect("the rows").rows;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].owner, lead);
        assert_eq!(rows[1].owner, worker);
    }

    /// A driver that answers slowly, recording how many calls overlapped.
    use crate::driver::TabDriver;
    use forge_primitives::browser::BrowserPart;
    use serde_json::Value;

    struct SlowDriver {
        tabs: Arc<Mutex<Vec<crate::tabs::DriverTab>>>,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl TabDriver for SlowDriver {
        async fn tabs_within(
            &self,
            _bound: Duration,
        ) -> Result<Vec<crate::tabs::DriverTab>, String> {
            Ok(self.tabs.lock().expect("the fake's tabs").clone())
        }

        async fn select(&self, _index: usize, _bound: Duration) -> Result<(), String> {
            Ok(())
        }

        async fn call_within(
            &self,
            _tool: &str,
            _args: Value,
            _bound: Duration,
        ) -> Result<Vec<BrowserPart>, String> {
            let now = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(now, Ordering::AcqRel);
            tokio::time::sleep(Duration::from_millis(80)).await;
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            Ok(vec![BrowserPart::Text { text: "ok".to_owned() }])
        }

        fn is_running(&self) -> bool {
            true
        }
    }

    /// **Two calls on one profile take turns.** The driver's current page is
    /// a single slot, so overlapping calls would swap it under each other.
    #[tokio::test]
    async fn two_calls_on_one_profile_take_turns() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let driver = SlowDriver {
            tabs: Arc::new(Mutex::new(vec![crate::tabs::DriverTab {
                index: 0,
                id: "T1".to_owned(),
                target: "T1".to_owned(),
                title: String::new(),
                url: "about:blank".to_owned(),
                current: true,
            }])),
            in_flight: Arc::clone(&in_flight),
            peak: Arc::clone(&peak),
        };
        service.install_fake(&seat, driver).await;
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the tab is adopted");

        let (first, second) = tokio::join!(
            service.call(&seat, "browser_snapshot", json!({})),
            service.call(&seat, "browser_snapshot", json!({})),
        );
        first.expect("the first call runs");
        second.expect("the second call runs");
        assert_eq!(peak.load(Ordering::Acquire), 1, "the calls never overlapped");
    }

    /// **A call that cannot get its turn is refused in words.** The wait is
    /// bounded, and the refusal names the wait rather than hanging the turn.
    #[tokio::test]
    async fn a_call_that_waits_too_long_is_refused_as_busy() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        service.install_fake(&seat, FakeDriver::with_blank("T1")).await;
        let profile = service.entry(&seat).await.expect("the profile entry");
        let held = profile.call_lock.lock().await;

        let refused =
            service.call(&seat, "browser_snapshot", json!({})).await.expect_err("the call is busy");
        assert!(refused.contains("the browser is busy"), "{refused}");
        drop(held);
        service.call(&seat, "browser_snapshot", json!({})).await.expect("the turn comes back");
    }

    /// **A failed upload does not wedge the profile.** An upload holds the
    /// chooser decision across its driver call, and the failure path takes
    /// the same non-reentrant lock; re-acquiring it there left every later
    /// call and the shutdown waiting forever.
    #[tokio::test]
    async fn a_failed_upload_leaves_the_profile_usable_and_stoppable() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let driver = FakeDriver::with_blank("T1");
        driver.fail_uploads();
        service.install_fake(&seat, driver).await;
        let file = dir.path().join("upload.txt");
        std::fs::write(&file, b"the page's file").expect("the upload file");

        let upload = tokio::time::timeout(
            Duration::from_secs(5),
            service.call(&seat, "browser_file_upload", json!({ "paths": [file] })),
        )
        .await
        .expect("the failed upload answers rather than deadlocking");
        assert!(upload.is_err(), "the driver's failure is the call's");

        let control = tokio::time::timeout(
            Duration::from_secs(5),
            service.call(&seat, "browser_snapshot", json!({})),
        )
        .await
        .expect("the profile still answers a call");
        control.expect("the control call runs");

        tokio::time::timeout(Duration::from_secs(5), service.stop_all())
            .await
            .expect("the stop completes");
    }

    /// **Warming a profile opens its host without a tab.** A seat's first call
    /// is still what opens one, so a spawn that warms pays the launch early
    /// without leaving an empty tab in the strip.
    #[tokio::test]
    async fn warming_a_profile_opens_its_host_without_a_tab() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        // The test host is stubbed as in the other service tests.
        service.install_fake(&seat, FakeDriver::default()).await;

        service.warm(&seat).await;

        let profile = service.find("O/default").await.expect("the entry exists");
        assert!(
            profile.tabs.lock().expect("the tabs").numbers().is_empty(),
            "warming starts the profile, never a tab",
        );
    }

    /// **A session that goes takes its tabs with it, except a parked hand-off's.**
    #[tokio::test]
    async fn a_sweep_closes_a_gone_session_tabs_but_not_a_held_one() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let lead = SessionSlot::lead("O", "p");
        let worker = SessionSlot::worker("O", "p", "w");
        let driver = FakeDriver::with_blank("T1");
        service.install_fake(&lead, driver.clone()).await;
        service.call(&lead, "browser_snapshot", json!({})).await.expect("the lead's tab");
        let lead_tab = service.tabs(&lead).await.expect("rows").rows[0].number;
        driver.push_target("T2");
        service.call(&worker, "browser_snapshot", json!({})).await.expect("the worker's tab");
        let worker_tab = service.tabs(&worker).await.expect("rows").rows[1].number;

        service.hand_off_started(&worker, worker_tab).await.expect("the tab is held");
        service.sweep_sessions(std::slice::from_ref(&lead), 1).await;
        let rows = service.tabs(&lead).await.expect("rows").rows;
        assert!(rows.iter().any(|row| row.number == lead_tab));
        assert!(rows.iter().any(|row| row.number == worker_tab), "the held tab survives its owner");
        assert_eq!(
            rows.iter().find(|row| row.number == worker_tab).map(|row| row.state),
            Some(forge_primitives::browser::BrowserTabState::Held),
        );

        service.hand_off_finished(&worker, worker_tab).await;
        let rows = service.tabs(&lead).await.expect("rows").rows;
        assert!(
            !rows.iter().any(|row| row.number == worker_tab),
            "it closes once the owner is gone"
        );
    }

    /// **The rows say how the profile is doing and when it closes.** A strip
    /// drawing "connected" or "closing in Nm" reads these fields, so a missing
    /// one is a row that lies.
    #[tokio::test]
    async fn the_rows_carry_connection_and_the_idle_deadline() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        service.configure_idle(30);
        let lead = SessionSlot::lead("O", "p");
        service.install_fake(&lead, FakeDriver::with_blank("T1")).await;
        service.call(&lead, "browser_snapshot", json!({})).await.expect("the call");

        let list = service.tabs(&lead).await.expect("rows");
        assert!(list.up, "the host is up");
        assert!(list.connected, "the fake driver is running");
        let deadline = list.closing_at_ms.expect("a live profile has an idle deadline");
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock")
                .as_millis(),
        )
        .expect("the clock fits");
        assert!(deadline > now + 29 * 60_000, "the deadline is the 30-minute bound");
        assert!(deadline <= now + 30 * 60_000 + 5_000, "and not further out");
    }

    /// **A replacement keeps the slot and loses the tabs.** `/new` and `/resume`
    /// are the same seat with a new occupant, so the previous occupant's tabs go
    /// the way a despawn's do.
    #[tokio::test]
    async fn a_replacement_closes_the_inherited_tabs() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let lead = SessionSlot::lead("O", "p");
        service.install_fake(&lead, FakeDriver::with_blank("T1")).await;
        service.call(&lead, "browser_snapshot", json!({})).await.expect("the call");
        assert_eq!(service.tabs(&lead).await.expect("rows").rows.len(), 1);

        service.replace_seat(&lead).await;
        assert!(
            service.tabs(&lead).await.expect("rows").rows.is_empty(),
            "the old occupant's tab is gone",
        );
    }

    /// **The boot tab is not a session's until one adopts it.** It is absent
    /// from the list, cannot be selected or closed, and never reaches a row.
    #[tokio::test]
    async fn the_boot_tab_is_invisible_and_untouchable_until_adopted() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let lead = SessionSlot::lead("O", "p");
        service
            .install_fake_with_boot(&lead, FakeDriver::with_blank("T1"), (1, "T1".to_owned()))
            .await;

        assert!(
            service.tabs(&lead).await.expect("rows").rows.is_empty(),
            "the boot tab is not a row",
        );
        let parts =
            service.call(&lead, "browser_tabs", json!({"action": "list"})).await.expect("the list");
        assert!(
            parts.iter().all(|part| matches!(part, forge_primitives::browser::BrowserPart::Text { text } if !text.contains("tab 1"))),
            "the list does not name the boot tab",
        );
        let refused =
            service.call(&lead, "browser_tabs", json!({"action": "select", "index": 1})).await;
        assert!(refused.is_err(), "a session cannot select the boot tab");

        service.call(&lead, "browser_snapshot", json!({})).await.expect("the page call adopts it");
        let rows = service.tabs(&lead).await.expect("rows").rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, 1);
        assert_eq!(rows[0].owner, lead);
    }

    /// **A stale hand-off cannot close a new host's tab.** The old host's
    /// numbers die with it; a resolution that arrives later finds nothing held.
    #[tokio::test]
    async fn a_stale_hand_off_never_closes_a_new_hosts_tab() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let worker = SessionSlot::worker("O", "p", "w");
        let lead = SessionSlot::lead("O", "p");
        service
            .install_fake_with_boot(&lead, FakeDriver::with_blank("T1"), (1, "T1".to_owned()))
            .await;
        service
            .call(&worker, "browser_snapshot", json!({}))
            .await
            .expect("the worker adopts tab 1");
        service.hand_off_started(&worker, 1).await.expect("the tab is held");

        // The host ends; the registry and holds go with it.
        let profile = service.find("O/default").await.expect("the profile");
        profile.end(HostEnd::Closed).await;
        service
            .install_fake_with_boot(&lead, FakeDriver::with_blank("T2"), (1, "T2".to_owned()))
            .await;
        service
            .call(&lead, "browser_snapshot", json!({}))
            .await
            .expect("the lead adopts the new tab 1");

        service.hand_off_finished(&worker, 1).await;
        let rows = service.tabs(&lead).await.expect("rows").rows;
        assert_eq!(rows.len(), 1, "the new host's tab 1 is not the old hand-off's");
        assert_eq!(rows[0].owner, lead);
    }

    /// **A hand-off is refused by name on a tab that is not the seat's.** An
    /// ask parked on a stranger's tab sends the person to a page the session
    /// never opened; an unknown or closed number is the same refusal.
    #[tokio::test]
    async fn a_hand_off_is_refused_on_a_tab_that_is_not_the_seats() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let worker = SessionSlot::worker("O", "p", "w");
        let lead = SessionSlot::lead("O", "p");
        service.install_fake(&lead, FakeDriver::with_blank("T1")).await;
        service
            .call(&worker, "browser_snapshot", json!({}))
            .await
            .expect("the worker adopts tab 1");

        assert_eq!(
            service.hand_off_started(&lead, 1).await,
            Err(format!("tab 1 belongs to {}", worker.display())),
            "another session's tab is refused",
        );
        assert_eq!(
            service.hand_off_started(&worker, 9).await,
            Err("tab 9 is not open".to_owned()),
            "an unknown number is refused",
        );
        service.call(&worker, "browser_close", json!({})).await.expect("the tab closes");
        assert_eq!(
            service.hand_off_started(&worker, 1).await,
            Err("tab 1 is not open".to_owned()),
            "a closed tab is refused",
        );
    }

    /// **Done on a live seat's tab leaves it open.** The seat that touched
    /// the browser is live even when no sweep has run; a swept-only live set
    /// treated it as gone and closed its tab the moment the person answered.
    #[tokio::test]
    async fn a_hand_off_done_on_a_live_seats_tab_leaves_it_open() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let worker = SessionSlot::worker("O", "p", "w");
        let lead = SessionSlot::lead("O", "p");
        service
            .install_fake_with_boot(&lead, FakeDriver::with_blank("T1"), (1, "T1".to_owned()))
            .await;
        service
            .call(&worker, "browser_snapshot", json!({}))
            .await
            .expect("the worker adopts tab 1, which makes it live");
        service.hand_off_started(&worker, 1).await.expect("the tab is held");

        service.hand_off_finished(&worker, 1).await;

        let rows = service.tabs(&worker).await.expect("rows").rows;
        assert_eq!(rows.len(), 1, "the seat's own tab survives its hand-off");
        assert_eq!(rows[0].owner, worker);
    }

    /// **A closed seat's held tab goes when Done lands**, even though the
    /// sweep rebuilds `live` from configuration and still lists the lead.
    /// The workspace's session-ended tombstone is what the fleet answers
    /// from, and the sweep cannot re-add it.
    #[tokio::test]
    async fn a_closed_seats_held_tab_goes_when_done_lands() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let worker = SessionSlot::worker("O", "p", "w");
        let lead = SessionSlot::lead("O", "p");
        service
            .install_fake_with_boot(&lead, FakeDriver::with_blank("T1"), (1, "T1".to_owned()))
            .await;
        service
            .call(&worker, "browser_snapshot", json!({}))
            .await
            .expect("the worker adopts tab 1, which makes it live");
        service.hand_off_started(&worker, 1).await.expect("the tab is held");

        // The sweep sees both configured seats and would call the worker
        // live again; the tombstone is what it cannot re-add.
        service.sweep_sessions(&[lead.clone(), worker.clone()], 1).await;
        service.session_ended(&worker);
        service.sweep_sessions(&[lead, worker.clone()], 2).await;
        service.hand_off_finished(&worker, 1).await;

        let rows = service.tabs(&worker).await.expect("rows").rows;
        assert!(rows.is_empty(), "Done closes the tab of a seat that ended");
    }

    /// **An older sweep cannot overwrite a newer one.** Snapshots are captured
    /// on the command path and applied from spawned tasks, so the lock's order
    /// is not the capture order: the newer snapshot drops an ended worker's
    /// tombstone by not listing it, and the older one, applied last, would
    /// rebuild `live` with the worker in it and let its held tab survive Done.
    #[tokio::test]
    async fn an_older_sweep_cannot_re_admit_an_ended_seat() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let worker = SessionSlot::worker("O", "p", "w");
        let lead = SessionSlot::lead("O", "p");
        service
            .install_fake_with_boot(&lead, FakeDriver::with_blank("T1"), (1, "T1".to_owned()))
            .await;
        service
            .call(&worker, "browser_snapshot", json!({}))
            .await
            .expect("the worker adopts tab 1, which makes it live");
        service.hand_off_started(&worker, 1).await.expect("the tab is held");

        service.session_ended(&worker);
        // The newer snapshot no longer lists the worker, so applying it drops
        // the tombstone; the older one, applied last, still lists it.
        service.sweep_sessions(std::slice::from_ref(&lead), 2).await;
        service.sweep_sessions(&[lead.clone(), worker.clone()], 1).await;

        let live = service.live.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(live.contains(&lead), "the newer snapshot is applied, not ignored");
        assert!(
            !live.contains(&worker),
            "the older snapshot must not re-admit a seat the newer one dropped",
        );
        service.hand_off_finished(&worker, 1).await;
        let rows = service.tabs(&worker).await.expect("rows").rows;
        assert!(
            rows.is_empty(),
            "a stale snapshot cannot keep the ended seat's tab open past Done",
        );
    }

    /// **The relayed sequence never goes backwards across hosts.** A host
    /// restarts its own count at zero; the service's counter is what the
    /// viewer's newest-wins gate reads.
    #[tokio::test]
    async fn the_relayed_frame_sequence_is_the_services_and_only_rises() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let profile = service.entry(&seat).await.expect("a profile entry");
        let first = profile.next_frame_seq(1);
        let second = profile.next_frame_seq(1);
        assert_eq!(second, first + 1, "one tab's frames rise one at a time");
        profile.host_gone();
        let after = profile.next_frame_seq(1);
        assert!(after > second, "a host's death does not reset the wire's sequence");
    }

    /// **The tool's own rows name the caller's tab and its owner.** A list
    /// that cannot say which row is the current one, or whose it is, leaves
    /// the session driving by a number it has to guess at.
    #[tokio::test]
    async fn render_tabs_names_the_current_row_and_its_owner() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let worker = SessionSlot::worker("O", "p", "w");
        let lead = SessionSlot::lead("O", "p");
        let profile = service.entry(&lead).await.expect("a profile entry");
        seed_tab(&profile, &worker);
        profile
            .tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .adopt(2, lead.clone(), "T2", "T2", "second", "http://b/")
            .expect("the lead's tab");

        let text = service.render_tabs(&profile, &worker);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("- 1: (current) "), "the caller's tab is marked: {text}");
        assert!(lines[0].ends_with(" - you"), "and reads as the caller's: {text}");
        assert!(
            lines.iter().any(|line| line.contains("O/p/lead")),
            "the other row names its owner"
        );
    }

    /// **An idle pass forgets a restarted profile's parks.** A dead host's
    /// hold is not a person waiting; without the pass's forget it kept the
    /// fresh host from ever closing on its idle clock.
    #[tokio::test]
    async fn an_idle_pass_forgets_a_restarted_profiles_parks() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        service.configure_idle(30);
        let worker = SessionSlot::worker("O", "p", "w");
        let lead = SessionSlot::lead("O", "p");
        service
            .install_fake_with_boot(&lead, FakeDriver::with_blank("T1"), (1, "T1".to_owned()))
            .await;
        service
            .call(&worker, "browser_snapshot", json!({}))
            .await
            .expect("the worker adopts tab 1");
        service.hand_off_started(&worker, 1).await.expect("the tab is held");
        let profile = service.find("O/default").await.expect("the profile");
        profile.host_gone();
        assert!(!service.held_tabs("O/default").is_empty(), "the park outlived its host");

        service.idle_pass().await;

        assert!(service.held_tabs("O/default").is_empty(), "the pass forgets the dead host's park");
    }

    /// **A host that dies takes its parks with it.** A hold that outlived its
    /// host would keep the successor from ever closing on its idle clock,
    /// with nothing left to answer it.
    #[tokio::test]
    async fn a_dead_host_takes_its_parks_with_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let worker = SessionSlot::worker("O", "p", "w");
        let lead = SessionSlot::lead("O", "p");
        service
            .install_fake_with_boot(&lead, FakeDriver::with_blank("T1"), (1, "T1".to_owned()))
            .await;
        service
            .call(&worker, "browser_snapshot", json!({}))
            .await
            .expect("the worker adopts tab 1");
        service.hand_off_started(&worker, 1).await.expect("the tab is held");
        assert!(!service.held_tabs("O/default").is_empty(), "the park is held");

        // The host dies as the monitor records it; the next open reads the
        // restart and forgets the park.
        let profile = service.find("O/default").await.expect("the profile");
        profile.host_gone();
        service.open(&worker).await.expect("open");

        assert!(service.held_tabs("O/default").is_empty(), "the dead host's park is gone");
    }

    /// **A dead host's rows go with it**, so a strip never points at tabs
    /// nothing is running.
    #[tokio::test]
    async fn a_dead_host_takes_its_rows_with_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let worker = SessionSlot::worker("O", "p", "w");
        let lead = SessionSlot::lead("O", "p");
        service
            .install_fake_with_boot(&lead, FakeDriver::with_blank("T1"), (1, "T1".to_owned()))
            .await;
        service
            .call(&worker, "browser_snapshot", json!({}))
            .await
            .expect("the worker adopts tab 1");
        assert_eq!(service.tabs(&worker).await.expect("rows").rows.len(), 1);

        let profile = service.find("O/default").await.expect("the profile");
        profile.deferred_tab_events.lock().unwrap_or_else(PoisonError::into_inner).push((
            profile.host_generation.load(Ordering::Acquire),
            crate::control::HostEvent::TabCreated {
                tab: 9,
                opener: Some(1),
                url: "http://stale/".to_owned(),
                request: None,
            },
        ));
        profile.host_gone();

        // **The death is pushed at once**, so a subscribed strip with no
        // takeover does not keep drawing dead tabs until a sweep.
        let pushed = profile.rows.borrow().clone();
        assert!(!pushed.up, "the strip is told the browser is down");
        assert!(pushed.rows.is_empty(), "and has no tabs to draw");
        assert!(
            profile.deferred_tab_events.lock().unwrap_or_else(PoisonError::into_inner).is_empty(),
            "a stale popup event cannot pair against the successor",
        );

        let rows = service.tabs(&worker).await.expect("rows").rows;
        assert!(rows.is_empty(), "the dead host's rows are gone");
    }
}

/// The server's browser guards, each one line from a defect that was live the
/// round before: the reconcile's per-target generation re-read (and the
/// deferred-events branch of its moved-host check), the sweep's lock, the `up`
/// flag, and a second ending call's hands off a successor's rows. The
/// reconcile's separate moved-host orphan skip was dropped: the per-target
/// re-read spares the same pages, so the branch was redundant.
#[cfg(test)]
mod server_guards {
    use super::tests::{Caught, FakeDriver, ended, records, saw, seed_tab};
    use super::{BrowserService, Call, Driven, Profile, pair_tab_events};
    use crate::control::HostEvent;
    use crate::driver::TabDriver;
    use crate::tabs::DriverTab;
    use forge_primitives::SessionSlot;
    use forge_primitives::browser::BrowserPart;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;
    use tracing_subscriber::layer::SubscriberExt as _;

    /// One row of a driver's listing.
    fn row(index: u16, target: &str) -> DriverTab {
        DriverTab {
            index: usize::from(index - 1),
            id: target.to_owned(),
            target: target.to_owned(),
            title: String::new(),
            url: "about:blank".to_owned(),
            current: index == 1,
        }
    }

    /// A host endpoint that records every `GET /json/close/<target>` it is sent
    /// and answers with an empty body, so the caller returns instead of timing
    /// out. `on_close` is told the 1-based number of each close as it lands.
    fn close_endpoint(on_close: impl Fn(usize) + Send + 'static) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("a test port");
        let port = listener.local_addr().expect("an address").port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            // Bounded, so the thread ends with the test's closes rather than
            // holding the listener open for the life of the process.
            for _ in 0..8 {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = [0_u8; 1024];
                let read = std::io::Read::read(&mut stream, &mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_owned();
                let count = {
                    let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
                    log.push(path);
                    log.len()
                };
                on_close(count);
                let _ = std::io::Write::write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
                );
            }
        });
        (port, seen)
    }

    /// A driver whose listing is fixed, with an optional one-shot generation
    /// bump: what a host death during the pass looks like to `reconcile`.
    struct ListingDriver {
        listing: Vec<DriverTab>,
        bump: Option<(Arc<Profile>, u64)>,
        bumped: AtomicBool,
    }

    #[async_trait::async_trait]
    impl TabDriver for ListingDriver {
        async fn tabs_within(&self, _bound: Duration) -> Result<Vec<DriverTab>, String> {
            if let Some((profile, by)) = &self.bump
                && !self.bumped.swap(true, Ordering::AcqRel)
            {
                profile.host_generation.fetch_add(*by, Ordering::AcqRel);
            }
            Ok(self.listing.clone())
        }

        async fn select(&self, _index: usize, _bound: Duration) -> Result<(), String> {
            Ok(())
        }

        async fn call_within(
            &self,
            _tool: &str,
            _args: Value,
            _bound: Duration,
        ) -> Result<Vec<BrowserPart>, String> {
            Ok(Vec::new())
        }

        fn is_running(&self) -> bool {
            true
        }
    }

    fn driver(listing: Vec<DriverTab>, bump: Option<(Arc<Profile>, u64)>) -> Arc<dyn TabDriver> {
        Arc::new(ListingDriver { listing, bump, bumped: AtomicBool::new(false) })
    }

    /// A service and the profile its guards run against: a seat installed with
    /// an empty listing, so the registry starts with no rows of its own.
    async fn profile_for_test(service: &BrowserService) -> Arc<Profile> {
        let seat = SessionSlot::lead("O", "p");
        service
            .install_fake(
                &seat,
                ListingDriver { listing: Vec::new(), bump: None, bumped: AtomicBool::new(false) },
            )
            .await;
        service.find("O/default").await.expect("the profile")
    }

    /// **An event deferred while the host is dying goes back for the next
    /// pass.** The pass takes the deferred list from the listing and then
    /// drains the queue; holding the queue parks it between the two. If the
    /// generation moved under it, what it did not pair is added back rather
    /// than replacing the list, so the event another path deferred meanwhile
    /// is not wiped.
    #[tokio::test]
    async fn a_reconcile_that_lost_its_host_keeps_an_event_deferred_under_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let generation = profile.host_generation.load(Ordering::Acquire);
        let queue = profile.host_tab_events.lock().await;
        let listing = driver(Vec::new(), Some((Arc::clone(&profile), 1)));
        let pass = {
            let service = Arc::clone(&service);
            let profile = Arc::clone(&profile);
            tokio::spawn(async move { service.reconcile(&profile, &listing, 9).await })
        };
        tokio::task::yield_now().await;
        profile
            .deferred_tab_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((generation, HostEvent::TabClosed { tab: 99 }));
        drop(queue);
        pass.await.expect("the pass completes");

        assert_eq!(
            profile.host_generation.load(Ordering::Acquire),
            generation + 1,
            "the pass never saw a moved host, so the test pins nothing"
        );
        let deferred =
            profile.deferred_tab_events.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(
            deferred.iter().any(|(_, event)| matches!(event, HostEvent::TabClosed { tab: 99 })),
            "an event deferred while the host was replaced was wiped: {deferred:?}"
        );
    }

    /// The control: on a host that stands, the pass replaces the deferred list
    /// with its own pairing, so the same concurrent defer is dropped.
    #[tokio::test]
    async fn a_reconcile_on_one_host_replaces_the_events_deferred_under_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let generation = profile.host_generation.load(Ordering::Acquire);
        let queue = profile.host_tab_events.lock().await;
        let listing = driver(Vec::new(), None);
        let pass = {
            let service = Arc::clone(&service);
            let profile = Arc::clone(&profile);
            tokio::spawn(async move { service.reconcile(&profile, &listing, 9).await })
        };
        tokio::task::yield_now().await;
        profile
            .deferred_tab_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((generation, HostEvent::TabClosed { tab: 99 }));
        drop(queue);
        pass.await.expect("the pass completes");

        let deferred =
            profile.deferred_tab_events.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(
            !deferred.iter().any(|(_, event)| matches!(event, HostEvent::TabClosed { tab: 99 })),
            "a stable pass kept an event it did not pair: {deferred:?}"
        );
    }

    /// **A pairing pass that runs out of time puts its births back.** The
    /// pass is bounded by a timeout, and a driver recovery that never answers
    /// cancels the future where it stands; a birth taken and dropped with it
    /// is a popup that never pairs, and the next pass prunes its page as an
    /// orphan.
    #[tokio::test]
    async fn a_cancelled_pairing_pass_puts_its_births_back() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let installed = FakeDriver::with_blank("T1");
        service.install_fake(&seat, installed).await;
        let profile = service.find("O/default").await.expect("the profile");
        let generation = profile.host_generation.load(Ordering::Acquire);
        let (births, taken) = tokio::sync::mpsc::unbounded_channel();
        *profile.host_tab_events.lock().await = Some(taken);
        births
            .send((
                generation,
                HostEvent::TabCreated {
                    tab: 2,
                    opener: Some(1),
                    url: "u".to_owned(),
                    request: None,
                },
            ))
            .expect("the birth is queued");

        // **The driver recovery is the pass's slow step**, and it takes this
        // lock first: holding it parks the pass where a slow recovery would,
        // with the births already taken.
        let rebuilding = profile.rebuild.lock().await;
        let listing = driver(Vec::new(), None);
        let pass = tokio::time::timeout(
            Duration::from_millis(250),
            service.reconcile(&profile, &listing, 9),
        )
        .await;
        drop(rebuilding);
        assert!(pass.is_err(), "the pass was not the cancelled one the test means to pin");

        let deferred =
            profile.deferred_tab_events.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(
            deferred.iter().any(|(_, event)| matches!(event, HostEvent::TabCreated { tab: 2, .. })),
            "a cancelled pass ate the popup's birth: {deferred:?}"
        );
    }

    /// **A birth for a tab the host has ended is answered, not kept.** A popup
    /// the host gave up closed before its page existed, and a birth kept for
    /// it would be re-deferred on every pass - a driver recovery each time,
    /// for a page that will never be listed.
    #[test]
    fn a_birth_for_a_closed_tab_is_dropped() {
        let mut tabs = crate::tabs::TabRegistry::default();
        tabs.close(9);
        let birth =
            HostEvent::TabCreated { tab: 9, opener: Some(1), url: "u".to_owned(), request: None };
        let waiting = pair_tab_events(&mut tabs, &[], vec![(1, birth)], 1);
        assert!(waiting.is_empty(), "a birth for a closed tab is dropped: {waiting:?}");

        // The control: the same birth for a tab nobody closed waits.
        let live =
            HostEvent::TabCreated { tab: 10, opener: Some(1), url: "u".to_owned(), request: None };
        let waiting = pair_tab_events(&mut tabs, &[], vec![(1, live.clone())], 1);
        assert_eq!(waiting, vec![(1, live)], "a birth with no page and no close waits");
    }

    /// **A pairing pass that could not list tries again.** The birth is still
    /// the receiver's when the listing fails, so a retry keyed on the deferred
    /// list alone would never fire and the popup would wait for a call that
    /// never comes.
    #[tokio::test]
    async fn a_pairing_pass_that_could_not_list_tries_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let fake = FakeDriver::with_blank("T1");
        fake.stale_once();
        fake.push_target("T2");
        service.install_fake(&seat, fake).await;
        let profile = service.find("O/default").await.expect("the profile");
        seed_tab(&profile, &seat);
        let ring_name = format!("/forge-pair-retry-{}", std::process::id());
        let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
        let (input, requests) = std::sync::mpsc::channel();
        profile.state.lock().await.host = Some(crate::spawn::ActiveHost {
            pid: std::process::id(),
            port: 9,
            ring: Arc::new(ring),
            profile: dir.path().join("pair-retry"),
            input,
            reader: None,
            monitor: None,
        });
        let generation = profile.host_generation.load(Ordering::Acquire);
        let (births, taken) = tokio::sync::mpsc::unbounded_channel();
        *profile.host_tab_events.lock().await = Some(taken);
        births
            .send((
                generation,
                HostEvent::TabCreated {
                    tab: 2,
                    opener: Some(1),
                    url: "about:blank".to_owned(),
                    request: None,
                },
            ))
            .expect("the birth is queued");
        let _ = service.pairing.send(super::PairingRequest {
            key: "O/default".to_owned(),
            tab: 2,
            opener: 1,
            url: "about:blank".to_owned(),
            until: tokio::time::Instant::now() + Duration::from_secs(5),
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let paired =
                    profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).number_of("T2")
                        == Some(2);
                if paired {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("a pass that could not list tried again and paired the popup");
        drop(requests);
        crate::osr::Ring::unlink(&ring_name);
    }

    /// **The service starts its own pairing pass.** A birth with no call
    /// behind it has to become a row on its own, and a pass started only by
    /// the boot's one call is a line a later edit can drop without a test
    /// going red.
    #[tokio::test]
    async fn the_pairing_pass_starts_with_the_service() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while service.pairing_queue.lock().unwrap_or_else(PoisonError::into_inner).is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the service's construction starts the pairing pass");
    }

    /// **The host can move again between closes**, and a successor normally
    /// reuses the freed port: the generation is re-read before each one, so the
    /// page after the move is left to the successor.
    #[tokio::test]
    async fn a_reconcile_stops_closing_when_the_host_moves_between_pages() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let moving = Arc::clone(&profile);
        let (port, seen) = close_endpoint(move |count| {
            if count == 1 {
                moving.host_generation.fetch_add(1, Ordering::AcqRel);
            }
        });
        let listing = driver(vec![row(1, "A"), row(2, "B")], None);

        service.reconcile(&profile, &listing, port).await;

        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(
            seen,
            ["/json/close/A"],
            "a close after the host moved took the successor's page"
        );
    }

    /// The control for the re-read: on one host both pages are closed.
    #[tokio::test]
    async fn a_reconcile_on_one_host_closes_every_orphan() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let (port, seen) = close_endpoint(|_| {});
        let listing = driver(vec![row(1, "A"), row(2, "B")], None);

        service.reconcile(&profile, &listing, port).await;

        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert_eq!(seen, ["/json/close/A", "/json/close/B"], "an orphan was left open");
    }

    /// A host standing on the profile with the pid the test names, so an
    /// exited process can stand in for a host that had already gone.
    async fn install_host_with_pid(
        profile: &Arc<Profile>,
        dir: &std::path::Path,
        pid: u32,
        port: u16,
    ) -> (super::HostIdentity, String) {
        install_host_with_monitor(profile, dir, pid, port, None).await
    }

    /// A host standing on the profile with the monitor the test gives it, so
    /// a teardown can be shown to leave a dead host's monitor alive.
    async fn install_host_with_monitor(
        profile: &Arc<Profile>,
        dir: &std::path::Path,
        pid: u32,
        port: u16,
        monitor: Option<tokio::task::JoinHandle<()>>,
    ) -> (super::HostIdentity, String) {
        let ring_name = format!("/forge-guard-{pid}-{port}");
        let generation = {
            let mut state = profile.state.lock().await;
            let generation = state.driver_generation;
            let ring = crate::osr::Ring::create(&ring_name, 4, 4).expect("a test ring");
            let (input, _input_rx) = std::sync::mpsc::channel();
            state.host = Some(crate::spawn::ActiveHost {
                pid,
                port,
                ring: Arc::new(ring),
                profile: dir.join(format!("host-{port}")),
                input,
                reader: None,
                monitor,
            });
            generation
        };
        (super::HostIdentity { pid, port, generation }, ring_name)
    }

    /// A host standing on the profile, as a call that drove it would have left
    /// it: an identity to end, plus the ring name the test unlinks.
    async fn install_host(
        profile: &Arc<Profile>,
        dir: &std::path::Path,
        port: u16,
    ) -> (super::HostIdentity, String) {
        install_host_with_pid(profile, dir, std::process::id(), port).await
    }

    /// Write a stand-in host executable.
    fn write_exec(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, body).expect("the stand-in host");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in host is executable");
    }

    /// Spawn a stand-in host through `spawn_with`, and wait until it has
    /// exited. `gone` leaves the status for the monitor to read.
    fn spawn_stub(
        exe: &std::path::Path,
        profile: &Arc<Profile>,
        dir: &std::path::Path,
    ) -> (u32, u16) {
        let port = crate::spawn::free_port().expect("a free port");
        let ring = format!("/forge-stub-{}-{port}", std::process::id());
        let frames = dir.join("frames.sock");
        let (pid, _input, _events) =
            crate::spawn::spawn_with(exe, &profile.dir, port, &ring, 4, 4, 0, (15, 75), &frames)
                .expect("the stand-in host spawns");
        let mut waited = 0;
        while !crate::spawn::gone(pid) {
            assert!(waited < 5_000, "the stand-in host did not exit");
            std::thread::sleep(Duration::from_millis(5));
            waited += 5;
        }
        (pid, port)
    }

    /// **One sweep at a time**: a sweep waits for the one in front of it rather
    /// than racing its writes.
    #[tokio::test]
    async fn a_sweep_waits_for_the_one_in_front_of_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let _profile = profile_for_test(&service).await;
        let seat = SessionSlot::lead("O", "p");

        let held = service.sweep_lock.lock().await;
        let pass = {
            let service = Arc::clone(&service);
            let seat = seat.clone();
            tokio::spawn(async move { service.sweep_sessions(&[seat], 5).await })
        };
        tokio::task::yield_now().await;
        assert!(
            service.live.lock().unwrap_or_else(PoisonError::into_inner).is_empty(),
            "a sweep applied while another held the sweep lock"
        );
        drop(held);
        pass.await.expect("the sweep completes");
        assert!(
            service.live.lock().unwrap_or_else(PoisonError::into_inner).contains(&seat),
            "the sweep never applied once the lock was free"
        );
    }

    /// **An ended host leaves the profile reading down.** This pins the final
    /// state, not the ordering against `host_gone()`'s rows: the flag is
    /// cleared on the ending path, and a read after the end sees false.
    #[tokio::test]
    async fn ending_a_host_clears_the_profiles_up_flag() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let (identity, ring) = install_host(&profile, dir.path(), 9).await;
        profile.up.store(true, Ordering::Release);

        let _ = service.end_dead_host(&profile, identity, "the test ends it").await;

        assert!(
            !profile.up.load(Ordering::Acquire),
            "the profile still reads as up after its host was ended"
        );
        crate::osr::Ring::unlink(&ring);
    }

    /// **A second ending call leaves a successor's rows alone.** The identity
    /// match refuses a host this call did not drive - the successor on the
    /// same pid and port has a newer driver generation - so it neither
    /// publishes a second sentence nor wipes the successor's rows. (The
    /// take-before-sentence ordering within one call has no observable of its
    /// own: the whole block holds the state lock, so the identity match is the
    /// discriminator.)
    #[tokio::test]
    async fn a_second_ending_call_leaves_a_successors_rows_alone() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let seat = SessionSlot::lead("O", "p");
        let (first, first_ring) = install_host(&profile, dir.path(), 9).await;
        profile.up.store(true, Ordering::Release);

        let _ = service.end_dead_host(&profile, first, "the first call ends it").await;
        let sentence = profile.failures.borrow().clone();

        // A successor starts on the SAME port - the freed one it reuses - with
        // its own driver generation and a row of its own.
        let (_, second_ring) = install_host(&profile, dir.path(), 9).await;
        profile.state.lock().await.driver_generation += 1;
        profile
            .tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .adopt(1, seat, "T2", "T2", "", "")
            .expect("the successor's row");

        let again = service.end_dead_host(&profile, first, "a second call ends it").await;

        assert!(
            matches!(again, super::DeadHost::NotOurs),
            "a second call took a host that is not the one it drove"
        );
        assert_eq!(
            profile.failures.borrow().clone(),
            sentence,
            "a second call published its own ending"
        );
        assert!(
            profile.state.lock().await.host.is_some(),
            "a second call took the successor's host"
        );
        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).numbers(),
            vec![1],
            "a second call wiped the successor's rows"
        );
        crate::osr::Ring::unlink(&first_ring);
        crate::osr::Ring::unlink(&second_ring);
    }

    /// **The level says whether the end was forge's problem.** A fault that
    /// made a host unusable is a warning; releasing an idle profile is not.
    #[test]
    fn the_level_says_whether_the_end_was_forge_s_problem() {
        for (end, level) in [
            (super::HostEnd::Undrivable, tracing::Level::WARN),
            (super::HostEnd::NeverReady, tracing::Level::WARN),
            (super::HostEnd::Unresponsive, tracing::Level::WARN),
            (super::HostEnd::Idle, tracing::Level::INFO),
            (super::HostEnd::Closed, tracing::Level::INFO),
            (super::HostEnd::Stopped, tracing::Level::INFO),
        ] {
            assert_eq!(end.level(), level, "{end:?} reads at {level}");
        }
    }

    /// **A deliberate end is a warning that names itself**, with the profile,
    /// pid, port, the cause and the host's own last words, so a reader can act
    /// on it without the seat that ended the host.
    #[tokio::test]
    async fn an_ended_host_is_a_warning_that_names_itself_and_its_last_words() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let (identity, ring) = install_host(&profile, dir.path(), 10).await;
        std::fs::create_dir_all(&profile.dir).expect("the profile dir");
        std::fs::write(
            profile.dir.join("cef-host.log"),
            "first line\nsecond line\nthird line\nfourth line\nfifth line\n",
        )
        .expect("the host log");

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let host = service
            .end_dead_host(&profile, identity, "the driver's initializeServer timed out")
            .await;
        drop(guard);

        assert!(matches!(host, super::DeadHost::Ended { .. }), "the host was not ended: {host:?}");
        let records = ended(&caught);
        assert_eq!(records.len(), 1, "one end, one record: {records:?}");
        let (level, fields) = &records[0];
        assert_eq!(*level, tracing::Level::WARN, "an end forge caused reads at WARN: {fields}");
        for needle in [
            format!("profile={}", profile.key()),
            format!("pid={}", std::process::id()),
            "port=10".to_owned(),
            "cause=no driver could drive it: the driver's initializeServer timed out".to_owned(),
            "outcome=ended".to_owned(),
            "second line".to_owned(),
            "fifth line".to_owned(),
        ] {
            assert!(fields.contains(&needle), "the record carries `{needle}`: {fields}");
        }
        assert!(!fields.contains("first line"), "the tail keeps the last four lines: {fields}");
        crate::osr::Ring::unlink(&ring);
    }

    /// **A host that had already exited is not an end.** A teardown that
    /// finds it gone writes nothing.
    #[tokio::test]
    async fn a_host_that_already_exited_is_not_reported_as_ended() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let mut exited = std::process::Command::new("true").spawn().expect("a child");
        let pid = exited.id();
        exited.wait().expect("the child exits");
        let (_, ring) = install_host_with_pid(&profile, dir.path(), pid, 11).await;

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        // **The capture is live**: without this sentinel a refactor that
        // stops the layer firing would pass this test by catching nothing.
        tracing::info!(event_name = "test_sentinel", "the capture is live");
        let mut state = profile.state.lock().await;
        profile.teardown(&mut state, Some(super::HostEnd::Closed), "").await;
        drop(state);
        drop(guard);

        assert!(saw(&caught, "test_sentinel"), "the capture is not seeing records");
        assert!(ended(&caught).is_empty(), "a dead host was recorded as an end: {caught:?}");
        crate::osr::Ring::unlink(&ring);
    }

    /// **An end that finds the host gone records nothing.** A host that
    /// crashed is not a host the service ended, and its monitor must be left
    /// to write the death.
    #[tokio::test]
    async fn an_end_that_finds_the_host_gone_writes_no_record() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let mut exited = std::process::Command::new("true").spawn().expect("a child");
        let pid = exited.id();
        exited.wait().expect("the child exits");
        let (identity, ring) = install_host_with_pid(&profile, dir.path(), pid, 16).await;

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        // **The capture is live**: the absence below is only as good as the
        // layer that would have seen a record.
        tracing::info!(event_name = "test_sentinel", "the capture is live");
        let host = service.end_dead_host(&profile, identity, "a listing timed out").await;
        drop(guard);

        assert!(saw(&caught, "test_sentinel"), "the capture is not seeing records");
        assert!(matches!(host, super::DeadHost::AlreadyGone), "a gone host is not ended: {host:?}");
        assert!(ended(&caught).is_empty(), "a gone host was recorded as an end: {caught:?}");
        crate::osr::Ring::unlink(&ring);
    }

    /// **An idle release is a note, not a warning.** The browser closing on
    /// its own clock is the service working as configured, and the record
    /// says so at the level that means it.
    #[tokio::test]
    async fn an_idle_release_is_a_note_not_a_warning() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        service.configure_idle(0);
        let profile = profile_for_test(&service).await;
        let (_, ring) = install_host(&profile, dir.path(), 12).await;
        *profile.touch.lock().unwrap_or_else(PoisonError::into_inner) = super::Touch::now();

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        service.idle_pass().await;
        drop(guard);

        let records = ended(&caught);
        assert_eq!(records.len(), 1, "one release, one record: {records:?}");
        let (level, fields) = &records[0];
        assert_eq!(*level, tracing::Level::INFO, "an idle release is not a warning: {fields}");
        assert!(fields.contains("cause=the idle clock released it"), "{fields}");
        crate::osr::Ring::unlink(&ring);
    }

    /// **A profile close is a note that names the profile.** Closing a browser
    /// is routine, and the record has to say which profile went.
    #[tokio::test]
    async fn a_profile_close_is_a_note_that_names_the_profile() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let profile = profile_for_test(&service).await;
        let (_, ring) = install_host(&profile, dir.path(), 13).await;

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        service.close(&seat).await.expect("the profile closes");
        drop(guard);

        let records = ended(&caught);
        assert_eq!(records.len(), 1, "one close, one record: {records:?}");
        let (level, fields) = &records[0];
        assert_eq!(*level, tracing::Level::INFO, "a close is not a warning: {fields}");
        assert!(fields.contains("cause=its profile was closed"), "{fields}");
        assert!(fields.contains(&format!("profile={}", profile.key())), "{fields}");
        assert!(fields.contains("port=13"), "{fields}");
        crate::osr::Ring::unlink(&ring);
    }

    /// **A host that stopped answering is a warning that carries the fault
    /// that stopped it.** `after_failure` has the call's own error and nothing
    /// else does, so losing it leaves the record with no reason.
    #[tokio::test]
    async fn a_stopped_host_warns_with_the_fault_that_stopped_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let (identity, ring) = install_host(&profile, dir.path(), 14).await;
        let why = "the page answered with a script error";

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let args = json!({});
        let _ = service
            .after_failure(
                &profile,
                Driven {
                    generation: identity.generation,
                    pid: identity.pid,
                    port: identity.port,
                    tab: 1,
                    bound: Duration::from_secs(1),
                },
                Call { tool: "browser_snapshot", reported: "browser_snapshot", args: &args },
                Err(why.to_owned()),
            )
            .await;
        drop(guard);

        let records = ended(&caught);
        assert_eq!(records.len(), 1, "one end, one record: {records:?}");
        let (level, fields) = &records[0];
        assert_eq!(*level, tracing::Level::WARN, "a stopped host reads at WARN: {fields}");
        assert!(
            fields.contains(&format!("cause=it stopped answering: {why}")),
            "the cause carries the call's own fault: {fields}"
        );
        crate::osr::Ring::unlink(&ring);
    }

    /// **A host that never became usable is a warning with its reason and its
    /// tail.** The call sites themselves are driven by
    /// `a_host_that_never_came_ready_is_recorded_at_its_call_site` and
    /// `a_host_whose_ring_vanished_is_recorded_at_its_call_site`; this pins
    /// the record's shape.
    #[tokio::test]
    async fn a_never_ready_host_is_a_warning_with_its_reason_and_tail() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        super::log_host_ended(
            &profile,
            4242,
            15,
            super::HostEnd::NeverReady,
            "ended",
            "its endpoint never answered",
            "the last line",
        );
        drop(guard);

        let records = ended(&caught);
        assert_eq!(records.len(), 1, "one end, one record: {records:?}");
        let (level, fields) = &records[0];
        assert_eq!(
            *level,
            tracing::Level::WARN,
            "a launch that never became usable reads at WARN: {fields}"
        );
        for needle in [
            "profile=O/default".to_owned(),
            "pid=4242".to_owned(),
            "port=15".to_owned(),
            "cause=it never became usable: its endpoint never answered".to_owned(),
            "outcome=ended".to_owned(),
            "log_tail=the last line".to_owned(),
        ] {
            assert!(fields.contains(&needle), "the record carries `{needle}`: {fields}");
        }
    }

    /// **A crash is recorded once, with its cause, and never as an end.** The
    /// monitor is the writer; a liveness check that reaped the child first
    /// would leave it with no cause.
    #[tokio::test(start_paused = true)]
    async fn a_crash_is_recorded_once_and_never_as_an_end() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let stub = dir.path().join("crash-host.sh");
        write_exec(&stub, "#!/bin/sh\necho \"cef host: crashing\" >&2\nkill -9 $$\n");
        let (pid, port) = spawn_stub(&stub, &profile, dir.path());
        let (_, ring) = install_host_with_pid(&profile, dir.path(), pid, port).await;
        let uploads = profile.dir.join("uploads");
        std::fs::create_dir_all(&uploads).expect("the uploads dir");
        profile.up.store(true, Ordering::Release);

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let monitor = profile.watch_host(pid, port);
        tokio::time::advance(Duration::from_secs(2)).await;
        monitor.await.expect("the monitor finishes");
        drop(guard);

        let caught_records = records(&caught);
        let died = caught_records
            .iter()
            .filter(|(_, fields)| fields.contains("event_name=\"browser_host_died\""))
            .collect::<Vec<_>>();
        assert_eq!(died.len(), 1, "one death, one record: {caught_records:?}");
        let (level, fields) = died[0];
        assert_eq!(*level, tracing::Level::WARN, "{fields}");
        assert!(fields.contains("cause=signal 9"), "the cause is the signal: {fields}");
        assert!(
            fields.contains("log_tail=cef host: crashing"),
            "the record carries the host's last words: {fields}"
        );
        assert!(!saw(&caught, "browser_host_ended"), "a crash is not an end");
        assert!(!uploads.exists(), "the dead host's uploads go with it");
        crate::osr::Ring::unlink(&ring);
    }

    /// **A dead host's monitor must not clear its successor's state.** The
    /// monitor is left alive for a host that had already gone, and its
    /// cleanup runs only while that host is still the one installed.
    #[tokio::test(start_paused = true)]
    async fn a_dead_hosts_monitor_does_not_clear_a_successors_state() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("O", "p");
        let profile = profile_for_test(&service).await;
        let stub = dir.path().join("gone-host.sh");
        write_exec(&stub, "#!/bin/sh\nexit 0\n");
        let (old_pid, old_port) = spawn_stub(&stub, &profile, dir.path());
        let (_, old_ring) = install_host_with_pid(&profile, dir.path(), old_pid, old_port).await;

        // The successor installs after the old host is gone, on the same
        // port: the guard has to know the pid too.
        let (_, successor_ring) =
            install_host_with_pid(&profile, dir.path(), std::process::id(), old_port).await;
        profile.up.store(true, Ordering::Release);
        profile
            .tabs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .adopt(1, seat, "T2", "T2", "", "")
            .expect("the successor's row");
        let (_births, birth_events) = tokio::sync::mpsc::unbounded_channel();
        *profile.host_tab_events.lock().await = Some(birth_events);
        let uploads = profile.dir.join("uploads");
        std::fs::create_dir_all(&uploads).expect("the uploads dir");

        let monitor = profile.watch_host(old_pid, old_port);
        tokio::time::advance(Duration::from_secs(2)).await;
        monitor.await.expect("the monitor finishes");

        assert_eq!(
            profile.tabs.lock().unwrap_or_else(PoisonError::into_inner).numbers(),
            vec![1],
            "the successor's tab was cleared"
        );
        assert!(
            profile.failures.borrow().clone().is_none(),
            "the successor got the old host's crash sentence"
        );
        assert!(profile.up.load(Ordering::Acquire), "the successor reads down");
        assert!(
            profile.host_tab_events.lock().await.is_some(),
            "the successor's tab events were cleared"
        );
        assert!(uploads.exists(), "the successor's uploads were removed");
        crate::osr::Ring::unlink(&old_ring);
        crate::osr::Ring::unlink(&successor_ring);
    }

    /// **A launch that never came ready is recorded at its call site.** The
    /// stand-in host exits before its endpoint, so `start_host`'s own branch
    /// writes the record.
    #[tokio::test]
    async fn a_host_that_never_came_ready_is_recorded_at_its_call_site() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        std::fs::create_dir_all(&profile.dir).expect("the profile dir");
        let stub = dir.path().join("never-ready.sh");
        write_exec(&stub, "#!/bin/sh\necho 'the stub last words' >&2\nexit 1\n");

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        crate::spawn::set_test_spawn(Box::new(move |args| {
            crate::spawn::spawn_with(
                &stub,
                &args.profile,
                args.port,
                &args.ring,
                args.width,
                args.height,
                args.profile_id,
                args.encoding,
                &args.frames,
            )
        }));
        let refused = profile.start_host().await;
        drop(guard);

        let refused = refused.expect_err("a host that never came ready is an error");
        assert!(
            refused.contains("the stub last words"),
            "the launch error keeps the host's own last words: {refused}"
        );
        let caught_records = records(&caught);
        let started = caught_records
            .iter()
            .filter(|(_, fields)| fields.contains("event_name=\"browser_host_ended\""))
            .collect::<Vec<_>>();
        assert_eq!(started.len(), 1, "one end, one record: {caught_records:?}");
        let (level, fields) = started[0];
        assert_eq!(*level, tracing::Level::WARN, "{fields}");
        assert!(
            fields.contains(
                "cause=it never became usable: the browser host exited before its endpoint answered"
            ),
            "{fields}"
        );
        assert!(fields.contains("log_tail=the stub last words"), "{fields}");
        assert!(fields.contains("profile=O/default"), "{fields}");
    }

    /// **A launch whose ring vanished is recorded at its call site.** The
    /// stand-in answers the probe with a live ring, then drops the ring, so
    /// `start_host`'s ring arm is the one that runs.
    #[tokio::test]
    async fn a_host_whose_ring_vanished_is_recorded_at_its_call_site() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        std::fs::create_dir_all(&profile.dir).expect("the profile dir");
        let stub = dir.path().join("ring-host.sh");
        write_exec(&stub, "#!/bin/sh\nexec sleep 5\n");

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        crate::spawn::set_test_spawn(Box::new(move |args| {
            let (pid, input, events) = crate::spawn::spawn_with(
                &stub,
                &args.profile,
                args.port,
                &args.ring,
                args.width,
                args.height,
                args.profile_id,
                args.encoding,
                &args.frames,
            )?;
            let ring = crate::osr::Ring::create(&args.ring, 4, 4)
                .map_err(|why| format!("the stand-in ring: {why}"))?;
            ring.set_pid(pid);
            ring.set_state(crate::osr::STATE_LIVE);
            let listener = std::net::TcpListener::bind(("127.0.0.1", args.port))
                .map_err(|why| format!("the stand-in endpoint: {why}"))?;
            let name = args.ring.clone();
            std::thread::spawn(move || {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let body = r#"{"Browser":"stub"}"#;
                let _ = std::io::Write::write_all(
                    &mut stream,
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
                crate::osr::Ring::unlink(&name);
            });
            Ok((pid, input, events))
        }));
        let refused = profile.start_host().await;
        drop(guard);

        let refused = refused.expect_err("a host whose ring vanished is an error");
        assert!(refused.contains("the frame ring"), "the ring's own error: {refused}");
        let caught_records = records(&caught);
        let started = caught_records
            .iter()
            .filter(|(_, fields)| fields.contains("event_name=\"browser_host_ended\""))
            .collect::<Vec<_>>();
        assert_eq!(started.len(), 1, "one end, one record: {caught_records:?}");
        let (level, fields) = started[0];
        assert_eq!(*level, tracing::Level::WARN, "{fields}");
        assert!(fields.contains("cause=it never became usable: the frame ring"), "{fields}");
    }

    /// **A stop is a note, not a warning.** forge stopping is routine, and the
    /// record says which profile went.
    #[tokio::test]
    async fn stopping_forge_is_a_note_for_each_profile() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let (_, ring) = install_host(&profile, dir.path(), 19).await;

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        service.stop_all().await;
        drop(guard);

        let caught_records = ended(&caught);
        assert_eq!(caught_records.len(), 1, "one stop, one record: {caught_records:?}");
        let (level, fields) = &caught_records[0];
        assert_eq!(*level, tracing::Level::INFO, "a stop is not a warning: {fields}");
        assert!(fields.contains("cause=forge stopped"), "{fields}");
        assert!(fields.contains(&format!("profile={}", profile.key())), "{fields}");
        crate::osr::Ring::unlink(&ring);
    }

    /// **A teardown that finds a dead host leaves its monitor to write the
    /// death.** The monitor is in the host the state holds, and collapsing
    /// the abort to the live branch would drop `browser_host_died`.
    #[tokio::test(start_paused = true)]
    async fn a_teardown_of_a_dead_host_leaves_the_monitor_its_death() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let stub = dir.path().join("dead-host.sh");
        write_exec(&stub, "#!/bin/sh\nexit 0\n");
        let (pid, port) = spawn_stub(&stub, &profile, dir.path());
        let monitor = profile.watch_host(pid, port);
        let (_, ring) =
            install_host_with_monitor(&profile, dir.path(), pid, port, Some(monitor)).await;
        // Let the monitor take its first tick before the clock moves.
        tokio::task::yield_now().await;

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let mut state = profile.state.lock().await;
        profile.teardown(&mut state, Some(super::HostEnd::Closed), "").await;
        drop(state);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        drop(guard);

        let caught_records = records(&caught);
        let died = caught_records
            .iter()
            .filter(|(_, fields)| fields.contains("event_name=\"browser_host_died\""))
            .count();
        assert_eq!(died, 1, "the monitor's death is lost: {caught_records:?}");
        assert!(!saw(&caught, "browser_host_ended"), "a dead host is not an end");
        crate::osr::Ring::unlink(&ring);
    }

    /// **An end that finds a host gone leaves its monitor to write the
    /// death.** The same guard, on the call path.
    #[tokio::test(start_paused = true)]
    async fn an_end_of_a_gone_host_leaves_the_monitor_its_death() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let stub = dir.path().join("gone-host-2.sh");
        write_exec(&stub, "#!/bin/sh\nexit 0\n");
        let (pid, port) = spawn_stub(&stub, &profile, dir.path());
        let monitor = profile.watch_host(pid, port);
        let (identity, ring) =
            install_host_with_monitor(&profile, dir.path(), pid, port, Some(monitor)).await;
        // Let the monitor take its first tick before the clock moves.
        tokio::task::yield_now().await;

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let host = service.end_dead_host(&profile, identity, "a listing timed out").await;
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        drop(guard);

        assert!(matches!(host, super::DeadHost::AlreadyGone), "a gone host is not ended: {host:?}");
        let caught_records = records(&caught);
        let died = caught_records
            .iter()
            .filter(|(_, fields)| fields.contains("event_name=\"browser_host_died\""))
            .count();
        assert_eq!(died, 1, "the monitor's death is lost: {caught_records:?}");
        assert!(!saw(&caught, "browser_host_ended"), "a dead host is not an end");
        crate::osr::Ring::unlink(&ring);
    }

    /// **An ensure that finds a dead host reaps nothing**: the monitor's
    /// death keeps its signal, and the teardown still clears the host.
    #[tokio::test(start_paused = true)]
    async fn an_ensure_that_finds_a_dead_host_leaves_the_monitor_its_cause() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let profile = profile_for_test(&service).await;
        let stub = dir.path().join("dead-ensure.sh");
        write_exec(&stub, "#!/bin/sh\nkill -9 $$\n");
        let (pid, port) = spawn_stub(&stub, &profile, dir.path());
        let monitor = profile.watch_host(pid, port);
        let (_, ring) =
            install_host_with_monitor(&profile, dir.path(), pid, port, Some(monitor)).await;
        tokio::task::yield_now().await;

        let caught = Caught::default();
        let subscriber = tracing_subscriber::registry().with(caught.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        // The ensure starts a host after the teardown; the stand-in fails it
        // before a launch record can stand in for the death under test.
        crate::spawn::set_test_spawn(Box::new(|_args| Err("the test has no host".to_owned())));
        let mut state = profile.state.lock().await;
        let _ = profile.ensure_locked(&mut state, dir.path(), &dir.path().join("stack")).await;
        drop(state);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        drop(guard);

        let caught_records = records(&caught);
        let died = caught_records
            .iter()
            .filter(|(_, fields)| fields.contains("event_name=\"browser_host_died\""))
            .collect::<Vec<_>>();
        assert_eq!(died.len(), 1, "one death, one record: {caught_records:?}");
        assert!(
            died[0].1.contains("cause=signal 9"),
            "the status was taken before the monitor read it: {}",
            died[0].1
        );
        assert!(!saw(&caught, "browser_host_ended"), "a dead host is not an end");
        crate::osr::Ring::unlink(&ring);
    }
}

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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use forge_primitives::SessionSlot;
use forge_primitives::browser::{
    BrowserAudio, BrowserDownload, BrowserFileAsk, BrowserFrame, BrowserInputEvent,
    BrowserNavState, BrowserPart, BrowserProfileRow, BrowserUpload, BrowserViewState,
    MAX_UPLOAD_BYTES, MAX_UPLOAD_FILES,
};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc, watch};

use crate::driver::{self, Driver};
use crate::idle::{self, IdleWarn, Step, Touch, Verdict};
use crate::input;
use crate::profiles::{self, SHARED_PROFILE, Store};
use crate::spawn::{self, ActiveHost};
use crate::window::Cdp;
use crate::{channel, engine, host, osr, veto};

/// One profile's clock and shape, as one idle pass reads it.
struct Candidate {
    /// The profile's key, which names it in the calls that follow.
    key: String,
    /// Whether it is a named profile rather than the shared browser.
    named: bool,
    owner: SessionSlot,
    touch: Touch,
    /// Whether a viewer is attached: **a viewer is a use**.
    viewed: bool,
}

/// The view a profile's browser starts at, in device pixels. A controlling
/// client's surface resizes it while it is open.
pub const VIEW_WIDTH: u32 = 1440;
pub const VIEW_HEIGHT: u32 = 900;

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
    profiles: AsyncMutex<HashMap<String, Arc<Profile>>>,
    /// One gate per profile key, held across a name's open and its teardown.
    ///
    /// **A same-name call waits on its own profile's run**, not on the whole
    /// fleet: the map lock is released before a host is ended or a directory
    /// removed, so another profile's entry is served while that work runs.
    gates: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    ids: AtomicU16,
    /// The idle policy: the two minutes and the way the workspace is asked
    /// to warn. `None` until the boot configures it, and no sweep runs then.
    idle: Mutex<Option<Idle>>,
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
}

/// The idle release's settings, as the boot configures them.
#[derive(Clone)]
struct Idle {
    minutes: u64,
    grace: u64,
    warn: Arc<dyn IdleWarn>,
}

impl BrowserService {
    /// A service at `root`, with the vendored driver stack read from `stack`.
    pub fn at(root: PathBuf, stack: PathBuf) -> Self {
        Self {
            store: Store::at(&root),
            root,
            stack,
            profiles: AsyncMutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            ids: AtomicU16::new(1),
            idle: Mutex::new(None),
            encoding: Mutex::new((15, 75)),
            launch: Arc::new(AsyncMutex::new(())),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set the `[browser]` encoding pair every host spawned from here reads.
    pub fn configure_encoding(&self, fps: u16, quality: u8) {
        *self.encoding.lock().unwrap_or_else(PoisonError::into_inner) = (fps, quality);
    }

    fn encoding(&self) -> (u16, u8) {
        *self.encoding.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Set the idle policy and, from this point, the sweep's clock.
    pub fn configure_idle(&self, minutes: u64, grace: u64, warn: Arc<dyn IdleWarn>) {
        *self.idle.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Idle { minutes, grace, warn });
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

    /// The service the server runs: profiles under forge's app-support tree,
    /// the vendored stack installed beside them, and the client's own
    /// profiles taken over on the first boot that finds none.
    pub fn installed() -> Result<Self, String> {
        let root = crate::host_bundle::app_support()?.join("browser");
        let store = Store::at(&root);
        match store.migrate(Store::client_source().as_deref()) {
            Ok(migrated) if migrated.ran => tracing::info!(
                "the client's browser profiles are the server's (event_name \
                 browser_profiles_migrated, copied {}, kept {})",
                migrated.copied,
                migrated.kept,
            ),
            Ok(_) => {}
            // **A copy that could not finish is not a boot failure.** The
            // server runs; the logins it could not take over are named, and
            // the marker stays absent so the next boot tries the rest.
            Err(why) => tracing::warn!(
                "the client's browser profiles could not be copied (event_name \
                 browser_profiles_migration_failed): {why}"
            ),
        }
        Ok(Self::at(root.clone(), root.join("stack")))
    }

    /// Where a profile's cache root sits: `<root>/profiles/<name>/cef`, and
    /// `<root>/shared/cef` for the browser every seat drives.
    pub fn profile_dir(&self, name: &str) -> PathBuf {
        if name == SHARED_PROFILE {
            self.store.shared().join("cef")
        } else {
            self.store.named(name).join("cef")
        }
    }

    /// **An omitted profile is the asking seat's own, never the shared
    /// browser.** The shared browser's tabs are whatever the person or
    /// another seat left there, so it is reachable only by naming it
    /// `@shared`; the profile charset keeps `@` out of every real name.
    fn target(seat: &SessionSlot, name: Option<&str>) -> String {
        match name {
            Some(SHARED_PROFILE) => SHARED_PROFILE.to_owned(),
            Some(name) => name.to_owned(),
            None => profiles::auto_name(seat),
        }
    }

    /// The key a call, an attach or a raise drives, with a name that cannot
    /// be a profile refused by the rule rather than reaching for a directory
    /// it names.
    fn checked_target(seat: &SessionSlot, name: Option<&str>) -> Result<String, String> {
        let key = Self::target(seat, name);
        if key != SHARED_PROFILE
            && let Some(refusal) = profiles::name_refusal(&key)
        {
            return Err(refusal);
        }
        Ok(key)
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

    /// Open a profile, or answer the one already open. **The name's gate is
    /// held across the entry and the launch**, so a same-name close waits
    /// for the launch rather than deleting the directory under it.
    pub async fn open(
        &self,
        seat: &SessionSlot,
        name: Option<&str>,
    ) -> Result<Arc<Profile>, String> {
        let key = Self::checked_target(seat, name)?;
        let gate = self.gate(&key);
        let _held = gate.lock().await;
        let profile = self.entry_locked(seat, name).await?;
        profile.ensure(&self.root, self.stack()).await?;
        Ok(profile)
    }

    /// The profile's map entry, created on first use and left alone after.
    ///
    /// **A named profile carries its owner**, read from the record beside its
    /// directory - written on the first open and restored after a restart -
    /// and another seat naming it is refused here, before any browser starts.
    #[cfg(test)]
    async fn entry(&self, seat: &SessionSlot, name: Option<&str>) -> Result<Arc<Profile>, String> {
        let key = Self::checked_target(seat, name)?;
        let gate = self.gate(&key);
        let _held = gate.lock().await;
        self.entry_locked(seat, name).await
    }

    /// The entry itself, with the key's gate already held.
    async fn entry_locked(
        &self,
        seat: &SessionSlot,
        name: Option<&str>,
    ) -> Result<Arc<Profile>, String> {
        let key = Self::checked_target(seat, name)?;
        let dir = self.profile_dir(&key);
        let mut profiles = self.profiles.lock().await;
        if let Some(profile) = profiles.get(&key) {
            if key != SHARED_PROFILE && &profile.owner != seat {
                return Err(owner_refusal(&key, &profile.owner));
            }
            // **Every call and every raise is a use**: the idle clock starts
            // again here, and the warning that stood over the streak goes
            // with it.
            profile.touch.lock().unwrap_or_else(PoisonError::into_inner).touched();
            return Ok(Arc::clone(profile));
        }
        let owner = if key == SHARED_PROFILE {
            seat.clone()
        } else {
            match profiles::owner_of(&self.store.named(&key))? {
                Some(owner) if &owner != seat => return Err(owner_refusal(&key, &owner)),
                Some(owner) => owner,
                None => {
                    profiles::record_owner(&self.store.named(&key), seat)?;
                    seat.clone()
                }
            }
        };
        std::fs::create_dir_all(&dir)
            .map_err(|why| format!("the profile's directory cannot be made: {why}"))?;
        let profile = Arc::new(Profile {
            key: key.clone(),
            owner,
            id: self.ids.fetch_add(1, Ordering::Relaxed),
            dir,
            frames: watch::channel(None).0,
            view: watch::channel(BrowserViewState::default()).0,
            nav: watch::channel(None).0,
            failures: watch::channel(None).0,
            files: watch::channel(None).0,
            playwright_ask: Mutex::new(None),
            next_file_ask: AtomicU64::new(1 << 32),
            file_decision: AsyncMutex::new(()),
            rebuild: AsyncMutex::new(()),
            chooser_ledger: Mutex::new(ChooserLedger::default()),
            chooser_answer: Mutex::new(None),
            downloads: broadcast::channel(16).0,
            audio: broadcast::channel(64).0,
            recorded: watch::channel(None).0,
            recording: Mutex::new(None),
            surface: Mutex::new((VIEW_WIDTH, VIEW_HEIGHT, 1.0)),
            encoding: self.encoding(),
            launch: Arc::clone(&self.launch),
            stopped: Arc::clone(&self.stopped),
            viewers: Mutex::new(HashSet::new()),
            state: AsyncMutex::new(State::default()),
            control: Mutex::new(None),
            touch: Mutex::new(Touch::now()),
            ended: AtomicBool::new(false),
            up: AtomicBool::new(false),
            busy: AtomicU32::new(0),
        });
        profiles.insert(key, Arc::clone(&profile));
        Ok(profile)
    }

    /// One idle pass: every open profile's clock, and the warnings the
    /// workspace answers.
    ///
    /// **A viewer attached is a use**, so a surface the person is looking at
    /// is never warned or closed. **The shared browser is never announced**:
    /// its warn step arms the grace silently, and the close's own question is
    /// its only crossing.
    async fn idle_pass(&self) {
        let Some(settings) = self.idle_settings() else { return };
        let idle = Duration::from_secs(settings.minutes.saturating_mul(60));
        let grace = Duration::from_secs(settings.grace.saturating_mul(60));
        let candidates = {
            let profiles = self.profiles.lock().await;
            profiles
                .iter()
                .map(|(key, profile)| Candidate {
                    key: key.clone(),
                    named: key != SHARED_PROFILE,
                    owner: profile.owner.clone(),
                    touch: *profile.touch.lock().unwrap_or_else(PoisonError::into_inner),
                    viewed: !profile
                        .viewers
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .is_empty(),
                })
                .collect::<Vec<_>>()
        };
        for candidate in candidates {
            if candidate.viewed {
                continue;
            }
            match idle::step(
                candidate.touch.at,
                candidate.touch.warned,
                Instant::now(),
                idle,
                grace,
            ) {
                Step::Nothing => {}
                Step::Warn if candidate.named => {
                    let verdict = settings
                        .warn
                        .warn(
                            Some(&candidate.owner),
                            &candidate.key,
                            settings.minutes,
                            settings.grace,
                            false,
                        )
                        .await;
                    match verdict {
                        // **A warning that never went is never armed**: a
                        // duplicate warning is acceptable, a close nobody was
                        // warned about is not.
                        Verdict::Delivered | Verdict::Held => self.note_warned(&candidate.key),
                        // **Nobody to warn means no grace to serve**: the
                        // idle bound alone closes it.
                        Verdict::NoSession => self.close_idle(&candidate.key, true).await,
                        Verdict::Clear | Verdict::Unanswered => {}
                    }
                }
                // The shared browser's warn is an arm in silence.
                Step::Warn => self.note_warned(&candidate.key),
                Step::Close => {
                    let name = candidate.key.as_str();
                    let seat = candidate.named.then_some(&candidate.owner);
                    match settings
                        .warn
                        .warn(seat, name, settings.minutes, settings.grace, true)
                        .await
                    {
                        Verdict::Clear => self.close_idle(&candidate.key, false).await,
                        // Held: an abandoned hand-off closes once it is gone,
                        // on a later pass.
                        Verdict::Held => self.note_warned(&candidate.key),
                        Verdict::Delivered | Verdict::NoSession | Verdict::Unanswered => {}
                    }
                }
            }
        }
    }

    fn idle_settings(&self) -> Option<Idle> {
        self.idle.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn note_warned(&self, key: &str) {
        // The touch is read by the sweep alone, so the map needs no lock here:
        // the profile being gone is simply nothing to note.
        let profiles = self.profiles.try_lock();
        let Ok(profiles) = profiles else { return };
        if let Some(profile) = profiles.get(key) {
            profile.touch.lock().unwrap_or_else(PoisonError::into_inner).warned =
                Some(Instant::now());
        }
    }

    /// Close one profile's browser under the state lock, re-reading its clock
    /// and its viewers: a call or a raise that landed while the verdict was
    /// in flight wins, and a browser still in use is not closed.
    ///
    /// `quiet` is a close with nobody to warn: it may go at the idle bound,
    /// where a warned one waits out its grace.
    ///
    /// **The entry leaves the map before its host is ended, and the map lock
    /// is released with it**: a same-name call waits on the key's own gate,
    /// while every other profile keeps being served through the teardown.
    async fn close_idle(&self, key: &str, quiet: bool) {
        let Some(settings) = self.idle_settings() else { return };
        let idle = Duration::from_secs(settings.minutes.saturating_mul(60));
        let grace = Duration::from_secs(settings.grace.saturating_mul(60));
        let gate = self.gate(key);
        let _held = gate.lock().await;
        let Some(profile) = self.profiles.lock().await.get(key).map(Arc::clone) else { return };
        if !profile.viewers.lock().unwrap_or_else(PoisonError::into_inner).is_empty() {
            return;
        }
        let touch = *profile.touch.lock().unwrap_or_else(PoisonError::into_inner);
        let close = match idle::step(touch.at, touch.warned, Instant::now(), idle, grace) {
            Step::Close => true,
            // **A quieter close may go at the bound itself**, and a use that
            // landed meanwhile is still a fresh touch, which is `Nothing`.
            Step::Warn => quiet,
            Step::Nothing => false,
        };
        if !close {
            return;
        }
        self.profiles.lock().await.remove(key);
        // The state lock is taken after the entry is gone, so the teardown's
        // wait is never the map's.
        let mut state = profile.state.lock().await;
        profile.ended.store(true, Ordering::Release);
        profile.teardown(&mut state).await;
        drop(state);
        tracing::debug!("an idle browser closed (event_name browser_idle_closed, profile {key})");
    }

    /// End every open profile's browser and driver. **The profiles stay on
    /// disk**: a stop ends the run, never the logins, and the next boot
    /// opens each browser on its carried page. Answers the profiles whose
    /// host was still alive after its teardown, so a stop that leaves a
    /// process behind cannot report success.
    pub async fn stop_all(&self) -> Vec<String> {
        // **A stop is final**: an attach that already held a profile cannot
        // start a host behind the drain.
        self.stopped.store(true, Ordering::Release);
        let profiles: Vec<(String, Arc<Profile>)> = self.profiles.lock().await.drain().collect();
        let mut left = Vec::new();
        for (key, profile) in profiles {
            let pid = { profile.state.lock().await.host.as_ref().map(|host| host.pid) };
            profile.end().await;
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
        let mut profiles = vec![self.store.shared().join("cef")];
        if let Ok(entries) = std::fs::read_dir(self.store.profiles()) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    profiles.push(entry.path().join("cef"));
                }
            }
        }
        for profile in profiles {
            spawn::reap_stale_profile(&profile).await;
        }
    }

    /// **A seat that is down takes its named profiles with it.** Called with
    /// the seats that remain: an owned profile whose seat is not among them
    /// is ended and its directory removed, and the shared browser is never
    /// touched. An ownerless directory is a leftover and goes the same way.
    pub async fn sweep_seats(&self, seats: &[SessionSlot]) {
        let names = match std::fs::read_dir(self.store.profiles()) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            Err(_) => return,
        };
        for name in names {
            let dir = self.store.named(&name);
            let owner = match profiles::owner_of(&dir) {
                Ok(None) => None,
                Ok(Some(owner)) => Some(owner),
                // A record that cannot be read is not this sweep's: removing
                // a profile because its owner file is corrupt takes logins
                // nobody asked it to take.
                Err(_) => continue,
            };
            if owner.as_ref().is_some_and(|owner| seats.contains(owner)) {
                continue;
            }
            // **The key is the directory's own name, with no omitted-profile
            // default**: a sweep acts on a profile that exists, never on the
            // shared browser.
            let key = name.clone();
            if let Some(profile) = self.profiles.lock().await.remove(&key) {
                profile.end().await;
            }
            if let Err(why) = std::fs::remove_dir_all(&dir) {
                tracing::warn!(
                    "the '{name}' profile could not be removed (event_name \
                     browser_profile_sweep_failed): {why}"
                );
            } else {
                tracing::debug!(
                    "a down seat's browsers went with it (event_name browser_profile_swept, \
                     profile {name})"
                );
            }
        }
    }

    /// Run one browser tool through the profile the call names.
    pub async fn call(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String> {
        let (name, args) = take_profile(args)?;
        // The fleet's own doors: these never reach a driver.
        match tool {
            "browser_profile_delete" => return self.delete(seat, args).await,
            "browser_record_start" => return self.record_start(seat, name.as_deref()).await,
            "browser_record_stop" => return self.record_stop(seat, name.as_deref()).await,
            _ => {}
        }
        // **An agent's upload is bounded before a browser is opened for it.**
        // The driver reads whatever path it is handed, so forge checks the
        // files the page would take here rather than after the page has
        // loaded them: absolute, readable, regular, and inside the same caps
        // the person's own answer carries.
        let upload =
            if tool == "browser_file_upload" { Some(upload_request(&args)?) } else { None };
        let profile = self.open(seat, name.as_deref()).await?;
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
        // **The strip's work ring reads this mark**, and it comes off however
        // the call ends.
        profile.busy.fetch_add(1, Ordering::AcqRel);
        let _busy = Busy(&profile.busy);
        // **The state lock is only for the lookup.** A call drives a page for
        // seconds, and holding it across that would block an attach, a
        // teardown or the idle sweep for the whole of it. **The call names
        // the host it drove**, so a failure is diagnosed against the browser
        // that was there rather than whatever replaced it.
        let (driver, generation, pid, port) = {
            let state = profile.state.lock().await;
            let Some(driver) = state.driver.as_ref() else {
                return Err(format!("the browser for '{}' is not up", profile.key));
            };
            let Some(host) = state.host.as_ref() else {
                return Err(format!("the browser for '{}' is not up", profile.key));
            };
            (Arc::clone(driver), captured_generation(&state), host.pid, host.port)
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
        let retry_args = args.clone();
        let outcome = driver.call_within(tool, args, bound).await;
        // **The agent answered a chooser the driver had not reported yet.**
        // One event is owed to this answer, so it is consumed rather than
        // asking a person for a chooser that no longer exists.
        if let Some(upload) = upload.as_ref() {
            note_upload_finished(&profile, upload, outcome.is_ok());
        }
        // The who-answers decision is over; the failure is the driver's and
        // host's, and must not hold a role change out.
        drop(decision);
        let outcome =
            self.after_failure(&profile, generation, pid, port, tool, &retry_args, outcome).await;
        // **A call that changed the page rewrites the carry**, so a restart
        // opens where the session left it. A call that failed does not: the
        // page it was reaching for is not a page the person was shown.
        if outcome.is_ok()
            && let Some(url) = spawn::page_url(port).await
        {
            crate::carry::record(&profile.dir, &url);
        }
        outcome
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
        generation: u64,
        pid: u32,
        port: u16,
        tool: &str,
        args: &Value,
        outcome: Result<Vec<BrowserPart>, String>,
    ) -> Result<Vec<BrowserPart>, String> {
        let why = match outcome {
            Ok(parts) => {
                let mut state = profile.state.lock().await;
                if state.chooser_generation == generation {
                    state.stale_since = None;
                }
                return Ok(parts);
            }
            Err(why) => why,
        };
        if driver::attach_timed_out(&why) {
            // **The host answered its DevTools endpoint but its page did
            // not**, which is the wedge the bare 30 s `initializeServer`
            // timeout was hiding (#2534). It is named and ended, so the next
            // call starts a fresh browser with the profile's data intact.
            tracing::warn!(
                event_name = "browser_host_attach_timeout",
                profile = %profile.key,
                pid,
                port,
                "the driver could not attach to its browser host: {why}"
            );
            let ended = self.end_wedged_host(profile, generation, pid, port).await;
            let tail = spawn::log_tail(&profile.dir);
            return Err(attach_failure(pid, port, &profile.dir, &tail, ended));
        }
        if driver::stale_page(&why) {
            return self.rebuild_after_dead_page(profile, generation, tool, args, &why).await;
        }
        // **A driver built against a browser that is gone is dropped.** The
        // probe is the browser's own liveness, not the driver's; a host that
        // is not answering is torn down so the next call starts a fresh one.
        let host_up = spawn::probe(port).await;
        let _decision = profile.file_decision.lock().await;
        let mut state = profile.state.lock().await;
        let alive = state.driver.as_ref().is_some_and(|driver| driver.is_running());
        if clear_if_dead(&mut state, generation, alive && host_up, profile) && !host_up {
            profile.teardown(&mut state).await;
        }
        Err(why)
    }

    /// End a host whose page never answered the driver's attach, answering
    /// whether the end went through.
    async fn end_wedged_host(
        &self,
        profile: &Arc<Profile>,
        generation: u64,
        pid: u32,
        port: u16,
    ) -> bool {
        let _decision = profile.file_decision.lock().await;
        let host = {
            let mut state = profile.state.lock().await;
            if state.chooser_generation != generation {
                // A newer driver is installed: this call's failure says
                // nothing about the browser now serving the profile, and no
                // host was ended for it.
                return false;
            }
            state.driver = None;
            state.cdp = None;
            state.cdp_events = None;
            state.cdp_path = None;
            if let Some(watch) = state.nav_watch.take() {
                watch.abort();
            }
            profile.clear_chooser_state();
            state.host.take()
        };
        profile.up.store(false, Ordering::Release);
        let Some(host) = host else { return true };
        if let Some(monitor) = host.monitor {
            monitor.abort();
        }
        let ended = spawn::end_verified(pid, port, &profile.dir).await;
        if let Some(reader) = host.reader {
            reader.stop();
        }
        profile.failures.send_replace(Some(format!(
            "the browser host (pid {pid}, port {port}) stopped answering and was ended; the next \
             call brings it back"
        )));
        ended
    }

    /// Rebuild a driver whose page context died, and repeat the call only
    /// where repeating it cannot land twice.
    async fn rebuild_after_dead_page(
        &self,
        profile: &Arc<Profile>,
        generation: u64,
        tool: &str,
        args: &Value,
        why: &str,
    ) -> Result<Vec<BrowserPart>, String> {
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
            if state.chooser_generation != generation {
                log_rebuild(profile, tool, "not this call's generation", why);
                return Err(why.to_owned());
            }
            if let Some(at) = state.stale_since
                && at.elapsed() < REBUILD_COOLDOWN
            {
                log_rebuild(profile, tool, "not rebuilt again yet", why);
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
            log_rebuild(profile, tool, "rebuild failed", why);
            return Err(dead_page_answer(DeadPageOutcome::RebuildFailed { why: &cannot }));
        }
        let fresh = {
            let state = profile.state.lock().await;
            state.driver.as_ref().map(Arc::clone)
        };
        let Some(fresh) = fresh else {
            profile.state.lock().await.stale_since = Some(Instant::now());
            log_rebuild(profile, tool, "rebuild left no driver", why);
            return Err(dead_page_answer(DeadPageOutcome::RebuildFailed {
                why: "the rebuilt driver did not come up",
            }));
        };
        if repeatable(tool, args) {
            return match fresh.call(tool, args.clone()).await {
                Ok(parts) => {
                    profile.state.lock().await.stale_since = None;
                    log_rebuild(profile, tool, "rebuilt, call retried and answered", why);
                    Ok(parts)
                }
                Err(retry_why) => {
                    profile.state.lock().await.stale_since = Some(Instant::now());
                    log_rebuild(profile, tool, "rebuilt, retried call failed", why);
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
        log_rebuild(profile, tool, "rebuilt, call handed back", why);
        Err(dead_page_answer(DeadPageOutcome::NotRepeated { tool }))
    }

    /// Attach a viewer to a profile: its newest frame, its viewer state, and
    /// the profile's key for the calls that follow.
    ///
    /// Attaching also asks the host for a full-quality frame, so a viewer
    /// joining mid-run draws a crisp page first.
    pub async fn attach(
        &self,
        viewer: u64,
        seat: &SessionSlot,
        name: Option<&str>,
    ) -> Result<Attachment, String> {
        let profile = self.open(seat, name).await?;
        let frames = profile.frames.subscribe();
        let view = profile.view.subscribe();
        let nav = profile.nav.subscribe();
        let failures = profile.failures.subscribe();
        let files = profile.files.subscribe();
        let downloads = profile.downloads.subscribe();
        let audio = profile.audio.subscribe();
        {
            let mut viewers = profile.viewers.lock().unwrap_or_else(PoisonError::into_inner);
            viewers.insert(viewer);
        }
        profile.publish_view();
        let port = {
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
            state.host.as_ref().map(|host| host.port)
        };
        // **A raise is a read of the page too**: the page a restart reopens
        // is the page the person was shown. A held carry releases only once
        // this sees a real one.
        if let Some(port) = port
            && let Some(url) = spawn::page_url(port).await
        {
            crate::carry::record(&profile.dir, &url);
        }
        Ok(Attachment {
            key: profile.key.clone(),
            id: profile.id,
            frames,
            view,
            nav,
            failures,
            files,
            downloads,
            audio,
        })
    }

    /// A viewer is gone: its count comes off every profile it was on, and a
    /// role it held is released with it.
    pub async fn detach(&self, viewer: u64) {
        let profiles: Vec<Arc<Profile>> = self.profiles.lock().await.values().cloned().collect();
        for profile in profiles {
            // **The departure and the chooser it cancels are one decision**,
            // so a chooser event cannot publish an ask for a controller who
            // has already gone.
            let _decision = profile.file_decision.lock().await;
            let removed = {
                let mut viewers = profile.viewers.lock().unwrap_or_else(PoisonError::into_inner);
                viewers.remove(&viewer)
            };
            let held = {
                let mut control = profile.control.lock().unwrap_or_else(PoisonError::into_inner);
                let held = control.map(|(holder, _)| holder) == Some(viewer);
                if held {
                    *control = None;
                }
                held
            };
            if held {
                // **The person driving it is gone**: a chooser they were
                // being asked is not the next viewer's to answer.
                profile.cancel_files().await;
            }
            if removed || held {
                profile.publish_view();
            }
            if removed && profile.viewers.lock().unwrap_or_else(PoisonError::into_inner).is_empty()
            {
                let state = profile.state.lock().await;
                if let Some(host) = state.host.as_ref() {
                    // **Nobody is watching**: the host paints but stops
                    // encoding, and the next viewer resumes it.
                    host.ring.pause(true);
                }
            }
        }
    }

    /// End a profile's browser and forget it. The next call relaunches it
    /// over the same directory, so a close ends the run and never the logins.
    ///
    /// **A named profile is its owner's to close**: the seat is checked
    /// against the open entry or, for one only on disk, its record.
    pub async fn close(&self, seat: &SessionSlot, name: Option<&str>) -> Result<(), String> {
        let key = Self::checked_target(seat, name)?;
        if key != SHARED_PROFILE {
            let open = self.profiles.lock().await.get(&key).map(|profile| profile.owner.clone());
            let owner = match open {
                Some(owner) => Some(owner),
                None => profiles::owner_of(&self.store.named(&key))?,
            };
            if let Some(owner) = owner
                && &owner != seat
            {
                return Err(owner_refusal(&key, &owner));
            }
        }
        // **The entry leaves the map before its host is ended, and the
        // map lock is released with it**: a same-name call waits on the
        // key's own gate, while every other profile keeps being served
        // through the whole teardown.
        let gate = self.gate(&key);
        let _held = gate.lock().await;
        let profile = self.profiles.lock().await.remove(&key);
        let Some(profile) = profile else {
            return Err(format!("no browser profile is open under '{key}'"));
        };
        profile.end().await;
        Ok(())
    }

    /// The profiles this seat may see, for a client's strip: the shared
    /// browser, its own named profiles, and the ownerless ones a legacy tree
    /// left behind. **A name whose record cannot be read is drawn ownerless
    /// and marked unreadable**, not guessed at and not hidden: the profile
    /// and its logins exist, and the row is the only door to them.
    pub async fn profiles(&self, seat: &SessionSlot) -> Result<Vec<BrowserProfileRow>, String> {
        let open: HashMap<String, Arc<Profile>> = self
            .profiles
            .lock()
            .await
            .iter()
            .map(|(key, profile)| (key.clone(), Arc::clone(profile)))
            .collect();
        let mut rows = vec![row_of(SHARED_PROFILE, None, open.get(SHARED_PROFILE))];
        let mut names = Vec::new();
        match std::fs::read_dir(self.store.profiles()) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        names.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => {}
            Err(why) => return Err(format!("the profiles cannot be listed: {why}")),
        }
        names.sort();
        for name in names {
            // **An unreadable owner record is drawn, not skipped.** The
            // profile and its logins exist on disk; hiding the row made them
            // unreachable from the strip, and rule 25's default is to draw
            // what arrived.
            let (owner, unreadable) = match profiles::owner_of(&self.store.named(&name)) {
                Ok(owner) => (owner, false),
                Err(why) => {
                    tracing::debug!(
                        event_name = "browser_profile_owner_unreadable",
                        profile = %name,
                        "a profile's owner record cannot be read; its row is drawn ownerless: {why}"
                    );
                    (None, true)
                }
            };
            if owner.as_ref().is_some_and(|owner| owner != seat) {
                continue;
            }
            let mut row = row_of(&name, owner.as_ref(), open.get(&name));
            row.unreadable = unreadable;
            rows.push(row);
        }
        Ok(rows)
    }

    /// Delete a named profile and its logins, for a seat that owns it.
    pub async fn delete_profile(&self, seat: &SessionSlot, name: &str) -> Result<(), String> {
        self.delete_named(seat, name).await
    }

    /// Give `viewer` the controller role for a profile.
    ///
    /// **Exactly one controller.** A take moves the role to the caller and
    /// the previous holder becomes a watcher; there is no default holder and
    /// no override machinery - a surface starts controller-less.
    pub async fn take_control(&self, profile: &str, viewer: u64) -> Result<(), String> {
        let profile = self.find(profile).await?;
        // **A role change and the chooser it cancels are one decision**: a
        // chooser event cannot read the old controller and publish after the
        // role has moved.
        let _decision = profile.file_decision.lock().await;
        *profile.control.lock().unwrap_or_else(PoisonError::into_inner) = Some((viewer, 0));
        profile.cancel_files().await;
        profile.publish_view();
        Ok(())
    }

    /// Release the role, when this viewer still holds it.
    pub async fn release_control(&self, profile: &str, viewer: u64) {
        if let Ok(profile) = self.find(profile).await {
            let _decision = profile.file_decision.lock().await;
            let released = {
                let mut control = profile.control.lock().unwrap_or_else(PoisonError::into_inner);
                let released = control.map(|(holder, _)| holder) == Some(viewer);
                if released {
                    *control = None;
                }
                released
            };
            if released {
                profile.cancel_files().await;
                profile.publish_view();
            }
        }
    }

    /// Lay the page out to the controller's surface. A resize from anyone
    /// else is ignored: the page must not move under the person driving it.
    pub async fn resize(&self, profile: &str, viewer: u64, width: u32, height: u32, scale: f64) {
        let Ok(profile) = self.find(profile).await else { return };
        let Some((width, height, scale)) =
            controller_size(profile.controller(), viewer, width, height, scale)
        else {
            return;
        };
        // **A take owns the layout**: the controller's resize is remembered
        // and applied again when the recording ends.
        *profile.surface.lock().unwrap_or_else(PoisonError::into_inner) = (width, height, scale);
        if profile.recording.lock().unwrap_or_else(PoisonError::into_inner).is_some() {
            return;
        }
        let state = profile.state.lock().await;
        if let Some(host) = state.host.as_ref() {
            host.ring.request(width, height, scale);
        }
    }

    /// Inject one input event, and only for the viewer that holds the role.
    ///
    /// **The role is checked before the event is mapped or sent**, so input
    /// never reaches a page from a viewer that does not control it. An event
    /// at or below the last sequence the role carried was superseded before
    /// it arrived, and is dropped.
    pub async fn input(
        &self,
        profile: &str,
        viewer: u64,
        seq: u64,
        event: &BrowserInputEvent,
    ) -> Result<(), String> {
        let profile = self.find(profile).await?;
        {
            let mut control = profile.control.lock().unwrap_or_else(PoisonError::into_inner);
            match *control {
                Some((holder, last)) if holder == viewer => {
                    if seq <= last {
                        return Ok(());
                    }
                    *control = Some((holder, seq));
                }
                _ => return Err("this viewer is not the browser's controller".to_owned()),
            }
        }
        let (input, port) = {
            let state = profile.state.lock().await;
            let host = state
                .host
                .as_ref()
                .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
            (host.input.clone(), host.port)
        };
        match input::route(event) {
            // **A pointer gesture is the host's**, whose own mouse calls are
            // what CEF routes into a windowless popup.
            input::Route::Host(line) => input
                .send(line)
                .map_err(|_| format!("the browser for '{}' is not taking input", profile.key)),
            input::Route::Page(method, params) => {
                let mut state = profile.state.lock().await;
                send_page(&mut state, port, method, params).await
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
        id: u64,
        files: Vec<BrowserUpload>,
    ) -> Result<(), String> {
        let profile = self.find(profile).await?;
        // **The answer is one chooser decision**, so a role change or an
        // agent's upload cannot interleave between the check and the driver
        // call that answers it.
        let _decision = profile.file_decision.lock().await;
        // **A stale answer touches nothing.** The outstanding ask is checked
        // before the uploads directory is cleared or a byte is written, so a
        // superseded answer cannot wipe the files a live dialog is handing
        // over.
        let outstanding = profile.files.borrow().as_ref().map(|ask| ask.id);
        if outstanding != Some(id) {
            return Err(format!(
                "the file chooser for id {id} is not outstanding, so the answer is refused"
            ));
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
    pub async fn record_start(
        &self,
        seat: &SessionSlot,
        name: Option<&str>,
    ) -> Result<Vec<BrowserPart>, String> {
        /// Recordings kept beside the takes: the newest ten, this one
        /// included.
        const KEEP: usize = 10;
        /// A take refuses to start below this much free space.
        const FLOOR: u64 = 512 * 1024 * 1024;
        let profile = self.open(seat, name).await?;
        if profile.recording.lock().unwrap_or_else(PoisonError::into_inner).is_some() {
            return Err(format!(
                "a recording is already running for '{}'; stop it first",
                profile.key
            ));
        }
        let dir = self.root.join("recordings");
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
        let (input, ring) = {
            let state = profile.state.lock().await;
            let host = state
                .host
                .as_ref()
                .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
            (host.input.clone(), Arc::clone(&host.ring))
        };
        prune_recordings(&dir, KEEP);
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let path = dir.join(format!("{}-{stamp}.mp4", profile.key));
        profile.recorded.send_replace(None);
        let mut recorded = profile.recorded.subscribe();
        *profile.recording.lock().unwrap_or_else(PoisonError::into_inner) = Some(path.clone());
        // **The layout follows the take**: the controller's resize is held
        // for the duration, and restored after the stop.
        ring.request(crate::record::WIDTH, crate::record::HEIGHT, 1.0);
        let line = input::host_line(&crate::input::HostInput::RecordStart {
            path: path.to_string_lossy().into_owned(),
        });
        if let Err(why) = input.send(line) {
            *profile.recording.lock().unwrap_or_else(PoisonError::into_inner) = None;
            restore_surface(&profile, &ring);
            return Err(format!(
                "the browser for '{}' is not taking a recording: {why}",
                profile.key
            ));
        }
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
                restore_surface(&profile, &ring);
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
    pub async fn record_stop(
        &self,
        seat: &SessionSlot,
        name: Option<&str>,
    ) -> Result<Vec<BrowserPart>, String> {
        // **A stop never starts a browser.** The take lives in the host that
        // was recording, so a profile that is not open has no take to end.
        let key = Self::checked_target(seat, name)?;
        let Ok(profile) = self.find(&key).await else {
            return Err(format!("no recording is running for '{key}'"));
        };
        let taken = profile.recording.lock().unwrap_or_else(PoisonError::into_inner).take();
        let Some(path) = taken else {
            return Err(format!("no recording is running for '{}'", profile.key));
        };
        let (input, ring) = {
            let state = profile.state.lock().await;
            let host = state
                .host
                .as_ref()
                .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
            (host.input.clone(), Arc::clone(&host.ring))
        };
        let mut recorded = profile.recorded.subscribe();
        let delivered = input.send(input::host_line(&crate::input::HostInput::RecordStop));
        let answer = if delivered.is_ok() {
            recording_answer(&mut recorded, Some(false), Duration::from_secs(30)).await
        } else {
            None
        };
        profile.recorded.send_replace(None);
        restore_surface(&profile, &ring);
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

    /// Where the page's history sits, or nothing when it cannot be read.
    pub async fn nav_state(&self, profile: &str) -> Option<BrowserNavState> {
        let profile = self.find(profile).await.ok()?;
        let mut state = profile.state.lock().await;
        let port = state.host.as_ref().map(|host| host.port)?;
        let reply =
            call_page(&mut state, port, "Page.getNavigationHistory", json!({})).await.ok()?;
        let nav = input::history_state(&reply)?;
        profile.publish_nav(nav);
        Some(nav)
    }

    /// Move through the page's history, answering where it now sits.
    ///
    /// **Only the controller moves the page.** The arrows are the same page
    /// a watcher is looking at, so a watcher's move would move the person
    /// driving it; the role is checked before the history is read.
    pub async fn nav(
        &self,
        profile: &str,
        viewer: u64,
        back: bool,
    ) -> Result<BrowserNavState, String> {
        let profile = self.find(profile).await?;
        if profile.controller() != Some(viewer) {
            return Err("this viewer is not the browser's controller".to_owned());
        }
        let mut state = profile.state.lock().await;
        let port = state
            .host
            .as_ref()
            .map(|host| host.port)
            .ok_or_else(|| format!("the browser for '{}' is not up", profile.key))?;
        let reply = call_page(&mut state, port, "Page.getNavigationHistory", json!({})).await?;
        let (index, entries) = input::history_position(&reply)
            .ok_or_else(|| "the page's history could not be read".to_owned())?;
        let target = if back {
            index.checked_sub(1)
        } else {
            (index + 1 < entries.len()).then_some(index + 1)
        };
        if let Some(target) = target
            && let Some(id) =
                entries.get(target).and_then(|entry| entry.get("id")).and_then(Value::as_i64)
        {
            call_page(&mut state, port, "Page.navigateToHistoryEntry", json!({ "entryId": id }))
                .await?;
        }
        let reply = call_page(&mut state, port, "Page.getNavigationHistory", json!({})).await?;
        let nav = input::history_state(&reply)
            .ok_or_else(|| "the page's history could not be read".to_owned())?;
        profile.publish_nav(nav);
        Ok(nav)
    }

    /// A named profile is deleted by the seat that owns it; the shared
    /// browser is not a profile.
    async fn delete(&self, seat: &SessionSlot, args: Value) -> Result<Vec<BrowserPart>, String> {
        let name = args
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "browser_profile_delete needs the profile's `name`".to_owned())?;
        self.delete_named(seat, name).await?;
        Ok(vec![BrowserPart::Text { text: format!("deleted the '{name}' profile") }])
    }

    /// Delete one named profile and everything under its directory.
    async fn delete_named(&self, seat: &SessionSlot, name: &str) -> Result<(), String> {
        if name == SHARED_PROFILE {
            return Err(
                "the shared browser is not a profile, so there is nothing to delete".to_owned()
            );
        }
        // **The name becomes a path, so it is checked before any path use.**
        // A delete that skipped this joined a traversal onto the profile
        // root and removed a directory outside it.
        let name = checked_name(Some(name))?.unwrap_or_default();
        let key = name.clone();
        // **Ownership is read from the record, not the map**: a profile that
        // is not open is still its owner's to delete.
        match profiles::owner_of(&self.store.named(&name))? {
            Some(owner) if &owner != seat => {
                return Err(owner_refusal(&name, &owner));
            }
            _ => {}
        }
        // **The key's gate covers the end and the removal**, while the map
        // lock is released before either: a same-name call waits, then opens
        // a fresh directory, and every other profile is served throughout.
        let gate = self.gate(&key);
        let _held = gate.lock().await;
        let profile = self.profiles.lock().await.remove(&key);
        if let Some(profile) = profile {
            profile.end().await;
        }
        std::fs::remove_dir_all(self.store.named(&name))
            .map_err(|why| format!("the '{name}' profile could not be removed: {why}"))
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
}

/// A viewer's handle on one profile, as the transport keeps it.
pub struct Attachment {
    /// The profile's key, for the calls that follow (input, control, nav).
    pub key: String,
    /// The id the binary frames carry.
    pub id: u16,
    /// The newest frame, and a way to follow it.
    pub frames: watch::Receiver<Option<BrowserFrame>>,
    /// The controller and the viewer count, as they change.
    pub view: watch::Receiver<BrowserViewState>,
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
    owner: SessionSlot,
    /// The id the frames and the wire carry for this profile.
    id: u16,
    /// The profile's CEF cache root: cookies, storage and the carried page.
    dir: PathBuf,
    /// The `[browser]` encoding pair, fixed at the profile's creation.
    encoding: (u16, u8),
    /// The service's launch lock, shared by every profile.
    launch: Arc<AsyncMutex<()>>,
    /// The service's stop flag, shared by every profile.
    stopped: Arc<AtomicBool>,
    frames: watch::Sender<Option<BrowserFrame>>,
    /// The controller and viewer count, republished on every change.
    view: watch::Sender<BrowserViewState>,
    /// Where the page's history sits, published when it moves: a link click
    /// moves the bar's arrows without anyone asking.
    nav: watch::Sender<Option<BrowserNavState>>,
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
    /// The running take's file, when one runs.
    recording: Mutex<Option<PathBuf>>,
    /// The controller's last requested view, restored when a take ends.
    surface: Mutex<(u32, u32, f64)>,
    /// The viewer ids attached right now.
    viewers: Mutex<HashSet<u64>>,
    state: AsyncMutex<State>,
    /// The controller: its viewer id, and the last input sequence it sent.
    control: Mutex<Option<(u64, u64)>>,
    /// When a seat last used this profile, and when its owner was warned.
    touch: Mutex<Touch>,
    /// Whether this entry has been closed: **a caller that kept the handle
    /// from before the close is refused**, rather than starting a second
    /// host on a directory the map no longer names.
    ended: AtomicBool,
    /// Whether the host is up, for the strip's read: a lock-free flag so the
    /// list never waits on a call in flight.
    up: AtomicBool,
    /// Browser calls running on this profile now: the strip's work ring.
    busy: AtomicU32,
}

/// One profile as the strip draws it.
fn row_of(
    key: &str,
    owner: Option<&SessionSlot>,
    profile: Option<&Arc<Profile>>,
) -> BrowserProfileRow {
    BrowserProfileRow {
        name: key.to_owned(),
        owner: owner.cloned(),
        up: profile.is_some_and(|profile| profile.up.load(Ordering::Acquire)),
        windowed: profile.is_some_and(|profile| {
            !profile.viewers.lock().unwrap_or_else(PoisonError::into_inner).is_empty()
        }),
        busy: profile.is_some_and(|profile| profile.busy.load(Ordering::Acquire) > 0),
        // **The shared browser is never a closed row**: it is not a profile
        // to delete, and the strip's bulk door takes closed rows.
        closed: key != SHARED_PROFILE && profile.is_none(),
        unreadable: false,
    }
}

/// The most a finished download may carry to a viewer.
const MAX_DOWNLOAD_BYTES: u64 = 32 * 1024 * 1024;

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
fn read_download(dir: &Path, name: &str, path: &str, size: u64) -> BrowserDownload {
    let downloads = dir.join("downloads");
    let path = Path::new(path);
    if !path.starts_with(&downloads) {
        tracing::warn!(
            "a download named a path outside its profile and is refused (event_name \
             browser_download_outside, name {name}, path {})",
            path.display()
        );
        return BrowserDownload {
            name: name.to_owned(),
            bytes: Vec::new(),
            error: Some("the downloaded file could not be read on the server".to_owned()),
        };
    }
    if size > MAX_DOWNLOAD_BYTES {
        let _ = std::fs::remove_file(path);
        return BrowserDownload {
            name: name.to_owned(),
            bytes: Vec::new(),
            error: Some("this file is larger than 32 MiB, so the viewer cannot save it".to_owned()),
        };
    }
    let bytes = std::fs::read(path);
    let _ = std::fs::remove_file(path);
    match bytes {
        Ok(bytes) => BrowserDownload { name: name.to_owned(), bytes, error: None },
        Err(why) => {
            tracing::warn!(
                "a finished download could not be read (event_name browser_download_unreadable, \
                 name {name}): {why}"
            );
            BrowserDownload {
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
fn restore_surface(profile: &Profile, ring: &Arc<crate::osr::Ring>) {
    let (width, height, scale) = *profile.surface.lock().unwrap_or_else(PoisonError::into_inner);
    ring.request(width, height, scale);
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

/// One call's mark on its profile, taken off however the call ends.
struct Busy<'a>(&'a AtomicU32);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Profile {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn id(&self) -> u16 {
        self.id
    }

    pub fn owner(&self) -> &SessionSlot {
        &self.owner
    }

    /// The controller, when one holds the role.
    pub fn controller(&self) -> Option<u64> {
        self.control.lock().unwrap_or_else(PoisonError::into_inner).map(|(holder, _)| holder)
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
    async fn file_chooser(self: &Arc<Self>, chooser: driver::FileChooser) {
        // **The whole decision is one critical section**: the previous ask's
        // clear and cancel, the role read, and the new ask's publication.
        let _decision = self.file_decision.lock().await;
        if self.ended.load(Ordering::Acquire) || !self.up.load(Ordering::Acquire) {
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
        let Some(controller) = self.controller() else {
            tracing::debug!(
                "a page's file chooser is held for the agent (event_name \
                 browser_file_chooser_held, profile {})",
                self.key
            );
            return;
        };
        let id = self.next_file_ask.fetch_add(1, Ordering::Relaxed);
        *self.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(id);
        self.files.send_replace(Some(BrowserFileAsk { id, multiple: chooser.multiple }));
        tracing::info!(
            "a page's file chooser went to the controller (event_name browser_file_chooser, \
             profile {}, id {id}, viewer {controller}, url {})",
            self.key,
            chooser.url,
        );
    }

    /// Publish where the page's history sits, when it is news.
    fn publish_nav(&self, nav: BrowserNavState) {
        self.nav.send_if_modified(|current| {
            if *current == Some(nav) {
                false
            } else {
                *current = Some(nav);
                true
            }
        });
    }

    /// Republish the viewer state: the controller and how many are attached.
    fn publish_view(&self) {
        let controller = self.controller();
        let viewers =
            u32::try_from(self.viewers.lock().unwrap_or_else(PoisonError::into_inner).len())
                .unwrap_or(u32::MAX);
        self.view.send_replace(BrowserViewState { controller, viewers });
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
        // A host that died is replaced; a live one keeps its ring.
        if let Some(host) = state.host.as_ref()
            && !spawn::alive(host.pid)
        {
            self.teardown(state).await;
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
            })?
            .serving(&self.dir);
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
        tokio::spawn(async move {
            loop {
                match choosers.recv().await {
                    Ok(chooser) => this.file_chooser(chooser).await,
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
        state.chooser_generation = installed_generation(state);
        state.driver = Some(driver);
        self.watch_nav(state);
        Ok(())
    }

    /// Start the page's navigation watch for this host, once.
    fn watch_nav(self: &Arc<Self>, state: &mut State) {
        if state.nav_watch.is_some() {
            return;
        }
        if let Some(port) = state.host.as_ref().map(|host| host.port) {
            state.nav_watch = Some(start_nav_watch(Arc::clone(self), port));
        }
    }

    /// Spawn the host, wait for its endpoint, adopt its ring and install the
    /// passkey veto.
    async fn start_host(self: &Arc<Self>) -> Result<ActiveHost, String> {
        let _launch = self.launch.lock().await;
        // **The frame socket is a short path outside the profile**: a name
        // under app-support exceeds a Unix socket's address, and an omitted
        // profile is the seat's own now, whose folded name is longer than
        // `shared` ever was.
        let socket = channel::frame_socket_path(&self.dir, self.id)?;
        let frames = self.frames.clone();
        let audio = self.audio.clone();
        let reader = channel::serve(&socket, move |message| match message {
            channel::ChannelMessage::Frame(frame) => {
                // **The newest frame wins, and it is KEPT even with no
                // viewer attached.** `send` drops a value nobody is
                // watching, and the first paint happens before the first
                // viewer attaches - the page would then sit blank until
                // something repainted it.
                let encoded = frame.encode();
                frames.send_replace(Some(BrowserFrame {
                    profile: frame.profile,
                    seq: frame.seq,
                    encoded,
                }));
            }
            // **Audio is a stream, not a newest-wins value**: a viewer that
            // falls behind drops a packet and hears a glitch rather than
            // carrying everything from the start of playback.
            channel::ChannelMessage::Audio(packet) => {
                let _ = audio.send(BrowserAudio {
                    profile: packet.profile,
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
        if let Err(why) = spawn::wait_ready_for(pid, port, &ring_name, &self.dir).await {
            tracing::warn!(
                "a browser host never came ready (event_name browser_host_not_ready, profile {}, \
                 pid {pid}, port {port}): {why}",
                self.key,
            );
            spawn::end_verified(pid, port, &self.dir).await;
            reader.stop();
            return Err(why);
        }
        let ring = match osr::Ring::open(&ring_name) {
            Ok(ring) => Arc::new(ring),
            Err(why) => {
                tracing::warn!(
                    "a browser host's ring could not be adopted (event_name \
                     browser_host_ring_failed, profile {}, pid {pid}, port {port}): {why}",
                    self.key,
                );
                spawn::end(pid).await;
                reader.stop();
                return Err(why);
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
        // before the profile serves any call, so the first document the
        // driver navigates to starts behind it. **An accepted carry is HELD**:
        // the host is still on `about:blank` for a moment, and a probe
        // meeting that blank page must not erase the page the hold exists
        // for.
        let carried = host::carried_page(&self.dir);
        match veto::install(port, &self.dir, Some(carried.clone())).await {
            Ok(veto::Carried::Opened) => crate::carry::hold(&self.dir, &carried),
            Ok(veto::Carried::Failed(why)) => tracing::warn!(
                "the carried page did not open for {} (event_name browser_carry_failed): {why}",
                self.key
            ),
            Ok(veto::Carried::None) => {}
            Err(why) => tracing::warn!(
                "the passkey veto did not install for {} (event_name browser_veto_failed): {why}",
                self.key
            ),
        }
        tracing::info!(
            "the browser host is up (event_name browser_host_up, profile {}, pid {pid}, port {port})",
            self.key
        );
        // **A host that dies under a viewer is said, not left as a frozen
        // frame.** The watch carries the reason to every attached client; a
        // deliberate close aborts this first, so it never reads as a crash.
        let failures = self.failures.clone();
        let files = self.files.clone();
        let dir = self.dir.clone();
        let key = self.key.clone();
        let profile = Arc::downgrade(self);
        let monitor = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let Some(status) = spawn::exit(pid) else { continue };
                // **The cause is the point of the line**: a signal and an
                // exit code mean different things to whoever reads it, and
                // the surface's sentence carries the same words.
                let cause = death_cause(status);
                tracing::warn!(
                    event_name = "browser_host_died",
                    profile = %key,
                    pid,
                    port,
                    cause = %cause,
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
                    *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = None;
                    *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
                        ChooserLedger::default();
                    profile.up.store(false, Ordering::Release);
                }
                files.send_replace(None);
                let _ = std::fs::remove_dir_all(dir.join("uploads"));
                failures.send_replace(Some(format!(
                    "the browser host exited ({cause}; pid {pid}); the next call brings it back"
                )));
                return;
            }
        });
        // **A page's file chooser goes where a person is.** The host writes
        // the request on its stdout; the transport offers it to the viewer
        // that owns the role, and `answer_files` hands the host the paths.
        let profile = Arc::downgrade(self);
        let downloads = self.downloads.clone();
        let recorded = self.recorded.clone();
        let dir = self.dir.clone();
        let key = self.key.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    crate::control::HostEvent::FileDialog { id, multiple } => {
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
                             profile {key}, id {id}, multiple {multiple})"
                        );
                        profile.files.send_replace(Some(BrowserFileAsk { id, multiple }));
                    }
                    crate::control::HostEvent::Download { name, path, size } => {
                        let download = read_download(&dir, &name, &path, size);
                        if downloads.send(download).is_err() {
                            tracing::debug!(
                                "a download had no viewer to take it (event_name \
                                 browser_download_unclaimed, profile {key}, name {name}, \
                                 size {size})"
                            );
                        }
                    }
                    crate::control::HostEvent::Recording {
                        started,
                        seconds,
                        bytes,
                        capped,
                        error,
                    } => {
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

    /// End whatever is up: the driver, then the host, then the frames.
    async fn teardown(&self, state: &mut State) {
        // **The profile is down before its chooser is cleared.** A chooser
        // event parked inside the driver's cancel resumes after this and must
        // see the profile down, or it would publish an ask for a dead host.
        self.up.store(false, Ordering::Release);
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
        state.cdp = None;
        state.cdp_events = None;
        state.cdp_path = None;
        if let Some(watch) = state.nav_watch.take() {
            watch.abort();
        }
        if let Some(host) = state.host.take() {
            if let Some(monitor) = host.monitor {
                monitor.abort();
            }
            spawn::end_verified(host.pid, host.port, &self.dir).await;
            if let Some(reader) = host.reader {
                reader.stop();
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
                // **A clean end takes its own trail with it**: the record,
                // the singleton lock and the ring are the reaper's, and none
                // is left for the next boot to trip over.
                spawn::remove_record(&self.dir);
                spawn::clear_singleton(&self.dir);
                osr::Ring::unlink(&spawn::ring_name(host.port));
            }
        }
    }

    /// End the profile for good, as a close or a delete does.
    pub async fn end(&self) {
        self.ended.store(true, Ordering::Release);
        // **Ending is one chooser decision too**: no answer or chooser event
        // can publish against a profile that is gone.
        let _decision = self.file_decision.lock().await;
        let mut state = self.state.lock().await;
        self.teardown(&mut state).await;
        *self.control.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.viewers.lock().unwrap_or_else(PoisonError::into_inner).clear();
        self.publish_view();
        self.frames.send_replace(None);
        self.nav.send_replace(None);
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

/// How long the watch waits for a page target that is not there yet.
const NAV_RETRY: Duration = Duration::from_millis(500);

/// Watch the page's own navigations and publish the bar's arrows.
///
/// **The page navigates without anyone asking**: a link click, a redirect
/// and a fragment change all move the history, so the bar reads this watch
/// rather than the next input's answer. The events that count are
/// [`input::history_event`]'s; a read that fails is no news, because the
/// frame may not be attached yet and greyed arrows are worse than the last
/// pair standing.
fn start_nav_watch(profile: Arc<Profile>, port: u16) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if profile.ended.load(Ordering::Acquire) {
                return;
            }
            let Some(url) = spawn::page_websocket(port).await else {
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
            // not the page a restart reopens; the tree names the main frame
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
                let method = event.get("method").and_then(Value::as_str).unwrap_or_default();
                if let Some(frame) = input::navigated_frame_id(&event) {
                    main_frame = Some(frame.to_owned());
                }
                if !input::history_event(method, &event, main_frame.as_deref()) {
                    continue;
                }
                // **A page the person navigated to is the page a restart
                // reopens.** A link click from a viewer is neither a call nor
                // a raise, so without this the carry kept whatever the
                // session last drove; only a main-frame web navigation is
                // recorded, and a non-web one can never clear the marker from
                // here. A same-document entry counts too: it moves the
                // history and names its URL without navigating a frame.
                if let Some(url) = input::navigation_url(method, &event, main_frame.as_deref())
                    && crate::carry::is_web(url)
                {
                    crate::carry::record(&profile.dir, url);
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
                profile.publish_nav(nav);
            }
            // The page's connection closed (a tab change, a dying renderer):
            // follow the page target again.
            if profile.ended.load(Ordering::Acquire) {
                return;
            }
            tokio::time::sleep(NAV_RETRY).await;
        }
    })
}

/// The page's input connection, (re)opened when the page target moved.
///
/// **One page, one connection.** The target's path changes when a driver
/// opens a tab, and every input and history call follows it.
async fn page_cdp(state: &mut State, port: u16) -> Result<&mut Cdp, String> {
    let path = spawn::page_websocket(port)
        .await
        .ok_or_else(|| "the browser has no page to receive input".to_owned())?;
    let Some(path) = path.strip_prefix(&spawn::ws_prefix(port)).map(str::to_owned) else {
        return Err("the page's websocket is not on the host's port".to_owned());
    };
    if state.cdp_path.as_deref() != Some(path.as_str()) {
        let (cdp, events) = Cdp::connect_watch(port, &path).await?;
        state.cdp = Some(cdp);
        state.cdp_events = Some(events);
        state.cdp_path = Some(path);
    }
    // **The page's events are drained and dropped.** Input and history are
    // send-and-reply paths; the channel exists because the reader task needs
    // a live one, and an unread backlog on a page that reports many events is
    // growth nobody would see.
    if let Some(events) = state.cdp_events.as_mut() {
        while events.try_recv().is_ok() {}
    }
    state.cdp.as_mut().ok_or_else(|| "the page's input connection is not open".to_owned())
}

/// Send one CDP call to the page without waiting for its reply.
async fn send_page(
    state: &mut State,
    port: u16,
    method: &'static str,
    params: Value,
) -> Result<(), String> {
    page_cdp(state, port).await?.send(method, params).await
}

/// One CDP call to the page, answered.
async fn call_page(
    state: &mut State,
    port: u16,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    page_cdp(state, port).await?.call(method, params, None).await
}

/// What a profile holds once it is up.
#[derive(Default)]
struct State {
    host: Option<ActiveHost>,
    driver: Option<Arc<Driver>>,
    /// Bumped whenever a driver is installed, so a call's dead-driver clear
    /// cannot wipe a newer driver's handle started in between.
    chooser_generation: u64,
    /// When this profile last rebuilt its driver after a dead page context;
    /// the cooldown stops a browser that stays dead costing a driver start
    /// on every call.
    stale_since: Option<Instant>,
    /// The input connection to the page target, and the path it was opened
    /// on so a tab change reconnects.
    cdp: Option<Cdp>,
    cdp_events: Option<mpsc::UnboundedReceiver<Value>>,
    cdp_path: Option<String>,
    /// The page's navigation watch: the bar's arrows after a link click.
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
    if alive || state.chooser_generation != generation {
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
    state.chooser_generation
}

/// The generation a fresh install takes: one past the current, so a clear
/// aimed at the old one can never match the new driver.
fn installed_generation(state: &State) -> u64 {
    state.chooser_generation.wrapping_add(1)
}

/// Whether repeating a tool call cannot land twice on the page.
///
/// A dead page context is rebuilt and the call repeated only here; a click,
/// a submit or a type is handed back with the rebuild named instead.
fn repeatable(tool: &str, args: &Value) -> bool {
    match tool {
        "browser_navigate" => {
            args.get("url").and_then(Value::as_str).is_some_and(|url| !url.is_empty())
        }
        "browser_snapshot" => true,
        "browser_tabs" => {
            matches!(args.get("action").and_then(Value::as_str), Some("list" | "select"))
        }
        _ => false,
    }
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
}

/// **The answer never leads with the driver's `_page` TypeError**: it names
/// an internal Playwright object, and a session can act only on what died
/// and what to do now (#2534). The driver's line stays in the rebuild log.
fn dead_page_answer(outcome: DeadPageOutcome<'_>) -> String {
    let died = "the browser's page context died (its renderer went away)";
    match outcome {
        DeadPageOutcome::NotRepeated { tool } => format!(
            "{died} and the driver has been rebuilt. {tool} was not run again because repeating \
             it could land twice; run it again now."
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

/// The answer for a driver that never attached.
///
/// **A bare `initializeServer` timeout names nothing**: not the browser, not
/// the profile, not why. The host's pid, port and its own log's last words
/// are what a recurrence is diagnosed by, and the outcome says what actually
/// happened to the host so the next call is understood: a fresh browser when
/// it was ended, or a host left alone because the record was not this
/// server's to end (#2534).
fn attach_failure(pid: u32, port: u16, profile: &Path, tail: &str, ended: bool) -> String {
    let words = if tail.is_empty() {
        "its log is empty".to_owned()
    } else {
        format!("its log ends: {tail}")
    };
    let outcome = if ended {
        "The host was ended through its SIGTERM path, which closes the browser and lets CEF \
         flush the profile, so the next call starts a fresh one with the data intact."
    } else {
        // **Neutral on purpose**: a false answer covers a pid that is not
        // this server's host to signal and a newer driver that already owns
        // the profile, and the sentence must not claim either cause.
        "This call left the host running: it did not signal one, so the next call may meet it \
         again."
    };
    format!(
        "the browser host (pid {pid}, port {port}, profile {}) answered its DevTools endpoint \
         but the driver could not attach within its 30 s bound: the host's page did not answer. \
         {outcome} {words}",
        profile.display(),
    )
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

/// Take the `profile` argument off a call.
///
/// It chooses the profile and no driver's schema declares it, so it never
/// reaches a tool. A `profile` that is not a name is the call's own mistake,
/// answered rather than guessed at. **`context` is read as the same thing**:
/// an older schema names the argument that way, and an argument left in the
/// call would reach the driver as an unknown key while the SHARED profile
/// drove - the wrong browser, silently.
pub fn take_profile(mut args: Value) -> Result<(Option<String>, Value), String> {
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

/// A seat as a person reads it: `org/project/label`.
fn seat_text(seat: &SessionSlot) -> String {
    format!("{}/{}/{}", seat.org(), seat.project(), seat.label())
}

/// The refusal a seat naming another seat's profile gets.
fn owner_refusal(name: &str, owner: &SessionSlot) -> String {
    format!("the profile '{name}' belongs to {}; another seat cannot use it", seat_text(owner))
}

/// The view a resize asks for, when the caller holds the role: the role as a
/// value, so a watcher's window is refused before any host is touched.
fn controller_size(
    controller: Option<u64>,
    viewer: u64,
    width: u32,
    height: u32,
    scale: f64,
) -> Option<(u32, u32, f64)> {
    if controller != Some(viewer) {
        return None;
    }
    let scale = if scale.is_finite() && scale > 0.0 { scale.clamp(0.1, 8.0) } else { 1.0 };
    Some((width.max(1), height.max(1), scale))
}

/// A profile name for a door that only acts on named profiles: the reserved
/// shared name is refused with its own sentence, and any other name by the
/// registry's rule.
fn checked_name(name: Option<&str>) -> Result<Option<String>, String> {
    let Some(name) = name else {
        return Ok(None);
    };
    if name == SHARED_PROFILE {
        return Err(
            "the shared browser is not a named profile, so it has no row to act on".to_owned()
        );
    }
    if let Some(refusal) = profiles::name_refusal(name) {
        return Err(refusal);
    }
    Ok(Some(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{
        BrowserService, ChooserAnswer, ChooserFuture, ChooserLedger, DeadPageOutcome,
        SHARED_PROFILE, State, UploadRequest, agent_upload_paths, agent_upload_refused,
        attach_failure, begin_upload, captured_generation, checked_name, clear_if_dead,
        controller_size, dead_page_answer, death_cause, file_name, installed_generation,
        note_agent_answered, note_surfaced_opened, note_upload_finished, row_of,
        stage_agent_uploads, take_profile, upload_request, write_uploads,
    };
    use forge_primitives::SessionSlot;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::Notify;

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
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex, PoisonError};
    use tokio::sync::mpsc;

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
        let read = read_download(dir.path(), "small.txt", small.to_str().expect("a path"), 5);
        assert_eq!(read.bytes, b"hello", "the bytes are handed on");
        assert!(read.error.is_none());
        assert!(!small.exists(), "the server's copy goes once it is read");

        let big = downloads.join("2-big.bin");
        let read = read_download(
            dir.path(),
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
        let read = read_download(dir.path(), "elsewhere.txt", outside.to_str().expect("a path"), 1);
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.files.send_replace(Some(BrowserFileAsk { id: 4, multiple: false }));
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        assert!(profile.files.borrow().is_none(), "the ask is cancelled with the take");

        profile.files.send_replace(Some(BrowserFileAsk { id: 5, multiple: false }));
        service.release_control(SHARED_PROFILE, 7).await;
        assert!(profile.files.borrow().is_none(), "and with the release");

        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves back");
        profile.files.send_replace(Some(BrowserFileAsk { id: 6, multiple: false }));
        service.detach(7).await;
        assert!(profile.files.borrow().is_none(), "and when the controller detaches");
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);

        profile
            .file_chooser(FileChooser {
                multiple: true,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;
        assert!(
            profile.files.borrow().is_none(),
            "no controller: the chooser is held for the agent's upload",
        );
        assert!(
            profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "and no person is asked",
        );

        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
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

        service.release_control(SHARED_PROFILE, 7).await;
        assert!(profile.files.borrow().is_none(), "the ask leaves with the role it was asked for");
        assert!(
            profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "and the driver's chooser is not answered again",
        );
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");

        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;
        let first = profile.files.borrow().as_ref().copied().expect("the first ask");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;
        let second = profile.files.borrow().as_ref().copied().expect("the second ask");
        assert_ne!(first.id, second.id, "the second chooser is a new ask");

        let refused = service
            .answer_files(
                SHARED_PROFILE,
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;

        assert!(agent_upload_refused(&profile), "the person's picker is open");
        service.release_control(SHARED_PROFILE, 7).await;
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");

        note_agent_answered(&profile);
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;
        assert!(profile.files.borrow().is_none(), "the owed event is consumed, not asked");
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0));
        }

        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);

        // No controller: the chooser is surfaced and held for the agent.
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
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

        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;

        let refused = begin_upload(&profile, Some(&UploadRequest::Cancel))
            .await
            .expect_err("the cancel form is refused too");
        assert!(refused.contains("picker"), "{refused}");
        service.release_control(SHARED_PROFILE, 7).await;
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
        let service =
            Arc::new(BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack")));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        let gate = Arc::new(GatedAnswer { entered: Notify::new(), release: Notify::new() });
        let calls: Arc<dyn ChooserAnswer> = gate.clone();
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;
        let first = profile.files.borrow().as_ref().copied().expect("the first ask");

        let superseding = {
            let profile = Arc::clone(&profile);
            tokio::spawn(async move {
                profile
                    .file_chooser(FileChooser {
                        multiple: false,
                        current: true,
                        url: "http://y/".to_owned(),
                    })
                    .await;
            })
        };
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .expect("the gated driver call starts");

        let mut answering = {
            let service = Arc::clone(&service);
            tokio::spawn(
                async move { service.answer_files(SHARED_PROFILE, first.id, Vec::new()).await },
            )
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        let gate = Arc::new(GatedAnswer { entered: Notify::new(), release: Notify::new() });
        let calls: Arc<dyn ChooserAnswer> = gate.clone();
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;

        let waiting = {
            let profile = Arc::clone(&profile);
            tokio::spawn(async move {
                profile
                    .file_chooser(FileChooser {
                        multiple: false,
                        current: true,
                        url: "http://y/".to_owned(),
                    })
                    .await;
            })
        };
        tokio::time::timeout(Duration::from_secs(2), gate.entered.notified())
            .await
            .expect("the gated driver call starts");

        let mut state = State::default();
        profile.teardown(&mut state).await;
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;
        assert!(profile.files.borrow().is_some());
        // Two more surfaces than the open ask: a drain that consumed a
        // constant one would leave them behind and fail the assertion below.
        note_surfaced_opened(&profile, 2);

        service.release_control(SHARED_PROFILE, 7).await;
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0), "the drain consumed it");
        }

        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves back");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://y/".to_owned(),
            })
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;
        let ask = profile.files.borrow().as_ref().copied().expect("the ask");

        service.answer_files(SHARED_PROFILE, ask.id, Vec::new()).await.expect("the answer lands");

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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");

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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, multiple: false }));
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, multiple: false }));
        *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(5);
        *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger { surfaced: 1, pre_answered: 0 };
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);

        // A newer driver installed after the call's lookup: generation 2.
        let mut state = State { chooser_generation: 2, ..Default::default() };
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
        let mut state = State { chooser_generation: 1, ..Default::default() };
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
        let mut state = State { chooser_generation: 7, ..Default::default() };
        assert_eq!(captured_generation(&state), 7, "a call carries the installed generation");
        assert_eq!(installed_generation(&state), 8, "an install moves one past it");
        state.chooser_generation = u64::MAX;
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, multiple: false }));
        *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(5);
        *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger { surfaced: 1, pre_answered: 0 };
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        // A newer driver is installed: generation 2.
        profile.state.lock().await.chooser_generation = 2;

        let why = service
            .after_failure(&profile, 1, 1, 9, "browser_click", &json!({}), Err(dead.to_owned()))
            .await
            .expect_err("a stale failure is not this call's to rebuild");
        assert_eq!(why, dead, "the call's own failure is handed back unchanged");
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
            .after_failure(&profile, 2, 1, 9, "browser_click", &json!({}), Err(dead.to_owned()))
            .await
            .expect_err("a closed profile cannot rebuild");
        assert!(why.contains("could not be rebuilt"), "{why}");
        assert!(profile.files.borrow().is_none(), "the dead driver's ask is cleared");
        assert!(
            profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "and its handle with it",
        );
        let again = service
            .after_failure(&profile, 2, 1, 9, "browser_click", &json!({}), Err(dead.to_owned()))
            .await
            .expect_err("the cooldown refuses a second rebuild");
        assert!(again.contains("not been rebuilt a second time yet"), "{again}");
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, multiple: false }));
        *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(5);
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        profile.state.lock().await.chooser_generation = 1;

        // The rebuild parks on the file decision this test holds; while it
        // waits, a newer driver installs.
        let held = profile.file_decision.lock().await;
        let bump = async {
            tokio::task::yield_now().await;
            profile.state.lock().await.chooser_generation = 2;
            drop(held);
        };
        let no_args = json!({});
        let (outcome, ()) = tokio::join!(
            service.after_failure(
                &profile,
                1,
                1,
                9,
                "browser_click",
                &no_args,
                Err(dead.to_owned()),
            ),
            bump,
        );
        let why = outcome.expect_err("the rebuild waited and then saw the newer generation");
        assert_eq!(why, dead, "the call's own failure is handed back");
        assert!(profile.files.borrow().is_some(), "the newer driver's ask survives");
        assert!(
            profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).is_some(),
            "and its handle survives",
        );
    }

    /// **A wedged host is not ended for a generation this call does not
    /// own**, and the answer says no host was signalled.
    #[tokio::test]
    async fn a_wedged_host_is_not_ended_for_a_newer_generation() {
        use forge_primitives::browser::BrowserFileAsk;

        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, multiple: false }));
        let calls: Arc<dyn ChooserAnswer> = Arc::new(InstantAnswer);
        *profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(calls);
        profile.state.lock().await.chooser_generation = 2;

        assert!(
            !service.end_wedged_host(&profile, 1, 1, 9).await,
            "a generation mismatch ends nothing, so the answer may not claim an end",
        );
        assert!(profile.files.borrow().is_some(), "and the newer driver's ask survives");
        assert!(
            profile.chooser_answer.lock().unwrap_or_else(PoisonError::into_inner).is_some(),
            "with its handle",
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger { surfaced: 0, pre_answered: 1 };

        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: false,
                url: "http://background/".to_owned(),
            })
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
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://foreground/".to_owned(),
            })
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;
        assert!(profile.files.borrow().is_some());

        service.detach(7).await;
        assert!(profile.files.borrow().is_none(), "the open ask leaves with the controller");
        assert!(
            profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "and the driver's chooser is not answered again",
        );

        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");

        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: false,
                url: "http://x/".to_owned(),
            })
            .await;
        assert!(profile.files.borrow().is_none(), "the background tab is not asked");
        assert!(profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),);
        {
            let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
            assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0), "and it is not counted");
        }

        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.up.store(true, Ordering::Release);
        service.take_control(SHARED_PROFILE, 7).await.expect("the role moves");
        profile
            .file_chooser(FileChooser {
                multiple: false,
                current: true,
                url: "http://x/".to_owned(),
            })
            .await;
        let ask = profile.files.borrow().as_ref().copied().expect("the ask");

        let refused = service
            .answer_files(
                SHARED_PROFILE,
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        profile.files.send_replace(Some(BrowserFileAsk { id: 5, multiple: false }));
        *profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner) = Some(5);
        *profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner) =
            ChooserLedger { surfaced: 1, pre_answered: 0 };

        let mut state = State::default();
        profile.teardown(&mut state).await;

        assert!(profile.files.borrow().is_none(), "the ask is cleared");
        assert!(
            profile.playwright_ask.lock().unwrap_or_else(PoisonError::into_inner).is_none(),
            "the driver's chooser is cleared",
        );
        let ledger = profile.chooser_ledger.lock().unwrap_or_else(PoisonError::into_inner);
        assert_eq!((ledger.surfaced, ledger.pre_answered), (0, 0));
    }

    /// **A stop refuses by name when there is no take**, and never starts a
    /// browser to look for one.
    #[tokio::test]
    async fn a_stop_with_no_take_is_refused_by_name() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let service = BrowserService::at(dir.path().to_path_buf(), dir.path().join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let refused = service.record_stop(&seat, None).await.expect_err("no take is running");
        assert!(
            refused.contains("no recording is running"),
            "the stop names the missing take: {refused}"
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        let uploads = profile.dir.join("uploads");
        std::fs::create_dir_all(&uploads).expect("the uploads dir");
        let kept = uploads.join("kept.txt");
        std::fs::write(&kept, b"a live dialog's file").expect("a live file");
        profile.files.send_replace(Some(BrowserFileAsk { id: 7, multiple: false }));

        let refused = service
            .answer_files(
                SHARED_PROFILE,
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
        let profile = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("a profile entry");
        let uploads = profile.dir.join("uploads");
        std::fs::create_dir_all(&uploads).expect("the uploads dir");
        std::fs::write(uploads.join("stale.txt"), b"stale").expect("a staged file");
        profile.files.send_replace(Some(BrowserFileAsk { id: 3, multiple: false }));

        service.close(&seat, Some(SHARED_PROFILE)).await.expect("the profile closes");
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
        let open = service.open(&seat, None).await;
        let profile = service.entry(&seat, None).await.expect("the profile entry");
        let mut failures = profile.failures.subscribe();
        // **A dead host takes its chooser and its staged uploads with it.**
        let uploads = profile.dir.join("uploads");
        std::fs::create_dir_all(&uploads).expect("the uploads dir");
        std::fs::write(uploads.join("staged.txt"), b"a chosen file").expect("a staged file");
        profile.files.send_replace(Some(forge_primitives::browser::BrowserFileAsk {
            id: 5,
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

        let _ = service.open(&seat, None).await;
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

    /// **A profile argument is taken off and must be a name.** A number is the
    /// call's own mistake, answered before any directory is consulted; a bare
    /// call is the asking seat's own profile (see
    /// [`BrowserService::target`]).
    #[test]
    fn a_profile_argument_is_taken_off_and_must_be_a_name() {
        let (name, rest) = take_profile(json!({ "url": "https://example.com", "profile": "hunt" }))
            .expect("a profile");
        assert_eq!(name.as_deref(), Some("hunt"));
        assert_eq!(rest, json!({ "url": "https://example.com" }));
        assert_eq!(take_profile(json!({ "url": "https://example.com" })).expect("none").0, None);
        assert_eq!(take_profile(serde_json::Value::Null).expect("bare").0, None);
        assert_eq!(take_profile(json!({ "profile": null })).expect("null").0, None);
        assert!(take_profile(json!({ "profile": 7 })).is_err(), "a number is not a name");
        assert_eq!(
            take_profile(json!({ "context": "hunt" })).expect("a v6 name").0.as_deref(),
            Some("hunt"),
        );
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
        let handed = dead_page_answer(DeadPageOutcome::NotRepeated { tool: "browser_tabs" });
        assert!(handed.contains("browser_tabs was not run again"), "{handed}");
        assert!(handed.contains("run it again now"), "{handed}");
    }

    /// **A wedged host's answer names it and says what happened to it**, not
    /// a bare `initializeServer` timeout: the pid, port, profile and the
    /// host's own last words are the diagnosis (#2534).
    #[test]
    fn an_attach_failure_names_the_host_and_its_outcome() {
        let ended = attach_failure(1234, 9411, std::path::Path::new("/p/cef"), "line", true);
        assert!(ended.contains("pid 1234"), "{ended}");
        assert!(ended.contains("port 9411"), "{ended}");
        assert!(ended.contains("/p/cef"), "{ended}");
        assert!(ended.contains("SIGTERM path"), "{ended}");
        assert!(ended.contains("its log ends: line"), "{ended}");
        let left = attach_failure(9, 8, std::path::Path::new("/p/cef"), "", false);
        assert!(left.contains("left the host running"), "{left}");
        assert!(!left.contains("pid is not this server's host"), "no cause is claimed: {left}");
        assert!(left.contains("its log is empty"), "{left}");
    }

    /// **An omitted profile is the asking seat's own, never the shared
    /// browser** (#2539). The shared browser's tabs are whatever the person
    /// or another seat left there, so it is reachable only by naming it
    /// `@shared`, which cannot collide with a real profile.
    #[tokio::test]
    async fn an_omitted_profile_is_the_seats_own_and_the_shared_browser_is_named() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");

        let mine = service.entry(&seat, None).await.expect("the seat's own");
        assert_eq!(mine.key, "proj-lead", "the seat's own profile, by its folded name");
        assert_eq!(&mine.owner, &seat);

        let shared = service.entry(&seat, Some(SHARED_PROFILE)).await.expect("the shared browser");
        assert_eq!(shared.key, SHARED_PROFILE);
        assert_ne!(mine.id, shared.id, "two browsers, not one");
        assert_eq!(service.entry(&seat, Some("hunt")).await.expect("a named one").key, "hunt");
    }

    /// **The fold is lossy, and a collision is a refusal, never a share.**
    /// Two seats whose project and label fold to one name are refused by the
    /// owner's name rather than silently driving each other's logins.
    #[tokio::test]
    async fn a_taken_auto_name_refuses_the_second_seat_by_its_owner() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let one = SessionSlot::lead("TestOrg", "my app");
        let two = SessionSlot::lead("OtherOrg", "my-app");

        service.entry(&one, None).await.expect("the first seat's own");
        let refused =
            service.entry(&two, None).await.err().expect("the folded collision is refused");
        assert!(refused.contains("belongs to"), "{refused}");
        assert!(refused.contains("my-app-lead"), "{refused}");
    }

    /// **A name is one path segment.** A separator or a dot-dot would let a
    /// session choose a directory outside the profile tree.
    #[test]
    fn a_profile_name_is_one_path_segment() {
        assert_eq!(checked_name(Some("job-hunt")), Ok(Some("job-hunt".to_owned())));
        assert_eq!(checked_name(Some("a.b_c-1")), Ok(Some("a.b_c-1".to_owned())));
        assert!(checked_name(Some("../escape")).is_err());
        assert!(checked_name(Some("a/b")).is_err());
        assert!(checked_name(Some("..")).is_err());
        assert!(checked_name(Some("")).is_err());
        assert!(
            checked_name(Some(SHARED_PROFILE)).is_err(),
            "the shared browser is not a named profile"
        );
        assert_eq!(checked_name(None), Ok(None));
        assert!(checked_name(Some(&"a".repeat(65))).is_err(), "a name has a bound");
    }

    /// **Only the controller's surface moves the page.** A watcher's window,
    /// or one from before anyone took the role, is refused before any host is
    /// touched; a nonsense ratio draws at 1 rather than poisoning CEF.
    #[test]
    fn a_resize_is_the_controllers_and_nobody_elses() {
        assert_eq!(controller_size(None, 7, 800, 600, 2.0), None, "no role, no size");
        assert_eq!(
            controller_size(Some(8), 7, 800, 600, 2.0),
            None,
            "a watcher's window never moves the page"
        );
        assert_eq!(controller_size(Some(7), 7, 800, 600, 2.0), Some((800, 600, 2.0)));
        assert_eq!(
            controller_size(Some(7), 7, 800, 600, f64::NAN),
            Some((800, 600, 1.0)),
            "a nonsense ratio draws at 1"
        );
        assert_eq!(controller_size(Some(7), 7, 0, 0, 99.0), Some((1, 1, 8.0)), "bounded both ways");
    }

    /// **Two profiles share nothing**: their wire ids differ (what a frame
    /// is keyed by), their cache roots differ, and one profile's frame never
    /// reaches the other's stream.
    #[tokio::test]
    async fn two_profiles_share_no_id_root_or_frame_stream() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let first = service.entry(&seat, Some("a")).await.expect("the first profile");
        let second = service.entry(&seat, Some("b")).await.expect("the second profile");

        assert_ne!(first.id(), second.id(), "a frame is keyed by the profile's id");
        assert_ne!(service.profile_dir("a"), service.profile_dir("b"), "and each has its own root");
        first.frames.send_replace(Some(forge_primitives::browser::BrowserFrame {
            profile: first.id(),
            seq: 1,
            encoded: vec![1],
        }));
        assert!(first.frames.borrow().is_some(), "the first profile's stream carries its frame");
        assert!(second.frames.borrow().is_none(), "and the second's does not");
    }

    /// **The strip's rows are the seat's own**: the shared browser, its named
    /// profiles and the ownerless ones a legacy tree left - never another
    /// seat's name, and a profile only on disk reads closed.
    #[tokio::test]
    async fn the_strip_lists_only_the_seats_profiles() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let other = SessionSlot::worker("TestOrg", "proj", "runner");
        service.entry(&seat, Some("hunt")).await.expect("the seat's profile");
        service.entry(&other, Some("scout")).await.expect("another seat's");
        std::fs::create_dir_all(root.join("profiles/legacy/cef")).expect("an ownerless tree");

        let rows = service.profiles(&seat).await.expect("the list");

        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(
            names,
            [SHARED_PROFILE, "hunt", "legacy"],
            "this seat's rows, and the shared one"
        );
        let hunt = rows.iter().find(|row| row.name == "hunt").expect("hunt's row");
        assert_eq!(hunt.owner.as_ref(), Some(&seat));
        assert!(!hunt.closed, "an open entry is not a closed row");
        assert!(!hunt.up, "no host is up here");
        let legacy = rows.iter().find(|row| row.name == "legacy").expect("legacy's row");
        assert!(legacy.closed, "a profile only on disk is closed");
        assert_eq!(legacy.owner, None, "and ownerless");
    }

    /// **A profile whose owner record is unreadable is drawn, marked and
    /// ownerless.** The files exist; hiding the row made them unreachable
    /// from the strip, and the mark is what tells a client not to offer a
    /// verb it could not attribute.
    #[tokio::test]
    async fn an_unreadable_owner_record_is_drawn_and_marked() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let named = root.join("profiles/broken");
        std::fs::create_dir_all(named.join("cef")).expect("the profile tree");
        std::fs::write(named.join("owner.json"), b"{ not json").expect("a corrupt record");

        let rows = service.profiles(&seat).await.expect("the list");
        let row = rows.iter().find(|row| row.name == "broken").expect("the broken row is drawn");
        assert!(row.unreadable, "the row carries the server's unreadable mark");
        assert_eq!(row.owner, None, "and is drawn ownerless");
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
        let profile = service.entry(&seat, Some("hunt")).await.expect("the profile");
        service.take_control(&profile.key, 7).await.expect("the take");

        let why = service.nav(&profile.key, 8, true).await.expect_err("a watcher is refused");
        assert!(why.contains("not the browser's controller"), "{why}");
    }

    /// **A close is the owner's**, and a name that is a path is refused
    /// before it is used as one.
    #[tokio::test]
    async fn a_close_is_the_owners_and_a_traversal_name_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let other = SessionSlot::worker("TestOrg", "proj", "runner");
        service.entry(&seat, Some("hunt")).await.expect("the seat's profile");

        let why = service.close(&other, Some("hunt")).await.expect_err("another seat's close");
        assert!(why.contains("belongs to"), "{why}");
        let why = service.close(&seat, Some("../escape")).await.expect_err("a traversal name");
        assert!(why.contains("cannot be a profile name"), "{why}");
        service.close(&seat, Some("hunt")).await.expect("the owner closes it");
        assert!(!service.profiles.lock().await.contains_key("hunt"), "and the entry is gone");
    }

    /// **An omitted profile has no key of its own**: it resolves to the
    /// asking seat's own at the door, while `@shared` is the explicit key for
    /// the browser every seat shares.
    #[test]
    fn an_omitted_profile_resolves_to_the_seats_own() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        assert_eq!(BrowserService::target(&seat, None), "proj-lead");
        assert_eq!(BrowserService::target(&seat, Some(SHARED_PROFILE)), SHARED_PROFILE);
        assert_eq!(BrowserService::target(&seat, Some("hunt")), "hunt");
    }

    /// **A name carries its owner across a restart**: the record beside the
    /// directory refuses another seat, and a fresh service reads it back
    /// rather than handing the name to whoever opens it first.
    #[tokio::test]
    async fn a_named_profile_refuses_another_seat_across_a_restart() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let owner = SessionSlot::lead("TestOrg", "proj");
        let other = SessionSlot::worker("TestOrg", "proj", "runner");

        let service = BrowserService::at(root.clone(), root.join("stack"));
        service.entry(&owner, Some("hunt")).await.expect("the first open records its owner");

        let again = BrowserService::at(root.clone(), root.join("stack"));
        let Err(refused) = again.entry(&other, Some("hunt")).await else {
            panic!("another seat's name is refused");
        };
        assert!(refused.contains("belongs to"), "the refusal says whose it is: {refused}");
        let same = again.entry(&owner, Some("hunt")).await.expect("the owner still drives it");
        assert_eq!(same.owner(), &owner, "and it is the same profile's");
    }

    /// The workspace, as one idle pass asks it: a scripted verdict per call,
    /// and every question recorded in order.
    struct RecordingWarn {
        answers: Mutex<std::collections::VecDeque<crate::idle::Verdict>>,
        heard: mpsc::UnboundedSender<(Option<SessionSlot>, String, bool)>,
    }

    #[async_trait::async_trait]
    impl crate::idle::IdleWarn for RecordingWarn {
        async fn warn(
            &self,
            seat: Option<&SessionSlot>,
            name: &str,
            _idle_minutes: u64,
            _grace_minutes: u64,
            at_close: bool,
        ) -> crate::idle::Verdict {
            let _ = self.heard.send((seat.cloned(), name.to_owned(), at_close));
            self.answers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front()
                .unwrap_or(crate::idle::Verdict::Unanswered)
        }
    }

    /// Every question one idle pass asked: the seat to warn, the profile's
    /// name, and whether it was the close's own question.
    type Heard = mpsc::UnboundedReceiver<(Option<SessionSlot>, String, bool)>;

    /// A service with a scripted idle policy: zero minutes, so one pass is
    /// the warn and the next is the close, exactly as a half hour of waiting
    /// would be.
    fn idle_service(
        root: &std::path::Path,
        answers: Vec<crate::idle::Verdict>,
    ) -> (Arc<BrowserService>, Heard) {
        let service = Arc::new(BrowserService::at(root.to_owned(), root.join("stack")));
        let (heard, rx) = mpsc::unbounded_channel();
        service.configure_idle(
            0,
            0,
            Arc::new(RecordingWarn { answers: Mutex::new(answers.into_iter().collect()), heard }),
        );
        (service, rx)
    }

    /// **An idle named profile's owner is warned once, and the browser closes
    /// when the grace is over.** The close asks its own question first, so a
    /// hand-off parked after the warning still holds it.
    #[tokio::test]
    async fn an_idle_profile_warns_its_owner_and_closes_after_the_grace() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let (service, mut heard) =
            idle_service(&root, vec![crate::idle::Verdict::Delivered, crate::idle::Verdict::Clear]);
        let seat = SessionSlot::lead("TestOrg", "proj");
        service.entry(&seat, Some("hunt")).await.expect("owner records");

        service.idle_pass().await;
        assert_eq!(
            heard.try_recv(),
            Ok((Some(seat.clone()), "hunt".to_owned(), false)),
            "the owner is told, and the grace arms",
        );
        assert!(
            service.profiles.lock().await.contains_key("hunt"),
            "the browser lives through its grace",
        );

        service.idle_pass().await;
        assert_eq!(
            heard.try_recv(),
            Ok((Some(seat), "hunt".to_owned(), true)),
            "the close asks whether anything holds it",
        );
        assert!(!service.profiles.lock().await.contains_key("hunt"), "and closes at the end of it");
        assert!(service.store.named("hunt").exists(), "the profile's logins stay on disk");
    }

    /// **A profile whose owner has no live session closes quietly.** There is
    /// nobody to warn, so no grace is owed: the warning step itself learns
    /// there is no seat and the browser closes at the idle bound.
    #[tokio::test]
    async fn an_idle_profile_with_no_live_session_closes_quietly() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let (service, mut heard) = idle_service(&root, vec![crate::idle::Verdict::NoSession]);
        let seat = SessionSlot::lead("TestOrg", "proj");
        service.entry(&seat, Some("hunt")).await.expect("owner records");

        service.idle_pass().await;
        assert_eq!(
            heard.try_recv(),
            Ok((Some(seat), "hunt".to_owned(), false)),
            "the warning is what learns there is no one to tell",
        );
        assert!(
            !service.profiles.lock().await.contains_key("hunt"),
            "and the browser closes without a grace",
        );
        assert!(service.store.named("hunt").exists(), "the profile's logins stay on disk");
        service.idle_pass().await;
        assert!(heard.try_recv().is_err(), "a closed profile is not asked about again");
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
        let profile = service.entry(&seat, Some("hunt")).await.expect("the entry");
        service.close(&seat, Some("hunt")).await.expect("the close");

        let why =
            profile.ensure(&service.root, service.stack()).await.expect_err("the ended entry");
        assert!(why.contains("closed"), "the caller is told to open it again: {why}");
        let again = service.entry(&seat, Some("hunt")).await.expect("a fresh entry");
        assert!(!Arc::ptr_eq(&profile, &again), "the next call answers a new entry");
    }

    /// **A parked hand-off holds the close**, and the next pass asks again -
    /// an abandoned hand-off closes later rather than never.
    #[tokio::test]
    async fn a_held_profile_is_not_closed_and_is_asked_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let (service, mut heard) = idle_service(
            &root,
            vec![
                crate::idle::Verdict::Delivered,
                crate::idle::Verdict::Held,
                crate::idle::Verdict::Clear,
            ],
        );
        service.entry(&SessionSlot::lead("TestOrg", "proj"), Some("hunt")).await.expect("owner");

        service.idle_pass().await;
        assert!(heard.try_recv().is_ok(), "the owner is warned");
        service.idle_pass().await;
        assert_eq!(
            heard.try_recv().map(|(_, _, at_close)| at_close),
            Ok(true),
            "the first close question",
        );
        assert!(service.profiles.lock().await.contains_key("hunt"), "a held profile is not closed");
        service.idle_pass().await;
        assert_eq!(
            heard.try_recv().map(|(_, _, at_close)| at_close),
            Ok(true),
            "and the pass after the hold lifts asks again",
        );
        assert!(!service.profiles.lock().await.contains_key("hunt"), "then closes it");
    }

    /// **A viewer attached is a use**: the surface the person is looking at
    /// is never warned and never closed, however long it stays up.
    #[tokio::test]
    async fn a_viewed_profile_is_never_idle() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let (service, mut heard) = idle_service(&root, Vec::new());
        let profile = service
            .entry(&SessionSlot::lead("TestOrg", "proj"), Some(SHARED_PROFILE))
            .await
            .expect("the shared browser");
        profile.viewers.lock().unwrap_or_else(PoisonError::into_inner).insert(9);

        service.idle_pass().await;

        assert!(heard.try_recv().is_err(), "no question is asked about a viewed browser");
        assert!(service.profiles.lock().await.contains_key(SHARED_PROFILE));
    }

    /// **The shared browser is never announced**: its warn is a silent arm,
    /// and the close's own keyless question is its only crossing.
    #[tokio::test]
    async fn the_shared_browsers_warning_is_a_silent_arm() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let (service, mut heard) = idle_service(&root, vec![crate::idle::Verdict::Clear]);
        service
            .entry(&SessionSlot::lead("TestOrg", "proj"), Some(SHARED_PROFILE))
            .await
            .expect("the shared browser");

        service.idle_pass().await;
        assert!(heard.try_recv().is_err(), "nothing is delivered for the shared browser");

        service.idle_pass().await;
        assert_eq!(
            heard.try_recv(),
            Ok((None, SHARED_PROFILE.to_owned(), true)),
            "only the close's keyless question crosses",
        );
        assert!(!service.profiles.lock().await.contains_key(SHARED_PROFILE), "and it closes");
    }

    /// **A delete's name is checked before it becomes a path.** This name
    /// climbs out of the profiles directory and the browser root both, so a
    /// delete that trusted it removed a directory nobody named.
    #[tokio::test]
    async fn a_delete_refuses_a_traversal_name_before_touching_disk() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("a directory outside the root");
        std::fs::write(outside.join("keep"), "keep").expect("a file");
        let service = BrowserService::at(root.clone(), root.join("stack"));

        let refused = service
            .delete(
                &SessionSlot::lead("TestOrg", "proj"),
                serde_json::json!({ "name": "../../outside" }),
            )
            .await
            .expect_err("a traversal name is refused");

        assert!(refused.contains("cannot be a profile name"), "{refused}");
        assert!(outside.join("keep").exists(), "nothing outside the root was removed");
    }

    /// **The strip's row reads the profile's live marks.** A call in flight
    /// is the work ring, an attached viewer is the window mark, and a profile
    /// known only on disk is the closed row the bulk delete acts on.
    #[tokio::test]
    async fn a_row_reads_the_profiles_live_marks() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let profile = service.entry(&seat, Some("hunt")).await.expect("the profile");

        profile.busy.fetch_add(1, Ordering::AcqRel);
        profile.viewers.lock().unwrap_or_else(PoisonError::into_inner).insert(9);
        let row = row_of("hunt", Some(&seat), Some(&profile));
        assert!(row.busy, "a call in flight is the work ring");
        assert!(row.windowed, "an attached viewer is the window mark");
        assert!(!row.closed, "an open profile is not a closed row");

        let on_disk = row_of("old", None, None);
        assert!(on_disk.closed, "a profile known only on disk is a closed row");
        assert!(!on_disk.busy && !on_disk.windowed, "with no live marks");
    }

    /// **Another profile is served while one is torn down.** The map lock
    /// used to be held across the end (up to the kill bound) and a recursive
    /// delete, so every profile's entry waited on one teardown.
    #[tokio::test]
    async fn another_profile_is_served_while_one_is_torn_down() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = Arc::new(BrowserService::at(root.clone(), root.join("stack")));
        let seat = SessionSlot::lead("TestOrg", "proj");
        let a = service.entry(&seat, Some("a")).await.expect("the first profile");
        service.entry(&seat, Some("b")).await.expect("the second profile");

        // **Hold A's state lock**: the delete takes A out of the map, then
        // its end parks here - the teardown window the other profile must
        // not wait on.
        let blocked = a.state.lock().await;
        let deleting = tokio::spawn({
            let service = Arc::clone(&service);
            let seat = seat.clone();
            async move { service.delete_profile(&seat, "a").await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !service.profiles.lock().await.contains_key("a") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the delete took its entry before the teardown");

        let b = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            service.entry(&seat, Some("b")),
        )
        .await
        .expect("another profile is served during the teardown")
        .expect("the entry answers");
        assert_eq!(b.key, "b", "the untouched profile");

        drop(blocked);
        deleting.await.expect("the delete task joins").expect("the delete completes");
    }

    /// **An idle close releases the fleet's map before it tears its host
    /// down**, the same rule the close and the delete follow: another
    /// profile's entry is served while the teardown runs.
    #[tokio::test]
    async fn another_profile_is_served_while_an_idle_close_tears_one_down() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let (service, _heard) = idle_service(&root, Vec::new());
        let seat = SessionSlot::lead("TestOrg", "proj");
        let a = service.entry(&seat, Some("a")).await.expect("the first profile");
        service.entry(&seat, Some("b")).await.expect("the second profile");

        // **Hold A's state lock**: the idle close takes A out of the map and
        // then parks in its teardown, which the other profile must not wait
        // on.
        let blocked = a.state.lock().await;
        let closing = tokio::spawn({
            let service = Arc::clone(&service);
            async move { service.close_idle("a", true).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !service.profiles.lock().await.contains_key("a") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the idle close took its entry before the teardown");

        let b = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            service.entry(&seat, Some("b")),
        )
        .await
        .expect("another profile is served during the teardown")
        .expect("the entry answers");
        assert_eq!(b.key, "b", "the untouched profile");

        drop(blocked);
        closing.await.expect("the close task joins");
    }

    /// **Another seat's profile is not this seat's to delete**, open or not.
    #[tokio::test]
    async fn a_delete_refuses_another_seats_profile() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let owner = SessionSlot::lead("TestOrg", "proj");
        let other = SessionSlot::worker("TestOrg", "proj", "runner");
        service.entry(&owner, Some("hunt")).await.expect("owner records");

        let refused = service
            .delete(&other, serde_json::json!({ "name": "hunt" }))
            .await
            .expect_err("another seat's name is refused");

        assert!(refused.contains("belongs to"), "{refused}");
        assert!(service.store.named("hunt").exists(), "the profile stays");
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
        let seat = SessionSlot::lead("TestOrg", "proj");
        service.entry(&seat, Some("hunt")).await.expect("owner records");
        service.entry(&seat, Some(SHARED_PROFILE)).await.expect("the shared browser");

        let left = service.stop_all().await;

        assert!(left.is_empty(), "no host outlived its teardown: {left:?}");
        assert!(service.profiles.lock().await.is_empty(), "nothing is left open");
        assert!(service.store.named("hunt").exists(), "the profile's logins stay");
    }

    /// **A seat that is down takes its named profiles with it.** The sweep is
    /// given the seats that remain: a live seat's profile and the shared
    /// browser stay, and the down seat's directory goes.
    #[tokio::test]
    async fn a_sweep_removes_a_down_seats_profiles_only() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser");
        let service = BrowserService::at(root.clone(), root.join("stack"));
        let alive = SessionSlot::lead("TestOrg", "proj");
        let gone = SessionSlot::worker("TestOrg", "proj", "runner");
        service.entry(&alive, Some("kept")).await.expect("owner records");
        service.entry(&gone, Some("gone")).await.expect("owner records");
        service.entry(&alive, Some(SHARED_PROFILE)).await.expect("the shared browser opens");

        service.sweep_seats(std::slice::from_ref(&alive)).await;

        assert!(service.store.named("kept").exists(), "a live seat's profile stays");
        assert!(!service.store.named("gone").exists(), "the down seat's goes");
        assert!(service.store.shared().exists(), "the shared browser is never swept");
        assert!(
            service.profiles.lock().await.get("gone").is_none(),
            "and its open entry goes with it",
        );
    }

    /// The profile's cache root is under the service's root, never a launch
    /// directory, and the shared browser has a directory of its own.
    #[test]
    fn a_profile_dir_hangs_off_the_service_root() {
        let service = BrowserService::at("/browser".into(), "/browser/stack".into());
        assert_eq!(service.profile_dir("hunt"), std::path::Path::new("/browser/profiles/hunt/cef"));
        assert_eq!(
            service.profile_dir(SHARED_PROFILE),
            std::path::Path::new("/browser/shared/cef")
        );
    }
}

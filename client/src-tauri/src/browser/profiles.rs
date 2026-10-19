//! Named profiles: what a session means by `profile: "name"`.
//!
//! **A named profile is a browser of its own.** It gets its own Chromium
//! process and its own data directory, so its logins, cookies and session
//! survive restarts natively - and, the reason this shape exists (Ved,
//! 2026-10-07), a hand-off's Open can raise THAT browser's window on THAT
//! profile's page, which an isolated context inside one shared browser can
//! never do: Chromium gives such a context no window presence at all.
//!
//! **Ownership rides the seat that opened it, and it is recorded.** The first
//! call naming a profile records its seat in `owner.json` inside the profile's
//! own directory, so the name stays that seat's across a client restart;
//! another seat naming it is refused by name. Only a delete or the sweeps on
//! despawn/connect end it. The browser's own profile carries no name and no
//! owner: every seat shares it.

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

#[cfg(desktop)]
use super::StackPaths;
use super::custom;
#[cfg(target_os = "android")]
use super::driver;
use super::driver::{Driver, ReplyPart};
#[cfg(desktop)]
use super::visibility;

/// The session a browser ask is made for, as the ask carries it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Hash, serde::Serialize)]
pub struct Seat {
    pub org: String,
    pub project: String,
    pub label: String,
}

impl fmt::Display for Seat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.org, self.project, self.label)
    }
}

/// Where a profile's driver comes from whenever it has to be rebuilt. The
/// desktop spawns the vendored node against the browser's CURRENT endpoint;
/// the phone starts the app's own in-app node against the socket bound here
/// (the Kotlin engine brings the WebView and relay with it).
pub(super) struct DriverStart<'a> {
    #[cfg(desktop)]
    pub node: &'a Path,
    #[cfg(desktop)]
    pub cli: &'a Path,
    /// The live endpoint (desktop), which a browser relaunch moves: a driver
    /// built against the old one is stale even while its own transport stays
    /// open.
    #[cfg(desktop)]
    pub endpoint: &'a str,
    /// The browser's own identity - the `/devtools/browser/<uuid>` its port
    /// file names on the desktop - which a relaunch changes even when it
    /// lands on the same port. The phone's is `webview-<generation>`: the
    /// WebView never moves within a run, but a CLAIMED RENDERER DEATH bumps
    /// the generation, and a changed identity is what makes the next call
    /// rebuild the driver. On the phone that rebuild cannot reattach today
    /// (issue #1931): the in-app node keeps its old socket link open (it only
    /// redials a dead one), so in the steady case the fresh driver's start
    /// gets no dial and its accept window times out - the call fails with the
    /// reason until the app is restarted.
    pub identity: &'a str,
    pub output: &'a Path,
    /// The unix socket the in-app driver dials (phone only).
    #[cfg(target_os = "android")]
    pub socket: &'a Path,
    /// The client UI's origin (phone only): the driver's tab pin keeps off
    /// it.
    #[cfg(target_os = "android")]
    pub ui_origin: &'a str,
    /// The Kotlin engine handle (phone only).
    #[cfg(target_os = "android")]
    pub engine: &'a super::android::Engine,
}

impl DriverStart<'_> {
    async fn start(&self) -> Result<Driver, String> {
        #[cfg(desktop)]
        {
            let driver = Driver::start(self.node, self.cli, self.endpoint, self.output).await?;
            Ok(driver)
        }
        #[cfg(target_os = "android")]
        {
            Driver::start_inapp(self.engine, self.socket, self.ui_origin, self.output).await
        }
    }
}

/// The driver a profile holds, and the browser identity it was built against.
pub(super) struct Held {
    identity: String,
    driver: Option<Arc<Driver>>,
}

/// A profile and the driver that serves it.
///
/// The lock is held across one call: calls to one profile run in order, while
/// different profiles run in parallel over their own drivers.
pub struct Profile {
    held: Mutex<Held>,
}

impl Profile {
    pub(super) fn new() -> Self {
        Self { held: Mutex::new(Held { identity: String::new(), driver: None }) }
    }

    /// The lock a call holds for its whole round: a delete takes the same one
    /// first, so no call runs while a profile's files are going. Desktop
    /// only - the delete that takes it is, and the phone's profiles never do.
    #[cfg(desktop)]
    pub(super) async fn call_lock(&self) -> tokio::sync::MutexGuard<'_, Held> {
        self.held.lock().await
    }

    /// Whether a driver is there to answer. **The browser's own liveness is a
    /// separate question**, asked per call by [`live_driver`]; this is only
    /// what a caller reads to decide between reuse and rebuild. A lock that
    /// cannot be taken means a call is running, which means it is there.
    #[cfg(desktop)]
    pub(super) fn is_alive(&self) -> bool {
        match self.held.try_lock() {
            Ok(held) => held.driver.as_ref().is_some_and(|driver| driver.is_running()),
            Err(_) => true,
        }
    }

    /// One call through this profile's driver, rebuilt when it is not there,
    /// when it died, or when the browser it was built against moved.
    ///
    /// **A hidden tab is named before it reads as a wedge on the desktop**,
    /// where the person can bring the tab to front; the phone's page cannot
    /// be raised that way, so its calls run as they always have.
    pub(super) async fn call(
        &self,
        start: &DriverStart<'_>,
        tool: &str,
        args: Value,
    ) -> Result<Vec<ReplyPart>, String> {
        let mut held = self.held.lock().await;
        let driver = live_driver(&mut held, start).await?;
        #[cfg(desktop)]
        {
            visibility::dispatch(&driver, tool, &args).await
        }
        #[cfg(target_os = "android")]
        {
            routed_call(&driver, tool, &args, driver::CALL_TIMEOUT).await
        }
    }

    /// Forget the driver, so the next call builds a fresh one. Called when a
    /// call failed AND the browser is no longer the one this driver was built
    /// against - a browser that died under it - while a plain tool failure
    /// (no such element) leaves the driver in place. The phone has no
    /// browser to relaunch, so nothing there drops drivers.
    #[cfg(desktop)]
    pub(super) async fn drop_driver(&self) {
        self.held.lock().await.driver = None;
    }
}

/// The driver to run with: rebuilt when there is none, when it died, or when
/// the browser moved out from under it.
///
/// **The identity comparison is the browser half of liveness.** The driver
/// child is tied to the browser only by CDP, so a browser that dies closes
/// the socket while the driver's own transport stays open - `is_running()`
/// keeps saying yes, and a call would re-run `connectOverCDP` against a port
/// nobody listens on. The host hands the browser's own identity (the port
/// file's `/devtools/browser/<uuid>`, which a relaunch replaces even on the
/// same port); a mismatch is the one thing that rebuilds a live-looking
/// driver.
async fn live_driver(held: &mut Held, start: &DriverStart<'_>) -> Result<Arc<Driver>, String> {
    if held.identity == start.identity
        && let Some(driver) = held.driver.as_ref()
        && driver.is_running()
    {
        return Ok(Arc::clone(driver));
    }
    let fresh = Arc::new(start.start().await?);
    held.identity = start.identity.to_owned();
    held.driver = Some(Arc::clone(&fresh));
    Ok(fresh)
}

/// A named profile: it belongs to the session that opened it, and its browser
/// lives on its own data directory. Desktop-only: the phone runs exactly one
/// browser and refuses a name (see `BrowserHost::call`'s phone arm).
#[cfg(desktop)]
pub struct Named {
    pub(super) owner: Seat,
    pub(super) profile: Profile,
    pub(super) dir: std::path::PathBuf,
}

/// The call's own shape fault, before any page is consulted: a malformed
/// call is its argument error, not the visibility gate's. Desktop only, with
/// the gate that consults it.
#[cfg(desktop)]
pub(super) fn routing_refusal(tool: &str, args: &Value) -> Option<String> {
    custom::route(tool, args).err()
}

/// Route one call the way the host always has: upstream's own tool, or the
/// snippet this client composes for an addition. `bound` is the call's own -
/// the visibility gate's input calls ask for less than the driver's default
/// so the probe after a failure fits.
pub(super) async fn routed_call(
    driver: &Driver,
    tool: &str,
    args: &Value,
    bound: Duration,
) -> Result<Vec<ReplyPart>, String> {
    match custom::route(tool, args)? {
        custom::Routed::Upstream { tool, args } => driver.call_within(&tool, args, bound).await,
        custom::Routed::Snippet(code) => {
            driver.call_within("browser_run_code_unsafe", json!({ "code": code }), bound).await
        }
    }
}

#[cfg(desktop)]
impl Named {
    /// The owner, the data directory, and the driver a named profile is
    /// opened with. **Neither the driver nor the browser is started here**: a
    /// session that names a profile and then drives it pays for both once,
    /// and one that names it and stops pays nothing. The ownership record IS
    /// written here: the name is answered for from disk from the next read
    /// on, so a restart cannot turn it into an open name.
    pub(super) fn open(owner: Seat, name: &str, paths: &StackPaths) -> Result<Self, String> {
        let dir = paths.profiles.join(name);
        record_owner(&dir, &owner)?;
        Ok(Self { owner, profile: Profile::new(), dir })
    }
}

/// The file inside a profile's directory that records the seat it belongs to.
/// Absent means the directory predates ownership: a legacy orphan the sweep
/// takes. Desktop-only: the phone runs one browser and holds no named
/// profiles.
#[cfg(desktop)]
pub(super) const OWNER_FILE: &str = "owner.json";

/// Record the seat a profile belongs to, beside the profile's own files so
/// the name, the logins and the owner travel together. Creates the directory,
/// which the owner file lives in. Desktop-only, with its callers.
#[cfg(desktop)]
pub(super) fn record_owner(dir: &Path, seat: &Seat) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|why| format!("the profile's directory cannot be made: {why}"))?;
    let body = serde_json::to_vec(seat)
        .map_err(|why| format!("the profile's ownership record cannot be written: {why}"))?;
    std::fs::write(dir.join(OWNER_FILE), body)
        .map_err(|why| format!("the profile's ownership record cannot be written: {why}"))
}

/// The seat recorded as this profile's owner. `Ok(None)` is a directory with
/// no record - a legacy profile, owned by nobody - and `Err` is a record that
/// cannot be read: the name refuses on that rather than guessing an owner,
/// and the sweep reads it as ownerless and takes it. Desktop-only, with its
/// callers.
#[cfg(desktop)]
pub(super) fn owner_of(dir: &Path) -> Result<Option<Seat>, String> {
    let body = match std::fs::read(dir.join(OWNER_FILE)) {
        Ok(body) => body,
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(why) => {
            return Err(format!("the profile's ownership record cannot be read: {why}"));
        }
    };
    serde_json::from_slice::<Seat>(&body)
        .map(Some)
        .map_err(|why| format!("the profile's ownership record cannot be read: {why}"))
}

/// What a session naming a profile gets.
#[cfg(desktop)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Drive the profile already under that name, which the caller owns.
    Drive,
    /// Open it: no profile carries that name yet.
    Open,
    /// Answer this instead.
    Refuse(String),
}

/// Why a name cannot be a profile, or `None` when it can.
///
/// A profile's name becomes a directory name, so it is held to what is safe
/// there: a name carrying a separator is a name trying to write somewhere
/// else.
pub(super) fn name_refusal(name: &str) -> Option<String> {
    let allowed = !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (!allowed).then(|| {
        format!(
            "'{name}' cannot be a profile name: 1 to 64 characters of letters, digits, \
             dot, dash or underscore"
        )
    })
}

/// What the asking session gets, given who holds the name.
#[cfg(desktop)]
pub(super) fn verdict(name: &str, seat: &Seat, held: Option<&Seat>) -> Verdict {
    match held {
        None => Verdict::Open,
        Some(owner) if owner == seat => Verdict::Drive,
        Some(owner) => Verdict::Refuse(format!(
            "the profile '{name}' belongs to {owner}; it goes when that seat is despawned, \
             or from the browser strip"
        )),
    }
}

/// Whether a session may raise `name`'s WINDOW - a hand-off's Open. Another
/// seat's profile is refused where the name resolves to a browser; a legacy
/// directory (no record) is nobody's and stays raisable by the person's own
/// door, which carries no seat at all, and an unreadable record refuses by
/// its own reason.
#[cfg(desktop)]
pub(super) fn raise_refusal(name: &str, dir: &Path, seat: &Seat) -> Result<(), String> {
    match owner_of(dir) {
        Ok(Some(owner)) if owner != *seat => Err(format!(
            "the profile '{name}' belongs to {owner}; a session raises only its own profile's \
             window"
        )),
        Ok(_) => Ok(()),
        Err(why) => Err(why),
    }
}

/// **One host launch per profile ACROSS PROCESSES.** The client's own mutex
/// serializes its ensures; a second client - the app opened twice - would
/// otherwise spawn a second host onto a profile CEF already holds. An flock
/// beside the cache root makes the critical section machine-wide, and the
/// loser re-checks for the live host under the guard.
#[cfg(desktop)]
pub(super) struct LaunchLock {
    _file: std::fs::File,
}

#[cfg(desktop)]
enum Locked {
    Held(LaunchLock),
    Contended,
    Unavailable(String),
}

#[cfg(desktop)]
fn try_lock(profile: &Path) -> Locked {
    let path = profile.join("launch.lock");
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(why) => {
            return Locked::Unavailable(format!(
                "the launch lock at {} cannot be opened: {why}",
                path.display()
            ));
        }
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Locked::Held(LaunchLock { _file: file }),
        Err(why) if why == rustix::io::Errno::WOULDBLOCK || why == rustix::io::Errno::AGAIN => {
            Locked::Contended
        }
        Err(why) => Locked::Unavailable(format!(
            "the launch lock at {} cannot be taken: {why}",
            path.display()
        )),
    }
}

#[cfg(desktop)]
impl LaunchLock {
    /// The lock for `profile`, waiting out whoever holds it up to the host's
    /// own launch deadline plus slack. A delete takes the same lock, so a
    /// launch and a removal can never interleave.
    pub(super) async fn acquire(profile: &Path) -> Result<Self, String> {
        let deadline = tokio::time::Instant::now()
            + crate::browser::cef::spawn::LAUNCH_TIMEOUT
            + Duration::from_secs(2);
        loop {
            match try_lock(profile) {
                Locked::Held(lock) => return Ok(lock),
                Locked::Unavailable(why) => return Err(why),
                Locked::Contended => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(format!(
                            "the browser on {} is being launched by another process and did not \
                             answer in time",
                            profile.display(),
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

/// The client's own claim on a profile's host: an exclusive flock on
/// `<profile>/client.lock`, held for the claiming client's process lifetime.
///
/// **The lock IS the liveness truth.** A recorded pid cannot be: a recycled
/// pid reads as a live owner, and one that died between two reads says
/// nothing a reader can trust. A process death of any kind releases the
/// flock, so a sweep's non-blocking take answering "free" is the one
/// trustworthy reading of "the client that owned this host is gone".
#[cfg(desktop)]
pub struct ClientLock {
    _file: std::fs::File,
}

/// The client's claim on `profile`, or `None` when another live client holds
/// it. Makes the directory the lock lives in, so a claim can be the first
/// thing to touch a fresh cache root.
#[cfg(desktop)]
pub fn claim(profile: &Path) -> Result<Option<ClientLock>, String> {
    std::fs::create_dir_all(profile)
        .map_err(|why| format!("the browser profile directory cannot be made: {why}"))?;
    let path = client_lock_path(profile);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|why| format!("the client lock at {} cannot be opened: {why}", path.display()))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            // The pid is written for a reader, never for a decision: the
            // flock above is what the sweep trusts.
            use std::io::Write as _;
            let _ = file.set_len(0);
            let _ = (&file).write_all(std::process::id().to_string().as_bytes());
            Ok(Some(ClientLock { _file: file }))
        }
        Err(why) if why == rustix::io::Errno::WOULDBLOCK || why == rustix::io::Errno::AGAIN => {
            Ok(None)
        }
        Err(why) => Err(format!("the client lock at {} cannot be taken: {why}", path.display())),
    }
}

/// The file the client's claim lives on, beside the launch lock.
#[cfg(desktop)]
fn client_lock_path(profile: &Path) -> std::path::PathBuf {
    profile.join("client.lock")
}

/// A named profile's cache root, derived from its ownership directory.
#[cfg(desktop)]
pub(super) fn cef_cache(dir: &Path) -> std::path::PathBuf {
    dir.join("cef")
}

/// Move a Brave-era profile into a CEF cache root, once.
///
/// **Both engines write Chromium's own profile**, so the migration is a copy
/// of `Default` and `Local State` into the cache root - no conversion. The
/// client's own Brave launches used `use-mock-keychain` and
/// `password-store=basic`, the same flags the host sets, so the cookies a
/// client-created profile carries decrypt under the host; a profile the
/// person's own Brave created with the real keychain would not, and is not
/// converted here.
///
/// Answers whether anything was copied. An empty answer is the normal case:
/// a cache root already opened, or no legacy profile to move.
#[cfg(desktop)]
pub(super) fn migrate_brave(legacy: &Path, cache: &Path) -> Result<bool, String> {
    if cache.join("Default").is_dir() || !legacy.join("Default").is_dir() {
        return Ok(false);
    }
    std::fs::create_dir_all(cache)
        .map_err(|why| format!("the profile's cache root cannot be made: {why}"))?;
    copy_tree(&legacy.join("Default"), &cache.join("Default"))?;
    let local_state = legacy.join("Local State");
    if local_state.is_file() {
        std::fs::copy(&local_state, cache.join("Local State"))
            .map_err(|why| format!("the profile's local state cannot be copied: {why}"))?;
    }
    Ok(true)
}

#[cfg(desktop)]
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|why| format!("{} cannot be made: {why}", to.display()))?;
    let entries = std::fs::read_dir(from)
        .map_err(|why| format!("{} cannot be read: {why}", from.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        // Another engine's live locks and ports mean nothing in this cache.
        if matches!(
            name.to_str(),
            Some(
                "SingletonLock"
                    | "SingletonSocket"
                    | "SingletonCookie"
                    | "DevToolsActivePort"
                    | "launch.lock"
                    | "client.lock"
                    | "host.json"
                    | "owner.json"
                    | "cef"
            )
        ) {
            continue;
        }
        let target = to.join(&name);
        let kind =
            entry.file_type().map_err(|why| format!("{} cannot be read: {why}", path.display()))?;
        if kind.is_dir() {
            copy_tree(&path, &target)?;
        } else if kind.is_file() {
            std::fs::copy(&path, &target)
                .map_err(|why| format!("{} cannot be copied: {why}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn seat(label: &str) -> Seat {
        Seat { org: "Busytools".to_owned(), project: "forge".to_owned(), label: label.to_owned() }
    }

    fn paths(root: &str) -> StackPaths {
        StackPaths {
            stack: PathBuf::from(root),
            user_data: PathBuf::from(root).join("user-data"),
            output: PathBuf::from(root).join("output"),
            profiles: PathBuf::from(root).join("profiles"),
        }
    }

    /// A name nobody holds opens, and once opened its opener drives it: the
    /// two halves of "creating on first use".
    #[test]
    fn a_fresh_name_opens_and_its_opener_then_drives_it() {
        let opener = seat("job-hunt");
        assert_eq!(verdict("hunt", &opener, None), Verdict::Open);
        assert_eq!(verdict("hunt", &opener, Some(&opener)), Verdict::Drive);
    }

    /// **The spec's own acceptance: two sessions, one profile name, the
    /// second refused by name.** The refusal carries the profile and its
    /// owner, since a session reading it has to know which of its calls was
    /// turned away and whose it is.
    #[test]
    fn another_sessions_name_is_refused_with_both_named() {
        let owner = seat("client-dev");
        let other = seat("browser-host");
        let Verdict::Refuse(refusal) = verdict("job-hunt", &other, Some(&owner)) else {
            panic!("the second session is refused");
        };
        assert!(refusal.contains("'job-hunt'"), "{refusal}");
        assert!(refusal.contains("Busytools/forge/client-dev"), "{refusal}");
    }

    /// A name that cannot become a directory is refused with the rule, rather
    /// than sanitised into some other name the session did not ask for.
    #[test]
    fn a_name_that_cannot_be_a_profile_is_refused_with_the_rule() {
        for bad in ["", ".", "..", "a/b", "a b", "job hunt", "caf\u{e9}", "../escape"] {
            let refusal = name_refusal(bad).unwrap_or_else(|| panic!("{bad:?} is refused"));
            assert!(refusal.contains("1 to 64"), "{refusal}");
        }
        let long = "x".repeat(65);
        assert!(name_refusal(&long).is_some(), "65 characters is past the bound");
        for good in ["job-hunt", "Job_1.2", "a", &"x".repeat(64)] {
            assert_eq!(name_refusal(good), None, "{good} is a usable name");
        }
    }

    /// A named profile's browser lives on its own directory, under the
    /// profiles root and never on the shared profile's: the directory is what
    /// carries that profile's logins. **And opening records the seat**: the
    /// name is answered for from disk from the next client on.
    #[test]
    fn a_named_profile_gets_its_own_browser_directory_and_records_its_seat() {
        let root = tempfile::tempdir().expect("a temp dir");
        let paths = paths(root.path().to_str().expect("a path"));
        let named = Named::open(seat("job-hunt"), "hunt", &paths).expect("the open");
        assert_eq!(named.dir, paths.profiles.join("hunt"));
        assert_ne!(named.dir, paths.user_data, "never the shared profile's directory");
        assert_eq!(
            owner_of(&named.dir),
            Ok(Some(seat("job-hunt"))),
            "the seat that opened the name is what the directory records",
        );
    }

    /// **A Brave-era profile moves once.** Both engines write Chromium's own
    /// profile, so the migration is a copy of `Default` and `Local State`;
    /// the second call answers nothing, and a cache root that already has a
    /// profile is never overwritten.
    #[test]
    fn a_brave_profile_migrates_once_into_the_cache_root() {
        let root = tempfile::tempdir().expect("a temp dir");
        let legacy = root.path().join("profiles").join("hunt");
        std::fs::create_dir_all(legacy.join("Default")).expect("the legacy default");
        std::fs::write(legacy.join("Default").join("Cookies"), b"the logins")
            .expect("legacy cookies");
        std::fs::write(legacy.join("Local State"), b"{}").expect("the legacy state");
        std::fs::write(legacy.join("SingletonLock"), b"").expect("a Brave lock");
        let cache = cef_cache(&legacy);

        assert_eq!(migrate_brave(&legacy, &cache), Ok(true), "the first launch moves it");
        assert_eq!(
            std::fs::read(cache.join("Default").join("Cookies")).expect("the cookies"),
            b"the logins",
            "the logins travel",
        );
        assert!(cache.join("Local State").is_file(), "and the local state");
        assert!(
            !cache.join("Default").join("SingletonLock").exists(),
            "another engine's live lock is not carried",
        );
        assert_eq!(migrate_brave(&legacy, &cache), Ok(false), "a second launch copies nothing");

        // A cache root already opened is never overwritten by the copy.
        let other = root.path().join("profiles").join("scout");
        std::fs::create_dir_all(other.join("Default")).expect("the legacy default");
        std::fs::write(other.join("Default").join("Cookies"), b"old").expect("old cookies");
        let opened = cef_cache(&other);
        std::fs::create_dir_all(opened.join("Default")).expect("an opened cache");
        std::fs::write(opened.join("Default").join("Cookies"), b"new").expect("new cookies");
        assert_eq!(migrate_brave(&other, &opened), Ok(false), "the cache wins");
        assert_eq!(
            std::fs::read(opened.join("Default").join("Cookies")).expect("the cookies"),
            b"new",
        );
    }

    /// **The record survives the client run and is the authority after it.**
    /// A written record reads back as the same seat; a directory without one
    /// is a legacy profile owned by nobody; and a record that cannot be read
    /// is an error rather than a guess - the name refuses on it and the sweep
    /// takes it as ownerless.
    #[test]
    fn an_owner_record_round_trips_and_absence_is_ownerless() {
        let root = tempfile::tempdir().expect("a temp dir");
        let dir = root.path().join("profiles").join("hunt");
        let owner = seat("job-hunt");

        assert_eq!(owner_of(&dir), Ok(None), "no directory is a legacy profile, owned by nobody");

        record_owner(&dir, &owner).expect("the record");
        assert_eq!(owner_of(&dir), Ok(Some(owner.clone())), "the seat reads back");

        std::fs::write(dir.join(OWNER_FILE), b"not json").expect("a tampered record");
        let why = owner_of(&dir).expect_err("an unreadable record is an error, not a guess");
        assert!(why.contains("cannot be read"), "{why}");

        // A directory with files but no record is legacy, not corrupt.
        let legacy = root.path().join("profiles").join("old");
        std::fs::create_dir_all(&legacy).expect("the legacy dir");
        std::fs::write(legacy.join("Cookies"), b"the logins").expect("legacy files");
        assert_eq!(owner_of(&legacy), Ok(None), "files without a record predate ownership");
    }

    /// The seat prints as the slot it is, which is what a refusal says.
    #[test]
    fn a_seat_prints_as_its_slot() {
        assert_eq!(seat("lead").to_string(), "Busytools/forge/lead");
    }

    /// A hand-off's raise is refused only for another seat's recorded
    /// profile: the owner's own, a legacy unowned directory and the
    /// unreadable-record reason each answer their own way.
    #[test]
    fn a_raise_is_refused_only_for_another_seats_record() {
        let root = tempfile::tempdir().expect("a temp dir");
        let mine = seat("job-hunt");
        let other = Seat { org: "Other".to_owned(), ..seat("job-hunt") };

        let mine_dir = root.path().join("mine");
        record_owner(&mine_dir, &mine).expect("the record");
        assert_eq!(raise_refusal("mine", &mine_dir, &mine), Ok(()), "its own is raisable");

        let theirs_dir = root.path().join("theirs");
        record_owner(&theirs_dir, &other).expect("the record");
        let refused = raise_refusal("theirs", &theirs_dir, &mine).expect_err("another seat's");
        assert!(refused.contains("Other/forge/job-hunt"), "{refused}");
        assert!(refused.contains("raises only its own"), "{refused}");

        let legacy_dir = root.path().join("legacy");
        std::fs::create_dir_all(&legacy_dir).expect("the legacy dir");
        assert_eq!(raise_refusal("legacy", &legacy_dir, &mine), Ok(()), "nobody's is nobody's");

        let broken_dir = root.path().join("broken");
        std::fs::create_dir_all(&broken_dir).expect("the dir");
        std::fs::write(broken_dir.join(OWNER_FILE), b"not json").expect("a bad record");
        let why = raise_refusal("broken", &broken_dir, &mine).expect_err("unreadable");
        assert!(why.contains("cannot be read"), "{why}");
    }
}

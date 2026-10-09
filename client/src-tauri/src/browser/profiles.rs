//! Named profiles: what a session means by `profile: "name"`.
//!
//! **A named profile is a browser of its own.** It gets its own Chromium
//! process and its own data directory, so its logins, cookies and session
//! survive restarts natively - and, the reason this shape exists (Ved,
//! 2026-10-07), a hand-off's Open can raise THAT browser's window on THAT
//! profile's page, which an isolated context inside one shared browser can
//! never do: Chromium gives such a context no window presence at all.
//!
//! **Ownership rides the session that opened it.** The first session to name a
//! profile owns it; another session naming it is refused with the owner's
//! name, until the owner releases it. The browser's own profile carries no
//! name and no owner: every session shares it.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

#[cfg(desktop)]
use super::StackPaths;
use super::custom;
use super::driver::{Driver, ReplyPart};

/// The session a browser ask is made for, as the ask carries it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, serde::Serialize)]
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
    /// Whether the launch this driver attaches to is the HEADED one: the hint
    /// mask only applies where the launch's UA override does, and a headed
    /// launch presents the browser's own UA and hints.
    #[cfg(desktop)]
    pub windowed: bool,
    /// Which profile this driver serves - `shared`, or a named profile - so a
    /// warn about its hint mask says WHICH browser stayed unmasked.
    #[cfg(desktop)]
    pub profile_label: &'a str,
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
            // **The hint mask only where the launch's UA override is.** A
            // headed launch presents the browser's own UA and its hints are
            // real already; its capture would also open a window at the
            // person, which is the one thing the mask must never do.
            if !self.windowed {
                driver.install_hint_mask(self.output, self.profile_label).await;
            } else {
                tauri_plugin_log::log::info!(
                    "the client-hint mask keeps off the headed launch (event_name \
                     browser_hint_mask, profile {}): the browser presents its own user agent and \
                     hints",
                    self.profile_label
                );
            }
            Ok(driver)
        }
        #[cfg(target_os = "android")]
        {
            Driver::start_inapp(self.engine, self.socket, self.ui_origin, self.output).await
        }
    }
}

/// The driver a profile holds, and the browser identity it was built against.
struct Held {
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
    pub(super) async fn call(
        &self,
        start: &DriverStart<'_>,
        tool: &str,
        args: Value,
    ) -> Result<Vec<ReplyPart>, String> {
        let mut held = self.held.lock().await;
        let driver = live_driver(&mut held, start).await?;
        routed_call(&driver, tool, &args).await
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

/// Route one call the way the host always has: upstream's own tool, or the
/// snippet this client composes for an addition.
async fn routed_call(driver: &Driver, tool: &str, args: &Value) -> Result<Vec<ReplyPart>, String> {
    match custom::route(tool, args)? {
        custom::Routed::Upstream { tool, args } => driver.call(&tool, args).await,
        custom::Routed::Snippet(code) => {
            driver.call("browser_run_code_unsafe", json!({ "code": code })).await
        }
    }
}

#[cfg(desktop)]
impl Named {
    /// The owner, the data directory, and the driver a named profile is
    /// opened with. **Neither the driver nor the browser is started here**: a
    /// session that names a profile and then drives it pays for both once,
    /// and one that names it and stops pays nothing.
    pub(super) fn open(owner: Seat, name: &str, paths: &StackPaths) -> Self {
        Self { owner, profile: Profile::new(), dir: paths.profiles.join(name) }
    }
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
            "the profile '{name}' belongs to {owner}; another session attaches to it only \
             after it is released"
        )),
    }
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
    /// carries that profile's logins.
    #[test]
    fn a_named_profile_gets_its_own_browser_directory() {
        let paths = paths("/data/browser");
        let named = Named::open(seat("job-hunt"), "hunt", &paths);
        assert_eq!(named.dir, PathBuf::from("/data/browser/profiles/hunt"));
        assert_ne!(named.dir, paths.user_data, "never the shared profile's directory");
    }

    /// The seat prints as the slot it is, which is what a refusal says.
    #[test]
    fn a_seat_prints_as_its_slot() {
        assert_eq!(seat("lead").to_string(), "Busytools/forge/lead");
    }
}

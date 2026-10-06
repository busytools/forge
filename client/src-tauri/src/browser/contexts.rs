//! Named contexts: what a session means by `context: "name"`.
//!
//! **Upstream's driver multiplexes nothing** (measured against the pinned
//! 0.0.83): attached over CDP it drives the browser's own context, and
//! `--isolated` makes it create one of its own. So a named context here is a
//! driver of its own over the one browser, and this module keeps the rules:
//! which name is usable, who owns it, and what it reopens from.
//!
//! **Ownership rides the session that opened it.** The first session to name a
//! context owns it; another session naming it is refused with the owner's
//! name, until the owner releases it. The browser's own context carries no
//! name and no owner: every session shares it.
//!
//! **Persistence is ours to keep.** Upstream reads a context's storage state
//! at creation and never writes it back, so a named context saves cookies into
//! its storage file and its open tabs as URLs after every call it serves, and
//! opening it again starts the driver over the saved storage and reopens those
//! tabs.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::driver::{self, Driver, ReplyPart};
use super::{StackPaths, custom};

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

/// Where a context's driver comes from whenever it has to be rebuilt: the
/// vendored node and CLI, the browser's CURRENT endpoint, the output
/// directory, and - for a named context - its storage file.
pub(super) struct DriverStart<'a> {
    pub node: &'a Path,
    pub cli: &'a Path,
    /// The live endpoint, which a browser relaunch moves: a driver built
    /// against the old one is stale even while its own transport stays open.
    pub endpoint: &'a str,
    /// The browser's own identity - the `/devtools/browser/<uuid>` its port
    /// file names - which a relaunch changes even when it lands on the same
    /// port. The endpoint string alone would compare equal there.
    pub identity: &'a str,
    pub output: &'a Path,
    /// The named context's storage file; `None` for the browser's own.
    pub storage: Option<&'a Path>,
    /// The named context's saved tabs, reopened through a driver built fresh:
    /// a build that skipped them would let the save that follows write the
    /// empty page over them, so a browser change would eat the context's
    /// pages. `None` for the browser's own context, which reopens nothing.
    pub tabs: Option<&'a Path>,
}

impl DriverStart<'_> {
    async fn start(&self) -> Result<Driver, String> {
        Driver::start(self.node, self.cli, self.endpoint, self.output, self.storage).await
    }
}

/// The driver a context holds, and the browser identity it was built against.
struct Held {
    identity: String,
    driver: Option<Arc<Driver>>,
}

/// A context and the driver that serves it.
///
/// The lock is held across one call: calls to one context run in order, while
/// different contexts run in parallel over their own drivers.
pub struct Context {
    held: Mutex<Held>,
}

impl Context {
    pub(super) fn new() -> Self {
        Self { held: Mutex::new(Held { identity: String::new(), driver: None }) }
    }

    /// Whether a driver is there to answer. **The browser's own liveness is a
    /// separate question**, asked per call by [`live_driver`]; this is only
    /// what a caller reads to decide between reuse and rebuild. A lock that
    /// cannot be taken means a call is running, which means it is there.
    pub(super) fn is_alive(&self) -> bool {
        match self.held.try_lock() {
            Ok(held) => held.driver.as_ref().is_some_and(|driver| driver.is_running()),
            Err(_) => true,
        }
    }

    /// One call through this context's driver, rebuilt when it is not there,
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

    /// Save the context's cookies and open tabs, through the same lock.
    pub(super) async fn save(
        &self,
        start: &DriverStart<'_>,
        storage: &Path,
        tabs: &Path,
    ) -> Result<(), String> {
        let mut held = self.held.lock().await;
        let driver = live_driver(&mut held, start).await?;
        save(&driver, storage, tabs).await
    }

    /// Forget the driver, so the next call builds a fresh one. Called when a
    /// call failed AND the browser is no longer the one this driver was built
    /// against - a browser that died under it - while a plain tool failure
    /// (no such element) leaves the driver in place.
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
///
/// **A build reopens the context's saved tabs.** An isolated driver's browser
/// context is its own, so a rebuilt driver starts on a blank page and the
/// save that follows every call would write that blank over the saved tabs -
/// which is why the reopen belongs here, on every build, rather than only on
/// the attach path: a browser that died under a context is a build too.
async fn live_driver(held: &mut Held, start: &DriverStart<'_>) -> Result<Arc<Driver>, String> {
    if held.identity == start.identity
        && let Some(driver) = held.driver.as_ref()
        && driver.is_running()
    {
        return Ok(Arc::clone(driver));
    }
    let fresh = Arc::new(start.start().await?);
    if let Some(tabs) = start.tabs {
        reopen_tabs(&fresh, tabs).await;
    }
    held.identity = start.identity.to_owned();
    held.driver = Some(Arc::clone(&fresh));
    Ok(fresh)
}

/// A named context: it belongs to the session that opened it, and it keeps
/// where its cookies and its open tabs are saved.
pub struct Named {
    pub(super) owner: Seat,
    pub(super) context: Context,
    pub(super) storage: std::path::PathBuf,
    pub(super) tabs: std::path::PathBuf,
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

impl Named {
    /// The paths and the owner a named context is opened with. **Its driver
    /// is built on the first call**, not here: a session that names a context
    /// and then drives it pays for the driver once, and one that names it and
    /// stops pays nothing.
    pub(super) fn open(owner: Seat, name: &str, paths: &StackPaths) -> Self {
        Self {
            owner,
            context: Context::new(),
            storage: paths.contexts.join(format!("{name}.json")),
            tabs: paths.contexts.join(format!("{name}.tabs")),
        }
    }

    /// One call through this context, then its save.
    ///
    /// The save runs whether the call answered or failed: a failed call can
    /// still have moved the page. A save that fails is the client's own
    /// problem - logged, never the call's answer.
    pub(super) async fn call(
        &self,
        start: &DriverStart<'_>,
        tool: &str,
        args: Value,
    ) -> Result<Vec<ReplyPart>, String> {
        let outcome = self.context.call(start, tool, args).await;
        if let Err(why) = self.context.save(start, &self.storage, &self.tabs).await {
            tauri_plugin_log::log::warn!("a browser context's save failed: {why}");
        }
        outcome
    }

    /// Save the context's cookies and its open tabs, without a call.
    pub(super) async fn save(&self, start: &DriverStart<'_>) -> Result<(), String> {
        self.context.save(start, &self.storage, &self.tabs).await
    }
}

/// The save itself: the driver writes the context's cookies to its storage
/// file and hands back its open tab URLs, which go to the tabs file.
async fn save(driver: &Driver, storage: &Path, tabs: &Path) -> Result<(), String> {
    let parts = driver
        .call("browser_run_code_unsafe", json!({ "code": custom::save_session(storage) }))
        .await?;
    let reported = driver::reported_value(&parts)
        .ok_or_else(|| "the driver's save report could not be read".to_owned())?;
    let urls = reported
        .lines()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    save_tabs(tabs, &urls)
}

/// Reopen a context's saved tabs through the driver's own tab tool, so the
/// driver's tab order and current-tab bookkeeping stay its own.
///
/// A tab that will not open is logged and the rest go on: one dead URL is not
/// a reason to refuse the whole context.
async fn reopen_tabs(driver: &Driver, tabs: &Path) {
    for url in saved_tabs(tabs) {
        if let Err(why) =
            routed_call(driver, "browser_tabs", &json!({ "action": "new", "url": url })).await
        {
            tauri_plugin_log::log::warn!("a saved browser tab did not reopen ({url}): {why}");
        }
    }
}

/// What a session naming a context gets.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Drive the context already under that name, which the caller owns.
    Drive,
    /// Open it: no context carries that name yet.
    Open,
    /// Answer this instead.
    Refuse(String),
}

/// Why a name cannot be a context, or `None` when it can.
///
/// A context's name becomes a file name under the profile, so it is held to
/// what is safe there: a name carrying a separator is a name trying to write
/// somewhere else.
pub(super) fn name_refusal(name: &str) -> Option<String> {
    let allowed = !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    (!allowed).then(|| {
        format!(
            "'{name}' cannot be a context name: 1 to 64 characters of letters, digits, \
             dot, dash or underscore"
        )
    })
}

/// What the asking session gets, given who holds the name.
pub(super) fn verdict(name: &str, seat: &Seat, held: Option<&Seat>) -> Verdict {
    match held {
        None => Verdict::Open,
        Some(owner) if owner == seat => Verdict::Drive,
        Some(owner) => Verdict::Refuse(format!(
            "the context '{name}' belongs to {owner}; another session attaches to it only \
             after it is released"
        )),
    }
}

/// The tab URLs saved for a context: one per line, blanks and the browser's
/// own empty page left out, in the order they were saved.
pub(super) fn saved_tabs(path: &Path) -> Vec<String> {
    let Ok(saved) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    saved
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "about:blank")
        .map(str::to_owned)
        .collect()
}

/// Save a context's open tabs, one URL per line.
pub(super) fn save_tabs(path: &Path, urls: &[String]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|why| format!("the context's tab file cannot be written: {why}"))?;
    }
    std::fs::write(path, urls.join("\n"))
        .map_err(|why| format!("the context's tab file cannot be written: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(label: &str) -> Seat {
        Seat { org: "Busytools".to_owned(), project: "forge".to_owned(), label: label.to_owned() }
    }

    /// A name nobody holds opens, and once opened its opener drives it: the
    /// two halves of "creating on first use".
    #[test]
    fn a_fresh_name_opens_and_its_opener_then_drives_it() {
        let opener = seat("job-hunt");
        assert_eq!(verdict("hunt", &opener, None), Verdict::Open);
        assert_eq!(verdict("hunt", &opener, Some(&opener)), Verdict::Drive);
    }

    /// **The spec's own acceptance: two sessions, one context name, the
    /// second refused by name.** The refusal carries the context and its
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

    /// A name that cannot become a file under the profile is refused with the
    /// rule, rather than sanitised into some other name the session did not
    /// ask for.
    #[test]
    fn a_name_that_cannot_be_a_context_is_refused_with_the_rule() {
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

    /// The tabs a context reopens from: what was saved, without the blank page
    /// a browser starts on, and a missing file is a context that saved none.
    #[test]
    fn saved_tabs_skip_blanks_and_a_missing_file_is_none() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("ctx.tabs");
        assert_eq!(saved_tabs(&file), Vec::<String>::new(), "nothing saved yet");

        let urls = vec![
            "https://example.com/".to_owned(),
            String::new(),
            "about:blank".to_owned(),
            "https://example.com/two".to_owned(),
        ];
        save_tabs(&file, &urls).expect("the tabs save");
        assert_eq!(
            saved_tabs(&file),
            vec!["https://example.com/".to_owned(), "https://example.com/two".to_owned()],
            "the empty page is not a tab anyone opened",
        );
    }

    /// The seat prints as the slot it is, which is what a refusal says.
    #[test]
    fn a_seat_prints_as_its_slot() {
        assert_eq!(seat("lead").to_string(), "Busytools/forge/lead");
    }
}

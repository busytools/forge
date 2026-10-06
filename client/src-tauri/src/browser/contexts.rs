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

/// A context and the driver that serves it.
///
/// The driver's lock is held across one call: calls to one context run in
/// order, while different contexts run in parallel over their own drivers.
pub struct Context {
    pub(super) driver: Mutex<Arc<Driver>>,
}

impl Context {
    pub(super) fn new(driver: Driver) -> Self {
        Self { driver: Mutex::new(Arc::new(driver)) }
    }

    /// Whether the driver behind this context is still there to answer. A lock
    /// that cannot be taken means a call is running, which means it is.
    pub(super) fn is_alive(&self) -> bool {
        match self.driver.try_lock() {
            Ok(driver) => driver.is_running(),
            Err(_) => true,
        }
    }

    /// One call through this context's driver.
    ///
    /// The driver's lock is held across the whole call, so a context's calls
    /// run in order while different contexts run in parallel - and it is why a
    /// call can never interleave with that context's save.
    pub(super) async fn call(&self, tool: &str, args: Value) -> Result<Vec<ReplyPart>, String> {
        let driver = self.driver.lock().await;
        routed_call(&driver, tool, &args).await
    }
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
    /// Start the driver behind a named context.
    ///
    /// The driver is ISOLATED with this context's storage file: upstream reads
    /// that file as it creates the context, which is how the cookies come
    /// back, and creates a context of its own - nothing shared with any other
    /// driver over the same browser.
    pub(super) async fn start(
        owner: Seat,
        name: &str,
        endpoint: &str,
        paths: &StackPaths,
    ) -> Result<Self, String> {
        let storage = paths.contexts.join(format!("{name}.json"));
        let tabs = paths.contexts.join(format!("{name}.tabs"));
        let driver = Driver::start(
            &driver::node_path(&paths.stack),
            &driver::cli_path(&paths.stack),
            endpoint,
            &paths.output,
            Some(&storage),
        )
        .await?;
        Ok(Self { owner, context: Context::new(driver), storage, tabs })
    }

    /// One call through this context, then its save.
    ///
    /// The save rides the same lock, and it runs whether the call answered or
    /// failed: a failed call can still have moved the page. A save that fails
    /// is the client's own problem - logged, never the call's answer.
    pub(super) async fn call(&self, tool: &str, args: Value) -> Result<Vec<ReplyPart>, String> {
        let driver = self.context.driver.lock().await;
        let outcome = routed_call(&driver, tool, &args).await;
        if let Err(why) = save(&driver, &self.storage, &self.tabs).await {
            tauri_plugin_log::log::warn!("a browser context's save failed: {why}");
        }
        outcome
    }

    /// Save the context's cookies and its open tabs, without a call.
    pub(super) async fn save(&self) -> Result<(), String> {
        let driver = self.context.driver.lock().await;
        save(&driver, &self.storage, &self.tabs).await
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
pub(super) async fn reopen_tabs(context: &Context, urls: &[String]) {
    for url in urls {
        if let Err(why) = context.call("browser_tabs", json!({ "action": "new", "url": url })).await
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

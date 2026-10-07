//! The client's browser host: the side that owns the browser process, the
//! profile and the drivers, and answers the asks a session's browser tools
//! send over the socket.
//!
//! **One browser, one profile, one driver per context**, and the browser
//! outlives the client: it is launched detached against a profile under the
//! app-support directory, so logins and cookies survive a client restart and
//! a forge restart touches nothing here (spec section 3). The browser's own
//! context belongs to the profile and is shared by every session; a NAMED
//! context is a driver of its own over the same browser, owned by the session
//! that opened it and kept in [`contexts`].
//!
//! The pieces:
//! - [`chromium`] - where the vendored Chromium is, how it is launched, and
//!   how a launch is found again after a restart.
//! - [`contexts`] - which names are usable, who owns one, and what it
//!   reopens from.
//! - [`driver`] - upstream `@playwright/mcp` as a child process, spoken to as
//!   an MCP client.
//!
//! Nothing here decides what a tool MEANS: the ask carries upstream's own
//! tool name and arguments, the driver runs them, and the answer is the parts
//! it returned.

pub mod chromium;
pub mod contexts;
pub mod custom;
pub mod driver;
pub mod screencast;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tauri::Emitter as _;
use tauri::Manager as _;

use contexts::{Context, DriverStart, Named, Seat};
use driver::ReplyPart;
use serde_json::Value;
use tokio::sync::Mutex;

/// Where the vendored stack is, and the machine-local directories the browser
/// owns.
#[derive(Clone, Debug)]
pub struct StackPaths {
    /// The vendored tree: node, the driver, the Chromium.
    pub stack: PathBuf,
    /// The browser's own profile: logins, cookies, the HTTP cache, and the
    /// `DevToolsActivePort` file a launch writes.
    pub profile: PathBuf,
    /// Where upstream's own file-writing tools land when a call names no
    /// filename.
    pub output: PathBuf,
    /// Where a named context keeps its cookies and its open tabs: one pair of
    /// files per name.
    pub contexts: PathBuf,
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
            profile: data.join("browser/profile"),
            output: data.join("browser/output"),
            contexts: data.join("browser/contexts"),
        }
    }
}

/// The host: the browser's own context, the named ones, and where everything
/// lives.
pub struct BrowserHost {
    paths: Result<StackPaths, String>,
    /// Every `chromium::ensure` runs under this, whichever context asks for
    /// one: a burst of first calls launches ONE browser rather than two onto
    /// one profile.
    launch: Mutex<()>,
    /// The browser's own context, shared by every session.
    default: Mutex<Option<Arc<Context>>>,
    /// The named contexts, by name.
    named: Mutex<HashMap<String, Arc<Named>>>,
    /// The takeover's live view, when one is up.
    live: Mutex<Option<screencast::Live>>,
}

impl BrowserHost {
    pub fn new(paths: StackPaths) -> Self {
        Self {
            paths: Ok(paths),
            launch: Mutex::new(()),
            default: Mutex::new(None),
            named: Mutex::new(HashMap::new()),
            live: Mutex::new(None),
        }
    }

    /// A host that cannot act, for a client whose directories did not
    /// resolve: every call answers why rather than the client refusing to
    /// start over a feature it may never be asked for.
    pub fn unavailable(why: String) -> Self {
        Self {
            paths: Err(why),
            launch: Mutex::new(()),
            default: Mutex::new(None),
            named: Mutex::new(HashMap::new()),
            live: Mutex::new(None),
        }
    }

    /// Run one browser tool for a session.
    ///
    /// No `context` argument drives the browser's own context. A name drives
    /// the named context under it, opened on first use by the asking session;
    /// a name another session already holds is refused with the owner's name.
    ///
    /// **Every call resolves the browser first**, so the endpoint it hands the
    /// context is the one the browser answers on NOW: a browser that died (or
    /// was relaunched for a window) moves that endpoint, and the context
    /// rebuilds its driver rather than talking to a port nobody listens on.
    pub async fn call(
        &self,
        seat: &Seat,
        tool: &str,
        args: Value,
    ) -> Result<Vec<ReplyPart>, String> {
        let (context, args) = take_context(args)?;
        // A name that cannot be a context is decided before anything else: no
        // directory, no lock and no browser is consulted to answer it.
        if let Some(name) = context.as_deref()
            && let Some(refusal) = contexts::name_refusal(name)
        {
            return Err(refusal);
        }
        let paths = self.paths.clone()?;
        let active = self.active_browser(&paths).await?;
        let endpoint = format!("http://127.0.0.1:{}", active.port);
        let node = driver::node_path(&paths.stack);
        let cli = driver::cli_path(&paths.stack);
        match context {
            None => {
                let context = self.default_context().await;
                let start = DriverStart {
                    node: &node,
                    cli: &cli,
                    endpoint: &endpoint,
                    identity: &active.path,
                    output: &paths.output,
                    storage: None,
                    tabs: None,
                };
                let outcome = context.call(&start, tool, args).await;
                self.forget_a_dead_browser(&paths, &context, active.port, outcome.is_err()).await;
                outcome
            }
            Some(name) => {
                let named = self.named_context(seat, &name).await?;
                let start = DriverStart {
                    node: &node,
                    cli: &cli,
                    endpoint: &endpoint,
                    identity: &active.path,
                    output: &paths.output,
                    storage: Some(&named.storage),
                    tabs: Some(&named.tabs),
                };
                let outcome = named.call(&start, tool, args).await;
                self.forget_a_dead_browser(&paths, &named.context, active.port, outcome.is_err())
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
        paths: &StackPaths,
        context: &Context,
        port: u16,
        failed: bool,
    ) {
        if !failed || chromium::answers_as(&paths.profile, port).await {
            return;
        }
        context.drop_driver().await;
    }

    /// Close a named context, whoever opened it: the client's own UI acting on
    /// the row.
    ///
    /// **The human's door, and no seat.** A session drives only the context it
    /// opened - the verdict refuses the rest - but the row is the person's, and
    /// a context whose owning session is GONE is exactly what this is for.
    /// **The save lands before the name is free, and the map's lock is not
    /// held across it**: a save drives the browser and answers on a call's own
    /// clock, while the lock is only for the map. The name goes only once the
    /// save landed - a close that could not persist keeps the context, so a
    /// failed save is not also a lost name.
    pub async fn close(&self, name: &str) -> Result<(), String> {
        let paths = self.paths.clone()?;
        let entry = {
            let named = self.named.lock().await;
            let Some(entry) = named.get(name).map(Arc::clone) else {
                return Err(format!("no browser context is open under '{name}'"));
            };
            entry
        };
        let active = self.active_browser(&paths).await?;
        let endpoint = format!("http://127.0.0.1:{}", active.port);
        let node = driver::node_path(&paths.stack);
        let cli = driver::cli_path(&paths.stack);
        let start = DriverStart {
            node: &node,
            cli: &cli,
            endpoint: &endpoint,
            identity: &active.path,
            output: &paths.output,
            storage: Some(&entry.storage),
            tabs: Some(&entry.tabs),
        };
        entry.save(&start).await?;
        // Compared before it goes: a name that was re-opened while the save
        // ran belongs to the new context, not to what was just saved.
        let mut named = self.named.lock().await;
        if let Some(held) = named.get(name)
            && Arc::ptr_eq(held, &entry)
        {
            named.remove(name);
        }
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
        self.active_browser(&paths).await
    }

    /// The live browser, launched when nothing is up.
    async fn active_browser(&self, paths: &StackPaths) -> Result<chromium::ActivePort, String> {
        let _launching = self.launch.lock().await;
        chromium::ensure(&chromium::chrome_binary(&paths.stack), &paths.profile).await
    }

    /// The named contexts this host holds, oldest name first, for its own
    /// strip. A context whose driver died but whose name is still held is
    /// listed as not running rather than dropped: the name is owned until it
    /// is released, and a row that vanished would read as released.
    pub async fn contexts(&self) -> Vec<ContextRow> {
        let named = self.named.lock().await;
        let mut rows: Vec<ContextRow> = named
            .iter()
            .map(|(name, entry)| ContextRow {
                name: name.clone(),
                owner: entry.owner.to_string(),
                running: entry.context.is_alive(),
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Bring the browser up visibly, which is what a hand-off's Open asks for.
    ///
    /// Serialized with every other launch, so a show racing a first call
    /// cannot leave two browsers on one profile. This is the client's own
    /// act and answers the core nothing: the hand-off's answer is Done or
    /// Not now, and never the window itself.
    pub async fn show(&self) -> Result<chromium::ActivePort, String> {
        let paths = self.paths.clone()?;
        let _launching = self.launch.lock().await;
        chromium::show(&chromium::chrome_binary(&paths.stack), &paths.profile).await
    }

    /// Bring the in-app browser view up over the client's window, under the
    /// bar the web side draws: the approved takeover.
    ///
    /// `bar_px` is how much of the top the bar occupies - the frames are the
    /// whole page and the web side sizes them into what is left. The browser
    /// is brought up first (a person waiting on a cold launch is the one wait
    /// worth removing), then a CDP session of our own starts the screencast
    /// and streams frames as `browser_frame` events.
    pub async fn takeover_open(
        &self,
        _bar_px: f64,
        app: &tauri::AppHandle,
    ) -> Result<(), String> {
        let paths = self.paths.clone()?;
        let active = self.active_browser(&paths).await?;
        let endpoint = format!("ws://127.0.0.1:{}{}", active.port, active.path);
        let emitter = app.clone();
        let live = screencast::start(&endpoint, move |frame| {
            let _ = emitter.emit("browser_frame", frame);
        })
        .await?;
        let mut held = self.live.lock().await;
        if let Some(previous) = held.take() {
            previous.stop();
        }
        *held = Some(live);
        Ok(())
    }

    /// Take the view back down. Idempotent: back, Done and a reloaded window
    /// may each ask.
    pub async fn takeover_close(&self) -> Result<(), String> {
        self.paths.clone()?;
        if let Some(live) = self.live.lock().await.take() {
            live.stop();
        }
        Ok(())
    }

    /// Whether the shell is holding a takeover up - what a reloaded window
    /// reads to re-draw the screen it was on.
    pub async fn takeover_active(&self) -> Result<bool, String> {
        self.paths.clone()?;
        Ok(self.live.lock().await.is_some())
    }

    /// One input event into the live view, as CDP wants it.
    pub async fn takeover_input(&self, method: &str, params: Value) -> Result<(), String> {
        let held = self.live.lock().await;
        let Some(live) = held.as_ref() else {
            return Err("no takeover is up".to_owned());
        };
        live.input(method, params);
        Ok(())
    }

    /// Whether a WINDOW is up on the browser this client hosts.
    ///
    /// The marker a headed launch leaves, plus a browser still answering as
    /// that launch: a marker alone outlives a browser that died, and a dock
    /// that trusted it would refuse a raise that was the right thing to do.
    pub async fn window_up(&self) -> Result<bool, String> {
        let paths = self.paths.clone()?;
        if !chromium::launched_windowed(&paths.profile) {
            return Ok(false);
        }
        let Some(active) = chromium::read_active_port(&paths.profile) else {
            return Ok(false);
        };
        Ok(chromium::answers_as(&paths.profile, active.port).await)
    }

    /// The browser's own context: one, cached, whose driver builds and
    /// rebuilds itself under its own lock.
    ///
    /// Serialized: a burst of calls arriving on a cold host finds ONE context
    /// and builds ONE driver, because every call runs through it.
    async fn default_context(&self) -> Arc<Context> {
        let mut default = self.default.lock().await;
        if let Some(context) = default.as_ref() {
            return Arc::clone(context);
        }
        let context = Arc::new(Context::new());
        *default = Some(Arc::clone(&context));
        context
    }

    /// The named context under `name`, opened when it is not there.
    ///
    /// The map's lock is held across an open, so two sessions naming one fresh
    /// context race at the verdict rather than both opening: the first opens
    /// and owns it, and the second is refused by the same rule as any other
    /// attach. A context whose driver died between calls needs nothing here -
    /// its next call rebuilds the driver over the saved files.
    async fn named_context(&self, seat: &Seat, name: &str) -> Result<Arc<Named>, String> {
        if let Some(refusal) = contexts::name_refusal(name) {
            return Err(refusal);
        }
        let paths = self.paths.clone()?;
        let mut named = self.named.lock().await;
        let held = named.get(name).map(|entry| &entry.owner);
        match contexts::verdict(name, seat, held) {
            contexts::Verdict::Refuse(refusal) => Err(refusal),
            contexts::Verdict::Drive => {
                let Some(entry) = named.get(name).map(Arc::clone) else {
                    return Err(format!("the context '{name}' went away while it was read"));
                };
                Ok(entry)
            }
            contexts::Verdict::Open => {
                // The saved tabs are reopened by the context's first CALL,
                // where its driver is built: opening the name here costs
                // nothing, and a session that names a context and never
                // drives it holds no driver at all.
                let fresh = Arc::new(Named::open(seat.clone(), name, &paths));
                named.insert(name.to_owned(), Arc::clone(&fresh));
                Ok(fresh)
            }
        }
    }
}

/// Take the `context` argument off a call.
///
/// It chooses the context and no driver's schema declares it, so it never
/// reaches a tool. A `context` that is not a name is the call's own mistake,
/// answered rather than guessed at.
fn take_context(mut args: Value) -> Result<(Option<String>, Value), String> {
    let Some(fields) = args.as_object_mut() else {
        return Ok((None, args));
    };
    let Some(context) = fields.remove("context") else {
        return Ok((None, args));
    };
    match context {
        Value::Null => Ok((None, args)),
        Value::String(name) => Ok((Some(name), args)),
        other => Err(format!("`context` is the name of a context, not {other}")),
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
pub async fn browser_show(host: tauri::State<'_, Arc<BrowserHost>>) -> Result<(), String> {
    host.show().await.map(|_| ())
}

/// Whether a browser window is already up, for a dock's own line: a button
/// that says Open over a window already open is a click that does nothing.
#[tauri::command]
pub async fn browser_window(host: tauri::State<'_, Arc<BrowserHost>>) -> Result<bool, String> {
    host.window_up().await
}

/// Bring the in-app browser view up over the client's window, under the bar
/// the web side draws.
#[tauri::command]
pub async fn browser_takeover_open(
    host: tauri::State<'_, Arc<BrowserHost>>,
    app: tauri::AppHandle,
    bar_px: f64,
) -> Result<(), String> {
    host.takeover_open(bar_px, &app).await
}

/// One input event into the live view (`Input.dispatchMouseEvent`,
/// `Input.dispatchKeyEvent`, `Input.insertText`), with its params.
#[tauri::command]
pub async fn browser_takeover_input(
    host: tauri::State<'_, Arc<BrowserHost>>,
    method: String,
    params: Value,
) -> Result<(), String> {
    host.takeover_input(&method, params).await
}

/// Take the view back down.
#[tauri::command]
pub async fn browser_takeover_close(host: tauri::State<'_, Arc<BrowserHost>>) -> Result<(), String> {
    host.takeover_close().await
}

/// Whether the shell is holding a takeover up, for a window that has just
/// reloaded.
#[tauri::command]
pub async fn browser_takeover_state(host: tauri::State<'_, Arc<BrowserHost>>) -> Result<bool, String> {
    host.takeover_active().await
}

/// One named context, as the client's own browser strip draws it.
#[derive(Debug, serde::Serialize)]
pub struct ContextRow {
    /// The name a session drives it by.
    pub name: String,
    /// The slot of the session that opened it, as its refusal prints.
    pub owner: String,
    /// Whether its driver is still there to answer.
    pub running: bool,
}

/// The named contexts this host holds, for its own browser strip.
///
/// The contexts are the CLIENT's own state - it owns the drivers - so this is
/// the client reading itself, not a server read; the strip needs no new view
/// surface for it.
#[tauri::command]
pub async fn browser_contexts(
    host: tauri::State<'_, Arc<BrowserHost>>,
) -> Result<Vec<ContextRow>, String> {
    Ok(host.contexts().await)
}

/// Close a named context from the client's own UI: the strip's row, acting
/// for the person rather than for a session.
#[tauri::command]
pub async fn browser_context_close(
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

    /// A name that cannot be a context is decided before anything else: no
    /// directory, no lock and no driver is consulted to answer it.
    #[tokio::test]
    async fn a_bad_context_name_is_refused_before_anything_is_started() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let refused = host
            .call(
                &seat(),
                "browser_navigate",
                json!({ "url": "https://example.com", "context": "a b" }),
            )
            .await;
        let Err(why) = refused else {
            panic!("a name with a space is refused");
        };
        assert!(why.contains("1 to 64"), "{why}");
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

    /// A host whose directories did not resolve answers that before anything
    /// else, closing is idempotent, nothing is held up, and input with no view
    /// names that rather than vanishing.
    #[tokio::test]
    async fn a_takeover_on_an_unavailable_host_answers_the_reason_and_holds_nothing() {
        let host = BrowserHost::unavailable("the browser stack was never vendored".to_owned());
        assert_eq!(
            host.takeover_close().await,
            Err("the browser stack was never vendored".to_owned()),
            "the missing directories answer first",
        );
        assert_eq!(
            host.takeover_input("Input.insertText", serde_json::json!({ "text": "x" })).await,
            Err("no takeover is up".to_owned()),
            "with no view, input says so rather than vanishing",
        );
    }

    /// A host nothing has named holds no contexts, and answers the strip with
    /// an empty list rather than an error: no contexts is a state, not a
    /// failure.
    #[tokio::test]
    async fn a_fresh_host_holds_no_contexts() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        assert!(host.contexts().await.is_empty());
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
        assert!(absent.profile.starts_with(data.path()), "{absent:?}");
        assert!(absent.output.starts_with(data.path()), "{absent:?}");
        assert!(absent.contexts.starts_with(data.path()), "{absent:?}");
        assert!(
            !absent.profile.starts_with(resource.path()),
            "and none of them hangs off the resource directory: {absent:?}",
        );
        assert_ne!(absent.profile, absent.output, "the profile is not where output files land");
        assert_ne!(absent.contexts, absent.profile, "a context's files are not the profile itself");

        std::fs::create_dir_all(resource.path().join("browser-stack/node/bin")).expect("dirs");
        std::fs::write(resource.path().join("browser-stack/node/bin/node"), b"").expect("node");
        let bundled = StackPaths::from_dirs(resource.path(), data.path());
        assert_eq!(
            bundled.stack,
            resource.path().join("browser-stack"),
            "and the bundle's own copy wins once the vendoring is there",
        );
    }

    /// `context` chooses the context and never reaches a tool; a call without
    /// one is a call for the browser's own context; and a `context` that is
    /// not a name is the call's mistake, answered.
    #[test]
    fn a_context_argument_is_taken_off_and_must_be_a_name() {
        let (chosen, rest) =
            take_context(json!({ "url": "https://example.com", "context": "hunt" }))
                .expect("a name is taken off");
        assert_eq!(chosen, Some("hunt".to_owned()));
        assert_eq!(rest, json!({ "url": "https://example.com" }));

        let (none, rest) =
            take_context(json!({ "url": "https://example.com" })).expect("no context");
        assert_eq!(none, None);
        assert_eq!(rest, json!({ "url": "https://example.com" }));

        let (none, rest) = take_context(Value::Null).expect("a bare call");
        assert_eq!(none, None);
        assert_eq!(rest, Value::Null);

        let (none, _) = take_context(json!({ "context": null })).expect("null is no context");
        assert_eq!(none, None);

        let refused = take_context(json!({ "context": 7 })).expect_err("a number is not a name");
        assert!(refused.contains("`context` is the name"), "{refused}");
    }
}

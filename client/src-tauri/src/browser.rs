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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tauri::Manager as _;

use contexts::{Context, Named, Seat};
use driver::{Driver, ReplyPart};
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

        let bundled = resource.join("browser-stack");
        let stack = if bundled.join("node/bin/node").is_file() {
            bundled
        } else {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack")
        };
        Ok(Self {
            stack,
            profile: data.join("browser/profile"),
            output: data.join("browser/output"),
            contexts: data.join("browser/contexts"),
        })
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
}

impl BrowserHost {
    pub fn new(paths: StackPaths) -> Self {
        Self {
            paths: Ok(paths),
            launch: Mutex::new(()),
            default: Mutex::new(None),
            named: Mutex::new(HashMap::new()),
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
        }
    }

    /// Run one browser tool for a session.
    ///
    /// No `context` argument drives the browser's own context. A name drives
    /// the named context under it, opened on first use by the asking session;
    /// a name another session already holds is refused with the owner's name.
    pub async fn call(
        &self,
        seat: &Seat,
        tool: &str,
        args: Value,
    ) -> Result<Vec<ReplyPart>, String> {
        let (context, args) = take_context(args)?;
        match context {
            None => {
                let context = self.default_context().await?;
                context.call(tool, args).await
            }
            Some(name) => {
                let named = self.named_context(seat, &name).await?;
                named.call(tool, args).await
            }
        }
    }

    /// Release a named context: save it, close its driver and forget the name.
    ///
    /// Only the session that opened it releases it, mirroring who may attach;
    /// the name then opens fresh for whoever names it next.
    pub async fn release(&self, seat: &Seat, name: &str) -> Result<(), String> {
        let mut named = self.named.lock().await;
        let Some(entry) = named.get(name) else {
            return Err(format!("no browser context is open under '{name}'"));
        };
        if &entry.owner != seat {
            return Err(format!(
                "the context '{name}' belongs to {}; only the session that opened it releases it",
                entry.owner,
            ));
        }
        let Some(entry) = named.remove(name) else {
            return Err(format!("the context '{name}' went away while it was being released"));
        };
        drop(named);
        entry.save().await
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

    /// The browser's own context's driver, started when it is not up.
    ///
    /// Serialized: a burst of calls arriving on a cold host must launch ONE
    /// driver, and every caller behind the first finds it there. A driver that
    /// died is replaced on the next call.
    async fn default_context(&self) -> Result<Arc<Context>, String> {
        let paths = self.paths.clone()?;
        let mut default = self.default.lock().await;
        if let Some(context) = default.as_ref().filter(|context| context.is_alive()) {
            return Ok(Arc::clone(context));
        }
        let endpoint = format!("http://127.0.0.1:{}", self.active_browser(&paths).await?.port);
        let driver = Driver::start(
            &driver::node_path(&paths.stack),
            &driver::cli_path(&paths.stack),
            &endpoint,
            &paths.output,
            None,
        )
        .await?;
        let context = Arc::new(Context::new(driver));
        *default = Some(Arc::clone(&context));
        Ok(context)
    }

    /// The named context under `name`, opened when it is not there.
    ///
    /// The map's lock is held across an open, so two sessions naming one fresh
    /// context race at the verdict rather than both starting a driver: the
    /// first opens and owns it, and the second is refused by the same rule as
    /// any other attach.
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
                if entry.context.is_alive() {
                    return Ok(entry);
                }
                // The driver died between calls: bring it back over its saved
                // files, still under the owner that holds the name.
                let fresh = Arc::new(self.start_named(&paths, &entry.owner, name).await?);
                named.insert(name.to_owned(), Arc::clone(&fresh));
                Ok(fresh)
            }
            contexts::Verdict::Open => {
                let fresh = Arc::new(self.start_named(&paths, seat, name).await?);
                named.insert(name.to_owned(), Arc::clone(&fresh));
                Ok(fresh)
            }
        }
    }

    /// Start the driver behind a named context and reopen its saved tabs.
    async fn start_named(
        &self,
        paths: &StackPaths,
        owner: &Seat,
        name: &str,
    ) -> Result<Named, String> {
        let endpoint = format!("http://127.0.0.1:{}", self.active_browser(paths).await?.port);
        let named = Named::start(owner.clone(), name, &endpoint, paths).await?;
        contexts::reopen_tabs(&named.context, &contexts::saved_tabs(&named.tabs)).await;
        Ok(named)
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

/// Release the asking session's named context.
#[tauri::command]
pub async fn browser_context_release(
    host: tauri::State<'_, Arc<BrowserHost>>,
    seat: Seat,
    name: String,
) -> Result<(), String> {
    host.release(&seat, &name).await
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

    /// A host nothing has named holds no contexts, and answers the strip with
    /// an empty list rather than an error: no contexts is a state, not a
    /// failure.
    #[tokio::test]
    async fn a_fresh_host_holds_no_contexts() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        assert!(host.contexts().await.is_empty());
    }

    /// The profile, the output and the contexts are the app's OWN
    /// directories: a profile that landed beside the binary would hold logins
    /// in a place a bundle update erases.
    #[test]
    fn the_machine_local_directories_are_under_the_app_data() {
        let paths = StackPaths {
            stack: PathBuf::from("/stack"),
            profile: PathBuf::from("/data/browser/profile"),
            output: PathBuf::from("/data/browser/output"),
            contexts: PathBuf::from("/data/browser/contexts"),
        };
        assert!(paths.profile.starts_with("/data/browser"), "{paths:?}");
        assert!(paths.output.starts_with("/data/browser"), "{paths:?}");
        assert!(paths.contexts.starts_with("/data/browser"), "{paths:?}");
        assert_ne!(paths.profile, paths.output, "the profile is not where output files land");
        assert_ne!(paths.contexts, paths.profile, "a context's files are not the profile itself");
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

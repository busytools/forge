//! The client's browser host: the side that owns the browser process, the
//! profile and the driver, and answers the asks a session's browser tools
//! send over the socket.
//!
//! **One browser, one profile, one driver per client**, and the browser
//! outlives the client: it is launched detached against a profile under the
//! app-support directory, so logins and cookies survive a client restart and
//! a forge restart touches nothing here (spec section 3).
//!
//! The pieces:
//! - [`chromium`] - where the vendored Chromium is, how it is launched, and
//!   how a launch is found again after a restart.
//! - [`driver`] - upstream `@playwright/mcp` as a child process, spoken to as
//!   an MCP client.
//!
//! Nothing here decides what a tool MEANS: the ask carries upstream's own
//! tool name and arguments, the driver runs them, and the answer is the parts
//! it returned.

pub mod chromium;
pub mod custom;
pub mod driver;

use std::path::PathBuf;
use std::sync::Arc;

use tauri::Manager as _;

use driver::{Driver, ReplyPart};
use serde_json::Value;
use tokio::sync::Mutex;

/// Where the vendored stack is, and the two machine-local directories the
/// browser owns.
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
}

impl StackPaths {
    /// Resolve from the running app.
    ///
    /// The stack is the bundle's own resource directory, and in a DEV build
    /// - where no resources are copied - the checkout the binary was built
    /// from. Both are places this build knows, never the directory the client
    /// happened to be launched from.
    ///
    /// The two directories are the app's own data directory, and their
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
        })
    }
}

/// The host: the driver it holds, and where everything lives.
pub struct BrowserHost {
    paths: Result<StackPaths, String>,
    inner: Mutex<Option<Arc<Driver>>>,
}

impl BrowserHost {
    pub fn new(paths: StackPaths) -> Self {
        Self { paths: Ok(paths), inner: Mutex::new(None) }
    }

    /// A host that cannot act, for a client whose directories did not
    /// resolve: every call answers why rather than the client refusing to
    /// start over a feature it may never be asked for.
    pub fn unavailable(why: String) -> Self {
        Self { paths: Err(why), inner: Mutex::new(None) }
    }

    /// Run one browser tool, answering with the parts it returned or the
    /// reason it failed.
    ///
    /// A call this host answers itself - the additions beyond upstream's
    /// surface - is routed to a snippet over the same driver; everything
    /// else goes to the driver as upstream's own tool, with the added
    /// arguments stripped so its schema accepts the call.
    pub async fn call(&self, tool: &str, args: Value) -> Result<Vec<ReplyPart>, String> {
        let driver = self.driver().await?;
        match custom::route(tool, &args)? {
            custom::Routed::Upstream { tool, args } => {
                // The call itself runs off the lock: two sessions asking at
                // once are two requests on one connection, and pairing them
                // is `rmcp`'s.
                driver.call(&tool, args).await
            }
            custom::Routed::Snippet(code) => {
                driver.call("browser_run_code_unsafe", serde_json::json!({ "code": code })).await
            }
        }
    }

    /// Bring the browser up, without the driver.
    ///
    /// **The app's own start, so the browser is there before anything asks
    /// for it** rather than being launched under the first tool call: a
    /// session's call should not pay a cold launch, and a browser that cannot
    /// start at all says so in the app's log at startup instead of as a
    /// failed tool call. The driver stays lazy - it exists to serve calls,
    /// and one with no calls to serve is a child process held for nothing.
    ///
    /// Serialized through the same lock [`Self::driver`] takes, so a start
    /// racing a first call launches ONE browser rather than two onto one
    /// profile.
    pub async fn start(&self) -> Result<(), String> {
        let paths = self.paths.clone()?;
        let _held = self.inner.lock().await;
        chromium::ensure(&chromium::chrome_binary(&paths.stack), &paths.profile).await?;
        Ok(())
    }

    /// The driver to use, starting the browser and the driver if either is
    /// not up.
    ///
    /// Serialized: a burst of calls arriving on a cold host must launch ONE
    /// browser, and every caller behind the first finds it there.
    async fn driver(&self) -> Result<Arc<Driver>, String> {
        let paths = self.paths.clone()?;
        let mut inner = self.inner.lock().await;

        let browser =
            chromium::ensure(&chromium::chrome_binary(&paths.stack), &paths.profile).await?;
        if inner.as_ref().is_none_or(|driver| !driver.is_running()) {
            let endpoint = format!("http://127.0.0.1:{}", browser.port);
            let started = Driver::start(
                &driver::node_path(&paths.stack),
                &driver::cli_path(&paths.stack),
                &endpoint,
                &paths.output,
                None,
            )
            .await?;
            *inner = Some(Arc::new(started));
        }
        let Some(driver) = inner.as_ref() else {
            return Err("the browser driver went away while it was being started".to_owned());
        };
        Ok(Arc::clone(driver))
    }
}

/// One tool's answer, in the shape the frontend's ask handler returns.
#[derive(Debug, serde::Serialize)]
pub struct BrowserReply {
    pub parts: Vec<ReplyPart>,
}

/// Run one browser tool call: the frontend's half of the ask/answer pair.
///
/// The frontend receives a `browser_ask`, invokes this, and sends the parts
/// back as its answer - so a failure here is the tool's failure, with the
/// reason the driver gave.
#[tauri::command]
pub async fn browser_call(
    host: tauri::State<'_, Arc<BrowserHost>>,
    tool: String,
    args: Value,
) -> Result<BrowserReply, String> {
    host.call(&tool, args).await.map(|parts| BrowserReply { parts })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host whose directories did not resolve still starts, and every call
    /// names why it cannot act: a client that refused to boot over this would
    /// take the whole app down for a feature nobody may ask for.
    #[tokio::test]
    async fn a_host_with_no_directories_answers_why() {
        let host =
            BrowserHost::unavailable("the app's data directory cannot be resolved".to_owned());
        let refused = host.call("browser_close", Value::Null).await;
        assert_eq!(
            refused,
            Err("the app's data directory cannot be resolved".to_owned()),
            "the reason a client can act on, not a panic and not a silence",
        );
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

    /// The profile and the output are the app's OWN directories: a profile
    /// that landed beside the binary would hold logins in a place a bundle
    /// update erases.
    #[test]
    fn the_machine_local_directories_are_under_the_app_data() {
        let paths = StackPaths {
            stack: PathBuf::from("/stack"),
            profile: PathBuf::from("/data/browser/profile"),
            output: PathBuf::from("/data/browser/output"),
        };
        assert!(paths.profile.starts_with("/data/browser"), "{paths:?}");
        assert!(paths.output.starts_with("/data/browser"), "{paths:?}");
        assert_ne!(paths.profile, paths.output, "the profile is not where output files land");
    }
}

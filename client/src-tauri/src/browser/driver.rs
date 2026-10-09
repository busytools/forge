//! The driver: upstream `@playwright/mcp`, run as a child process and spoken
//! to as an MCP CLIENT.
//!
//! **Upstream before building** (rule 23): the actionability, the snapshot
//! with refs and the page-state the tools depend on are Playwright's, so the
//! driver IS Playwright's own MCP server, pointed at the browser this host
//! launched. `rmcp` carries the child process, the id correlation and the
//! cancellation, so this module is the wiring and the mapping and nothing
//! more.
//!
//! **`--no-webmcp` is what keeps the surface deterministic**: without it the
//! driver registers whatever tools a page offers through WebMCP and emits
//! `tools/list_changed`, so what a session sees would depend on what the page
//! it happens to have open chooses to expose.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt as _, TokioChildProcess};
use serde_json::Value;

/// The longest one tool call is given before the driver is presumed mute.
const CALL_TIMEOUT: Duration = Duration::from_secs(150);

/// **The longest a driver start is given before it is presumed wedged.**
/// Measured live 2026-10-07: a named profile's driver child spawned and sat
/// idle on its stdin while the parent never wrote the handshake - and the
/// start has no bound of its own, so the session's call parked with it, past
/// even [`CALL_TIMEOUT`]. A start that cannot answer names it instead.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// The phone's bounds, its own, and the ACCEPT-ONWARD segment must fit the
/// server's ask budget: `ASK_TIMEOUT` (15+15+150+20 = 200 s) is derived from
/// the DESKTOP's launch, handshake and call bounds, and the phone's cold
/// figures join it - 40 s to accept a node's FIRST dial (its boot is real
/// work: measured 8 s warm, 39 s on a loaded emulator) + 10 s to hand shake
/// + the driver's own 150 s call = 200 s, **exactly the bound**. That is
/// only the accept-onward segment, though: the call also carries an
/// unbounded pre-accept RPC segment (the engine generation read, the ensure
/// spin, the asset unpack, the UI-thread origin latch), so the whole chain
/// can MEET or exceed the 200 s ask rather than sit inside it. A node that
/// was ALREADY up redials every second, so a later call waits only 6 s - a
/// dead in-app node fails in seconds with the reason instead of paying the
/// cold window per call.
#[cfg(target_os = "android")]
const IN_APP_COLD_ACCEPT_TIMEOUT: Duration = Duration::from_secs(40);
#[cfg(target_os = "android")]
const IN_APP_WARM_ACCEPT_TIMEOUT: Duration = Duration::from_secs(6);
#[cfg(target_os = "android")]
const IN_APP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// One part of a tool's answer, on its way to the socket.
///
/// An image's bytes cross here as the base64 the MCP client's own model
/// carries: the socket wants them as a binary frame, and the frontend decodes
/// them into one.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplyPart {
    Text { text: String },
    Image { mime_type: String, data_base64: String },
}

/// The driver child, and the MCP client speaking to it.
pub struct Driver {
    /// Kept so the child is killed when the driver goes: a relaunch must not
    /// leave the old one holding the browser.
    _service: RunningService<rmcp::RoleClient, ()>,
    client: rmcp::Peer<rmcp::RoleClient>,
    /// The phone's own page, by origin: the WebView's devtools lists every
    /// debuggable page in the app process, the client's UI among them, and
    /// the driver must be pinned off it (see [`Driver::start_inapp`]).
    #[cfg(target_os = "android")]
    ui_origin: String,
}

impl Driver {
    /// Start the driver against a browser's CDP endpoint.
    ///
    /// The driver always attaches to the browser's own profile - the
    /// profile's, which outlives everything. **Isolation comes from the
    /// BROWSER the endpoint belongs to**: one browser per profile means one
    /// Chromium profile per name, no `--isolated` contexts and no storage
    /// files.
    pub async fn start(
        node: &Path,
        cli: &Path,
        cdp_endpoint: &str,
        output_dir: &Path,
    ) -> Result<Self, String> {
        if !node.is_file() || !cli.is_file() {
            return Err(format!(
                "the vendored driver is not there ({} / {}) - run `just vendor-browser-stack`",
                node.display(),
                cli.display(),
            ));
        }
        std::fs::create_dir_all(output_dir)
            .map_err(|why| format!("the browser output directory cannot be made: {why}"))?;
        let mask = write_mask(output_dir);

        tauri_plugin_log::log::info!("starting the driver against {cdp_endpoint}");
        let transport =
            TokioChildProcess::new(tokio::process::Command::new(node).configure(|cmd| {
                // **No gate on what a session opens or reads, and a cwd that
                // is pinned.** Upstream resolves a relative file path against
                // `process.cwd()` and refuses a path outside its roots, and
                // this child inherits whatever directory the CLIENT was
                // launched from - so a `file_upload` worked from Finder and
                // failed from a shell. forge has no gates here (the trust
                // bound is the profile), so the flag removes the root check
                // outright and the cwd is pinned under the app's data
                // directory rather than left to the launch directory. The
                // `--output-dir` is where upstream's own screenshot default
                // lands: the client runs from a directory nobody chose, and a
                // file dropped there is one nobody finds.
                for arg in driver_args(cli, cdp_endpoint, output_dir, mask.as_deref()) {
                    cmd.arg(arg);
                }
                cmd.current_dir(output_dir);
            }))
            .map_err(|why| format!("the driver would not start: {why}"))?;

        // **The handshake is bounded**: a child that spawns and never answers
        // parks the session exactly as a wedge does, with nothing to read.
        let service = tokio::time::timeout(START_TIMEOUT, ().serve(transport))
            .await
            .map_err(|_| {
                format!(
                    "the driver did not answer its MCP handshake within {} s",
                    START_TIMEOUT.as_secs()
                )
            })?
            .map_err(|why| format!("the driver did not answer its MCP handshake: {why}"))?;
        tauri_plugin_log::log::info!("the driver answers on {cdp_endpoint}");
        let client = service.peer().clone();
        Ok(Self {
            _service: service,
            client,
            #[cfg(target_os = "android")]
            ui_origin: String::new(),
        })
    }

    /// Start the driver on the phone: libnode runs in THIS process, so there
    /// is no child to spawn and no stdio to borrow - the Kotlin engine starts
    /// the in-app node, which dials the unix socket bound here and wears it
    /// as its stdio (see the bootstrap script). Everything after the
    /// transport is the same client as the desktop's.
    ///
    /// **The handshake is followed by a tab pin**, because the WebView's
    /// devtools lists every page in the app process and the client's own UI
    /// is one of them - measured: an unpinned driver drives the UI page
    /// (which playwright finds first). The pin selects the page that is not
    /// the UI's origin, and a pin that finds none fails the start rather than
    /// letting a session's first call navigate the client away.
    #[cfg(target_os = "android")]
    pub async fn start_inapp(
        engine: &super::android::Engine,
        socket: &Path,
        ui_origin: &str,
        output_dir: &Path,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(output_dir)
            .map_err(|why| format!("the browser output directory cannot be made: {why}"))?;
        // A socket file left by a dead run would refuse the bind.
        let _ = std::fs::remove_file(socket);
        let listener = tokio::net::UnixListener::bind(socket)
            .map_err(|why| format!("the driver socket could not be bound: {why}"))?;
        let relay = engine.start_driver(socket, output_dir).await?;
        tauri_plugin_log::log::info!(
            "the engine answers (relay port {}, node started: {})",
            relay.relay_port,
            relay.node_started
        );
        let accept_bound = if relay.node_started {
            IN_APP_COLD_ACCEPT_TIMEOUT
        } else {
            IN_APP_WARM_ACCEPT_TIMEOUT
        };
        let accept = tokio::time::timeout(accept_bound, listener.accept())
            .await
            .map_err(|_| {
                // An already-running node only redials a DEAD link, so a warm
                // window timing out is the post-renderer-death shape - the
                // session cannot tell that from a socket fault otherwise
                // (only logcat names the death).
                let hint = if relay.node_started {
                    ""
                } else {
                    " (the in-app node was already running and only redials a dead link; if a \
                     renderer death was reported, restarting the app is the repair - issue #1931)"
                };
                format!(
                    "the on-device driver did not dial its socket within {} s{hint}",
                    accept_bound.as_secs()
                )
            })?
            .map_err(|why| format!("the driver socket did not accept: {why}"))?;
        let (stream, _) = accept;
        let service = tokio::time::timeout(IN_APP_HANDSHAKE_TIMEOUT, ().serve(stream))
            .await
            .map_err(|_| {
                format!(
                    "the on-device driver did not answer its MCP handshake within {} s",
                    IN_APP_HANDSHAKE_TIMEOUT.as_secs()
                )
            })?
            .map_err(|why| format!("the driver did not answer its MCP handshake: {why}"))?;
        let client = service.peer().clone();
        let origin =
            if ui_origin.is_empty() { relay.ui_origin.clone() } else { ui_origin.to_owned() };
        if let Some(refusal) = start_origin_refusal(&origin) {
            return Err(refusal);
        }
        let driver = Self { _service: service, client, ui_origin: origin };
        driver.pin_browser_tab().await?;
        driver.seed_viewport(&relay).await;
        Ok(driver)
    }

    /// Seed the driver's context with the engine's real viewport.
    ///
    /// **Playwright's geometry checks run against ITS context viewport, and
    /// that starts at nothing over CDP**: an invisible WebView lays its page
    /// out (the page's own innerWidth reads 980), yet `browser_click`
    /// refuses with "element is outside of the viewport" and
    /// `browser_take_screenshot` with "Cannot take screenshot with 0 width"
    /// until an emulation override is set - which `browser_resize` is. A
    /// failure here is logged, not fatal: navigate/type/evaluate carry
    /// without it.
    #[cfg(target_os = "android")]
    async fn seed_viewport(&self, relay: &super::android::Relay) {
        if relay.viewport_width == 0 || relay.viewport_height == 0 {
            return;
        }
        let args =
            serde_json::json!({ "width": relay.viewport_width, "height": relay.viewport_height });
        match self.call_raw("browser_resize", args).await {
            Ok(_) => tauri_plugin_log::log::info!(
                "the driver's context seeded with the engine's viewport ({}x{})",
                relay.viewport_width,
                relay.viewport_height
            ),
            Err(why) => tauri_plugin_log::log::warn!("the viewport seed did not take: {why}"),
        }
    }

    /// Point the driver's current tab at the browser page. See
    /// [`Driver::start_inapp`] for why this exists.
    #[cfg(target_os = "android")]
    async fn pin_browser_tab(&self) -> Result<(), String> {
        let listed = self.call_raw("browser_tabs", serde_json::json!({ "action": "list" })).await?;
        let text = listed
            .iter()
            .map(|part| match part {
                ReplyPart::Text { text } => text.as_str(),
                ReplyPart::Image { .. } => "[an image]",
            })
            .collect::<Vec<_>>()
            .join("\n");
        let tabs = browser_tabs_of(&text);
        let Some((index, url)) =
            tabs.iter().find(|(_, url)| !origin_match(url, self.ui_origin.as_str())).cloned()
        else {
            return Err(format!(
                "the browser page could not be told apart from the client's own screen \
                 (tabs: {tabs:?}, client origin: {})",
                self.ui_origin,
            ));
        };
        tauri_plugin_log::log::info!("pinning the driver to tab {index} ({url})");
        self.call_raw("browser_tabs", serde_json::json!({ "action": "select", "index": index }))
            .await
            .map(|_| ())
    }

    /// The phone's page is not a tab, and the phone has no second page: a
    /// session's own tab calls are refused where they could put the driver
    /// on the client's screen or take the engine's only page away. The pin
    /// keeps the driver off the UI by default; this keeps a session from
    /// putting it there with `browser_tabs select`, and refuses `close`
    /// outright. **The decision itself is `tab_call_refusal`** - pure, so
    /// the host tests can pin it; this method only fetches the tab list the
    /// select case needs.
    #[cfg(target_os = "android")]
    async fn refuse_a_client_page(&self, tool: &str, args: &Value) -> Result<(), String> {
        let needs_tabs = tool == "browser_tabs"
            && args.get("action").and_then(Value::as_str) == Some("select")
            && args.get("index").and_then(Value::as_u64).is_some();
        let listed = if needs_tabs {
            let parts =
                self.call_raw("browser_tabs", serde_json::json!({ "action": "list" })).await?;
            parts
                .iter()
                .map(|part| match part {
                    ReplyPart::Text { text } => text.as_str(),
                    ReplyPart::Image { .. } => "[an image]",
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };
        match tab_call_refusal(tool, args, &listed, self.ui_origin.as_str()) {
            Some(refusal) => Err(refusal),
            None => Ok(()),
        }
    }

    /// Whether the child is still there to answer.
    pub fn is_running(&self) -> bool {
        !self.client.is_transport_closed()
    }

    /// The tool names the driver itself publishes: the live side of the
    /// parity check, read from the pinned package rather than from a capture.
    pub async fn tool_names(&self) -> Result<Vec<String>, String> {
        let listed = self
            .client
            .list_tools(None)
            .await
            .map_err(|why| format!("the driver did not list its tools: {why}"))?;
        Ok(listed.tools.into_iter().map(|tool| tool.name.to_string()).collect())
    }

    /// Run one tool, answering with its parts or the reason it failed.
    ///
    /// **A request timeout, because rmcp carries none by default.** In 3.5.0
    /// the peer's request timeout is NONE, so a driver that accepts a request
    /// and never answers would hold the call - and, through the profile's own
    /// lock, every call behind it - forever. The bound sits above upstream's
    /// own (a 60 s navigation, a 30 s wait) with room: it is the wedge-breaker,
    /// not a deadline the tools keep.
    pub async fn call(&self, tool: &str, args: Value) -> Result<Vec<ReplyPart>, String> {
        #[cfg(target_os = "android")]
        self.refuse_a_client_page(tool, &args).await?;
        self.call_raw(tool, args).await
    }

    /// The dispatch itself, unguarded: the guard's own tab list runs through
    /// this, or the guard would recurse into itself.
    async fn call_raw(&self, tool: &str, args: Value) -> Result<Vec<ReplyPart>, String> {
        let arguments = match args {
            Value::Object(fields) => fields,
            Value::Null => serde_json::Map::new(),
            other => {
                return Err(format!(
                    "{tool} was called with arguments that are not an object: {other}"
                ));
            }
        };
        let result = tokio::time::timeout(
            CALL_TIMEOUT,
            self.client
                .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments)),
        )
        .await
        .map_err(|_| {
            format!("the driver did not answer {tool} within {} s", CALL_TIMEOUT.as_secs())
        })?
        .map_err(|why| format!("the driver did not answer {tool}: {why}"))?;

        let parts = parts_of(&result)?;
        if result.is_error == Some(true) {
            // Upstream reports a failed call as an error result whose content
            // is the reason; the socket's failure arm carries the words.
            let reason = parts
                .iter()
                .map(|part| match part {
                    ReplyPart::Text { text } => text.clone(),
                    ReplyPart::Image { .. } => "[an image]".to_owned(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(if reason.is_empty() { format!("{tool} failed") } else { reason });
        }
        Ok(parts)
    }
}

/// Map one MCP result onto the parts the socket carries.
///
/// **A block this host cannot carry is an error rather than a silence.** The
/// tools phase 1 registers answer with text and images; a resource or an
/// audio block is something no view could draw and no rule allows dropping,
/// so the call fails naming it.
fn parts_of(result: &rmcp::model::CallToolResult) -> Result<Vec<ReplyPart>, String> {
    use rmcp::model::ContentBlock;

    let mut parts = Vec::with_capacity(result.content.len());
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => parts.push(ReplyPart::Text { text: text.text.clone() }),
            ContentBlock::Image(image) => parts.push(ReplyPart::Image {
                mime_type: image.mime_type.clone(),
                data_base64: image.data.clone(),
            }),
            other => {
                return Err(format!(
                    "the driver answered with a block this host cannot carry: {other:?}"
                ));
            }
        }
    }
    Ok(parts)
}

/// The tabs a `browser_tabs` list answer names, as (index, url) pairs. The
/// driver draws them as `- 0: (current) [title](url)` lines; the phone's pin
/// and guard only need the index and the page it points at.
///
/// Kept free of cfg so it is tested on the desktop too: it parses format the
/// phone's correctness rests on.
pub fn browser_tabs_of(list_text: &str) -> Vec<(usize, String)> {
    let mut tabs = Vec::new();
    for line in list_text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- ") else {
            continue;
        };
        let Some((index_text, remainder)) = rest.split_once(':') else {
            continue;
        };
        let Ok(index) = index_text.trim().parse::<usize>() else {
            continue;
        };
        // The LAST parenthesized run is the URL: the line may open with
        // `(current)`, and the title's own brackets are not parentheses.
        let Some(open) = remainder.rfind('(') else {
            continue;
        };
        let Some(close) = remainder.rfind(')') else {
            continue;
        };
        if close <= open {
            continue;
        }
        tabs.push((index, remainder[open + 1..close].to_owned()));
    }
    tabs
}

/// A URL's origin (`scheme://authority`), or the whole thing when it has no
/// path - compared by EQUALITY, never by prefix: `http://tauri.localhost` as
/// a prefix would also match `http://tauri.localhost.evil.test`.
pub fn origin_of(url: &str) -> &str {
    let Some(scheme_end) = url.find("://") else {
        return url;
    };
    let rest = &url[scheme_end + 3..];
    let end = rest.find(['/', '?', '#']).map_or(url.len(), |i| scheme_end + 3 + i);
    &url[..end]
}

/// Whether a page URL belongs to `origin` - by origin equality, so a page on
/// a lookalike host does not read as the client's own.
pub fn origin_match(url: &str, origin: &str) -> bool {
    !origin.is_empty() && origin_of(url).eq_ignore_ascii_case(origin)
}

/// Why the phone's driver cannot start with this origin, or `None` when it
/// can. Pure, so the host tests pin what `cfg(android)` hides - the same
/// purpose `tab_call_refusal` serves.
///
/// **An empty origin fails the start - it cannot fall through to the pin.**
/// `origin_match` refuses an empty origin by design, so an empty one INVERTS
/// the pin (every URL fails the match, and the first tab - which is the
/// client's own page - is picked) and disarms the guard entirely. Reachable
/// whenever the Kotlin side answers "" (no client webview yet, or its
/// UI-thread latch times out on a busy thread).
pub fn start_origin_refusal(origin: &str) -> Option<String> {
    origin.is_empty().then(|| {
        "the client UI's origin could not be read, so the browser page cannot be told apart \
         from it; the driver is not started"
            .to_owned()
    })
}

/// Why a tab-shaped call is refused on the phone, or `None` when the driver
/// may run it. Pure, so the host tests pin what `cfg(android)` hides.
///
/// The harms: `browser_close` and `browser_tabs close` take the engine's
/// only page away (`Target.closeTarget` reports success while the target
/// STAYS LISTED on the WebView, so the driver's own bookkeeping drifts and
/// its current tab can re-point at the client's UI page), and `browser_tabs
/// select` can put the driver there directly. **An empty `ui_origin` cannot
/// tell any page apart from the client's own**, so a select under one is
/// refused rather than run blind (`start_inapp` refuses the whole start on
/// an empty origin too - this is the second line).
pub fn tab_call_refusal(tool: &str, args: &Value, listed: &str, ui_origin: &str) -> Option<String> {
    if tool == "browser_close" {
        return Some(
            "the phone's browser page is the app's own screen and is never closed: navigate \
             it away instead (browser_navigate), or leave it where it is"
                .to_owned(),
        );
    }
    if tool != "browser_tabs" {
        return None;
    }
    let action = args.get("action").and_then(Value::as_str).unwrap_or_default();
    if action == "close" {
        return Some(
            "the phone hosts one browser page and closing it takes the engine's only page: \
             navigate it away instead (browser_navigate)"
                .to_owned(),
        );
    }
    if action != "select" {
        return None;
    }
    let Some(index) = args.get("index").and_then(Value::as_u64) else {
        return None;
    };
    if ui_origin.is_empty() {
        return Some(format!(
            "the client UI's origin is not known, so tab {index} cannot be checked against it"
        ));
    }
    for (candidate, url) in browser_tabs_of(listed) {
        if candidate == index as usize && origin_match(&url, ui_origin) {
            return Some(format!(
                "tab {index} is this client's own screen, not a browser page: the browser \
                 tools drive the shared browser only"
            ));
        }
    }
    None
}

/// The mask script the driver loads as its initialization script: what a
/// page can read in JavaScript about being driven.
///
/// **The engine's own launch does the real work** (`chromium.rs` carries
/// `--disable-blink-features=AutomationControlled` and the headless UA), so
/// each patch below only fires where that did not hold - a browser adopted
/// from a launch older than this build - and everything here is left alone
/// where the engine already reports the truth. Every patch is wrapped, and a
/// page that froze one of these objects is not a reason to break the page.
const MASK_SCRIPT: &str = r#"(() => {
  // navigator.webdriver: a true is masked; the engine's own false is left
  // untouched, native getter and all.
  try {
    if (navigator.webdriver) {
      Object.defineProperty(Object.getPrototypeOf(navigator), 'webdriver', {
        get: () => undefined,
        configurable: true,
      });
    }
  } catch {}

  // window.chrome: a shim only where the engine has none at all.
  try {
    if (!window.chrome) {
      window.chrome = {
        app: {
          isInstalled: false,
          InstallState: { DISABLED: 'disabled', INSTALLED: 'installed', NOT_INSTALLED: 'not_installed' },
          RunningState: { CANNOT_RUN: 'cannot_run', READY_TO_RUN: 'ready_to_run', RUNNING: 'running' },
        },
        runtime: {},
      };
    }
  } catch {}

  // The user agent's headless brand, where the engine reports it: the
  // launch's own flag fixes this in the browser, and this is the fallback
  // for a browser that was launched without it.
  try {
    if (navigator.userAgent.includes('HeadlessChrome')) {
      const userAgent = navigator.userAgent.replace('HeadlessChrome', 'Chrome');
      Object.defineProperty(Object.getPrototypeOf(navigator), 'userAgent', {
        get: () => userAgent,
        configurable: true,
      });
      const appVersion = navigator.appVersion.replace('HeadlessChrome', 'Chrome');
      Object.defineProperty(Object.getPrototypeOf(navigator), 'appVersion', {
        get: () => appVersion,
        configurable: true,
      });
    }
  } catch {}
})();
"#;

/// Where the driver's initialization script is written: named so a reader
/// finding it in the output directory knows it is forge's own.
fn mask_path(output_dir: &Path) -> PathBuf {
    output_dir.join("browser-mask.js")
}

/// Write the mask script and answer its path, or `None` where it could not
/// be written.
///
/// **Whole or not at all**: two profiles' drivers can start at once and both
/// write this one file, and a reader that caught a half-written script would
/// load half a mask - so the file is written to a temp name unique to this
/// process and renamed into place. Rewritten on every driver start, so a
/// running browser from an older client cannot hand the driver a stale mask.
///
/// **A write that fails is not a refusal.** The engine's own launch carries
/// the mask that matters; the script is the fallback, so a start without it
/// warns by name and goes on, exactly as a launch whose version could not be
/// read does.
fn write_mask(output_dir: &Path) -> Option<PathBuf> {
    let path = mask_path(output_dir);
    let temp = output_dir.join(format!("browser-mask.js.{}.tmp", std::process::id()));
    match std::fs::write(&temp, MASK_SCRIPT).and_then(|()| std::fs::rename(&temp, &path)) {
        Ok(()) => Some(path),
        Err(why) => {
            let _ = std::fs::remove_file(&temp);
            tauri_plugin_log::log::warn!(
                "the page-visible mask is not loaded this run (event_name browser_mask_script): \
                 the script could not be written to {}: {why}",
                path.display()
            );
            None
        }
    }
}

/// The command line one driver start runs: the vendored CLI, the browser's
/// own endpoint, and the mask - **an option the pinned driver already has**
/// (`--init-script`), so nothing vendored is patched. No mask, no argument.
fn driver_args(cli: &Path, endpoint: &str, output_dir: &Path, mask: Option<&Path>) -> Vec<String> {
    let mut args = vec![
        cli.display().to_string(),
        "--cdp-endpoint".to_owned(),
        endpoint.to_owned(),
        "--no-webmcp".to_owned(),
        "--allow-unrestricted-file-access".to_owned(),
        "--output-dir".to_owned(),
        output_dir.display().to_string(),
    ];
    if let Some(mask) = mask {
        args.push("--init-script".to_owned());
        args.push(mask.display().to_string());
    }
    args
}

/// The CLI inside the vendored package, run by the vendored node.
pub fn cli_path(stack: &Path) -> PathBuf {
    stack.join("playwright-mcp/node_modules/@playwright/mcp/cli.js")
}

/// The vendored node.
pub fn node_path(stack: &Path) -> PathBuf {
    stack.join("node/bin/node")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::CallToolResult;
    use serde_json::json;

    /// A text answer is its text, and a result flagged as an error is not a
    /// result at all: it is the call's reason, which the tool returns as a
    /// failure.
    #[test]
    fn a_text_answer_maps_to_text_and_an_error_result_carries_its_reason() {
        use rmcp::model::ContentBlock;

        let result = CallToolResult::success(vec![ContentBlock::text("navigated")]);
        assert_eq!(parts_of(&result), Ok(vec![ReplyPart::Text { text: "navigated".to_owned() }]));

        let failed = CallToolResult::error(vec![ContentBlock::text("no such element")]);
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(
            parts_of(&failed),
            Ok(vec![ReplyPart::Text { text: "no such element".to_owned() }]),
        );
    }

    /// An image crosses with its mime and its bytes base64 - the shape the
    /// frontend decodes into the binary frame the socket carries.
    #[test]
    fn an_image_answer_maps_to_an_image_part() {
        use rmcp::model::ContentBlock;

        let result = CallToolResult::success(vec![ContentBlock::image("AP8Q", "image/png")]);
        assert_eq!(
            parts_of(&result),
            Ok(vec![ReplyPart::Image {
                mime_type: "image/png".to_owned(),
                data_base64: "AP8Q".to_owned(),
            }]),
        );
    }

    /// A block nothing here can carry is an error naming it, not a part
    /// quietly left out: a dropped block is a result a person cannot see was
    /// ever there.
    #[test]
    fn a_block_this_host_cannot_carry_fails_the_call() {
        let result = CallToolResult::success(vec![rmcp::model::ContentBlock::Audio(
            rmcp::model::AudioContent::new("AP8Q", "audio/wav"),
        )]);
        let refused = parts_of(&result).expect_err("an audio block is not carried");
        assert!(refused.contains("cannot carry"), "{refused}");
    }

    /// The tab list parser, against the shapes the driver really prints -
    /// these are copied from measured answers, parentheses inside URLs
    /// included.
    #[test]
    fn a_tab_list_parses_to_index_and_url() {
        let text = "### Result\n- 0: (current) [](https://ui.forge.local/)\n- 1: [](https://browser.forge.local/)\n";
        assert_eq!(
            browser_tabs_of(text),
            vec![
                (0, "https://ui.forge.local/".to_owned()),
                (1, "https://browser.forge.local/".to_owned())
            ],
        );

        let with_error_page =
            "- 0: (current) [Webpage not available](chrome-error://chromewebdata/)";
        assert_eq!(
            browser_tabs_of(with_error_page),
            vec![(0, "chrome-error://chromewebdata/".to_owned())],
        );

        let data_page = "- 0: (current) [](data:text/html,<h1 id=h>Forge Probe</h1>)";
        assert_eq!(
            browser_tabs_of(data_page),
            vec![(0, "data:text/html,<h1 id=h>Forge Probe</h1>".to_owned())],
        );

        assert!(browser_tabs_of("no tabs here").is_empty());
    }

    /// The start's own empty-origin gate, pinned beside the tab refusals.
    #[test]
    fn a_start_with_no_origin_is_refused() {
        assert!(
            start_origin_refusal("").is_some(),
            "an empty origin would invert the pin and disarm the guard - the start must fail",
        );
        assert!(start_origin_refusal("http://tauri.localhost").is_none(), "a real origin starts",);
    }

    /// The phone's tab-shaped refusals, pinned where `cfg(android)` cannot
    /// hide them: the close door (both spellings), the select door, and the
    /// empty-origin contract - an empty origin must never fall through to
    /// "pick the first tab", which is the client's own page.
    #[test]
    fn a_tab_call_is_refused_where_the_phone_has_no_answer_for_it() {
        let ui = "http://tauri.localhost";
        let list =
            "- 0: (current) [](http://tauri.localhost/session/x)\n- 1: [](https://example.com/)";

        let closed = tab_call_refusal("browser_close", &json!({}), "", ui);
        assert!(closed.is_some(), "the MCP's close-page tool takes the engine's only page");

        let close_no_index =
            tab_call_refusal("browser_tabs", &json!({ "action": "close" }), "", ui);
        assert!(
            close_no_index.is_some(),
            "close with no index targets the CURRENT tab - the pinned browser page",
        );
        let close_index =
            tab_call_refusal("browser_tabs", &json!({ "action": "close", "index": 1 }), "", ui);
        assert!(close_index.is_some(), "and a close with an index is the same door");

        let onto_ui =
            tab_call_refusal("browser_tabs", &json!({ "action": "select", "index": 0 }), list, ui);
        assert!(
            onto_ui.as_deref().unwrap_or_default().contains("tab 0"),
            "selecting the client's own page is refused by name: {onto_ui:?}",
        );
        assert!(
            tab_call_refusal("browser_tabs", &json!({ "action": "select", "index": 1 }), list, ui)
                .is_none(),
            "a real browser page is selectable",
        );
        assert!(
            tab_call_refusal("browser_tabs", &json!({ "action": "select", "index": 1 }), list, "")
                .is_some(),
            "**an empty origin cannot tell any page apart - it must refuse, never fall through**",
        );
        assert!(
            tab_call_refusal("browser_tabs", &json!({ "action": "list" }), list, ui).is_none(),
            "listing is not a door",
        );
        assert!(
            tab_call_refusal("browser_navigate", &json!({ "url": "https://example.com/" }), "", ui)
                .is_none(),
            "and nothing else on the phone is gated here",
        );
    }

    /// Origin extraction and the classifier built on it: equality, not a
    /// prefix - `http://tauri.localhost.evil.test` IS a page a hostile site
    /// could hold, and a prefix check would read it as the client's own.
    #[test]
    fn an_origin_is_compared_by_equality() {
        assert_eq!(origin_of("http://tauri.localhost/session/x"), "http://tauri.localhost");
        assert_eq!(origin_of("https://tauri.localhost"), "https://tauri.localhost");
        assert_eq!(origin_of("http://10.0.2.2:8123/"), "http://10.0.2.2:8123");
        assert_eq!(origin_of("about:blank"), "about:blank");

        assert!(origin_match(
            "http://tauri.localhost/session/Scratch/android-spike/lead",
            "http://tauri.localhost",
        ));
        assert!(!origin_match("http://tauri.localhost.evil.test/x", "http://tauri.localhost"));
        assert!(!origin_match("https://tauri.localhost/x", "http://tauri.localhost"));
        assert!(!origin_match("about:blank", ""));
    }

    /// The driver's own paths are the vendoring's layout, pinned by name.
    #[test]
    fn the_driver_is_the_cli_the_vendoring_unpacks() {
        assert_eq!(node_path(Path::new("/stack")), Path::new("/stack/node/bin/node"));
        assert_eq!(
            cli_path(Path::new("/stack")),
            Path::new("/stack/playwright-mcp/node_modules/@playwright/mcp/cli.js"),
        );
    }

    /// **The mask rides the driver's own `--init-script` option** - the
    /// pinned driver's, so nothing vendored is forked or patched - and it
    /// names a real file, because the driver refuses a start whose
    /// init-script file is missing.
    #[test]
    fn the_driver_command_line_carries_the_mask_script() {
        let args = driver_args(
            Path::new("/stack/driver.js"),
            "http://127.0.0.1:9333",
            Path::new("/out"),
            Some(Path::new("/out/browser-mask.js")),
        );
        assert_eq!(args.first().map(String::as_str), Some("/stack/driver.js"), "{args:?}");
        let after =
            |flag: &str| args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone());
        assert_eq!(after("--cdp-endpoint").as_deref(), Some("http://127.0.0.1:9333"), "{args:?}");
        assert_eq!(
            after("--init-script").as_deref(),
            Some("/out/browser-mask.js"),
            "the mask is the driver's own initialization script: {args:?}",
        );
        assert!(args.contains(&"--no-webmcp".to_owned()), "{args:?}");
        assert!(args.contains(&"--allow-unrestricted-file-access".to_owned()), "{args:?}");

        let unmasked = driver_args(
            Path::new("/stack/driver.js"),
            "http://127.0.0.1:9333",
            Path::new("/out"),
            None,
        );
        assert!(
            !unmasked.contains(&"--init-script".to_owned()),
            "a start whose script could not be written goes on without the flag: {unmasked:?}",
        );
    }

    /// The script written for the driver masks the three tells a page can
    /// read in JavaScript, and each is conditional: the engine's own false,
    /// its own window.chrome and its own clean UA are left untouched, since
    /// a patch that always fires is a tell of its own.
    #[test]
    fn the_written_mask_script_masks_the_three_page_tells() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = write_mask(dir.path()).expect("the mask writes");
        assert_eq!(path, dir.path().join("browser-mask.js"));
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .expect("the dir lists")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(
            left,
            [std::ffi::OsString::from("browser-mask.js")],
            "the write lands whole under its own name, with no temp left behind",
        );
        let written = std::fs::read_to_string(&path).expect("the mask reads back");
        assert!(
            written.contains("if (navigator.webdriver)"),
            "the webdriver tell, masked only when it reads true: {written}",
        );
        assert!(
            written.contains("if (!window.chrome)"),
            "the chrome-object tell, shimmed only when missing: {written}",
        );
        assert!(
            written.contains("navigator.userAgent.includes('HeadlessChrome')"),
            "the user agent tell, scrubbed only where the brand is there: {written}",
        );
    }
}

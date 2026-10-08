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

/// The phone's bounds, its own, and they must fit the server's ask budget:
/// `ASK_TIMEOUT` (15+15+150+20 s) is derived from the DESKTOP's launch,
/// handshake and call bounds, and the phone's cold figures join it - 35 s to
/// accept a node's FIRST dial (its boot is real work: measured 8 s warm,
/// 39 s on a loaded emulator) + 10 s to hand shake + the driver's own 150 s
/// call = 195 s, inside the bound. A node that was ALREADY up redials every
/// second, so a later call waits only 6 s - a dead in-app node fails in
/// seconds with the reason instead of paying the cold window per call.
#[cfg(target_os = "android")]
const IN_APP_COLD_ACCEPT_TIMEOUT: Duration = Duration::from_secs(35);
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

        tauri_plugin_log::log::info!("starting the driver against {cdp_endpoint}");
        let transport =
            TokioChildProcess::new(tokio::process::Command::new(node).configure(|cmd| {
                cmd.arg(cli);
                cmd.arg("--cdp-endpoint")
                    .arg(cdp_endpoint)
                    .arg("--no-webmcp")
                    // **No gate on what a session opens or reads, and a cwd
                    // that is pinned.** Upstream resolves a relative file path
                    // against `process.cwd()` and refuses a path outside its
                    // roots, and this child inherits whatever directory the
                    // CLIENT was launched from - so a `file_upload` worked
                    // from Finder and failed from a shell. forge has no gates
                    // here (the trust bound is the profile), so the flag
                    // removes the root check outright and the cwd is pinned
                    // under the app's data directory rather than left to the
                    // launch directory.
                    .arg("--allow-unrestricted-file-access")
                    .current_dir(output_dir)
                    // Where upstream's own screenshot default lands: the
                    // client runs from a directory nobody chose, and a file
                    // dropped there is one nobody finds.
                    .arg("--output-dir")
                    .arg(output_dir);
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
                format!(
                    "the on-device driver did not dial its socket within {} s",
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
        let origin = if ui_origin.is_empty() { relay.ui_origin.clone() } else { ui_origin.to_owned() };
        let driver = Self { _service: service, client, ui_origin: origin };
        driver.pin_browser_tab().await?;
        Ok(driver)
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
        let Some((index, url)) = tabs
            .iter()
            .find(|(_, url)| !origin_match(url, self.ui_origin.as_str()))
            .cloned()
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
    /// outright - `close` with no index closes the CURRENT tab (which after
    /// the pin IS the browser page), the pinned driver re-points its current
    /// tab at whatever remains (the client's UI page), and every later call
    /// would drive forge's own screen. `browser_close` is the same harm
    /// behind the MCP's own close-page tool, and no relaunch exists here to
    /// reopen what it closed.
    #[cfg(target_os = "android")]
    async fn refuse_a_client_page(&self, tool: &str, args: &Value) -> Result<(), String> {
        if tool == "browser_close" {
            return Err(
                "the phone's browser page is the app's own screen and is never closed: \
                 navigate it away instead (browser_navigate), or leave it where it is"
                    .to_owned(),
            );
        }
        if tool != "browser_tabs" {
            return Ok(());
        }
        let action = args.get("action").and_then(Value::as_str).unwrap_or_default();
        if action == "close" {
            return Err(
                "the phone hosts one browser page and closing it takes the engine's only \
                 page: navigate it away instead (browser_navigate)"
                    .to_owned(),
            );
        }
        if action != "select" {
            return Ok(());
        }
        let Some(index) = args.get("index").and_then(Value::as_u64) else {
            return Ok(());
        };
        let listed = self.call_raw("browser_tabs", serde_json::json!({ "action": "list" })).await?;
        let text = listed
            .iter()
            .map(|part| match part {
                ReplyPart::Text { text } => text.as_str(),
                ReplyPart::Image { .. } => "[an image]",
            })
            .collect::<Vec<_>>()
            .join("\n");
        for (candidate, url) in browser_tabs_of(&text) {
            if candidate == index as usize && origin_match(&url, self.ui_origin.as_str()) {
                return Err(format!(
                    "tab {index} is this client's own screen, not a browser page: the browser \
                     tools drive the shared browser only"
                ));
            }
        }
        Ok(())
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
            vec![(0, "https://ui.forge.local/".to_owned()), (1, "https://browser.forge.local/".to_owned())],
        );

        let with_error_page = "- 0: (current) [Webpage not available](chrome-error://chromewebdata/)";
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
}

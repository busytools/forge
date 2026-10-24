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
use std::sync::PoisonError;
use std::time::Duration;

use base64::Engine as _;
use forge_primitives::browser::BrowserPart;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt as _, TokioChildProcess};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use tokio::net::UnixListener;
use tokio::sync::broadcast;

use crate::spawn;

/// The longest one tool call is given before the driver is presumed mute.
///
/// **A request timeout, because rmcp carries none by default.** The bound
/// sits above upstream's own (a 60 s navigation, a 30 s wait) with room: it
/// is the wedge-breaker, not a deadline the tools keep.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(150);

/// How long one discovered tab's arm is waited for.
const ARM_BOUND: Duration = Duration::from_secs(3);

/// How long the created tab's target is looked for in `/json/list`.
const DISCOVER_BOUND: Duration = Duration::from_secs(1);

/// **The longest a driver start is given before it is presumed wedged.**
/// Measured live 2026-10-07: a named profile's driver child spawned and sat
/// idle on its stdin while the parent never wrote the handshake - and the
/// start has no bound of its own, so the session's call parked with it, past
/// even [`CALL_TIMEOUT`]. A start that cannot answer names it instead.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// What is left of a call's budget - at least enough to send the call, so a
/// spent budget answers a timeout rather than a silent skip.
fn left(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now()).max(Duration::from_millis(1))
}

/// A page's file chooser, as the driver saw it open.
///
/// **The driver owns the chooser now**: Playwright intercepts it for every
/// tab, so CEF's own dialog never fires while the driver is attached. The
/// server decides who answers - the controlling person through the client's
/// picker, or an agent's `browser_file_upload` - and the answer goes back
/// through the same MCP tool that owns the captured chooser.
#[derive(Clone, Debug, Deserialize)]
pub struct FileChooser {
    pub multiple: bool,
    /// Whether the tab that raised it is the current one. The MCP's upload
    /// answers the current tab's chooser, so a background tab's ask cannot be
    /// answered there and is never put to a person.
    pub current: bool,
    #[serde(default)]
    pub url: String,
}

/// The driver child, and the MCP client speaking to it.
pub struct Driver {
    /// Kept so the child is killed when the driver goes: a relaunch must not
    /// leave the old one holding the browser.
    _service: RunningService<rmcp::RoleClient, ()>,
    client: rmcp::Peer<rmcp::RoleClient>,
    /// The host's DevTools port and the profile it serves: the endpoint
    /// names the port, and a blank tab is held from navigation until the
    /// installer has armed that very target.
    port: Option<u16>,
    profile: Option<PathBuf>,
    /// The target of the last tab opened blank. A navigation that follows
    /// the creation in a separate call still waits for that tab's arm.
    opened_blank: std::sync::Mutex<Option<String>>,
    /// Every chooser the patched MCP surfaced, for the profile's own task.
    choosers: broadcast::Sender<FileChooser>,
    chooser_task: tokio::task::JoinHandle<()>,
    _chooser_socket: ChooserSocket,
}

/// The socket file a driver owns; **dropping it removes the file**, so a
/// replaced or ended driver leaves no socket a later page could reach.
struct ChooserSocket {
    path: PathBuf,
}

impl Drop for ChooserSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
        chooser_socket: &Path,
    ) -> Result<Self, String> {
        Self::start_within(node, cli, cdp_endpoint, output_dir, chooser_socket, START_TIMEOUT).await
    }

    /// [`Self::start`] with the handshake's budget given. **A seam for
    /// tests**: a test that spawns a real child must not race the production
    /// wall-clock constant for a reason unrelated to what it checks.
    pub async fn start_within(
        node: &Path,
        cli: &Path,
        cdp_endpoint: &str,
        output_dir: &Path,
        chooser_socket: &Path,
        handshake: Duration,
    ) -> Result<Self, String> {
        if !node.is_file() || !cli.is_file() {
            return Err(format!(
                "the vendored driver is not there ({} / {}) - run \
                 scripts/vendor_browser_stack.sh",
                node.display(),
                cli.display(),
            ));
        }
        std::fs::create_dir_all(output_dir)
            .map_err(|why| format!("the browser output directory cannot be made: {why}"))?;

        tracing::info!("starting the driver against {cdp_endpoint}");
        let transport =
            TokioChildProcess::new(tokio::process::Command::new(node).configure(|cmd| {
                // **No gate on what a session opens or reads, and a cwd that
                // is pinned.** Upstream resolves a relative file path against
                // `process.cwd()` and refuses a path outside its roots, and
                // this child would otherwise inherit whatever directory the
                // server was launched from - so a `file_upload` would work
                // from one launch and fail from another. forge has no gates
                // here (the trust bound is the profile), so the flag removes
                // the root check outright and the cwd is pinned under the
                // app's data directory rather than left to the launch
                // directory. The `--output-dir` is where upstream's own
                // screenshot default lands.
                for arg in driver_args(cli, cdp_endpoint, output_dir) {
                    cmd.arg(arg);
                }
                cmd.current_dir(output_dir);
                cmd.env("FORGE_FILE_CHOOSER_SOCKET", chooser_socket);
            }))
            .map_err(|why| format!("the driver would not start: {why}"))?;

        // **The handshake is bounded**: a child that spawns and never answers
        // parks the session exactly as a wedge does, with nothing to read.
        let service = tokio::time::timeout(handshake, ().serve(transport))
            .await
            .map_err(|_| {
                format!(
                    "the driver did not answer its MCP handshake within {} s",
                    handshake.as_secs()
                )
            })?
            .map_err(|why| format!("the driver did not answer its MCP handshake: {why}"))?;
        tracing::info!("the driver answers on {cdp_endpoint}");
        // **The chooser socket is bound once the child is up**, before any
        // tool can drive a page: the MCP only connects to it when a page
        // raises a chooser, and the handshake never clicks.
        let listener = bind_chooser_socket(chooser_socket)?;
        let (choosers, _) = broadcast::channel(8);
        let chooser_task = tokio::spawn(serve_choosers(listener, choosers.clone()));
        let client = service.peer().clone();
        // **The MCP's backend is built on its first TOOL CALL, not on the
        // handshake.** Until one runs, no tab listener exists, Playwright has
        // not enabled file chooser interception, and a person's click falls
        // through to CEF's own dialog - the path the unified design replaces.
        // A tab listing is the lightest call that builds the backend, so the
        // driver is already watching for a chooser before the first person
        // touches the page after a restart.
        let mut warm_args = serde_json::Map::new();
        warm_args.insert("action".to_owned(), serde_json::Value::String("list".to_owned()));
        let warm = client.call_tool(
            CallToolRequestParams::new("browser_tabs".to_owned()).with_arguments(warm_args),
        );
        match tokio::time::timeout(Duration::from_secs(10), warm).await {
            Ok(Ok(_)) => {}
            Ok(Err(why)) => {
                tracing::debug!(
                    "the driver's first tab listing failed (event_name browser_driver_warm_failed): \
                     {why}"
                );
            }
            Err(_) => {
                tracing::debug!(
                    "the driver's first tab listing did not answer (event_name \
                     browser_driver_warm_timeout)"
                );
            }
        }
        Ok(Self {
            _service: service,
            client,
            port: endpoint_port(cdp_endpoint),
            profile: None,
            opened_blank: std::sync::Mutex::new(None),
            choosers,
            chooser_task,
            _chooser_socket: ChooserSocket { path: chooser_socket.to_path_buf() },
        })
    }

    /// Every file chooser the patched MCP has surfaced for this driver.
    pub fn file_choosers(&self) -> broadcast::Receiver<FileChooser> {
        self.choosers.subscribe()
    }

    /// The profile this driver serves, named in its own warnings.
    pub fn serving(mut self, profile: &Path) -> Self {
        self.profile = Some(profile.to_path_buf());
        self
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
    pub async fn call(&self, tool: &str, args: Value) -> Result<Vec<BrowserPart>, String> {
        self.call_within(tool, args, CALL_TIMEOUT).await
    }

    /// One call under a bound of its own, for the calls that probe after a
    /// failure and must leave that probe room inside the driver's own bound.
    pub async fn call_within(
        &self,
        tool: &str,
        args: Value,
        bound: Duration,
    ) -> Result<Vec<BrowserPart>, String> {
        if let Some(port) = self.port {
            // **The recorded blank belongs to the tab it was created for.** A
            // select, a new tab or a close changes which tab a later
            // navigation means, so the record goes before any of those is
            // handled - a `new` that opens blank again records its own; a
            // listing is a read and leaves the record alone.
            let keeps_it = tool == "browser_tabs"
                && args.get("action").and_then(Value::as_str) == Some("list");
            if !keeps_it && (tool == "browser_tabs" || tool == "browser_close") {
                *self.opened_blank.lock().unwrap_or_else(PoisonError::into_inner) = None;
            }
            if let Some(url) = blank_first_url(tool, &args) {
                return self.blank_then_navigate(port, tool, &args, url, bound).await;
            }
            if is_blank_tab_open(tool, &args) {
                return self.blank_and_remember(port, tool, args, bound).await;
            }
            if tool == "browser_navigate" {
                // **A navigation that follows a blank creation still waits**
                // for that tab's arm: the hole is one call wide, and this is
                // the call.
                if let Some(blank) = self.take_opened_blank() {
                    wait_target_armed(port, &blank, self.profile.as_deref(), ARM_BOUND).await;
                }
            }
        }
        self.call_raw(tool, args, bound).await
    }

    /// **A tab opened at a URL opens blank first.** The engine answers no
    /// page-domain command while a tab is paused, so a document that starts
    /// with the tab itself runs before the veto can register; opening it
    /// blank, waiting for the installer to arm THAT target, and only then
    /// navigating puts the veto in front of the first real document.
    async fn blank_then_navigate(
        &self,
        port: u16,
        tool: &str,
        args: &Value,
        url: String,
        bound: Duration,
    ) -> Result<Vec<BrowserPart>, String> {
        // **The whole rewrite keeps the one call's budget.** Its pieces share
        // a deadline, so the pair cannot spend two call timeouts plus the
        // waits inside a surface whose own ask budget is one call.
        let deadline = tokio::time::Instant::now() + bound;
        let before = blank_snapshot(port).await;
        let mut blank = args.clone();
        if let Value::Object(fields) = &mut blank {
            fields.remove("url");
        }
        let answer = self.call_raw(tool, blank, left(deadline)).await?;
        let target = created_blank(port, &before, left(deadline).min(DISCOVER_BOUND)).await;
        if let Some(target) = &target {
            wait_target_armed(port, target, self.profile.as_deref(), left(deadline).min(ARM_BOUND))
                .await;
        } else {
            self.unidentified(port);
        }
        if let Err(why) = self
            .call_raw("browser_navigate", serde_json::json!({ "url": url }), left(deadline))
            .await
        {
            // **The blank tab it made is closed again**, so a failed call
            // leaves nothing behind and a retry does not open a second one.
            if let Some(target) = &target {
                spawn::close_target(port, target).await;
            }
            return Err(why);
        }
        // **The reply is the tabs as they stand after the navigation**, not
        // the blank creation's own listing.
        match self
            .call_raw("browser_tabs", serde_json::json!({ "action": "list" }), left(deadline))
            .await
        {
            Ok(listing) => Ok(listing),
            Err(why) => {
                // **A failed refresh is said, not hidden behind the creation
                // answer**: the tab exists, and the missing listing is the
                // truth about this call.
                tracing::warn!(
                    "the tab list could not be refreshed after a navigation (event_name \
                     cef_passkey_tab_list_failed, port {port}, profile {}): {why}",
                    self.profile
                        .as_deref()
                        .map_or_else(|| "-".to_owned(), |profile| profile.display().to_string()),
                );
                let mut answer = answer;
                answer.push(BrowserPart::Text {
                    text: format!("the tab list could not be refreshed: {why}"),
                });
                Ok(answer)
            }
        }
    }

    /// A tab opened with no URL: record the target it created, so a later
    /// `browser_navigate` can wait for that tab's arm.
    async fn blank_and_remember(
        &self,
        port: u16,
        tool: &str,
        args: Value,
        bound: Duration,
    ) -> Result<Vec<BrowserPart>, String> {
        let deadline = tokio::time::Instant::now() + bound;
        let before = blank_snapshot(port).await;
        let answer = self.call_raw(tool, args, left(deadline)).await?;
        match created_blank(port, &before, left(deadline).min(DISCOVER_BOUND)).await {
            Some(target) => {
                *self.opened_blank.lock().unwrap_or_else(PoisonError::into_inner) = Some(target);
            }
            None => self.unidentified(port),
        }
        Ok(answer)
    }

    /// **A tab whose target never showed in `/json/list` is named**, not
    /// navigated silently: the wait is skipped, and the skip is the record.
    fn unidentified(&self, port: u16) {
        tracing::warn!(
            "a tab opened blank could not be identified before its navigation (event_name \
             cef_passkey_tab_unidentified, port {port}, profile {})",
            self.profile
                .as_deref()
                .map_or_else(|| "-".to_owned(), |profile| profile.display().to_string()),
        );
    }

    fn take_opened_blank(&self) -> Option<String> {
        self.opened_blank.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// The dispatch itself, unguarded: the guard's own tab list runs through
    /// this, or the guard would recurse into itself.
    async fn call_raw(
        &self,
        tool: &str,
        args: Value,
        bound: Duration,
    ) -> Result<Vec<BrowserPart>, String> {
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
            bound,
            self.client
                .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments)),
        )
        .await
        .map_err(|_| did_not_answer(tool, bound))?
        .map_err(|why| format!("the driver did not answer {tool}: {why}"))?;

        let parts = parts_of(&result)?;
        if result.is_error == Some(true) {
            // Upstream reports a failed call as an error result whose content
            // is the reason; the answer's failure arm carries the words.
            let reason = parts
                .iter()
                .map(|part| match part {
                    BrowserPart::Text { text } => text.clone(),
                    BrowserPart::Image { .. } => "[an image]".to_owned(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Err(if reason.is_empty() { format!("{tool} failed") } else { reason });
        }
        Ok(parts)
    }
}

/// The sentence a call that ran past its bound reports: one builder, so a
/// probe can tell this failure from a tool's own and not by matching prose it
/// would have to keep in step.
pub fn did_not_answer(tool: &str, bound: Duration) -> String {
    format!("the driver did not answer {tool} within {} s", bound.as_secs())
}

/// Whether a failure is the driver never getting attached at all.
///
/// **The MCP handshake says nothing about the browser.** Upstream creates
/// its backend lazily on the first `tools/call`, and `connectOverCDP` only
/// then opens the browser websocket and waits for its protocol handshake -
/// with Playwright's own 30 s default and no words from the host. Measured
/// 2026-10-23 against a host whose page was wedged: `<ws connected>`, then
/// `TimeoutError: async initializeServer: Timeout 30000ms exceeded`
/// (#2534). A ws that never connects at all reports the other prefix; both
/// are the attach, not the tool.
pub fn attach_timed_out(why: &str) -> bool {
    (why.contains("initializeServer: Timeout")
        || why.contains("browserType.connectOverCDP: Timeout"))
        && why.contains("ms exceeded")
}

/// The driver's own words for a page whose renderer is gone: each wrapping
/// prefix with the reading together, so a page script's own TypeError that
/// carries the same reading is not mistaken for it.
///
/// **Both measured prefixes are here.** `browserBackend.callTool` is a tool
/// on the page; `browserContext.newPage` is a tab being created, which dies
/// the same way and carries the same reading (#2534, 2026-10-22).
const DEAD_PAGE: [&str; 2] = [
    "browserBackend.callTool: Cannot read properties of undefined (reading '_page')",
    "browserContext.newPage: Cannot read properties of undefined (reading '_page')",
];

/// Whether a failure is the driver's held page outliving its renderer.
pub fn stale_page(why: &str) -> bool {
    DEAD_PAGE.iter().any(|line| why.contains(line))
}

/// Map one MCP result onto the parts the socket carries.
///
/// **A block this host cannot carry is an error rather than a silence.** The
/// tools answer with text and images; a resource or an audio block is
/// something no view could draw and no rule allows dropping, so the call
/// fails naming it.
fn parts_of(result: &rmcp::model::CallToolResult) -> Result<Vec<BrowserPart>, String> {
    use rmcp::model::ContentBlock;

    let mut parts = Vec::with_capacity(result.content.len());
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => {
                parts.push(BrowserPart::Text { text: text.text.clone() });
            }
            ContentBlock::Image(image) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .map_err(|why| format!("an image answer did not decode: {why}"))?;
                parts.push(BrowserPart::Image { mime_type: image.mime_type.clone(), bytes });
            }
            other => {
                return Err(format!(
                    "the driver answered with a block this host cannot carry: {other:?}"
                ));
            }
        }
    }
    Ok(parts)
}

/// The URL a `browser_tabs new` call names, when it names one. **The tab is
/// opened without it**, so the veto can be registered before the document
/// the URL starts exists.
pub fn blank_first_url(tool: &str, args: &Value) -> Option<String> {
    if tool != "browser_tabs" || args.get("action").and_then(Value::as_str) != Some("new") {
        return None;
    }
    args.get("url").and_then(Value::as_str).filter(|url| !url.is_empty()).map(str::to_owned)
}

/// Whether the call opens a tab with no URL, which lands on the blank
/// document the installer arms by itself.
pub fn is_blank_tab_open(tool: &str, args: &Value) -> bool {
    tool == "browser_tabs"
        && args.get("action").and_then(Value::as_str) == Some("new")
        && args.get("url").and_then(Value::as_str).is_none_or(str::is_empty)
}

/// The port in a host endpoint like `http://127.0.0.1:9411`.
pub fn endpoint_port(endpoint: &str) -> Option<u16> {
    endpoint.strip_prefix("http://127.0.0.1:")?.trim_end_matches('/').parse().ok()
}

/// The page targets on the host before a creation, so the one it made can be
/// told from the ones that were already there.
async fn blank_snapshot(port: u16) -> std::collections::HashSet<String> {
    spawn::page_targets(port).await.into_iter().map(|(id, _)| id).collect()
}

/// The about:blank target a creation added, found by diffing the target list
/// against the snapshot. Bounded: a creation that never shows a blank target
/// answers `None`, and the call proceeds unwaited rather than hanging.
async fn created_blank(
    port: u16,
    before: &std::collections::HashSet<String>,
    budget: Duration,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        for (id, url) in spawn::page_targets(port).await {
            if url == "about:blank" && !before.contains(&id) {
                return Some(id);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait, bounded, for the installer to arm ONE target. **Bounded, not a
/// guarantee**: a tab the installer could not arm still navigates, with the
/// miss named by port and profile.
async fn wait_target_armed(port: u16, target: &str, profile: Option<&Path>, budget: Duration) {
    let deadline = tokio::time::Instant::now() + budget;
    while !crate::veto::target_armed(port, target) {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                "a tab opened blank was not armed before its navigation (event_name \
                 cef_passkey_veto_unarmed_tab, port {port}, profile {})",
                profile.map_or_else(|| "-".to_owned(), |profile| profile.display().to_string()),
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The tabs a `browser_tabs` list answer names, as (index, url) pairs. The
/// driver draws them as `- 0: (current) [title](url)` lines; the pin only
/// needs the index and the page it points at.
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

/// Bind the socket the patched MCP surfaces its file choosers on.
///
/// **A stale socket from a driver that died is removed first**: a Unix
/// socket is a file, and `bind` refuses one that already exists.
fn bind_chooser_socket(path: &Path) -> Result<UnixListener, String> {
    use std::os::unix::fs::PermissionsExt as _;

    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|why| {
        format!("the file chooser socket {} cannot be bound: {why}", path.display())
    })?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

/// One connection is one chooser: the patched MCP writes a JSON line and
/// closes, and the profile's task decides who answers it.
async fn serve_choosers(listener: UnixListener, choosers: broadcast::Sender<FileChooser>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let choosers = choosers.clone();
        tokio::spawn(async move {
            // **One chooser is one short line**: a writer cannot make the
            // driver allocate an unbounded string, and the patched MCP's
            // line (a mode, the tab's current flag and a 512-byte URL) is
            // far under this.
            let mut lines = BufReader::new(stream.take(4096)).lines();
            let Ok(Some(line)) = lines.next_line().await else { return };
            match serde_json::from_str::<FileChooser>(&line) {
                Ok(chooser) => {
                    let _ = choosers.send(chooser);
                }
                Err(why) => {
                    tracing::debug!(
                        "a file chooser line could not be read (event_name \
                         browser_chooser_unreadable): {why}"
                    );
                }
            }
        });
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.chooser_task.abort();
    }
}

/// The command line one driver start runs: the vendored CLI, the browser's
/// own endpoint, the output directory. No mask rides along - every launch is
/// the person's own real browser, which presents its own UA, its own client
/// hints and its own `window.chrome`, so there is nothing left for a script
/// to patch.
fn driver_args(cli: &Path, endpoint: &str, output_dir: &Path) -> Vec<String> {
    vec![
        cli.display().to_string(),
        "--cdp-endpoint".to_owned(),
        endpoint.to_owned(),
        "--no-webmcp".to_owned(),
        "--allow-unrestricted-file-access".to_owned(),
        "--output-dir".to_owned(),
        output_dir.display().to_string(),
    ]
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

    /// **An attach that never finished is the host's failure, not the
    /// tool's.** The measured line from #2534 is this class; a page's own
    /// timeout, or a tool's `Timeout 30000ms exceeded`, is not.
    #[test]
    fn the_drivers_attach_timeout_is_recognised_by_its_own_line() {
        let measured = "### Error\nTimeoutError: async initializeServer: Timeout 30000ms exceeded.\n\
                        Call log:\n  - <ws preparing> retrieving websocket url from \
                        http://127.0.0.1:9413\n  - <ws connecting> \
                        ws://127.0.0.1:9413/devtools/browser/88b56812\n  - <ws connected> \
                        ws://127.0.0.1:9413/devtools/browser/88b56812";
        assert!(attach_timed_out(measured), "the measured initializeServer timeout is the attach");
        assert!(
            attach_timed_out(
                "browserType.connectOverCDP: Timeout 30000ms exceeded.\nCall log:\n  - <ws \
                 preparing> retrieving websocket url from http://127.0.0.1:9413"
            ),
            "a browser websocket that never connected is the attach too",
        );
        assert!(
            !attach_timed_out("### Error\nTimeoutError: page.click: Timeout 30000ms exceeded."),
            "a tool's own timeout is not the attach",
        );
        assert!(
            !attach_timed_out(
                "### Error\nTimeoutError: async initializeServer: the browser is gone"
            ),
            "a different initializeServer failure without the timeout is not this class",
        );
    }

    /// **Both measured dead-page prefixes are recognised**, and a page
    /// script's own TypeError carrying the same reading is not.
    #[test]
    fn a_dead_page_context_is_recognised_by_the_drivers_own_prefixes() {
        assert!(stale_page(
            "### Error TypeError: browserBackend.callTool: Cannot read properties of undefined \
             (reading '_page')"
        ));
        assert!(
            stale_page(
                "### Error\nTypeError: browserContext.newPage: Cannot read properties of \
                 undefined (reading '_page')"
            ),
            "a tab creation dying is the same dead page, measured 2026-10-22",
        );
        assert!(
            !stale_page(
                "page.evaluate: TypeError: Cannot read properties of undefined (reading '_page')"
            ),
            "a page's own TypeError is not the driver's dead context",
        );
    }

    /// A text answer is its text, and a result flagged as an error is not a
    /// result at all: it is the call's reason, which the tool returns as a
    /// failure.
    #[test]
    fn a_text_answer_maps_to_text_and_an_error_result_carries_its_reason() {
        use rmcp::model::ContentBlock;

        let result = CallToolResult::success(vec![ContentBlock::text("navigated")]);
        assert_eq!(parts_of(&result), Ok(vec![BrowserPart::Text { text: "navigated".to_owned() }]),);

        let failed = CallToolResult::error(vec![ContentBlock::text("no such element")]);
        assert_eq!(failed.is_error, Some(true));
        assert_eq!(
            parts_of(&failed),
            Ok(vec![BrowserPart::Text { text: "no such element".to_owned() }]),
        );
    }

    /// An image crosses with its mime and its decoded bytes - the shape the
    /// socket's own image frame carries.
    #[test]
    fn an_image_answer_maps_to_an_image_part() {
        use rmcp::model::ContentBlock;

        let result = CallToolResult::success(vec![ContentBlock::image("AP8Q", "image/png")]);
        assert_eq!(
            parts_of(&result),
            Ok(vec![BrowserPart::Image {
                mime_type: "image/png".to_owned(),
                bytes: vec![0, 255, 16],
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

    /// **Only a new-tab call with a URL is rewritten.** A list, a select, or
    /// a new tab with no URL keeps its single call, and navigation is not a
    /// tab call at all.
    #[test]
    fn only_a_new_tab_with_a_url_opens_blank_first() {
        let with_url = json!({ "action": "new", "url": "https://example.com/job" });
        assert_eq!(
            blank_first_url("browser_tabs", &with_url),
            Some("https://example.com/job".to_owned())
        );
        assert_eq!(blank_first_url("browser_tabs", &json!({ "action": "new" })), None);
        assert_eq!(blank_first_url("browser_tabs", &json!({ "action": "new", "url": "" })), None);
        assert_eq!(blank_first_url("browser_tabs", &json!({ "action": "list" })), None);
        assert_eq!(
            blank_first_url("browser_navigate", &json!({ "url": "https://example.com" })),
            None,
        );
        assert!(is_blank_tab_open("browser_tabs", &json!({ "action": "new" })));
        assert!(!is_blank_tab_open("browser_tabs", &with_url));
    }

    #[test]
    fn a_host_endpoint_names_its_port() {
        assert_eq!(endpoint_port("http://127.0.0.1:9411"), Some(9411));
        assert_eq!(endpoint_port("http://127.0.0.1:9411/"), Some(9411));
        assert_eq!(endpoint_port("https://example.com"), None);
        assert_eq!(endpoint_port("http://127.0.0.1:notaport"), None);
    }

    /// Origin extraction: equality, not a prefix - a lookalike host is not
    /// the page it copies.
    #[test]
    fn an_origin_is_compared_by_equality() {
        assert_eq!(origin_of("http://tauri.localhost/session/x"), "http://tauri.localhost");
        assert_eq!(origin_of("https://tauri.localhost"), "https://tauri.localhost");
        assert_eq!(origin_of("http://10.0.2.2:8123/"), "http://10.0.2.2:8123");
        assert_eq!(origin_of("about:blank"), "about:blank");
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

    /// The command line: the vendored CLI, the browser's endpoint, the
    /// output directory - and nothing else. No init-script: every launch is
    /// the person's own real browser, so there is no mask to carry.
    #[test]
    fn the_driver_command_line_is_the_cli_the_endpoint_and_the_output_dir() {
        let args =
            driver_args(Path::new("/stack/driver.js"), "http://127.0.0.1:9333", Path::new("/out"));
        assert_eq!(args.first().map(String::as_str), Some("/stack/driver.js"), "{args:?}");
        let after =
            |flag: &str| args.windows(2).find(|pair| pair[0] == flag).map(|pair| pair[1].clone());
        assert_eq!(after("--cdp-endpoint").as_deref(), Some("http://127.0.0.1:9333"), "{args:?}");
        assert_eq!(after("--output-dir").as_deref(), Some("/out"), "{args:?}");
        assert!(!args.contains(&"--init-script".to_owned()), "no mask rides the driver: {args:?}");
        assert!(args.contains(&"--no-webmcp".to_owned()), "{args:?}");
        assert!(args.contains(&"--allow-unrestricted-file-access".to_owned()), "{args:?}");
    }

    /// **The patched MCP surfaces a chooser on the driver's socket**: one
    /// line per connection, read into the event the profile answers. A line
    /// that is not a chooser is dropped rather than read as one.
    #[tokio::test]
    async fn a_chooser_line_reaches_the_driver_and_a_bad_one_does_not() {
        use tokio::io::AsyncWriteExt as _;

        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("file-chooser.sock");
        let listener = bind_chooser_socket(&path).expect("the socket binds");
        let (events, mut choosers) = broadcast::channel(4);
        let task = tokio::spawn(serve_choosers(listener, events));

        let mut bad = tokio::net::UnixStream::connect(&path).await.expect("connect");
        bad.write_all(b"not a chooser\n").await.expect("write the bad line");
        drop(bad);
        let mut good = tokio::net::UnixStream::connect(&path).await.expect("connect");
        good.write_all(
            b"{\"multiple\":true,\"current\":true,\"url\":\"http://127.0.0.1:8799/\"}\n",
        )
        .await
        .expect("write the chooser");
        drop(good);

        let chooser = tokio::time::timeout(Duration::from_secs(2), choosers.recv())
            .await
            .expect("the chooser arrives")
            .expect("the channel feeds");
        assert!(chooser.multiple, "the mode is the page's");
        assert_eq!(chooser.url, "http://127.0.0.1:8799/");
        task.abort();
    }

    /// **A driver's socket file goes with it**, so a page cannot reach a
    /// stale listener after the driver was replaced.
    #[tokio::test]
    async fn a_chooser_socket_is_removed_when_its_owner_goes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("chooser.sock");
        let listener = bind_chooser_socket(&path).expect("the socket binds");
        {
            let _owner = ChooserSocket { path: path.clone() };
            assert!(path.exists(), "the socket is there while its owner is");
        }
        assert!(!path.exists(), "and gone when the owner drops");
        drop(listener);
    }
}

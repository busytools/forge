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

use crate::tabs::DriverTab;

/// The whole budget one call is given before the driver is presumed mute.
///
/// **A request timeout, because rmcp carries none by default, and one budget
/// per call**: retries and re-listings spend what is left rather than a fresh
/// bound each, so a mute call answers by name inside it. The two sides set
/// the number: upstream's worst legitimate call is a click that navigates,
/// about 80 s (a 5 s action, a 60 s navigation, a 10 s load and the settle),
/// and nextest terminates a test at 120 s, which a wedged call also spends a
/// ~15 s host boot inside; 90 sits 10 s above the first and 15 s under the
/// second.
///
/// **`browser_evaluate` and `browser_run_code_unsafe` await arbitrary code
/// and set no timeout of their own**, so this is their only ceiling: a script
/// that runs past it is killed by the same budget rather than left running.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(90);

/// **The longest a driver start is given before it is presumed wedged.**
/// Measured live 2026-10-07: a named profile's driver child spawned and sat
/// idle on its stdin while the parent never wrote the handshake - and the
/// start has no bound of its own, so the session's call parked with it, past
/// even [`CALL_TIMEOUT`]. A start that cannot answer names it instead.
pub(super) const START_TIMEOUT: Duration = Duration::from_secs(15);

/// The driver's surface the call path drives, so a service test can put a
/// fake in the profile's driver slot without a child process.
#[async_trait::async_trait]
pub trait TabDriver: Send + Sync {
    /// The driver's tabs: each page's CDP target id and its row.
    async fn tabs_within(&self, bound: Duration) -> Result<Vec<DriverTab>, String>;

    /// Bring one tab to the front by its CDP target id.
    async fn select(&self, target: &str, bound: Duration) -> Result<(), String>;

    /// Run one tool under a bound of its own.
    async fn call_within(
        &self,
        tool: &str,
        args: Value,
        bound: Duration,
    ) -> Result<Vec<BrowserPart>, String>;

    /// Whether the child is still there to answer.
    fn is_running(&self) -> bool;
}

#[async_trait::async_trait]
impl TabDriver for Driver {
    async fn tabs_within(&self, bound: Duration) -> Result<Vec<DriverTab>, String> {
        Driver::tabs_within(self, bound).await
    }

    async fn select(&self, target: &str, bound: Duration) -> Result<(), String> {
        Driver::select(self, target, bound).await
    }

    async fn call_within(
        &self,
        tool: &str,
        args: Value,
        bound: Duration,
    ) -> Result<Vec<BrowserPart>, String> {
        Driver::call_within(self, tool, args, bound).await
    }

    fn is_running(&self) -> bool {
        Driver::is_running(self)
    }
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
            choosers,
            chooser_task,
            _chooser_socket: ChooserSocket { path: chooser_socket.to_path_buf() },
        })
    }

    /// Every file chooser the patched MCP has surfaced for this driver.
    pub fn file_choosers(&self) -> broadcast::Receiver<FileChooser> {
        self.choosers.subscribe()
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
        self.call_raw(tool, args, bound).await
    }

    /// The driver's tabs under the call budget.
    pub async fn tabs(&self) -> Result<Vec<DriverTab>, String> {
        self.tabs_within(CALL_TIMEOUT).await
    }

    /// The driver's tabs: each page's guid, its live target id and its row.
    pub async fn tabs_within(&self, bound: Duration) -> Result<Vec<DriverTab>, String> {
        let parts =
            self.call_raw("browser_tabs", serde_json::json!({ "action": "list" }), bound).await?;
        let text = parts
            .iter()
            .filter_map(|part| match part {
                BrowserPart::Text { text } => Some(text.as_str()),
                BrowserPart::Image { .. } => None,
            })
            .collect::<String>();
        let tabs = crate::tabs::parse_export(&text)?;
        Ok(tabs)
    }

    /// Bring one tab to the front by its CDP target id.
    pub async fn select(&self, target: &str, bound: Duration) -> Result<(), String> {
        self.call_raw(
            "browser_tabs",
            serde_json::json!({ "action": "select", "target": target }),
            bound,
        )
        .await
        .map(|_| ())
    }

    /// The dispatch itself.
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
            return Err(failure_reason(&parts).unwrap_or_else(|| format!("{tool} failed")));
        }
        Ok(parts)
    }
}

/// A failed call's own words, with upstream's terse stale-ref sentence
/// replaced by one that says what happened and what to do about it.
fn failure_reason(parts: &[BrowserPart]) -> Option<String> {
    let reason = parts
        .iter()
        .map(|part| match part {
            BrowserPart::Text { text } => text.clone(),
            BrowserPart::Image { .. } => "[an image]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    if reason.is_empty() {
        return None;
    }
    Some(stale_ref_recovery(&reason).unwrap_or(reason))
}

/// Upstream's stale-ref failure, rewritten to name the cause and both ways
/// out.
///
/// **Measured against `@playwright/mcp` 0.0.83 on a React form, 2026-10-27.**
/// A ref names the element object its snapshot captured, so it survives an
/// ordinary re-render that keeps that node (eight over 2 s, click still
/// landed) and stops resolving once a re-render replaces the node (a form
/// remounting every 250 ms failed on the next tick) or the page navigates.
/// A full-page snapshot keeps the ref of an element that stayed and carries
/// the same role and accessible name; a stayed element that is no longer
/// visible or pointer-interactive gets no ref, one whose role or name changed
/// gets a new one, and a target-scoped snapshot answers with its subtree
/// alone, so refs outside it do not resolve until a full-page snapshot. A
/// unique role/name selector is resolved when the call runs, so it needs no
/// snapshot, and a name shared by several elements is refused rather than
/// guessed at.
fn stale_ref_recovery(why: &str) -> Option<String> {
    const OPEN: &str = "Ref ";
    const CLOSE: &str = " not found in the current page snapshot. Try capturing new snapshot.";
    let open = why.find(OPEN)?;
    let target_start = open + OPEN.len();
    let target_end = target_start + why[target_start..].find(CLOSE)?;
    let target = &why[target_start..target_end];
    if target.is_empty() || !target.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(why.replace(
        &why[open..target_end + CLOSE.len()],
        &format!(
            "Ref {target} is not in the current page snapshot any more: either the element it \
             named was replaced or renamed, or the snapshot in hand was taken of a subtree, of \
             another tab, or after a navigation. While the page still changes between calls, pass \
             a unique role/name selector such as role=button[name=\"Save\"], scoped when names \
             repeat, and it is resolved when the call runs. Once it has settled, take a full-page \
             browser_snapshot (no target) and use its refs."
        ),
    ))
}

/// The sentence a call that ran past its bound reports: one builder, so a
/// probe can tell this failure from a tool's own and not by matching prose it
/// would have to keep in step.
pub fn did_not_answer(tool: &str, bound: Duration) -> String {
    format!("the driver did not answer {tool} within {}", duration_words(bound))
}

/// A bound as a sentence names it: whole seconds, or milliseconds under one
/// so a spent budget cannot read "within 0 s".
pub(crate) fn duration_words(bound: Duration) -> String {
    if bound >= Duration::from_secs(1) {
        format!("{} s", bound.as_secs())
    } else {
        format!("{} ms", bound.as_millis())
    }
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

/// Whether a failure is one of ours running past its bound: the driver never
/// answered the MCP request. A tool's own timeout arrives as the tool's
/// error text and does not match this prefix.
pub fn timed_out(why: &str) -> bool {
    why.starts_with("the driver did not answer ")
}

/// Whether a failure says the driver itself is dead and must be replaced: its
/// held page outlived its renderer, it never finished attaching, or it
/// stopped answering at all. **The call that finds any of these replaces it
/// on that call.**
pub fn died(why: &str) -> bool {
    stale_page(why) || timed_out(why) || attach_timed_out(why)
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

/// The port in a host endpoint like `http://127.0.0.1:9411`.
pub fn endpoint_port(endpoint: &str) -> Option<u16> {
    endpoint.strip_prefix("http://127.0.0.1:")?.trim_end_matches('/').parse().ok()
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

    /// **A bound of ours is a dead driver; a tool's own timeout is not.** The
    /// builder is the only place the sentence is written, so this reads it
    /// rather than matching prose by hand.
    /// **A spent budget's remainder is under a second**, and whole seconds
    /// would read "within 0 s".
    #[test]
    fn a_bound_under_a_second_is_named_in_milliseconds() {
        assert_eq!(duration_words(Duration::from_millis(1)), "1 ms");
        assert_eq!(duration_words(Duration::from_secs(90)), "90 s");
    }

    #[test]
    fn a_call_bound_that_expired_is_a_dead_driver_and_a_tools_timeout_is_not() {
        let expired = did_not_answer("browser_tabs", CALL_TIMEOUT);
        assert!(timed_out(&expired), "our own bound expiring is the driver's silence");
        assert!(died(&expired), "and it makes the driver dead");
        assert!(
            !timed_out("### Error\nTimeoutError: page.click: Timeout 30000ms exceeded."),
            "a tool's own timeout is the tool's, not the driver's",
        );
        assert!(
            !died("### Error\nTimeoutError: page.click: Timeout 30000ms exceeded."),
            "so it replaces nothing",
        );
        assert!(died(&attach_failure_line()), "an attach that never finished is still dead");
        assert!(
            died("browserBackend.callTool: Cannot read properties of undefined (reading '_page')"),
            "and so is a held page outliving its renderer",
        );
    }

    /// The measured attach timeout, in whole.
    fn attach_failure_line() -> String {
        "### Error\nTimeoutError: async initializeServer: Timeout 30000ms exceeded.".to_owned()
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

    /// **A stale-ref failure answers with the condition, the route that fits
    /// a page still changing before the one that fits a settled page, and the
    /// ref it names** - and the driver's own envelope around it is left
    /// alone. Both ref shapes are real: `e26` on a first frame and `f2e26`
    /// after a main-frame navigation.
    #[test]
    fn a_stale_ref_failure_names_the_cause_and_both_ways_out() {
        for target in ["e26", "f2e26"] {
            let measured = format!(
                "### Error\nError: Ref {target} not found in the current page snapshot. \
                 Try capturing new snapshot."
            );
            let rewritten = failure_reason(&[BrowserPart::Text { text: measured }])
                .expect("the measured stale-ref failure has a reason");
            assert!(
                rewritten.starts_with(&format!(
                    "### Error\nError: Ref {target} is not in the current page snapshot any more"
                )),
                "the envelope and the ref survive: {rewritten}",
            );
            assert!(
                rewritten.contains(
                    "either the element it named was replaced or renamed, or the snapshot in hand \
                     was taken of a subtree, of another tab, or after a navigation"
                ),
                "the condition is named without closing the list of causes: {rewritten}",
            );
            let selector = rewritten
                .find("pass a unique role/name selector")
                .expect("the still-changing page is answered with a selector");
            assert!(
                rewritten.contains("scoped when names repeat"),
                "a repeated name is scoped rather than guessed at: {rewritten}",
            );
            let snapshot = rewritten
                .find("take a full-page browser_snapshot (no target) and use its refs")
                .expect("the settled page is answered with a full-page snapshot");
            assert!(
                selector < snapshot,
                "the route for a still-changing page comes first: {rewritten}"
            );
            assert!(!rewritten.contains("Try capturing new snapshot"), "{rewritten}");
        }
    }

    /// A failure that is not the measured stale-ref sentence is handed back
    /// verbatim, including one that names a ref in other words.
    #[test]
    fn another_failure_is_handed_back_verbatim() {
        for measured in [
            "### Error\nTimeoutError: page.click: Timeout 30000ms exceeded.",
            "### Error\nError: Ref e26 points at a detached element.",
            "### Error\nError: No such element",
        ] {
            assert_eq!(stale_ref_recovery(measured), None, "{measured}");
            assert_eq!(
                failure_reason(&[BrowserPart::Text { text: measured.to_owned() }]).as_deref(),
                Some(measured),
            );
        }
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

    /// **A listing carries the target id a call selects by**, and nothing
    /// else names a page: the driver's own position renumbers on every close.
    #[test]
    fn a_listing_carries_the_target_a_call_selects_by() {
        let listing = "FORGE-TABS [{\"i\":0,\"target\":\"tA\",\"title\":\"x\",\
                       \"url\":\"u\",\"current\":false},{\"i\":1,\"target\":\"tB\",\
                       \"title\":\"y\",\"url\":\"v\",\"current\":true}]\n";
        let tabs = crate::tabs::parse_export(listing).expect("the export parses");
        assert_eq!(tabs[1].target, "tB", "the target is the page's name");
        assert!(tabs[1].current, "and the row says whether it is the current tab");
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

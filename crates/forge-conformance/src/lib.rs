//! Utilities for the wire-conformance harness.
//!
//! Intentionally separate from `forge-sdk` so the SDK crate surface
//! stays focused on what library consumers use. Nothing here is public
//! SDK API.
//!
//! ## Two testing modes
//!
//! Each scenario runs in one of two modes:
//!
//! - **Live mode** (`FORGE_WIRE_CAPTURE=1`): spawn real `claude`, drive
//!   forge-sdk through the scenario, capture the full stdin/stdout
//!   trace to `target/wire-traces/`. Burns API tokens. Updates the
//!   committed baseline on request.
//! - **Replay mode** (default): load the committed baseline trace
//!   fixture for the scenario, feed every inbound line through
//!   `forge_sdk::transport::codec::decode_dispatch`, assert everything
//!   decodes cleanly (including no `DecodedLine::Unknown` variants
//!   unless explicitly expected). Runs on every `cargo check` /
//!   `just check` — no API cost.
//!
//! ## Baselines
//!
//! Committed baselines live under `crates/forge-conformance/baselines/
//! <cli-version>/<scenario>.jsonl`. Each scenario knows its own name;
//! the `cli-version` dir rotates when we bump the pinned CLI version
//! through the upgrade ritual.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod session_redact;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge_sdk::Error;
use forge_sdk::transport::Transport;
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};
use forge_sdk::transport::process::Subprocess;

/// One captured line in a trace.
#[derive(Default, Debug)]
pub struct TraceLog {
    /// `(direction, line)` pairs in the order they happened on the wire.
    /// Direction is `"in"` (CLI → SDK, from stdout) or `"out"` (SDK → CLI, to stdin).
    entries: Vec<(&'static str, String)>,
}

impl TraceLog {
    /// Serialise as JSONL: one `{"dir":"in"|"out","line":"..."}` per line.
    ///
    /// # Errors
    ///
    /// Returns a `serde_json::Error` if any entry fails to serialise.
    pub fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut body = String::new();
        for (dir, line) in &self.entries {
            let obj = serde_json::json!({ "dir": dir, "line": line });
            body.push_str(&serde_json::to_string(&obj)?);
            body.push('\n');
        }
        Ok(body)
    }

    /// Slice of inbound lines (CLI → SDK).
    #[must_use]
    pub fn inbound(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(d, _)| *d == "in")
            .map(|(_, l)| l.as_str())
            .collect()
    }

    /// Slice of outbound lines (SDK → CLI).
    #[must_use]
    pub fn outbound(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(d, _)| *d == "out")
            .map(|(_, l)| l.as_str())
            .collect()
    }
}

/// Transport wrapper that tees every line through to a shared log while
/// delegating the actual I/O to the wrapped `Subprocess`.
pub struct RecordingTransport {
    inner: Subprocess,
    log: Arc<Mutex<TraceLog>>,
}

impl RecordingTransport {
    /// Wrap a live `Subprocess`. Returns the wrapper + a shared handle to
    /// the trace log so the caller can read it back after shutdown.
    #[must_use]
    pub fn new(inner: Subprocess) -> (Self, Arc<Mutex<TraceLog>>) {
        let log = Arc::new(Mutex::new(TraceLog::default()));
        (
            Self {
                inner,
                log: log.clone(),
            },
            log,
        )
    }
}

#[async_trait]
impl Transport for RecordingTransport {
    async fn read_line(&mut self) -> Result<Option<String>, Error> {
        let line = self.inner.read_line().await?;
        if let Some(ref s) = line {
            self.log.lock().unwrap().entries.push(("in", s.clone()));
        }
        Ok(line)
    }

    async fn write_line(&mut self, line: &str) -> Result<(), Error> {
        self.log
            .lock()
            .unwrap()
            .entries
            .push(("out", line.trim_end_matches('\n').to_string()));
        self.inner.write_line(line).await
    }

    async fn end_input(&mut self) -> Result<(), Error> {
        self.inner.end_input().await
    }

    async fn close(&mut self) -> Result<(), Error> {
        self.inner.close().await
    }
}

/// Pinned CLI version these baselines were captured against.
///
/// When we run the `just upgrade-cli` ritual, this constant bumps along
/// with the baselines under `baselines/<version>/`.
pub const PINNED_CLI_VERSION: &str = "2.1.117";

/// Directory holding the committed trace baselines for the pinned CLI
/// version.
#[must_use]
pub fn baseline_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("baselines")
        .join(PINNED_CLI_VERSION)
}

/// Load a trace fixture by scenario name.
///
/// Looks up `baselines/<PINNED_CLI_VERSION>/<scenario>.jsonl` and returns
/// the parsed `(direction, line)` pairs. Panics if the baseline is
/// missing — scenarios are expected to ship their baseline on creation.
///
/// # Panics
///
/// If the fixture file is missing, unreadable, or malformed.
#[must_use]
pub fn load_baseline(scenario: &str) -> TraceLog {
    let path = baseline_dir().join(format!("{scenario}.jsonl"));
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing baseline for scenario '{scenario}' at {}: {e}. \
             Run `FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-conformance \
             --run-ignored only <live_capture_test>` to capture it.",
            path.display()
        )
    });
    let mut log = TraceLog::default();
    for (i, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let obj: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|e| {
            panic!(
                "{}:{}: malformed baseline entry: {e}",
                path.display(),
                i + 1
            )
        });
        let dir_str = obj.get("dir").and_then(|v| v.as_str()).unwrap_or("");
        let line_val = obj
            .get("line")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let dir_static: &'static str = match dir_str {
            "in" => "in",
            "out" => "out",
            other => panic!(
                "{}:{}: bad dir '{other}' (expected 'in' or 'out')",
                path.display(),
                i + 1
            ),
        };
        log.entries.push((dir_static, line_val));
    }
    log
}

/// Outcome of running every inbound line in a log through
/// `decode_dispatch`. Split across known decode categories plus Unknowns
/// and outright errors.
#[derive(Debug, Default)]
pub struct DecodeReport {
    /// Count of inbound lines that decoded to a regular `Message`.
    pub messages: usize,
    /// Count of inbound lines that decoded to a `ControlRequest`.
    pub controls: usize,
    /// Count of `control_cancel_request` frames.
    pub control_cancels: usize,
    /// Count of `control_response` frames (replies from the CLI to our
    /// outbound `control_requests` — initialize, `set_model`, interrupt, …).
    pub control_responses: usize,
    /// Unrecognised `type` values seen. Each entry is the `type` string
    /// the CLI sent.
    pub unknown_types: Vec<String>,
    /// Unrecognised `control_request.subtype` values seen.
    pub unknown_control_subtypes: Vec<String>,
    /// Hard decode errors — line was recognised but inner shape was
    /// invalid, or JSON malformed.
    pub decode_errors: Vec<(usize, String)>,
}

impl DecodeReport {
    /// True if no Unknowns or decode errors were seen.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.unknown_types.is_empty()
            && self.unknown_control_subtypes.is_empty()
            && self.decode_errors.is_empty()
    }
}

/// Run a live-capture scenario end-to-end: build options, spawn a recorded
/// `claude`, drive the scenario to a `Result` frame, dump the trace to
/// `target/wire-traces/`, assert every inbound line decodes cleanly.
///
/// Caller supplies:
/// - `scenario`: a short slug (e.g. `"bash_tool"`) used in trace filenames.
/// - `options`: fully-built [`forge_sdk::Options`] — set tools,
///   `permission_mode`, hooks, MCP servers, etc. here.
/// - `drive`: async closure that drives the scenario once the client is
///   ready. Typically calls `send_user_message(...)` and may register
///   turn-specific state.
///
/// # Skip semantics
///
/// When `FORGE_WIRE_CAPTURE` is unset, returns `Ok(None)` immediately
/// without touching the network — scenarios compile and link in CI but
/// only run when the developer opts in.
///
/// # Errors
///
/// Any [`forge_sdk::Error`] surfaced during spawn / drive / drain.
/// Panics on trace-dump failure or decode-completeness failure — those
/// are harness invariants, not recoverable errors.
///
/// # Panics
///
/// - Transport panics are propagated.
/// - Trace file I/O failure.
/// - Decode-completeness regression in the captured inbound frames.
#[allow(clippy::too_many_lines)]
pub async fn run_live_scenario<F, Fut>(
    scenario: &str,
    options: forge_sdk::Options,
    drive: F,
) -> Result<Option<ScenarioCapture>, Error>
where
    F: FnOnce(forge_sdk::Client) -> Fut,
    Fut: std::future::Future<Output = Result<forge_sdk::Client, Error>>,
{
    if std::env::var("FORGE_WIRE_CAPTURE").is_err() {
        eprintln!("FORGE_WIRE_CAPTURE not set; skipping scenario {scenario}");
        return Ok(None);
    }

    let sub = Subprocess::spawn(&options).await?;
    let (transport, log_arc) = RecordingTransport::new(sub);

    // Scope-local helper to dump the trace regardless of how the test ends.
    let dump = |log: &TraceLog, tag: &str| -> std::path::PathBuf {
        let target =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wire-traces");
        std::fs::create_dir_all(&target).expect("create wire-traces dir");
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = target.join(format!("capture-{tag}-{ts}.jsonl"));
        let body = log.to_jsonl().expect("jsonl serialise");
        std::fs::write(&path, body).expect("write trace");
        path
    };

    let client = forge_sdk::Client::spawn_with_transport(options, Box::new(transport))
        .await
        .inspect_err(|_e| {
            let log = log_arc.lock().unwrap();
            let path = dump(&log, &format!("{scenario}-spawn-failed"));
            eprintln!(
                "{scenario}: spawn_with_transport failed, trace at {} [in={} out={}]",
                path.display(),
                log.inbound().len(),
                log.outbound().len()
            );
        })?;

    // Hand off to the scenario driver — on failure dump a partial trace.
    let mut client = match drive(client).await {
        Ok(c) => c,
        Err(e) => {
            let log = log_arc.lock().unwrap();
            let path = dump(&log, &format!("{scenario}-drive-failed"));
            eprintln!(
                "{scenario}: drive failed, trace at {} [in={} out={}]",
                path.display(),
                log.inbound().len(),
                log.outbound().len()
            );
            return Err(e);
        }
    };

    // Drain until a `Result` frame, then close stdin and drain to EOF.
    //
    // We can't close stdin BEFORE the drain — the CLI is still
    // emitting `hook_callback` / `mcp_message` control_requests during
    // the turn, and our handlers reply on stdin. Closing too early
    // breaks the pipe mid-handler. So the order is:
    //
    // 1. Read until `Result` (with a short per-read timeout in case
    //    `drive` already drained everything inside its closure — in
    //    that case the timeout fires, we assume drain is done, and
    //    fall through to end_input).
    // 2. Close stdin so the CLI exits cleanly.
    // 3. Drain any trailing frames to EOF.
    let read_timeout = std::time::Duration::from_secs(30);
    let mut saw_result = false;
    let mut summary: Option<(u64, Option<f64>, u64)> = None;
    loop {
        match tokio::time::timeout(read_timeout, client.next_event()).await {
            Ok(Ok(Some(msg))) => {
                if let forge_sdk::Message::Result {
                    num_turns,
                    total_cost_usd,
                    duration_ms,
                    ..
                } = &msg
                {
                    saw_result = true;
                    summary = Some((*num_turns, *total_cost_usd, *duration_ms));
                    break;
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                let log = log_arc.lock().unwrap();
                let path = dump(&log, &format!("{scenario}-drain-failed"));
                eprintln!(
                    "{scenario}: drain failed, trace at {} [in={} out={}]",
                    path.display(),
                    log.inbound().len(),
                    log.outbound().len()
                );
                return Err(e);
            }
            Err(_timeout) => {
                // 30 s without a frame — `drive` must have drained the
                // Result already. Break and let the end_input path
                // handle cleanup + trailing frames.
                break;
            }
        }
    }

    // Close stdin now that no more handler writes are expected.
    if let Err(e) = client.end_input().await {
        eprintln!("{scenario}: end_input failed, continuing to disconnect: {e}");
    }
    // Drain any trailing frames that arrive between end_input and EOF
    // (some CLI paths emit a final `system:close` or trailing
    // `rate_limit_event`). Errors from writes during trailing
    // handlers (`BrokenPipe`) are expected here — stdin is closed.
    let trailing_timeout = std::time::Duration::from_secs(5);
    while let Ok(evt) = tokio::time::timeout(trailing_timeout, client.next_event()).await {
        match evt {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    if let Err(e) = client.disconnect().await {
        eprintln!("{scenario}: disconnect failed (non-fatal, trace already captured): {e}");
    }

    // Successful (or at least drained) run — dump the trace, verify every
    // inbound line decodes. Failure here is a hard panic.
    let log = log_arc.lock().unwrap();
    let trace_path = dump(&log, scenario);
    let report = decode_all_inbound(&log);
    assert!(
        report.is_clean(),
        "{scenario}: decode regressions in captured trace\n\
         trace: {}\n\
         report: {report:#?}",
        trace_path.display()
    );
    match summary {
        Some((turns, cost, dur)) => eprintln!(
            "{scenario}: captured in={} out={} | turns={turns} cost_usd={cost:?} \
             duration_ms={dur} | trace={}",
            log.inbound().len(),
            log.outbound().len(),
            trace_path.display()
        ),
        None => eprintln!(
            "{scenario}: captured in={} out={} | NO Result frame (early termination, \
             drive likely drained it) | trace={}",
            log.inbound().len(),
            log.outbound().len(),
            trace_path.display()
        ),
    }

    Ok(Some(ScenarioCapture {
        trace_path,
        inbound: log.inbound().len(),
        outbound: log.outbound().len(),
        saw_result,
        summary,
    }))
}

/// Outcome of a successful live-capture scenario run. Returned by
/// [`run_live_scenario`] when the scenario produced a trace (regardless of
/// whether a `Result` frame was seen before EOF).
#[derive(Debug)]
#[non_exhaustive]
pub struct ScenarioCapture {
    /// Absolute path of the `target/wire-traces/` dump for this run.
    pub trace_path: std::path::PathBuf,
    /// Number of inbound lines (CLI → SDK) the transport recorded.
    pub inbound: usize,
    /// Number of outbound lines (SDK → CLI) the transport recorded.
    pub outbound: usize,
    /// Whether a `Result` frame was observed before the loop ended.
    pub saw_result: bool,
    /// `(num_turns, total_cost_usd, duration_ms)` from the final `Result`
    /// frame — `None` when the scenario ended without one.
    pub summary: Option<(u64, Option<f64>, u64)>,
}

/// Run every inbound line from `log` through `decode_dispatch`, returning
/// a categorised report.
#[must_use]
pub fn decode_all_inbound(log: &TraceLog) -> DecodeReport {
    use forge_sdk::control::ControlRequestKind;
    let mut report = DecodeReport::default();
    for (idx, line) in log.inbound().iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match decode_dispatch(line, (idx + 1) as u64) {
            Ok(DecodedLine::Message(_)) => report.messages += 1,
            Ok(DecodedLine::Control(req)) => {
                report.controls += 1;
                if let ControlRequestKind::Unknown { subtype, .. } = &req.request {
                    report.unknown_control_subtypes.push(subtype.clone());
                }
            }
            Ok(DecodedLine::ControlCancel { .. }) => report.control_cancels += 1,
            Ok(DecodedLine::ControlResponse { .. }) => report.control_responses += 1,
            Ok(DecodedLine::Unknown { type_str, .. }) => {
                report.unknown_types.push(type_str);
            }
            // `DecodedLine` is `#[non_exhaustive]` — future variants
            // surface as a count, not an error. Update categories
            // in `DecodeReport` if you add a variant that needs
            // dedicated tracking.
            Ok(_) => {}
            Err(e) => report.decode_errors.push((idx, format!("{e}"))),
        }
    }
    report
}

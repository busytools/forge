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
//! Committed baselines live under `crates/forge-test-harness/baselines/sdk/
//! <cli-version>/<scenario>.jsonl`. Each scenario knows its own name;
//! the `cli-version` dir rotates when we bump the pinned CLI version
//! through the upgrade ritual.

pub mod session_redact;

use std::sync::Arc;

use forge_sdk::Error;
use forge_sdk::OptionsBuilder;
use forge_sdk::transport::codec::{DecodedLine, decode_dispatch};
use parking_lot::Mutex;

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
    pub fn inbound(&self) -> Vec<&str> {
        self.entries.iter().filter(|(d, _)| *d == "in").map(|(_, l)| l.as_str()).collect()
    }

    /// Slice of outbound lines (SDK → CLI).
    pub fn outbound(&self) -> Vec<&str> {
        self.entries.iter().filter(|(d, _)| *d == "out").map(|(_, l)| l.as_str()).collect()
    }
}

/// Install inbound + outbound wire-tee callbacks on a builder so the
/// resulting [`forge_sdk::Options`] captures every stream-json line
/// the SDK exchanges with the `claude` subprocess. Returns the
/// configured builder + a shared handle to the trace log. Wire
/// capture is a spawn-time configuration concern, not a
/// transport-injection one.
pub fn attach_recording(builder: OptionsBuilder) -> (OptionsBuilder, Arc<Mutex<TraceLog>>) {
    let log = Arc::new(Mutex::new(TraceLog::default()));
    let log_in = log.clone();
    let log_out = log.clone();
    let builder = builder
        .tee_inbound(move |line: &str| {
            log_in.lock().entries.push(("in", line.to_string()));
        })
        .tee_outbound(move |line: &str| {
            log_out.lock().entries.push(("out", line.to_string()));
        });
    (builder, log)
}

/// Pinned CLI version these baselines were captured against.
///
/// When we run the `just upgrade-cli` ritual, this constant bumps along
/// with the baselines under `baselines/<version>/`.
pub const PINNED_CLI_VERSION: &str = "2.1.156";

/// Directory holding the committed trace baselines for the pinned CLI
/// version. Resolves to
/// `crates/forge-test-harness/baselines/sdk/<PINNED_CLI_VERSION>/`.
pub fn baseline_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("baselines")
        .join("sdk")
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
pub fn load_baseline(scenario: &str) -> TraceLog {
    let path = baseline_dir().join(format!("{scenario}.jsonl"));
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing baseline for scenario '{scenario}' at {}: {e}. \
             Run `FORGE_WIRE_CAPTURE=1 cargo nextest run -p forge-test-harness \
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
            panic!("{}:{}: malformed baseline entry: {e}", path.display(), i + 1)
        });
        let dir_str = obj.get("dir").and_then(|v| v.as_str()).unwrap_or("");
        let line_val = obj.get("line").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let dir_static: &'static str = match dir_str {
            "in" => "in",
            "out" => "out",
            other => {
                panic!("{}:{}: bad dir '{other}' (expected 'in' or 'out')", path.display(), i + 1)
            }
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
pub async fn run_live_scenario<F, Fut>(
    scenario: &str,
    options: forge_sdk::Options,
    drive: F,
) -> Result<Option<ScenarioCapture>, Error>
where
    F: FnOnce(forge_sdk::Client, forge_sdk::ClientEvents) -> Fut,
    Fut: std::future::Future<Output = Result<(forge_sdk::Client, forge_sdk::ClientEvents), Error>>,
{
    if std::env::var("FORGE_WIRE_CAPTURE").is_err() {
        eprintln!("FORGE_WIRE_CAPTURE not set; skipping scenario {scenario}");
        return Ok(None);
    }

    // Install wire-recording tees on the supplied options. The harness
    // takes a fully-configured Options (built by the scenario), so we
    // wrap it: rebuild via `OptionsBuilder::from_options` if it
    // existed, else mutate the fields directly.
    let log_arc = Arc::new(Mutex::new(TraceLog::default()));
    let options = {
        let log_in = log_arc.clone();
        let log_out = log_arc.clone();
        let mut opts = options;
        opts.tee_inbound = Some(Arc::new(move |line: &str| {
            log_in.lock().entries.push(("in", line.to_string()));
        }));
        opts.tee_outbound = Some(Arc::new(move |line: &str| {
            log_out.lock().entries.push(("out", line.to_string()));
        }));
        opts
    };

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

    let (client, events) = forge_sdk::Client::spawn(options).await.inspect_err(|_e| {
        let log = log_arc.lock();
        let path = dump(&log, &format!("{scenario}-spawn-failed"));
        eprintln!(
            "{scenario}: Client::spawn failed, trace at {} [in={} out={}]",
            path.display(),
            log.inbound().len(),
            log.outbound().len()
        );
    })?;

    // Hand off to the scenario driver — on failure dump a partial trace.
    let (client, mut events) = match drive(client, events).await {
        Ok(pair) => pair,
        Err(e) => {
            let log = log_arc.lock();
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
        match tokio::time::timeout(read_timeout, events.recv()).await {
            Ok(Some(Ok(msg))) => {
                if let forge_primitives::Message::Result {
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
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                let log = log_arc.lock();
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
                // 30 s without a frame. Two legitimate causes:
                // (a) `drive` already drained the `Result` inside its
                //     closure — common for scenarios that issue
                //     follow-up control_requests after consuming the
                //     turn (e.g. `context_usage`, `rewind_files`).
                // (b) The CLI is genuinely hung. The harness can't
                //     distinguish the two without a signal from
                //     `drive`. Log loudly so a real hang surfaces in
                //     the test output instead of silently passing as
                //     "no Result frame seen". `summary` stays `None`,
                //     which the post-drain summary line already
                //     reports as "NO Result frame".
                eprintln!(
                    "{scenario}: 30s read_timeout fired with no Result \
                     frame seen — `drive` may have drained it OR the \
                     CLI is hung. Proceeding to cleanup."
                );
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
    // Other errors (decoder regressions, malformed JSON, mid-drain
    // shape drift) are NOT expected and would otherwise be hidden;
    // log them loudly so they surface in the captured trace's
    // diagnostics instead of being promoted into a baseline.
    let trailing_timeout = std::time::Duration::from_secs(5);
    while let Ok(evt) = tokio::time::timeout(trailing_timeout, events.recv()).await {
        match evt {
            Some(Ok(_)) => {}
            None => break,
            Some(Err(Error::Io(io))) if io.kind() == std::io::ErrorKind::BrokenPipe => break,
            Some(Err(e)) => {
                eprintln!(
                    "{scenario}: trailing-drain saw non-BrokenPipe error: {e}. \
                     Continuing to dump partial trace."
                );
                break;
            }
        }
    }
    if let Err(e) = client.disconnect().await {
        eprintln!("{scenario}: disconnect failed (non-fatal, trace already captured): {e}");
    }

    // Successful (or at least drained) run — dump the trace, verify every
    // inbound line decodes. Failure here is a hard panic.
    let log = log_arc.lock();
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
            Err(e) => report.decode_errors.push((idx, format!("{e}"))),
        }
    }
    report
}

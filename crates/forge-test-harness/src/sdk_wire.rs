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
//!   `just check` - no API cost.
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
    /// Serialise as JSONL: one `{"dir":"in"|"out","line":"..."}` per line,
    /// redacting on the way out.
    ///
    /// This is the redaction point for both live-capture write sites and
    /// for baseline promotion, which copies the file this produces. Two
    /// committed artifacts bypass it: the `real_session_*` baselines,
    /// redacted harder by [`session_redact::redact_session_path`], and
    /// the reference captures under `.claude/skills/claude-cli-upgrade/`,
    /// redacted by nothing.
    ///
    /// # Errors
    ///
    /// If any entry fails to serialise or cannot be redacted safely.
    pub fn to_jsonl(&self) -> Result<String, String> {
        let redactor = session_redact::WireRedactor::for_trace(
            self.entries.iter().map(|(_, line)| line.as_str()),
        )?;
        self.encode(|line| redactor.redact_line(line))
    }

    /// Serialise without redacting, for a diagnostic dump that is never
    /// promoted - a spawn failure is diagnosed from the real cwd.
    ///
    /// # Errors
    ///
    /// If any entry fails to serialise.
    fn to_jsonl_verbatim(&self) -> Result<String, String> {
        self.encode(|line| Ok(line.to_string()))
    }

    /// Serialise for `kind`.
    ///
    /// # Errors
    ///
    /// As the chosen serialisation.
    fn to_jsonl_for(&self, kind: DumpKind) -> Result<String, String> {
        match kind {
            DumpKind::Promotable => self.to_jsonl(),
            DumpKind::Diagnostic => self.to_jsonl_verbatim(),
        }
    }

    fn encode(
        &self,
        mut per_line: impl FnMut(&str) -> Result<String, String>,
    ) -> Result<String, String> {
        let mut body = String::new();
        for (dir, line) in &self.entries {
            let obj = serde_json::json!({ "dir": dir, "line": per_line(line)? });
            body.push_str(&serde_json::to_string(&obj).map_err(|e| format!("serialise: {e}"))?);
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

/// What a dump is for, deciding its filename prefix AND whether it is
/// redacted together, so the two cannot disagree: `README.md` promotes
/// with `cp target/wire-traces/capture-*.jsonl`, so anything that glob
/// reaches must be redacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DumpKind {
    /// Inside the promotion glob, so redacted.
    Promotable,
    /// Outside it, so it keeps the paths a failed run is diagnosed from.
    Diagnostic,
}

impl DumpKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Promotable => "capture",
            Self::Diagnostic => "diag",
        }
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
/// Bumping it means re-capturing the live-capture baselines against
/// the new CLI and promoting them into a fresh
/// `baselines/sdk/<version>/`. Replay is expected to fail in between.
///
/// `just conformance-capture-sdk <test>` takes a NEXTEST TEST NAME, not
/// a baseline name; the two namespaces diverge (`worker_spawn.jsonl`
/// comes from `worker_spawn_scenario`). It substring-matches, so a
/// loose argument fires several live captures and bills for all of
/// them. The `real_session_*` baselines have no capture recipe at all -
/// they come from the `sdk_redact_session` example. The full ritual is
/// in `.claude/skills/claude-cli-upgrade/`.
pub const PINNED_CLI_VERSION: &str = "2.1.220";

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
/// missing - scenarios are expected to ship their baseline on creation.
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
    /// outbound `control_requests` - initialize, `set_model`, interrupt, …).
    pub control_responses: usize,
    /// Unrecognised `type` values seen. Each entry is the `type` string
    /// the CLI sent.
    pub unknown_types: Vec<String>,
    /// Unrecognised `control_request.subtype` values seen.
    pub unknown_control_subtypes: Vec<String>,
    /// `system` subtypes that fell through to the generic
    /// `Message::System` bucket and are not in
    /// [`EXPECTED_GENERIC_SYSTEM_SUBTYPES`].
    pub unmodelled_system_subtypes: Vec<String>,
    /// Content-block `type` values that decoded to
    /// `ContentBlock::Unknown`.
    pub unmodelled_content_block_types: Vec<String>,
    /// Wire values a `#[serde(other)]` catch-all absorbed. Each entry is
    /// `<json pointer>: <wire value> -> <fallback>`.
    pub absorbed_by_catch_all: Vec<String>,
    /// Hard decode errors - line was recognised but inner shape was
    /// invalid, or JSON malformed.
    pub decode_errors: Vec<(usize, String)>,
}

impl DecodeReport {
    /// True if no Unknowns or decode errors were seen.
    pub fn is_clean(&self) -> bool {
        self.unknown_types.is_empty()
            && self.unknown_control_subtypes.is_empty()
            && self.unmodelled_system_subtypes.is_empty()
            && self.unmodelled_content_block_types.is_empty()
            && self.absorbed_by_catch_all.is_empty()
            && self.decode_errors.is_empty()
    }
}

/// `system` subtypes the decoder deliberately leaves generic. Everything
/// else reaching `Message::System` is a subtype nobody has modelled yet.
///
/// The decoder answers "is this modelled?" on its own - a typed subtype
/// never reaches `Message::System` - so this list holds only the
/// exceptions rather than mirroring every modelled subtype. A new CLI
/// subtype is absent from the list and goes red.
///
/// An entry is only harmless while that subtype has no typed variant.
/// `SystemRepr` is untagged, so a subtype that IS modelled degrades into
/// the generic bucket when its payload shape drifts, and today that is
/// caught only because the subtype is missing from this list. Give `init`
/// or `status` a typed variant and its entry here turns load-bearing: it
/// starts suppressing payload drift on a shape the decoder claims to
/// model. Remove the entry in the same change that adds the variant.
pub const EXPECTED_GENERIC_SYSTEM_SUBTYPES: &[&str] = &["init", "status"];

/// Serialised forms of the catch-all variants that stand in for a wire
/// value the decoder does not model (`#[serde(other)]`).
///
/// `"other"` is not defensive padding. Seven of the eight reachable
/// catch-alls are unit variants that serialise to a bare `"unknown"`;
/// `WorkflowProgressEvent::Other` is internally tagged and serialises to
/// `{"type":"other"}`, so what changes is the nested `type` key, which
/// exists on both sides and is reached through the object walk.
const CATCH_ALL_MARKERS: &[&str] = &["unknown", "other"];

/// Run a live-capture scenario end-to-end: build options, spawn a recorded
/// `claude`, drive the scenario to a `Result` frame, dump the trace to
/// `target/wire-traces/`, assert every inbound line decodes cleanly.
///
/// Caller supplies:
/// - `scenario`: a short slug (e.g. `"bash_tool"`) used in trace filenames.
/// - `options`: fully-built [`forge_sdk::Options`] - set tools,
///   `permission_mode`, hooks, MCP servers, etc. here.
/// - `drive`: async closure that drives the scenario once the client is
///   ready. Typically calls `send_user_message(...)` and may register
///   turn-specific state.
///
/// # Skip semantics
///
/// When `FORGE_WIRE_CAPTURE` is unset, returns `Ok(None)` immediately
/// without touching the network - scenarios compile and link in CI but
/// only run when the developer opts in.
///
/// # Errors
///
/// Any [`forge_sdk::Error`] surfaced during spawn / drive / drain.
/// Panics on trace-dump failure or decode-completeness failure - those
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
) -> Result<Option<()>, Error>
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

    // Dump the trace regardless of how the test ends.
    let dump = |log: &TraceLog, tag: &str, kind: DumpKind| -> std::path::PathBuf {
        let target =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wire-traces");
        std::fs::create_dir_all(&target).expect("create wire-traces dir");
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let path = target.join(format!("{}-{tag}-{ts}.jsonl", kind.prefix()));
        let body = log.to_jsonl_for(kind).expect("redact + serialise trace");
        std::fs::write(&path, body).expect("write trace");
        path
    };

    let (client, events) = forge_sdk::Client::spawn(options).await.inspect_err(|_e| {
        let log = log_arc.lock();
        let path = dump(&log, &format!("{scenario}-spawn-failed"), DumpKind::Diagnostic);
        eprintln!(
            "{scenario}: Client::spawn failed, trace at {} [in={} out={}]",
            path.display(),
            log.inbound().len(),
            log.outbound().len()
        );
    })?;

    // Hand off to the scenario driver - on failure dump a partial trace.
    let (client, mut events) = match drive(client, events).await {
        Ok(pair) => pair,
        Err(e) => {
            let log = log_arc.lock();
            let path = dump(&log, &format!("{scenario}-drive-failed"), DumpKind::Diagnostic);
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
    // We can't close stdin BEFORE the drain - the CLI is still
    // emitting `hook_callback` / `mcp_message` control_requests during
    // the turn, and our handlers reply on stdin. Closing too early
    // breaks the pipe mid-handler. So the order is:
    //
    // 1. Read until `Result` (with a short per-read timeout in case
    //    `drive` already drained everything inside its closure - in
    //    that case the timeout fires, we assume drain is done, and
    //    fall through to end_input).
    // 2. Close stdin so the CLI exits cleanly.
    // 3. Drain any trailing frames to EOF.
    let read_timeout = std::time::Duration::from_secs(30);
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
                    summary = Some((*num_turns, *total_cost_usd, *duration_ms));
                    break;
                }
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => {
                let log = log_arc.lock();
                let path = dump(&log, &format!("{scenario}-drain-failed"), DumpKind::Diagnostic);
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
                //     closure - common for scenarios that issue
                //     follow-up control_requests after consuming the
                //     turn (e.g. `context_usage`).
                // (b) The CLI is genuinely hung. The harness can't
                //     distinguish the two without a signal from
                //     `drive`. Log loudly so a real hang surfaces in
                //     the test output instead of silently passing as
                //     "no Result frame seen". `summary` stays `None`,
                //     which the post-drain summary line already
                //     reports as "NO Result frame".
                eprintln!(
                    "{scenario}: 30s read_timeout fired with no Result \
                     frame seen - `drive` may have drained it OR the \
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
    // handlers (`BrokenPipe`) are expected here - stdin is closed.
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

    // Successful (or at least drained) run - dump the trace, verify every
    // inbound line decodes. Failure here is a hard panic.
    let log = log_arc.lock();
    let trace_path = dump(&log, scenario, DumpKind::Promotable);
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

    Ok(Some(()))
}

/// Record every fallback the decoder took on `msg`, which decoded from
/// `line`.
///
/// `decode_dispatch` only reports a fallback for the top-level `type` and
/// for `control_request.subtype`. Every other discriminator the decoder
/// does not model is absorbed silently, in one of two ways that need
/// different detection:
///
/// - **Preserved.** `Message::System` and `ContentBlock::Unknown` keep
///   the payload verbatim, so the decoded value is indistinguishable from
///   a modelled one. Detected by asking which bucket it landed in.
/// - **Replaced.** A `#[serde(other)]` variant discards the wire value,
///   so re-encoding emits the marker instead. Detected by re-encoding and
///   comparing, which the preserved kind is invisible to because it
///   round-trips exactly.
fn record_fallbacks(msg: &forge_primitives::Message, line: &str, report: &mut DecodeReport) {
    use forge_primitives::{ContentBlock, Message};

    if let Message::System { subtype, .. } = msg
        && !EXPECTED_GENERIC_SYSTEM_SUBTYPES.contains(&subtype.as_str())
    {
        report.unmodelled_system_subtypes.push(subtype.clone());
    }

    let blocks = match msg {
        Message::Assistant { message, .. } => Some(&message.content),
        Message::User { message, .. } => Some(&message.content),
        _ => None,
    };
    for block in blocks.into_iter().flatten() {
        if let ContentBlock::Unknown { type_str, .. } = block {
            report.unmodelled_content_block_types.push(type_str.clone());
        }
    }

    let (Ok(raw), Ok(reencoded)) =
        (serde_json::from_str::<serde_json::Value>(line), serde_json::to_value(msg))
    else {
        return;
    };
    collect_catch_all_drift(&raw, &reencoded, "", &mut report.absorbed_by_catch_all);
}

/// Walk `raw` and `reencoded` together, recording every shared key whose
/// value the decoder replaced with a catch-all marker.
///
/// Only keys present on both sides are compared, so a field the decoder
/// drops is not mistaken for one it corrupted, and only a change *into* a
/// marker counts - which is why the two benign round-trip differences in
/// the committed baselines (user `content` normalising a bare string into
/// a block list, and `0` formatting as `0.0`) need no exception here.
fn collect_catch_all_drift(
    raw: &serde_json::Value,
    reencoded: &serde_json::Value,
    path: &str,
    out: &mut Vec<String>,
) {
    use serde_json::Value;
    match (raw, reencoded) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, av) in a {
                if let Some(bv) = b.get(key) {
                    collect_catch_all_drift(av, bv, &format!("{path}/{key}"), out);
                }
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (av, bv)) in a.iter().zip(b).enumerate() {
                collect_catch_all_drift(av, bv, &format!("{path}/{i}"), out);
            }
        }
        (_, Value::String(marker))
            if CATCH_ALL_MARKERS.contains(&marker.as_str()) && raw != reencoded =>
        {
            out.push(format!("{path}: {raw} -> \"{marker}\""));
        }
        _ => {}
    }
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
            Ok(DecodedLine::Message(msg)) => {
                report.messages += 1;
                record_fallbacks(&msg, line, &mut report);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialisation is the chokepoint both trace-write sites go
    /// through, so neither can put an unredacted line on disk. The
    /// owner name in the second entry is only reachable because the
    /// first entry named a home path.
    #[test]
    fn to_jsonl_redacts_every_entry() {
        let mut log = TraceLog::default();
        log.entries.push((
            "in",
            r#"{"type":"system","cwd":"/Users/alexandra/Projects/forge"}"#.to_string(),
        ));
        log.entries.push((
            "in",
            concat!(
                r#"{"type":"control_response","response":{"response":{"#,
                r#""account":{"email":"a@b.co"},"#,
                r#""note":"see ~/.claude-alt and `alexandra/proxy`"}}}"#
            )
            .to_string(),
        ));

        let body = log.to_jsonl().expect("jsonl serialise");

        for leak in ["alexandra", "a@b.co", ".claude-alt"] {
            assert!(!body.contains(leak), "to_jsonl leaked {leak}:\n{body}");
        }
    }

    /// Both halves together: what the promotion glob reaches is
    /// redacted, and what keeps its paths sits outside it. Asserted
    /// apart, either could drift alone.
    #[test]
    fn only_the_dump_outside_the_promotion_glob_keeps_its_paths() {
        let mut log = TraceLog::default();
        log.entries.push((
            "in",
            r#"{"type":"system","cwd":"/Users/alexandra/Projects/forge"}"#.to_string(),
        ));
        let cwd = "/Users/alexandra/Projects/forge";

        let promotable = log.to_jsonl_for(DumpKind::Promotable).expect("jsonl serialise");
        let diagnostic = log.to_jsonl_for(DumpKind::Diagnostic).expect("jsonl serialise");

        assert_eq!(DumpKind::Promotable.prefix(), "capture");
        assert!(!promotable.contains(cwd), "a promotable dump kept the path: {promotable}");
        assert_ne!(DumpKind::Diagnostic.prefix(), "capture");
        assert!(diagnostic.contains(cwd), "a diagnostic dump lost the path: {diagnostic}");
    }

    fn report_for(line: &str) -> DecodeReport {
        let mut log = TraceLog::default();
        log.entries.push(("in", line.to_string()));
        decode_all_inbound(&log)
    }

    /// Nothing else may fire, or the assertion above it proves nothing:
    /// a report can go dirty for a reason that has no bearing on the
    /// discriminator under test.
    fn assert_only(report: &DecodeReport, field: &str) {
        let mut others: Vec<&str> = Vec::new();
        if field != "system" && !report.unmodelled_system_subtypes.is_empty() {
            others.push("system");
        }
        if field != "block" && !report.unmodelled_content_block_types.is_empty() {
            others.push("block");
        }
        if field != "catch_all" && !report.absorbed_by_catch_all.is_empty() {
            others.push("catch_all");
        }
        assert!(report.unknown_types.is_empty(), "unknown_types fired: {report:#?}");
        assert!(report.decode_errors.is_empty(), "decode_errors fired: {report:#?}");
        assert!(others.is_empty(), "expected only {field}, also got {others:?}: {report:#?}");
    }

    const NEW_SUBTYPE: &str = r#"{"type":"system","subtype":"context_compaction_started",
        "session_id":"s1","uuid":"u1","reason":"auto"}"#;

    #[test]
    fn unmodelled_system_subtype_is_reported() {
        let report = report_for(NEW_SUBTYPE);
        assert_eq!(report.unmodelled_system_subtypes, ["context_compaction_started"]);
        assert_only(&report, "system");
        assert!(!report.is_clean());
    }

    #[test]
    fn intentionally_generic_system_subtype_stays_clean() {
        let report = report_for(r#"{"type":"system","subtype":"init","session_id":"s1"}"#);
        assert!(report.is_clean(), "{report:#?}");
        assert_eq!(report.messages, 1);
    }

    #[test]
    fn modelled_system_subtype_stays_clean() {
        let report = report_for(
            r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":50,
                "estimated_tokens_delta":50,"uuid":"u1","session_id":"s1"}"#,
        );
        assert!(report.is_clean(), "{report:#?}");
        assert!(report.unmodelled_system_subtypes.is_empty());
    }

    #[test]
    fn unmodelled_content_block_type_is_reported() {
        let report = report_for(
            r#"{"type":"assistant","session_id":"s1","parent_tool_use_id":null,
                "message":{"id":"m1","role":"assistant","model":"claude-opus-5",
                "content":[{"type":"redacted_thinking","data":"abc"}]}}"#,
        );
        assert_eq!(report.unmodelled_content_block_types, ["redacted_thinking"]);
        assert_only(&report, "block");
        assert!(!report.is_clean());
    }

    #[test]
    fn catch_all_absorbing_a_wire_value_is_reported() {
        let report = report_for(
            r#"{"type":"assistant","session_id":"s1","parent_tool_use_id":null,
                "message":{"id":"m1","role":"assistant","model":"claude-opus-5",
                "stop_reason":"refusal","content":[{"type":"text","text":"hi"}]}}"#,
        );
        assert_eq!(
            report.absorbed_by_catch_all,
            [r#"/message/stop_reason: "refusal" -> "unknown""#]
        );
        assert_only(&report, "catch_all");
        assert!(!report.is_clean());
    }

    /// The two round-trip differences the committed baselines actually
    /// contain. Neither is a catch-all, and the check must say so.
    #[test]
    fn benign_round_trip_differences_stay_clean() {
        let bare_string_content = report_for(
            r#"{"type":"user","session_id":"s1","parent_tool_use_id":null,
                "message":{"role":"user","content":"<local-command-stdout>ok</local-command-stdout>"}}"#,
        );
        assert!(bare_string_content.is_clean(), "{bare_string_content:#?}");

        let integral_cost = report_for(
            r#"{"type":"result","subtype":"success","session_id":"s1","is_error":false,
                "num_turns":1,"duration_ms":10,"duration_api_ms":9,"total_cost_usd":0}"#,
        );
        assert!(integral_cost.is_clean(), "{integral_cost:#?}");
    }

    /// The `"other"` half of [`CATCH_ALL_MARKERS`]: this one changes a
    /// nested `type` key rather than a leaf value, so it is only found
    /// because the walk descends into arrays and objects.
    #[test]
    fn nested_other_marker_is_reported() {
        let report = report_for(
            r#"{"type":"system","subtype":"task_progress","task_id":"t1",
                "description":"d","uuid":"u1","session_id":"s1",
                "usage":{"total_tokens":1,"tool_uses":2,"duration_ms":3},
                "workflow_progress":[{"type":"workflowRetry","index":0}]}"#,
        );
        assert_eq!(
            report.absorbed_by_catch_all,
            [r#"/workflow_progress/0/type: "workflowRetry" -> "other""#]
        );
        assert_only(&report, "catch_all");
        assert!(!report.is_clean());
    }

    /// `SystemRepr` is untagged, so a modelled subtype whose payload
    /// drifts falls through to the generic bucket rather than erroring.
    /// This is what makes an allowlist entry stop being harmless once
    /// its subtype grows a typed variant, and the reason
    /// `EXPECTED_GENERIC_SYSTEM_SUBTYPES` says to drop the entry in the
    /// same change.
    #[test]
    fn modelled_subtype_with_a_drifted_payload_is_reported() {
        // thinking_tokens without its required estimated_tokens field.
        let report = report_for(
            r#"{"type":"system","subtype":"thinking_tokens",
                "estimated_tokens_delta":50,"uuid":"u1","session_id":"s1"}"#,
        );
        assert_eq!(report.unmodelled_system_subtypes, ["thinking_tokens"]);
        assert_only(&report, "system");
        assert!(!report.is_clean());
    }

    /// Guards the shape the whole thing exists for: the frame that now
    /// goes red used to be counted as an ordinary message.
    #[test]
    fn an_unmodelled_subtype_still_decodes_as_a_plain_message() {
        let report = report_for(NEW_SUBTYPE);
        assert_eq!(report.messages, 1);
        assert!(report.unknown_types.is_empty());
    }
}

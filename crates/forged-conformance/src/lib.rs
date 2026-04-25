//! Wire-conformance harness for forged. Mirrors the design of
//! `crates/forge-conformance/` (which captures the forge-sdk ↔ claude
//! wire); here we capture the forged ↔ client wire.
//!
//! ## Two modes
//!
//! - **Live capture** (`FORGED_WIRE_CAPTURE=1`) — spin up a real forged
//!   on an ephemeral port, drive the scenario, dump the bidirectional
//!   trace to `target/forged-wire-traces/`. Promote a captured file
//!   into `baselines/<forged-version>/<scenario>.jsonl` to lock the
//!   baseline.
//! - **Replay** (default) — load the committed baseline and run typed
//!   wire-shape decode through forged's JSON-RPC framing types.
//!   Replay keeps the harness on the `cargo nextest run` happy-path
//!   with no external dependencies.
//!
//! ## Typed decoder
//!
//! The replay path doesn't only check JSON-RPC framing — it dispatches
//! every inbound request on `method` and validates the params against
//! the daemon's wire-shape param structs (`SendUserMessageParams`,
//! `SubscribeParams`, etc.) re-exported via `forged::methods::session`
//! et al. Outbound responses correlate by `id` against the inbound
//! request's method — once we know the method we know the result
//! shape. Notifications dispatch on `method` too. Anything that fails
//! to decode is recorded as a [`DecodeReport::failures`] entry, and
//! [`DecodeReport::is_clean`] returns false. CLI invariant #10(c)
//! mandates clean decode for committed baselines.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::path::PathBuf;

/// Forged version baselines were captured against. Bumped in lockstep
/// with `Cargo.toml` workspace version. When this changes a fresh
/// baseline directory is created and live-capture re-runs are needed.
pub const PINNED_FORGED_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Path to the directory holding committed baselines for the current
/// pinned forged version.
#[must_use]
pub fn baseline_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("baselines")
        .join(PINNED_FORGED_VERSION)
}

/// Load a committed baseline trace from `baselines/<version>/<scenario>.jsonl`.
///
/// # Panics
///
/// Panics if the baseline file is missing or any line fails to parse —
/// these are wire-conformance contract violations and crashing loudly
/// is the correct response.
#[must_use]
pub fn load_baseline(scenario: &str) -> Vec<TraceEntry> {
    let path = baseline_dir().join(format!("{scenario}.jsonl"));
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing baseline for '{scenario}' at {}: {e}",
            path.display()
        )
    });
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("parse trace line: {e} :: {l}")))
        .collect()
}

/// One line of a captured forged wire trace.
///
/// `dir` is "in" or "out" from the daemon's perspective:
///
/// - "out" — daemon → client (notifications, responses, reverse-RPC
///   requests).
/// - "in"  — client → daemon (requests, reverse-RPC responses).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraceEntry {
    /// Direction marker — "in" or "out" from the daemon's perspective.
    pub dir: String,
    /// JSON-encoded body of the line as it appeared on the wire.
    pub line: String,
}

/// Outcome of decoding a baseline trace through the typed framing.
#[derive(Debug, Default)]
pub struct DecodeReport {
    /// Lines that decoded cleanly through the typed dispatcher,
    /// counting successes per `(direction, method-or-result-shape)`.
    pub successes: usize,
    /// One entry per line that failed typed decode.
    pub failures: Vec<DecodeFailure>,
    /// Inbound requests/notifications whose `method` is not recognised
    /// by the dispatcher — the dispatcher's white-list was incomplete
    /// or the daemon emitted a method this version of the harness
    /// doesn't know about. Treated as a failure.
    pub unknown_methods: Vec<UnknownMethodEntry>,
}

impl DecodeReport {
    /// True iff every line decoded against a known typed shape and
    /// every method dispatched into the white-list.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty() && self.unknown_methods.is_empty()
    }
}

/// One decode failure. Carries the line index and the offending
/// reason so test assertions can pinpoint the exact wire frame.
#[derive(Debug, Clone)]
pub struct DecodeFailure {
    /// Zero-indexed line number in the trace.
    pub line: usize,
    /// "in" or "out" — direction marker from the trace entry.
    pub dir: String,
    /// The frame's `method` (or `<response>` for response frames).
    pub method: String,
    /// Why the decode failed (typically `serde_json::Error::to_string`).
    pub reason: String,
    /// The raw line for context.
    pub raw: String,
}

/// One unknown-method entry. Dispatching on the method failed: the
/// frame is structurally a JSON-RPC request/notification but the
/// dispatcher's white-list does not know how to type-check it.
#[derive(Debug, Clone)]
pub struct UnknownMethodEntry {
    /// Zero-indexed line number in the trace.
    pub line: usize,
    /// "in" or "out" — direction marker from the trace entry.
    pub dir: String,
    /// The unknown method.
    pub method: String,
}

/// Run the full typed-decode dispatcher over every entry in the
/// trace. Returns the aggregated [`DecodeReport`].
///
/// # Panics
///
/// On a malformed JSON line — the trace was supposed to contain wire
/// frames, so non-JSON content is a structural violation. Tests should
/// already have asserted that, but this is the second guard.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "method-by-method dispatch is naturally long"
)]
pub fn decode_full(entries: &[TraceEntry]) -> DecodeReport {
    let mut report = DecodeReport::default();
    // Track outbound requests by id so responses can correlate with
    // their method. Requests are issued by either the client (inbound
    // dir = "in") or the daemon (outbound dir = "out", reverse-RPC).
    let mut id_to_method: HashMap<String, String> = HashMap::new();

    for (i, e) in entries.iter().enumerate() {
        let v: serde_json::Value = match serde_json::from_str(&e.line) {
            Ok(v) => v,
            Err(err) => {
                report.failures.push(DecodeFailure {
                    line: i,
                    dir: e.dir.clone(),
                    method: "<unparsable>".into(),
                    reason: err.to_string(),
                    raw: e.line.clone(),
                });
                continue;
            }
        };

        let has_method = v.get("method").is_some();
        let has_id = v.get("id").is_some();
        let has_result_or_error = v.get("result").is_some() || v.get("error").is_some();

        if has_method && has_id {
            // Request — record id→method for response correlation.
            let method = v["method"].as_str().unwrap_or("").to_string();
            if let Some(id) = v.get("id").and_then(serde_json::Value::as_str) {
                id_to_method.insert(id.to_string(), method.clone());
            } else if let Some(id_n) = v.get("id").and_then(serde_json::Value::as_i64) {
                id_to_method.insert(id_n.to_string(), method.clone());
            }
            // Validate params shape against the dispatcher.
            let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);
            match decode_request_params(&method, &params) {
                Ok(()) => report.successes += 1,
                Err(KnownReason::Unknown) => {
                    report.unknown_methods.push(UnknownMethodEntry {
                        line: i,
                        dir: e.dir.clone(),
                        method,
                    });
                }
                Err(KnownReason::Failed(msg)) => {
                    report.failures.push(DecodeFailure {
                        line: i,
                        dir: e.dir.clone(),
                        method,
                        reason: msg,
                        raw: e.line.clone(),
                    });
                }
            }
        } else if has_method {
            // Notification.
            let method = v["method"].as_str().unwrap_or("").to_string();
            let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);
            match decode_notification_params(&method, &params) {
                Ok(()) => report.successes += 1,
                Err(KnownReason::Unknown) => {
                    report.unknown_methods.push(UnknownMethodEntry {
                        line: i,
                        dir: e.dir.clone(),
                        method,
                    });
                }
                Err(KnownReason::Failed(msg)) => {
                    report.failures.push(DecodeFailure {
                        line: i,
                        dir: e.dir.clone(),
                        method,
                        reason: msg,
                        raw: e.line.clone(),
                    });
                }
            }
        } else if has_id && has_result_or_error {
            // Response — correlate to the original method via id.
            let id = if let Some(s) = v.get("id").and_then(serde_json::Value::as_str) {
                s.to_string()
            } else if let Some(n) = v.get("id").and_then(serde_json::Value::as_i64) {
                n.to_string()
            } else {
                report.failures.push(DecodeFailure {
                    line: i,
                    dir: e.dir.clone(),
                    method: "<response>".into(),
                    reason: "response carried non-string non-int id".into(),
                    raw: e.line.clone(),
                });
                continue;
            };
            let method = id_to_method.remove(&id).unwrap_or_default();
            if v.get("error").is_some() {
                // Error response — only structural validation: code +
                // message must decode as the standard JSON-RPC envelope.
                if let Err(err) = serde_json::from_value::<JsonRpcErrorEnvelope>(v.clone()) {
                    report.failures.push(DecodeFailure {
                        line: i,
                        dir: e.dir.clone(),
                        method: format!("error[{method}]"),
                        reason: err.to_string(),
                        raw: e.line.clone(),
                    });
                } else {
                    report.successes += 1;
                }
                continue;
            }
            // Success response — typed decode against the result shape.
            let result = v.get("result").cloned().unwrap_or(serde_json::Value::Null);
            match decode_response_result(&method, &result) {
                Ok(()) => report.successes += 1,
                Err(KnownReason::Unknown) => {
                    // Method we didn't see the original request for —
                    // can happen if the trace was clipped. Treat as
                    // unknown rather than a hard failure.
                    report.unknown_methods.push(UnknownMethodEntry {
                        line: i,
                        dir: e.dir.clone(),
                        method: format!("response[{method}]"),
                    });
                }
                Err(KnownReason::Failed(msg)) => {
                    report.failures.push(DecodeFailure {
                        line: i,
                        dir: e.dir.clone(),
                        method: format!("response[{method}]"),
                        reason: msg,
                        raw: e.line.clone(),
                    });
                }
            }
        } else {
            report.failures.push(DecodeFailure {
                line: i,
                dir: e.dir.clone(),
                method: "<unknown>".into(),
                reason: "no method/result/error/notification shape".into(),
                raw: e.line.clone(),
            });
        }
    }
    report
}

/// Internal failure reason — Unknown means the dispatcher's white-list
/// doesn't know the method; Failed means the method is known but the
/// params/result didn't decode.
enum KnownReason {
    Unknown,
    Failed(String),
}

/// Validate a request's `params` against the daemon's typed shapes.
fn decode_request_params(method: &str, params: &serde_json::Value) -> Result<(), KnownReason> {
    use serde_json::from_value;
    match method {
        // Empty-params methods — accept any object/null.
        "daemon.status" | "session.peers" => Ok(()),
        // Session methods.
        "session.spawn" => {
            // Params shape is the wire-Options blob; re-validate via
            // the public dispatcher entry point.
            let _ = forged::methods::session::parse_spawn_params(params)
                .map_err(|e| KnownReason::Failed(e.to_string()))?;
            Ok(())
        }
        "session.send_user_message" => from_value::<RequestSendUserMessage>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "session.subscribe" => from_value::<RequestSubscribe>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "session.unsubscribe"
        | "session.disconnect"
        | "session.end_input"
        | "session.interrupt"
        | "session.claim_primary"
        | "mcp.status"
        | "context.get" => from_value::<RequestSessionIdOnly>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        // Multi-client / prompts.
        "prompts.respond" => from_value::<RequestPromptsRespond>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        // Filesystem listing.
        "sessions.list" => from_value::<RequestSessionsList>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "sessions.info" | "sessions.messages" | "sessions.delete" | "sessions.list_subagents" => {
            from_value::<RequestSessionsInfo>(params.clone())
                .map(|_| ())
                .map_err(|e| KnownReason::Failed(e.to_string()))
        }
        // Reverse-RPC outbound requests issued by the daemon.
        m if m.starts_with("hook.") || m == "permission.request" => {
            // These are shape-loose — the params carry the SDK-side
            // input + context as nested JSON. Just sanity-check that
            // params is an object.
            if params.is_object() {
                Ok(())
            } else {
                Err(KnownReason::Failed(format!(
                    "expected object params for {method}, got {params}"
                )))
            }
        }
        _ => Err(KnownReason::Unknown),
    }
}

fn decode_notification_params(method: &str, params: &serde_json::Value) -> Result<(), KnownReason> {
    use serde_json::from_value;
    match method {
        "client.identify" => from_value::<NotifyClientIdentify>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "session.event" => from_value::<NotifySessionEvent>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "session.role_assigned" => from_value::<NotifyRoleAssigned>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "session.primary_changed" => from_value::<NotifyPrimaryChanged>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "session.closed" => from_value::<NotifySessionClosed>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "prompts.expired" => from_value::<NotifyPromptsExpired>(params.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        _ => Err(KnownReason::Unknown),
    }
}

fn decode_response_result(method: &str, result: &serde_json::Value) -> Result<(), KnownReason> {
    use serde_json::from_value;
    match method {
        "" => Err(KnownReason::Unknown),
        // `DaemonStatus` carries `version: &'static str` so it cannot
        // round-trip via Value → struct (Deserialize implementation is
        // only valid for `'static`). Validate the shape with a
        // local owned-strings mirror instead.
        "daemon.status" => from_value::<ResponseDaemonStatus>(result.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "session.spawn" => from_value::<forged::methods::session::SpawnResult>(result.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        "session.subscribe" => {
            from_value::<forged::methods::session::SubscribeResult>(result.clone())
                .map(|_| ())
                .map_err(|e| KnownReason::Failed(e.to_string()))
        }
        "session.peers" => from_value::<forged::methods::multi_client::PeersResult>(result.clone())
            .map(|_| ())
            .map_err(|e| KnownReason::Failed(e.to_string())),
        // No-content methods — null result expected.
        "session.send_user_message"
        | "session.unsubscribe"
        | "session.disconnect"
        | "session.end_input"
        | "session.interrupt"
        | "session.claim_primary"
        | "prompts.respond" => {
            if result.is_null() {
                Ok(())
            } else {
                Err(KnownReason::Failed(format!(
                    "expected null for {method}, got {result}"
                )))
            }
        }
        // Reverse-RPC responses — answers from the client to a
        // daemon-issued reverse request. Shape varies; do a
        // permissive object/null check.
        m if m == "permission.request" || m.starts_with("hook.") => {
            if result.is_object() || result.is_null() {
                Ok(())
            } else {
                Err(KnownReason::Failed(format!(
                    "expected object/null result for reverse-RPC {method}, got {result}"
                )))
            }
        }
        _ => Err(KnownReason::Unknown),
    }
}

/// Owned-strings mirror of [`forged::methods::daemon::DaemonStatus`] —
/// the original carries `version: &'static str` so it cannot round-
/// trip through `Value → struct`. Field-equivalent for shape checks.
#[derive(serde::Deserialize)]
struct ResponseDaemonStatus {
    #[allow(dead_code)]
    uptime_seconds: u64,
    #[allow(dead_code)]
    active_sessions: usize,
    #[allow(dead_code)]
    connected_clients: usize,
    #[serde(default)]
    #[allow(dead_code)]
    last_error: Option<String>,
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    build: String,
    #[serde(default)]
    #[allow(dead_code)]
    wg_ip_bound: Option<String>,
}

// =============================================================================
// Re-declarations of wire-shape param structs that live inside
// `forged`'s dispatcher as private types. Mirroring them here keeps the
// harness readable without leaking the dispatcher's privates.
// =============================================================================

#[derive(serde::Deserialize)]
struct RequestSessionIdOnly {
    #[allow(dead_code)]
    session_id: String,
}

#[derive(serde::Deserialize)]
struct RequestSendUserMessage {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    prompt: String,
}

#[derive(serde::Deserialize)]
struct RequestSubscribe {
    #[allow(dead_code)]
    session_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    since: Option<String>,
}

#[derive(serde::Deserialize)]
struct RequestPromptsRespond {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    prompt_id: String,
    #[allow(dead_code)]
    result: serde_json::Value,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct RequestSessionsList {
    #[allow(dead_code)]
    directory: Option<String>,
    #[allow(dead_code)]
    limit: Option<usize>,
    #[allow(dead_code)]
    offset: usize,
}

#[derive(serde::Deserialize)]
struct RequestSessionsInfo {
    #[allow(dead_code)]
    session_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    directory: Option<String>,
}

#[derive(serde::Deserialize)]
struct NotifyClientIdentify {
    #[allow(dead_code)]
    connection_id: String,
    #[allow(dead_code)]
    server_version: String,
    #[allow(dead_code)]
    server_build: String,
}

#[derive(serde::Deserialize)]
struct NotifySessionEvent {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    event_id: serde_json::Value,
    #[allow(dead_code)]
    message: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct NotifyRoleAssigned {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    role: String,
    #[allow(dead_code)]
    primary: serde_json::Value,
    #[allow(dead_code)]
    reason: String,
}

#[derive(serde::Deserialize)]
struct NotifyPrimaryChanged {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    primary: serde_json::Value,
    #[serde(default)]
    #[allow(dead_code)]
    previous: serde_json::Value,
    #[allow(dead_code)]
    reason: String,
}

#[derive(serde::Deserialize)]
struct NotifySessionClosed {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    reason: String,
}

#[derive(serde::Deserialize)]
struct NotifyPromptsExpired {
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    prompt_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    reason: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    fallback: Option<String>,
}

#[derive(serde::Deserialize)]
struct JsonRpcErrorEnvelope {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: serde_json::Value,
    #[allow(dead_code)]
    error: JsonRpcErrorObject,
}

#[derive(serde::Deserialize)]
struct JsonRpcErrorObject {
    #[allow(dead_code)]
    code: i64,
    #[allow(dead_code)]
    message: String,
    #[serde(default)]
    #[allow(dead_code)]
    data: Option<serde_json::Value>,
}

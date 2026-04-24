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
            Ok(DecodedLine::Unknown { type_str, .. }) => {
                report.unknown_types.push(type_str);
            }
            Err(e) => report.decode_errors.push((idx, format!("{e}"))),
        }
    }
    report
}

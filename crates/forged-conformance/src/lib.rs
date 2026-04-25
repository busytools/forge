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
//! - **Replay** (default) — load the committed baseline and assert
//!   every line is structurally a valid JSON-RPC frame (request,
//!   notification, or response). Replay keeps the harness on the
//!   `cargo nextest run` happy-path with no external dependencies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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

//! Opt-in LIVE engine parity matrix: `claude` vs the certified
//! `claude-headless` port.
//!
//! Each test is one check in the matrix. It runs the same session on both
//! binaries and fails with `field: reference=<v> port=<v>` for every answer
//! that differs. Run with `just engine-parity` (all checks) or
//! `just engine-parity <check_suffix>` (one check). Never part of `just
//! check` or CI: both binaries and a real model call are required, and a test
//! skips with a clear message when `claude-headless` is not on PATH.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::future::Future;

use forge_test_harness::engine_parity::{
    PORT_BINARY, REFERENCE_BINARY, capture_context_usage, capture_control_protocol, capture_hooks,
    capture_init_frame, capture_model_resolution, capture_permission, capture_streaming_shapes,
    capture_usage_and_cost, compare, unavailable_reason,
};

/// Run one check on both engines and compare. Skipped, not failed, when
/// either binary is missing.
async fn run_check<F, Fut>(check: &str, capture: F)
where
    F: Fn(&'static str) -> Fut,
    Fut: Future<Output = Result<serde_json::Value, String>>,
{
    if let Some(reason) = unavailable_reason() {
        eprintln!("engine-parity {check}: skipped - {reason}");
        return;
    }
    eprintln!("engine-parity {check}: running {REFERENCE_BINARY} then {PORT_BINARY}");
    let reference = capture(REFERENCE_BINARY)
        .await
        .unwrap_or_else(|error| panic!("engine-parity {check}: reference capture failed: {error}"));
    let port = capture(PORT_BINARY)
        .await
        .unwrap_or_else(|error| panic!("engine-parity {check}: port capture failed: {error}"));
    compare(check, &reference, &port);
}

#[tokio::test]
#[ignore = "live parity matrix: needs both engines on PATH and a real model call; run `just engine-parity`"]
async fn engine_parity_init_frame() {
    run_check("init_frame", capture_init_frame).await;
}

#[tokio::test]
#[ignore = "live parity matrix: needs both engines on PATH and a real model call; run `just engine-parity`"]
async fn engine_parity_context_usage() {
    run_check("context_usage", capture_context_usage).await;
}

#[tokio::test]
#[ignore = "live parity matrix: needs both engines on PATH and a real model call; run `just engine-parity`"]
async fn engine_parity_usage_and_cost() {
    run_check("usage_and_cost", capture_usage_and_cost).await;
}

#[tokio::test]
#[ignore = "live parity matrix: needs both engines on PATH and a real model call; run `just engine-parity`"]
async fn engine_parity_model_resolution() {
    run_check("model_resolution", capture_model_resolution).await;
}

#[tokio::test]
#[ignore = "live parity matrix: needs both engines on PATH and a real model call; run `just engine-parity`"]
async fn engine_parity_streaming_shapes() {
    run_check("streaming_shapes", capture_streaming_shapes).await;
}

#[tokio::test]
#[ignore = "live parity matrix: needs both engines on PATH and a real model call; run `just engine-parity`"]
async fn engine_parity_control_protocol() {
    run_check("control_protocol", capture_control_protocol).await;
}

#[tokio::test]
#[ignore = "live parity matrix: needs both engines on PATH and a real model call; run `just engine-parity`"]
async fn engine_parity_hooks() {
    run_check("hooks", capture_hooks).await;
}

#[tokio::test]
#[ignore = "live parity matrix: needs both engines on PATH and a real model call; run `just engine-parity`"]
async fn engine_parity_permission() {
    run_check("permission", capture_permission).await;
}

//! Replay every committed baseline through forged's JSON-RPC framing
//! and assert each line is structurally a valid JSON-RPC frame AND
//! decodes cleanly through the typed dispatcher.
//!
//! This is the always-on conformance gate — runs on every
//! `cargo nextest run` / `just check`, no external dependencies.
//!
//! Per CLAUDE.md invariant #10(c), every committed baseline must
//! round-trip through `forged_conformance::decode_full` with no
//! decode failures and no unknown-method dispatches.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forged_conformance::{baseline_dir, decode_full, load_baseline};

#[test]
fn m1_status_baseline_decodes_cleanly() {
    let entries = load_baseline("m1_status");
    assert!(
        !entries.is_empty(),
        "m1_status baseline must be non-empty (capture via FORGED_WIRE_CAPTURE=1 capture_m1_status)"
    );
    for (i, e) in entries.iter().enumerate() {
        let v: serde_json::Value = serde_json::from_str(&e.line)
            .unwrap_or_else(|err| panic!("line {i} ({}) failed to parse: {err}", e.dir));

        let has_method = v.get("method").is_some();
        let has_result = v.get("result").is_some();
        let has_error = v.get("error").is_some();
        assert!(
            has_method || has_result || has_error,
            "line {i} ({}) is none of method/result/error: {}",
            e.dir,
            e.line
        );

        // jsonrpc marker must be present and equal "2.0".
        let marker = v.get("jsonrpc").and_then(|j| j.as_str());
        assert_eq!(
            marker,
            Some("2.0"),
            "line {i} ({}) missing or wrong jsonrpc marker: {}",
            e.dir,
            e.line
        );
    }

    // Typed decode round-trip.
    let report = decode_full(&entries);
    assert!(
        report.is_clean(),
        "m1_status baseline failed typed decode: {} successes, {} failures, {} unknown methods\nfailures: {:#?}\nunknown: {:#?}",
        report.successes,
        report.failures.len(),
        report.unknown_methods.len(),
        report.failures,
        report.unknown_methods,
    );
}

#[test]
fn all_baselines_decode_cleanly() {
    let dir = baseline_dir();
    if !dir.exists() {
        eprintln!(
            "no baselines directory at {} — skipping (run a live capture first)",
            dir.display()
        );
        return;
    }

    let mut scenarios: Vec<String> = std::fs::read_dir(&dir)
        .expect("read baseline_dir")
        .filter_map(std::result::Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".jsonl").map(str::to_string)
        })
        .collect();
    scenarios.sort();

    if scenarios.is_empty() {
        eprintln!(
            "no baselines in {} — skipping. Capture some with FORGED_WIRE_CAPTURE=1.",
            dir.display()
        );
        return;
    }

    for scenario in &scenarios {
        let entries = load_baseline(scenario);
        for (i, e) in entries.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(&e.line).unwrap_or_else(|err| {
                panic!("[{scenario}] line {i} ({}) failed to parse: {err}", e.dir)
            });
            let has_method = v.get("method").is_some();
            let has_result = v.get("result").is_some();
            let has_error = v.get("error").is_some();
            assert!(
                has_method || has_result || has_error,
                "[{scenario}] line {i} ({}) not method/result/error: {}",
                e.dir,
                e.line
            );
        }
        // Typed decode for this scenario.
        let report = decode_full(&entries);
        assert!(
            report.is_clean(),
            "[{scenario}] failed typed decode: {} successes, {} failures, {} unknown methods\nfailures: {:#?}\nunknown: {:#?}",
            report.successes,
            report.failures.len(),
            report.unknown_methods.len(),
            report.failures,
            report.unknown_methods,
        );
    }
}

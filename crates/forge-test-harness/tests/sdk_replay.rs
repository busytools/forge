//! Replay every committed baseline through `decode_dispatch` and assert
//! zero decode failures. Runs on every `cargo test` / `just check` -
//! no API cost.
//!
//! This is the "always-on" half of the conformance story:
//! - Live capture mode (`FORGE_WIRE_CAPTURE=1`, the `--ignored` tests)
//!   re-records baselines against the current pinned CLI version.
//! - This file loads each committed baseline and verifies every inbound
//!   line still decodes cleanly via forge-sdk's decoder. If forge-sdk's
//!   decoder regresses, this test fails loudly.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_test_harness::sdk_wire::{
    PINNED_CLI_VERSION, baseline_dir, decode_all_inbound, load_baseline,
};

fn committed_scenarios() -> Vec<String> {
    let dir = baseline_dir();
    if !dir.exists() {
        return Vec::new();
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
    scenarios
}

/// Replay otherwise reads inbound lines only, so nothing offline asserts a
/// byte forge puts on the wire. Correlates each captured `initialize`
/// response back to its request by `request_id` and checks the version we
/// answered is the one that was asked for.
#[test]
fn initialize_answers_the_requested_protocol_version() {
    let mut checked = 0usize;

    for scenario in committed_scenarios() {
        let log = load_baseline(&scenario);

        let mut requested: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for line in log.inbound() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let msg = &v["request"]["message"];
            if v["request"]["subtype"] != "mcp_message" || msg["method"] != "initialize" {
                continue;
            }
            let (Some(id), Some(want)) =
                (v["request_id"].as_str(), msg["params"]["protocolVersion"].as_str())
            else {
                continue;
            };
            requested.insert(id.to_owned(), want.to_owned());
        }

        for line in log.outbound() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let resp = &v["response"];
            let Some(answered) =
                resp["response"]["mcp_response"]["result"]["protocolVersion"].as_str()
            else {
                continue;
            };
            let id = resp["request_id"].as_str().unwrap_or_default();
            let want = requested.get(id).unwrap_or_else(|| {
                panic!("{scenario}: initialize response {id} has no matching request")
            });
            assert_eq!(answered, want, "{scenario}: answered {answered}, CLI asked for {want}");
            checked += 1;
        }
    }

    // Without this the test passes vacuously the moment the correlation stops
    // matching - the exact failure shape it exists to catch.
    assert!(checked > 0, "no initialize handshake found in any committed baseline");
}

#[test]
fn all_baselines_decode_cleanly() {
    let dir = baseline_dir();
    if !dir.exists() {
        eprintln!(
            "no baselines directory at {} - skipping (run a live capture first)",
            dir.display()
        );
        return;
    }

    let scenarios = committed_scenarios();

    if scenarios.is_empty() {
        eprintln!(
            "no baselines in {} - skipping. Capture some with FORGE_WIRE_CAPTURE=1.",
            dir.display()
        );
        return;
    }

    let mut failures: Vec<(String, String)> = Vec::new();
    let mut summary: Vec<(String, usize, usize, usize, usize)> = Vec::new();

    for scenario in &scenarios {
        let log = load_baseline(scenario);
        let report = decode_all_inbound(&log);
        summary.push((
            scenario.clone(),
            report.messages,
            report.controls,
            report.control_cancels,
            report.control_responses,
        ));
        if !report.is_clean() {
            failures.push((scenario.clone(), format!("{report:#?}")));
        }
    }

    eprintln!("pinned CLI version: {PINNED_CLI_VERSION}");
    eprintln!(
        "scenarios: {} | decode summary (messages / controls / cancels / responses):",
        scenarios.len()
    );
    for (name, m, c, cc, cr) in &summary {
        eprintln!("  {name}: {m}m / {c}c / {cc}cc / {cr}cr");
    }

    if !failures.is_empty() {
        eprintln!("\n{} scenario(s) failed decode-completeness:", failures.len());
        for (scen, rpt) in &failures {
            eprintln!("--- {scen} ---\n{rpt}\n");
        }
        panic!(
            "decode completeness regressions detected - either forge-sdk's decoder drifted \
             or the CLI's wire shape changed and we need to recapture baselines."
        );
    }
}

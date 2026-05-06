//! Replay every committed baseline through `decode_dispatch` and assert
//! zero decode failures. Runs on every `cargo test` / `just check` —
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
            "no baselines in {} — skipping. Capture some with FORGE_WIRE_CAPTURE=1.",
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
            "decode completeness regressions detected — either forge-sdk's decoder drifted \
             or the CLI's wire shape changed and we need to recapture baselines."
        );
    }
}

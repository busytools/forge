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

use forge_sdk::mcp::McpServerBuilder;
use forge_sdk::mcp::protocol::JsonRpcRequest;
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
/// byte forge puts on the wire.
///
/// Two different things are checked per handshake, and only the second
/// catches a regression made after the baseline was captured: that the
/// recorded answer agrees with the recorded request, and that **the current
/// code still produces that answer when handed that request**. Comparing the
/// two recorded lines alone is a self-consistency check - it would stay green
/// while the server was rewritten to answer anything at all.
///
/// Expect this to fail between a `PINNED_CLI_VERSION` bump and the re-capture
/// that follows it; `.claude/skills/claude-cli-upgrade/` says not to run
/// replay in that window.
#[tokio::test]
async fn initialize_answers_the_requested_protocol_version() {
    let mut total = 0usize;

    for scenario in committed_scenarios() {
        let log = load_baseline(&scenario);

        let mut requests: std::collections::HashMap<String, JsonRpcRequest> =
            std::collections::HashMap::new();
        for line in log.inbound() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let msg = &v["request"]["message"];
            if v["request"]["subtype"] != "mcp_message" || msg["method"] != "initialize" {
                continue;
            }
            let (Some(id), Ok(req)) =
                (v["request_id"].as_str(), serde_json::from_value::<JsonRpcRequest>(msg.clone()))
            else {
                continue;
            };
            requests.insert(id.to_owned(), req);
        }

        let mut checked = 0usize;
        for line in log.outbound() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let resp = &v["response"];
            let mcp = &resp["response"]["mcp_response"];
            let Some(recorded) = mcp["result"]["protocolVersion"].as_str() else { continue };

            let id = resp["request_id"].as_str().unwrap_or_default();
            let req = requests.get(id).unwrap_or_else(|| {
                panic!("{scenario}: initialize response {id} has no matching request")
            });

            let asked = req.params.as_ref().and_then(|p| p["protocolVersion"].as_str());
            assert_eq!(Some(recorded), asked, "{scenario}: capture answers a version nobody asked");
            assert_eq!(mcp.get("id"), req.id.as_ref(), "{scenario}: inner JSON-RPC id differs");

            let info = &mcp["result"]["serverInfo"];
            let live = McpServerBuilder::new(
                info["name"].as_str().unwrap_or_default(),
                info["version"].as_str().unwrap_or_default(),
            )
            .build()
            .dispatch(req)
            .await
            .expect("initialize always answers");
            let live = serde_json::to_value(&live).expect("serialise");
            assert_eq!(
                live["result"]["protocolVersion"].as_str(),
                Some(recorded),
                "{scenario}: the code no longer answers what the baseline captured"
            );
            checked += 1;
        }

        assert_eq!(
            checked,
            requests.len(),
            "{scenario}: {} initialize request(s) captured but {checked} answer(s) verified",
            requests.len()
        );
        total += checked;
    }

    // Guards the whole thing going quiet if the correlation ever stops
    // matching - the exact failure shape it exists to catch.
    assert!(total > 0, "no initialize handshake found in any committed baseline");
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

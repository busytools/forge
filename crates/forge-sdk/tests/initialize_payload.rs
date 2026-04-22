//! Verifies the `initialize` `control_request` body forge-sdk sends
//! matches Python SDK v0.1.64 `_internal/query.py:196-207`:
//!
//! - `hooks` — always present; value is `null` when no callbacks.
//! - `agents` — omitted unless callers configured agents.
//! - `excludeDynamicSections` — omitted unless explicitly set.
//! - `skills` — omitted unless a concrete list (not empty / not `"all"`).
//!
//! A spawn-time fixture captures the initialize frame to a tempfile;
//! tests parse it and assert field presence.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;

use forge_sdk::{Client, OptionsBuilder};
use serde_json::Value;

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

async fn capture_init(apply: impl FnOnce(OptionsBuilder) -> OptionsBuilder) -> Value {
    let dir = tempfile::tempdir().expect("tempdir");
    let dump = dir.path().join("init.json");
    let mut builder = OptionsBuilder::new().binary(fixture("mock_claude_capture_init.sh"));
    builder = builder.env(
        "FORGE_TEST_INIT_CAPTURE",
        dump.to_string_lossy().into_owned(),
    );
    builder = apply(builder);
    let opts = builder.build();
    let client = Client::spawn(opts).await.expect("spawn");
    client.disconnect().await.expect("disconnect");
    let body = fs::read_to_string(&dump).expect("init captured");
    let value: Value = serde_json::from_str(&body).expect("decode");
    value
        .get("request")
        .cloned()
        .expect("request field present")
}

#[tokio::test]
async fn default_init_omits_conditional_fields() {
    let req = capture_init(|b| b).await;
    // hooks is always present — null when no callbacks registered.
    assert!(req.get("hooks").is_some(), "hooks must be present");
    assert!(req["hooks"].is_null(), "hooks must be null when empty");
    // The conditional fields must NOT appear.
    assert!(
        req.get("agents").is_none(),
        "agents must be omitted when no agents configured, got {req:?}"
    );
    assert!(
        req.get("excludeDynamicSections").is_none(),
        "excludeDynamicSections must be omitted when unset, got {req:?}"
    );
    assert!(
        req.get("skills").is_none(),
        "skills must be omitted when no concrete list, got {req:?}"
    );
}

#[tokio::test]
async fn exclude_dynamic_sections_when_set_is_included() {
    let req = capture_init(|b| b.exclude_dynamic_sections(true)).await;
    assert_eq!(req["excludeDynamicSections"], true);
}

#[tokio::test]
async fn exclude_dynamic_sections_via_preset_wins() {
    use forge_sdk::SystemPromptKind;
    let req = capture_init(|b| {
        // Top-level is false, preset flips to true — preset wins per
        // Python types.py:43-66 (preset is the canonical path).
        b.exclude_dynamic_sections(false)
            .system_prompt(SystemPromptKind::Preset {
                append: None,
                exclude_dynamic_sections: Some(true),
            })
    })
    .await;
    assert_eq!(req["excludeDynamicSections"], true);
}

#[tokio::test]
async fn skills_all_marker_omits_field() {
    // 'all' sentinel maps to --allowedTools only per Python; initialize
    // payload must NOT contain the skills list.
    let req = capture_init(|b| b.skills(["all"])).await;
    assert!(
        req.get("skills").is_none(),
        "'all' sentinel must stay out of initialize.skills"
    );
}

#[tokio::test]
async fn skills_concrete_list_is_included() {
    let req = capture_init(|b| b.skills(["create-story", "other-skill"])).await;
    let list = req["skills"].as_array().expect("skills is array");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0], "create-story");
    assert_eq!(list[1], "other-skill");
}

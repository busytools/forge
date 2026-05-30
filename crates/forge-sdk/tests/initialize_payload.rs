//! Verifies the `initialize` `control_request` body forge-sdk sends
//! matches Python SDK v0.1.64 `_internal/query.py:196-207`:
//!
//! - `hooks` - always present; value is `null` when no callbacks.
//! - `agents` - omitted unless callers configured agents.
//! - `excludeDynamicSections` - omitted unless explicitly set.
//! - `skills` - omitted unless a concrete list (not empty / not `"all"`).
//!
//! A spawn-time fixture captures the initialize frame to a tempfile;
//! tests parse it and assert field presence.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

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
    builder = builder.env("FORGE_TEST_INIT_CAPTURE", dump.to_string_lossy().into_owned());
    builder = apply(builder);
    let opts = builder.build();
    let (client, _events) = Client::spawn(opts).await.expect("spawn");
    client.disconnect().await.expect("disconnect");
    let body = fs::read_to_string(&dump).expect("init captured");
    let value: Value = serde_json::from_str(&body).expect("decode");
    value.get("request").cloned().expect("request field present")
}

#[tokio::test]
async fn default_init_omits_conditional_fields() {
    let req = capture_init(|b| b).await;
    // hooks is always present - null when no callbacks registered.
    assert!(req.get("hooks").is_some(), "hooks must be present");
    assert!(req["hooks"].is_null(), "hooks must be null when empty");
    // The conditional fields must NOT appear.
    assert!(
        req.get("agents").is_none(),
        "agents must be omitted when no agents configured, got {req:?}"
    );
    assert!(
        req.get("excludeDynamicSections").is_none(),
        "excludeDynamicSections must be omitted when no preset sets it, got {req:?}"
    );
}

#[tokio::test]
async fn exclude_dynamic_sections_via_preset_is_included() {
    use forge_sdk::SystemPromptKind;
    let req = capture_init(|b| {
        b.system_prompt(SystemPromptKind::Preset {
            append: None,
            exclude_dynamic_sections: Some(true),
        })
    })
    .await;
    assert_eq!(req["excludeDynamicSections"], true);
}

#[test]
fn initialize_body_field_order_matches_python_insertion_order() {
    // Pins the wire-byte invariant: with `serde_json/preserve_order`
    // enabled in the workspace, our `init_body` Map (which inserts
    // `subtype` first, `hooks` second) must serialize with `subtype`
    // BEFORE `hooks` to match Python SDK's insertion-ordered dict at
    // `_internal/query.py:196-200`. Without preserve_order the
    // default `BTreeMap` would sort alphabetically (`hooks` first),
    // breaking the byte-identical-wire-compatibility invariant.
    let mut init_body = serde_json::Map::new();
    init_body.insert("subtype".into(), serde_json::Value::String("initialize".into()));
    init_body.insert("hooks".into(), serde_json::Value::Null);
    let serialized = serde_json::to_string(&init_body).expect("serialize");
    let subtype_idx = serialized.find("\"subtype\"").expect("subtype present");
    let hooks_idx = serialized.find("\"hooks\"").expect("hooks present");
    assert!(
        subtype_idx < hooks_idx,
        "expected subtype before hooks (preserve_order feature must be enabled), \
         got: {serialized}"
    );
}

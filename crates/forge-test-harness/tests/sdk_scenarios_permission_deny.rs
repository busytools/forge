//! Live-capture scenario: `can_use_tool` callback denies a tool call.
//!
//! Matches the canonical permission-deny example.
//! (`examples/tool_permission_callback.py`): register a
//! `can_use_tool` callback and run in `PermissionMode::Ask` — the CLI
//! should emit a `can_use_tool` `control_request` per tool use, the
//! SDK handler replies with `deny`, and the CLI reports denial in the
//! turn.


use forge_sdk::{OptionsBuilder, PermissionDecision, PermissionMode, ToolPermissionContext};
use forge_test_harness::sdk_wire::run_live_scenario;

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_permission_deny() {
    // Override the developer's settings.json so the CLI doesn't
    // short-circuit permission prompts via `skipAutoPermissionPrompt`
    // or auto-mode classifier rules. Without this, the user profile's
    // `permissions.allow: ["Bash(*)"]` + `skipAutoPermissionPrompt: true`
    // means the CLI never emits a `can_use_tool` control_request
    // (auto-mode classifier decides in-process). Force `default`
    // permission mode and empty allowlist via `--settings`.
    // Emit `--permission-mode default` explicitly (forge-sdk's
    // PermissionMode::Ask → "default" normally suppresses the flag,
    // which lets the developer's user-level `settings.json` win).
    // Overriding via extra_arg forces the CLI to adopt default mode
    // regardless of any autoMode / skipAutoPermissionPrompt settings.
    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::Ask)
        .extra_arg("permission-mode", Some("default".to_string()))
        // `--permission-prompt-tool stdio` tells the CLI to route
        // permission prompts as `can_use_tool` control_requests over
        // the stream-json pipe instead of handling them in-process.
        // The canonical example omits this but the CLI ships
        // without the user's `skipAutoPermissionPrompt: true` override;
        // against this developer's profile we need the explicit flag.
        .permission_prompt_tool_name("stdio")
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            eprintln!("can_use_tool fired for tool={}", ctx.tool_name);
            PermissionDecision::deny("forge-conformance denies in the scenario harness")
        })
        .build();

    run_live_scenario("permission_deny", opts, |client, events| async move {
        // Pick a tool NOT auto-approved by the developer's
        // `settings.json → permissions.allow` list. `Write` and `Edit`
        // typically require permission in Ask mode. Bash is usually
        // whitelisted in developer profiles so it'd bypass
        // `can_use_tool` via the auto-mode classifier (documented in
        // cross-project TIL `2026-04-20-auto-mode-classifier-preempts-sdk-can-use-tool`).
        client
            .send_user_message(
                "Use the Write tool to create a new file at \
                 /tmp/forge-deny-scenario.txt containing the word \
                 HELLO. Then confirm whether the Write succeeded.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

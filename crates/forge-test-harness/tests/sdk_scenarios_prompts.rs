//! Live-capture scenarios for the unified-prompt redesign.
//!
//! These scenarios capture the wire shapes the CLI emits for the
//! flows the unified prompt widget needs to render:
//!  - `ExitPlanMode` while in `--permission-mode plan` - does the
//!    CLI populate `permission_suggestions` with `setMode` entries,
//!    and which modes?
//!  - `Bash` against a non-allowed working directory - does the SDK
//!    accept `allow_with_input` with a modified `{command: "..."}`?
//!  - `Edit` / `Read` / `Write` against `/tmp/**` (outside the
//!    workspace) - what does the CLI emit in `permission_suggestions`
//!    for each tool kind?
//!  - `AskUserQuestion` answered with empty `selected_option_ids`
//!    and a `notes` annotation only - does the CLI accept it as an
//!    `Answered` response, or does it require `Cancelled` instead?
//!
//! All scenarios are gated by `FORGE_WIRE_CAPTURE=1`; replay-mode
//! verifies the captured baselines decode cleanly through
//! `decode_dispatch`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_primitives::{PermissionDecision, ToolPermissionContext};
use forge_sdk::{OptionsBuilder, PermissionMode};
use forge_test_harness::sdk_wire::run_live_scenario;
use serde_json::{Value, json};

/// Force the CLI into `default` permission mode and route permission
/// prompts to the SDK callback (stdio). Without this the user's
/// `settings.json → skipAutoPermissionPrompt: true` short-circuits
/// `can_use_tool` via the auto-mode classifier (cross-project TIL
/// `2026-04-20-auto-mode-classifier-preempts-sdk-can-use-tool`).
fn base_default_opts() -> OptionsBuilder {
    OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::Ask)
        .extra_arg("permission-mode", Some("default".to_string()))
        .permission_prompt_tool_name("stdio")
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_exit_plan_mode() {
    // Spawn in Plan mode so Claude has to call ExitPlanMode to leave
    // it. The ExitPlanMode call should surface through `can_use_tool`
    // with `permission_suggestions` carrying SetMode entries
    // (open question §6.3).
    let opts = OptionsBuilder::new()
        .max_turns(3)
        .permission_mode(PermissionMode::Plan)
        .extra_arg("permission-mode", Some("plan".to_string()))
        .permission_prompt_tool_name("stdio")
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            eprintln!(
                "can_use_tool fired for tool={} suggestions={} blocked_path={:?} decision_reason={:?}",
                ctx.tool_name,
                ctx.suggestions.len(),
                ctx.blocked_path,
                ctx.decision_reason
            );
            // Deny so the turn ends quickly; we just want the request
            // shape captured.
            PermissionDecision::deny("forge unified-prompt harness - captured request shape")
        })
        .build();

    run_live_scenario("exit_plan_mode", opts, |client, events| async move {
        client
            .send_user_message(
                "We are in plan mode. Write a one-sentence plan: \"I will add a TODO \
                 comment to a scratch file.\" Then call ExitPlanMode with that plan \
                 to exit plan mode. Do not actually edit any files.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_permission_allow_with_input_bash() {
    // Bash a non-allowed command, then reply via allow_with_input
    // carrying a modified `command` string. Captures both the CLI's
    // request shape AND our outbound `control_response` to confirm
    // the modified-input wire shape (open question §6.4).
    let opts = base_default_opts()
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            eprintln!(
                "can_use_tool fired for tool={} input={}",
                ctx.tool_name,
                serde_json::to_string(&ctx.tool_input).unwrap_or_default()
            );
            if ctx.tool_name == "Bash" {
                // Replace the model's command with a different one,
                // preserving the structure of the Bash input.
                let mut modified = ctx.tool_input.as_object().cloned().unwrap_or_default();
                modified.insert(
                    "command".to_owned(),
                    Value::String("echo forge-harness-modified".to_owned()),
                );
                modified.insert(
                    "description".to_owned(),
                    Value::String("Modified by forge harness".to_owned()),
                );
                return PermissionDecision::allow_with_input(Value::Object(modified));
            }
            PermissionDecision::deny(
                "forge unified-prompt harness - only Bash gets allow_with_input",
            )
        })
        .build();

    run_live_scenario("permission_allow_with_input_bash", opts, |client, events| async move {
        client
            .send_user_message(
                "Run this exact Bash command: `echo forge-harness-original`. \
                     Just run it once and report whether it succeeded.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_permission_suggestions_edit() {
    // Edit a file outside the workspace (`/tmp/**`) - typically the
    // CLI flags this as out-of-bounds and populates
    // `permission_suggestions` with `addRules` entries scoped to
    // `/tmp/**` (or `/private/tmp/**` on macOS). Captures the
    // suggestion shape for the Edit tool (open question §6.2).
    let opts = base_default_opts()
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            eprintln!(
                "can_use_tool fired for tool={} suggestions={} blocked_path={:?}",
                ctx.tool_name,
                ctx.suggestions.len(),
                ctx.blocked_path
            );
            PermissionDecision::deny("forge unified-prompt harness - captured suggestions only")
        })
        .build();

    run_live_scenario("permission_suggestions_edit", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Edit tool to change the file at \
                     /tmp/forge-unified-prompt-edit.txt - replace any occurrence of \
                     `old` with `new`. The file doesn't need to exist; just attempt \
                     the Edit so I can see the permission prompt.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_permission_suggestions_read() {
    // Read a file outside the workspace. Pairs with the Edit / Write
    // captures so we can compare what addRules entries differ across
    // tools.
    let opts = base_default_opts()
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            eprintln!(
                "can_use_tool fired for tool={} suggestions={} blocked_path={:?}",
                ctx.tool_name,
                ctx.suggestions.len(),
                ctx.blocked_path
            );
            PermissionDecision::deny("forge unified-prompt harness - captured suggestions only")
        })
        .build();

    run_live_scenario("permission_suggestions_read", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Read tool to read the file at \
                     /tmp/forge-unified-prompt-read.txt and report its contents.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_permission_suggestions_write() {
    // Write to a file outside the workspace.
    let opts = base_default_opts()
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            eprintln!(
                "can_use_tool fired for tool={} suggestions={} blocked_path={:?}",
                ctx.tool_name,
                ctx.suggestions.len(),
                ctx.blocked_path
            );
            PermissionDecision::deny("forge unified-prompt harness - captured suggestions only")
        })
        .build();

    run_live_scenario("permission_suggestions_write", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the Write tool to create a file at \
                     /tmp/forge-unified-prompt-write.txt containing the word HELLO. \
                     Just attempt the Write so I can see the permission prompt.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

#[tokio::test]
#[ignore = "burns real Anthropic API tokens; opt-in via FORGE_WIRE_CAPTURE=1"]
async fn wire_capture_question_notes_only_response() {
    // Force Claude to invoke `AskUserQuestion`, then reply via
    // `allow_with_input` with an `answers` map whose values are empty
    // strings and an `annotations` map carrying only `notes`. The
    // open question: does the CLI accept this as a valid Answered
    // response, or does it surface an error / require Cancelled?
    let opts = base_default_opts()
        .can_use_tool(|ctx: ToolPermissionContext| async move {
            eprintln!(
                "can_use_tool fired for tool={} input={}",
                ctx.tool_name,
                serde_json::to_string(&ctx.tool_input).unwrap_or_default()
            );
            if ctx.tool_name != "AskUserQuestion" {
                return PermissionDecision::allow();
            }
            // Build `updated_input` = original input + answers (empty)
            // + annotations (notes only). Mirrors the user_interaction
            // helper's wire shape but with no selections.
            let mut merged = ctx.tool_input.as_object().cloned().unwrap_or_default();
            let mut answers = serde_json::Map::new();
            let mut annotations = serde_json::Map::new();
            let questions = ctx
                .tool_input
                .get("questions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for q in questions {
                let Some(qtext) = q.get("question").and_then(Value::as_str) else {
                    continue;
                };
                // Empty answer to mirror "notes-only response":
                // no option was actually selected by the user.
                answers.insert(qtext.to_owned(), Value::String(String::new()));
                annotations.insert(
                    qtext.to_owned(),
                    json!({ "notes": "test feedback from forge unified-prompt harness" }),
                );
            }
            merged.insert("answers".to_owned(), Value::Object(answers));
            merged.insert("annotations".to_owned(), Value::Object(annotations));
            PermissionDecision::allow_with_input(Value::Object(merged))
        })
        .build();

    run_live_scenario("question_notes_only_response", opts, |client, events| async move {
        client
            .send_user_message(
                "Use the AskUserQuestion tool right now to ask me whether I prefer \
                     the colour red, blue, or green. Single question, three options. \
                     Do NOT answer it for me - just call the tool and stop.",
            )
            .await?;
        Ok((client, events))
    })
    .await
    .expect("scenario run");
}

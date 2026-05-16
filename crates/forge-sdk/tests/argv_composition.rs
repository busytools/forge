//! Exercise the argv builder against the Python SDK's subprocess_cli output.
//!
//! Ported from `_internal/transport/subprocess_cli.py:203-382` — each test
//! sets one option and asserts the resulting argv vector contains the
//! expected flag/value pairs in the same order Python emits them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::option_option,
    clippy::map_unwrap_or,
    clippy::doc_markdown
)]

use forge_sdk::argv::build_args;
use forge_sdk::subagents::{EffortLevel, EffortPreset};
use forge_sdk::{
    OptionsBuilder, PermissionMode, SdkPluginConfig, SystemPromptKind, ThinkingConfig, ToolsPreset,
};
use serde_json::json;

fn argv_of(builder: OptionsBuilder) -> Vec<String> {
    build_args(&builder.build()).expect("build_args")
}

/// Locate the index of a flag and return (flag, value) pair when the flag
/// expects a value, or (flag, None) when it's a bare flag.
fn find_flag<'a>(argv: &'a [String], flag: &str) -> Option<Option<&'a str>> {
    let i = argv.iter().position(|a| a == flag)?;
    Some(argv.get(i + 1).map(String::as_str))
}

#[test]
fn baseline_argv_matches_python() {
    let argv = argv_of(OptionsBuilder::new());
    // Python always leads with these three; forge-sdk likewise.
    assert_eq!(&argv[0..3], &["--output-format", "stream-json", "--verbose"]);
    // Python emits `--system-prompt ""` when no system_prompt is set
    // (subprocess_cli.py:209-210). forge-sdk matches.
    assert_eq!(find_flag(&argv, "--system-prompt"), Some(Some("")));
    // Ends with --input-format stream-json.
    let tail = &argv[argv.len() - 2..];
    assert_eq!(tail, &["--input-format", "stream-json"]);
}

#[test]
fn system_prompt_none_emits_empty_string() {
    let argv = argv_of(OptionsBuilder::new());
    assert_eq!(find_flag(&argv, "--system-prompt"), Some(Some("")));
    assert!(find_flag(&argv, "--system-prompt-file").is_none());
    assert!(find_flag(&argv, "--append-system-prompt").is_none());
}

#[test]
fn continue_conversation_emits_flag() {
    let argv = argv_of(OptionsBuilder::new().continue_conversation(true));
    assert!(argv.iter().any(|a| a == "--continue"));
}

#[test]
fn session_id_and_resume_are_distinct_flags() {
    let argv = argv_of(OptionsBuilder::new().session_id("new-sess-1"));
    assert_eq!(find_flag(&argv, "--session-id"), Some(Some("new-sess-1")));
    assert!(find_flag(&argv, "--resume").is_none());
}

#[test]
fn max_turns_and_budget_emitted_as_strings() {
    let argv = argv_of(OptionsBuilder::new().max_turns(10).max_budget_usd(2.5));
    assert_eq!(find_flag(&argv, "--max-turns"), Some(Some("10")));
    assert_eq!(find_flag(&argv, "--max-budget-usd"), Some(Some("2.5")));
}

#[test]
fn disallowed_tools_joined_with_commas() {
    let argv = argv_of(OptionsBuilder::new().disallowed_tools(vec!["Bash".into(), "Edit".into()]));
    assert_eq!(find_flag(&argv, "--disallowedTools"), Some(Some("Bash,Edit")));
}

#[test]
fn fallback_model_and_betas_emitted() {
    let argv = argv_of(
        OptionsBuilder::new()
            .fallback_model("claude-sonnet-4-6")
            .betas(vec!["context-1m-2025-08-07".into()]),
    );
    assert_eq!(find_flag(&argv, "--fallback-model"), Some(Some("claude-sonnet-4-6")));
    assert_eq!(find_flag(&argv, "--betas"), Some(Some("context-1m-2025-08-07")));
}

#[test]
fn include_partial_messages_and_fork_session_are_bare_flags() {
    let argv = argv_of(OptionsBuilder::new().include_partial_messages(true).fork_session(true));
    assert!(argv.iter().any(|a| a == "--include-partial-messages"));
    assert!(argv.iter().any(|a| a == "--fork-session"));
}

#[test]
fn add_dirs_emit_repeated_flag_per_path() {
    let argv = argv_of(OptionsBuilder::new().add_dir("/tmp/a").add_dir("/tmp/b"));
    let mut occurrences = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        if a == "--add-dir" {
            occurrences.push(argv.get(i + 1).map(String::as_str).unwrap_or(""));
        }
    }
    assert_eq!(occurrences, vec!["/tmp/a", "/tmp/b"]);
}

#[test]
fn plugins_emit_plugin_dir_per_entry() {
    let argv = argv_of(OptionsBuilder::new().plugins(vec![
        SdkPluginConfig::Local { path: "/plugins/a".into() },
        SdkPluginConfig::Local { path: "/plugins/b".into() },
    ]));
    let mut occurrences = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        if a == "--plugin-dir" {
            occurrences.push(argv.get(i + 1).map(String::as_str).unwrap_or(""));
        }
    }
    assert_eq!(occurrences, vec!["/plugins/a", "/plugins/b"]);
}

#[test]
fn extra_args_handles_bare_and_valued() {
    let argv = argv_of(
        OptionsBuilder::new()
            .extra_arg("custom-flag", Some("value".into()))
            .extra_arg("bare-flag", None),
    );
    assert!(argv.iter().any(|a| a == "--custom-flag"));
    assert!(argv.iter().any(|a| a == "--bare-flag"));
    assert_eq!(find_flag(&argv, "--custom-flag"), Some(Some("value")));
    // Bare flag must not be followed by a non-flag value.
    let i = argv.iter().position(|a| a == "--bare-flag").unwrap();
    let next = argv.get(i + 1).map(String::as_str);
    // Next element (if any) should itself start with "--".
    if let Some(next) = next {
        assert!(next.starts_with("--"), "got: {next}");
    }
}

#[test]
fn system_prompt_inline_emits_plain_value() {
    let argv =
        argv_of(OptionsBuilder::new().system_prompt(SystemPromptKind::Inline("be helpful".into())));
    assert_eq!(find_flag(&argv, "--system-prompt"), Some(Some("be helpful")));
}

#[test]
fn system_prompt_file_uses_dedicated_flag() {
    let argv =
        argv_of(OptionsBuilder::new().system_prompt(SystemPromptKind::File("/tmp/sp.txt".into())));
    assert_eq!(find_flag(&argv, "--system-prompt-file"), Some(Some("/tmp/sp.txt")));
    assert!(find_flag(&argv, "--system-prompt").is_none());
}

#[test]
fn system_prompt_preset_append_uses_dedicated_flag() {
    let argv = argv_of(
        OptionsBuilder::new().system_prompt(SystemPromptKind::preset_append("extra instructions")),
    );
    assert_eq!(find_flag(&argv, "--append-system-prompt"), Some(Some("extra instructions")));
}

#[test]
fn tools_preset_default_emits_default_literal() {
    let argv = argv_of(OptionsBuilder::new().tools(ToolsPreset::Default));
    assert_eq!(find_flag(&argv, "--tools"), Some(Some("default")));
}

#[test]
fn tools_list_joins_with_commas() {
    let argv =
        argv_of(OptionsBuilder::new().tools(ToolsPreset::List(vec!["Edit".into(), "Read".into()])));
    assert_eq!(find_flag(&argv, "--tools"), Some(Some("Edit,Read")));
}

#[test]
fn thinking_adaptive_emits_adaptive() {
    let argv = argv_of(OptionsBuilder::new().thinking(ThinkingConfig::Adaptive));
    assert_eq!(find_flag(&argv, "--thinking"), Some(Some("adaptive")));
}

#[test]
fn thinking_enabled_emits_max_thinking_tokens() {
    let argv =
        argv_of(OptionsBuilder::new().thinking(ThinkingConfig::Enabled { budget_tokens: 8000 }));
    assert_eq!(find_flag(&argv, "--max-thinking-tokens"), Some(Some("8000")));
    assert!(find_flag(&argv, "--thinking").is_none());
}

#[test]
fn thinking_disabled_emits_disabled() {
    let argv = argv_of(OptionsBuilder::new().thinking(ThinkingConfig::Disabled));
    assert_eq!(find_flag(&argv, "--thinking"), Some(Some("disabled")));
}

#[test]
fn max_thinking_tokens_fallback_when_no_thinking_config() {
    let argv = argv_of(OptionsBuilder::new().max_thinking_tokens(4096));
    assert_eq!(find_flag(&argv, "--max-thinking-tokens"), Some(Some("4096")));
}

#[test]
fn effort_preset_serialises_as_string() {
    let argv = argv_of(OptionsBuilder::new().effort(EffortLevel::Preset(EffortPreset::High)));
    assert_eq!(find_flag(&argv, "--effort"), Some(Some("high")));
}

#[test]
fn effort_numeric_serialises_as_integer() {
    let argv = argv_of(OptionsBuilder::new().effort(EffortLevel::Numeric(7)));
    assert_eq!(find_flag(&argv, "--effort"), Some(Some("7")));
}

#[test]
fn output_format_json_schema_flag() {
    let argv = argv_of(OptionsBuilder::new().output_format(json!({
        "type": "json_schema",
        "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}}
    })));
    let schema = find_flag(&argv, "--json-schema").expect("flag present");
    assert!(schema.unwrap().contains("\"ok\""), "{schema:?}");
}

#[test]
fn task_budget_emitted() {
    let argv = argv_of(OptionsBuilder::new().task_budget(100_000));
    assert_eq!(find_flag(&argv, "--task-budget"), Some(Some("100000")));
}

#[test]
fn permission_mode_default_is_suppressed() {
    let argv = argv_of(OptionsBuilder::new());
    assert!(find_flag(&argv, "--permission-mode").is_none());
}

#[test]
fn settings_path_passes_through_when_no_sandbox() {
    let argv = argv_of(OptionsBuilder::new().settings("/tmp/settings.json"));
    assert_eq!(find_flag(&argv, "--settings"), Some(Some("/tmp/settings.json")));
}

#[test]
fn settings_inline_json_passes_through_when_no_sandbox() {
    let argv = argv_of(OptionsBuilder::new().settings(r#"{"theme":"dark"}"#));
    assert_eq!(find_flag(&argv, "--settings"), Some(Some(r#"{"theme":"dark"}"#)));
}

#[test]
fn sandbox_all_fields_wire_as_python_camel_case() {
    // Verifies the field names land on the wire exactly as Python emits
    // them (types.py:782-856). Regression guard against the fabricated
    // names forge-sdk carried before 2026-04-22.
    let sandbox = forge_primitives::SandboxSettings {
        enabled: Some(true),
        auto_allow_bash_if_sandboxed: Some(true),
        excluded_commands: Some(vec!["git".into(), "docker".into()]),
        allow_unsandboxed_commands: Some(false),
        network: Some(forge_primitives::SandboxNetworkConfig {
            allow_unix_sockets: Some(vec!["/var/run/docker.sock".into()]),
            allow_all_unix_sockets: Some(false),
            allow_local_binding: Some(true),
            http_proxy_port: Some(3128),
            socks_proxy_port: Some(1080),
        }),
        ignore_violations: Some(forge_primitives::SandboxIgnoreViolations {
            file: Some(vec!["/tmp".into()]),
            network: Some(vec!["metrics.example".into()]),
        }),
        enable_weaker_nested_sandbox: Some(false),
    };
    let wire = serde_json::to_value(&sandbox).expect("serialize");
    assert_eq!(wire["enabled"], true);
    assert_eq!(wire["autoAllowBashIfSandboxed"], true);
    assert_eq!(wire["excludedCommands"], json!(["git", "docker"]));
    assert_eq!(wire["allowUnsandboxedCommands"], false);
    assert_eq!(wire["network"]["allowUnixSockets"], json!(["/var/run/docker.sock"]));
    assert_eq!(wire["network"]["allowAllUnixSockets"], false);
    assert_eq!(wire["network"]["allowLocalBinding"], true);
    assert_eq!(wire["network"]["httpProxyPort"], 3128);
    assert_eq!(wire["network"]["socksProxyPort"], 1080);
    assert_eq!(wire["ignoreViolations"]["file"], json!(["/tmp"]));
    assert_eq!(wire["ignoreViolations"]["network"], json!(["metrics.example"]));
    assert_eq!(wire["enableWeakerNestedSandbox"], false);
}

#[test]
fn sandbox_alone_merges_into_settings_json() {
    let sandbox =
        forge_primitives::SandboxSettings { enabled: Some(true), ..forge_primitives::SandboxSettings::default() };
    let argv = argv_of(OptionsBuilder::new().sandbox(sandbox));
    let value = find_flag(&argv, "--settings").expect("flag present").expect("value present");
    let parsed: serde_json::Value = serde_json::from_str(value).expect("json");
    assert_eq!(parsed["sandbox"]["enabled"], true);
}

#[test]
fn settings_inline_json_merges_with_sandbox() {
    let sandbox =
        forge_primitives::SandboxSettings { enabled: Some(true), ..forge_primitives::SandboxSettings::default() };
    let argv = argv_of(OptionsBuilder::new().settings(r#"{"theme":"dark"}"#).sandbox(sandbox));
    let value = find_flag(&argv, "--settings").expect("flag present").expect("value present");
    let parsed: serde_json::Value = serde_json::from_str(value).expect("json");
    assert_eq!(parsed["theme"], "dark");
    assert_eq!(parsed["sandbox"]["enabled"], true);
}

#[test]
fn enable_file_checkpointing_does_not_emit_cli_flag() {
    // Python SDK delivers this via the
    // CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING env var
    // (subprocess_cli.py:436-437), NOT a CLI flag. forge-sdk must
    // match — a `--enable-file-checkpointing` flag would be
    // unrecognised by the CLI and silently ignored.
    let argv = argv_of(OptionsBuilder::new().enable_file_checkpointing(true));
    assert!(
        !argv.iter().any(|a| a == "--enable-file-checkpointing"),
        "flag must NOT be emitted — it's delivered via env var"
    );
}

#[test]
fn permission_mode_non_default_emitted() {
    let mut options = OptionsBuilder::new().build();
    options.permission_mode = PermissionMode::AcceptEdits;
    let argv = build_args(&options).expect("build_args");
    assert_eq!(find_flag(&argv, "--permission-mode"), Some(Some("acceptEdits")));
}

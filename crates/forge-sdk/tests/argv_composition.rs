//! Exercise the argv builder for the flags forge actively emits.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::option_option,
    clippy::map_unwrap_or,
    clippy::doc_markdown
)]

use forge_sdk::argv::build_args;
use forge_sdk::subagents::{EffortPreset, SubagentEffort};
use forge_sdk::{OptionsBuilder, PermissionMode, SdkPluginConfig, SystemPromptKind};
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
fn baseline_argv_leading_and_trailing_flags() {
    let argv = argv_of(OptionsBuilder::new());
    assert_eq!(&argv[0..3], &["--output-format", "stream-json", "--verbose"]);
    // Empty system_prompt — the CLI's default would inject its own; the
    // explicit `--system-prompt ""` suppresses that.
    assert_eq!(find_flag(&argv, "--system-prompt"), Some(Some("")));
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
fn session_id_and_resume_are_distinct_flags() {
    let argv = argv_of(OptionsBuilder::new().session_id("new-sess-1"));
    assert_eq!(find_flag(&argv, "--session-id"), Some(Some("new-sess-1")));
    assert!(find_flag(&argv, "--resume").is_none());
}

#[test]
fn max_turns_emitted_as_string() {
    let argv = argv_of(OptionsBuilder::new().max_turns(10));
    assert_eq!(find_flag(&argv, "--max-turns"), Some(Some("10")));
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
    let i = argv.iter().position(|a| a == "--bare-flag").unwrap();
    let next = argv.get(i + 1).map(String::as_str);
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
fn effort_preset_serialises_as_string() {
    let argv = argv_of(OptionsBuilder::new().effort(SubagentEffort::Preset(EffortPreset::High)));
    assert_eq!(find_flag(&argv, "--effort"), Some(Some("high")));
}

#[test]
fn effort_numeric_serialises_as_integer() {
    let argv = argv_of(OptionsBuilder::new().effort(SubagentEffort::Numeric(7)));
    assert_eq!(find_flag(&argv, "--effort"), Some(Some("7")));
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
fn sandbox_all_fields_wire_as_camel_case() {
    // Verifies the field names land on the wire exactly as the CLI
    // accepts them. Regression guard against fabricated field names.
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
    let sandbox = forge_primitives::SandboxSettings {
        enabled: Some(true),
        ..forge_primitives::SandboxSettings::default()
    };
    let argv = argv_of(OptionsBuilder::new().sandbox(sandbox));
    let value = find_flag(&argv, "--settings").expect("flag present").expect("value present");
    let parsed: serde_json::Value = serde_json::from_str(value).expect("json");
    assert_eq!(parsed["sandbox"]["enabled"], true);
}

#[test]
fn settings_inline_json_merges_with_sandbox() {
    let sandbox = forge_primitives::SandboxSettings {
        enabled: Some(true),
        ..forge_primitives::SandboxSettings::default()
    };
    let argv = argv_of(OptionsBuilder::new().settings(r#"{"theme":"dark"}"#).sandbox(sandbox));
    let value = find_flag(&argv, "--settings").expect("flag present").expect("value present");
    let parsed: serde_json::Value = serde_json::from_str(value).expect("json");
    assert_eq!(parsed["theme"], "dark");
    assert_eq!(parsed["sandbox"]["enabled"], true);
}

#[test]
fn permission_mode_non_default_emitted() {
    let mut options = OptionsBuilder::new().build();
    options.permission_mode = PermissionMode::AcceptEdits;
    let argv = build_args(&options).expect("build_args");
    assert_eq!(find_flag(&argv, "--permission-mode"), Some(Some("acceptEdits")));
}

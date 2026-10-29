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
    // Empty system_prompt - the CLI's default would inject its own; the
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
fn permission_mode_non_default_emitted() {
    let mut options = OptionsBuilder::new().build();
    options.permission_mode = PermissionMode::AcceptEdits;
    let argv = build_args(&options).expect("build_args");
    assert_eq!(find_flag(&argv, "--permission-mode"), Some(Some("acceptEdits")));
}

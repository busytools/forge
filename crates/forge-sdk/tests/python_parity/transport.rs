//! Mirrors selected argv-shape tests from
//! `tests/test_transport.py::TestBuildCommand` in
//! `claude-agent-sdk-python` v0.1.64. The Python suite has 73 tests;
//! argv-shape coverage overlaps heavily with our existing
//! `tests/argv_composition.rs`. This port focuses on the tests that
//! exercise shapes not already covered elsewhere — chiefly the
//! combined-option fixtures Python uses as smoke checks for every
//! flag it emits.

use forge_sdk::argv::build_args;
use forge_sdk::{OptionsBuilder, PermissionMode, SystemPromptKind, ToolsPreset};

fn argv(builder: OptionsBuilder) -> Vec<String> {
    build_args(&builder.build()).expect("build_args")
}

fn has(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

/// Ported from `test_build_command_basic` — default options produce
/// `--output-format stream-json --verbose` + `--input-format
/// stream-json` and nothing else beyond our Python-parity
/// `--system-prompt ""`.
#[test]
fn build_command_basic() {
    let argv = argv(OptionsBuilder::new());
    assert_eq!(
        &argv[0..3],
        &["--output-format", "stream-json", "--verbose"]
    );
    assert!(has(&argv, "--system-prompt"));
    let tail = &argv[argv.len() - 2..];
    assert_eq!(tail, &["--input-format", "stream-json"]);
}

/// Ported from `test_build_command_with_system_prompt_string`.
#[test]
fn build_command_with_system_prompt_string() {
    let argv =
        argv(OptionsBuilder::new().system_prompt(SystemPromptKind::Inline("Be helpful".into())));
    assert!(has(&argv, "--system-prompt"));
    assert!(has(&argv, "Be helpful"));
}

/// Ported from `test_build_command_with_system_prompt_file`.
#[test]
fn build_command_with_system_prompt_file() {
    let argv = argv(
        OptionsBuilder::new().system_prompt(SystemPromptKind::File("/path/to/prompt.md".into())),
    );
    assert!(has(&argv, "--system-prompt-file"));
    assert!(has(&argv, "/path/to/prompt.md"));
    assert!(!has(&argv, "--append-system-prompt"));
}

/// Ported from `test_build_command_with_options` — the compound smoke
/// test Python runs to verify every common flag lands in argv with the
/// expected value.
#[test]
fn build_command_with_options() {
    let argv = argv(
        OptionsBuilder::new()
            .allowed_tools(["Read", "Write"])
            .disallowed_tools(vec!["Bash".into()])
            .model("claude-sonnet-4-5")
            .permission_mode(PermissionMode::AcceptEdits)
            .max_turns(5),
    );
    assert!(has(&argv, "--allowedTools"));
    assert!(has(&argv, "Read,Write"));
    assert!(has(&argv, "--disallowedTools"));
    assert!(has(&argv, "Bash"));
    assert!(has(&argv, "--model"));
    assert!(has(&argv, "claude-sonnet-4-5"));
    assert!(has(&argv, "--permission-mode"));
    assert!(has(&argv, "acceptEdits"));
    assert!(has(&argv, "--max-turns"));
    assert!(has(&argv, "5"));
}

/// Ported from `test_build_command_with_fallback_model`.
#[test]
fn build_command_with_fallback_model() {
    let argv = argv(OptionsBuilder::new().model("opus").fallback_model("sonnet"));
    assert!(has(&argv, "--model"));
    assert!(has(&argv, "opus"));
    assert!(has(&argv, "--fallback-model"));
    assert!(has(&argv, "sonnet"));
}

/// Ported from `test_session_continuation`. Python adds `--continue`
/// when `continue_conversation=True`.
#[test]
fn session_continuation_emits_continue_flag() {
    let argv = argv(OptionsBuilder::new().continue_conversation(true));
    assert!(has(&argv, "--continue"));
}

/// Ported from `test_session_id`.
#[test]
fn session_id_emits_flag_with_value() {
    let argv = argv(OptionsBuilder::new().session_id("custom-session-123"));
    assert!(has(&argv, "--session-id"));
    assert!(has(&argv, "custom-session-123"));
}

/// Ported from `test_session_id_not_set_by_default`.
#[test]
fn session_id_not_set_by_default() {
    let argv = argv(OptionsBuilder::new());
    assert!(!has(&argv, "--session-id"));
}

/// Ported from `test_build_command_with_add_dirs`. Python emits
/// `--add-dir <path>` once per directory; forge-sdk matches.
#[test]
fn build_command_with_add_dirs() {
    let argv = argv(
        OptionsBuilder::new()
            .add_dir("/extra/one")
            .add_dir("/extra/two"),
    );
    assert!(has(&argv, "--add-dir"));
    assert!(has(&argv, "/extra/one"));
    assert!(has(&argv, "/extra/two"));
}

/// Ported from `test_build_command_tools_preset_default`. Python
/// accepts `tools="default"` → `--tools default`.
#[test]
fn build_command_tools_preset_default() {
    let argv = argv(OptionsBuilder::new().tools(ToolsPreset::Default));
    let idx = argv.iter().position(|a| a == "--tools").expect("--tools");
    assert_eq!(argv[idx + 1], "default");
}

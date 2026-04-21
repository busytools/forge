//! Verify hooks can be registered and counted.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forge_sdk::{HookContext, HookDecision, HooksBuilder, OptionsBuilder, PreToolUseInput};

#[test]
fn hooks_attach_to_options() {
    let hooks =
        HooksBuilder::new()
            .pre_tool_use(
                "*",
                |_input: PreToolUseInput, _ctx: HookContext| async move { HookDecision::allow() },
            )
            .pre_tool_use(
                "Bash",
                |_input: PreToolUseInput, _ctx: HookContext| async move {
                    HookDecision::deny("no bash")
                },
            )
            .build();

    let opts = OptionsBuilder::new().hooks(hooks).build();
    let desc = format!("{opts:?}");
    assert!(
        desc.contains("pre_tool_use_count: 2"),
        "expected pre_tool_use_count: 2, got: {desc}"
    );
}

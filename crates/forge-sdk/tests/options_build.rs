//! Builder behaviour tests for `OptionsBuilder`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use forge_sdk::{OptionsBuilder, PermissionMode};

#[test]
fn default_options() {
    let opts = OptionsBuilder::new().build();
    assert_eq!(opts.binary, "claude");
    assert!(opts.cwd.is_none());
    assert!(opts.resume.is_none());
    assert_eq!(opts.permission_mode, PermissionMode::Ask);
    assert!(opts.model.is_none());
}

#[test]
fn builder_sets_model_and_cwd() {
    let opts = OptionsBuilder::new()
        .model("claude-opus-4-5")
        .cwd("/tmp/project")
        .build();
    assert_eq!(opts.model.as_deref(), Some("claude-opus-4-5"));
    assert_eq!(opts.cwd, Some(PathBuf::from("/tmp/project")));
}

#[test]
fn builder_sets_resume_session() {
    let opts = OptionsBuilder::new().resume("sess_abc").build();
    assert_eq!(opts.resume.as_deref(), Some("sess_abc"));
}

#[test]
fn builder_sets_permission_mode() {
    let opts = OptionsBuilder::new()
        .permission_mode(PermissionMode::AcceptEdits)
        .build();
    assert_eq!(opts.permission_mode, PermissionMode::AcceptEdits);
}

#[test]
fn builder_sets_custom_binary() {
    let opts = OptionsBuilder::new()
        .binary("/usr/local/bin/claude")
        .build();
    assert_eq!(opts.binary, "/usr/local/bin/claude");
}

#[test]
fn builder_stores_can_use_tool_callback() {
    use forge_sdk::{PermissionDecision, ToolPermissionContext};

    let opts = OptionsBuilder::new()
        .can_use_tool(|_ctx: ToolPermissionContext| async move { PermissionDecision::allow() })
        .build();
    assert!(opts.can_use_tool.is_some());
}

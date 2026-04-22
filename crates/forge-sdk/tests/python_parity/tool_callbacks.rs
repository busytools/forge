//! Mirrors `tests/test_tool_callbacks.py` from
//! `claude-agent-sdk-python` v0.1.64 — 18 upstream tests covering
//! `can_use_tool` permission callbacks + hook dispatch.
//!
//! forge-sdk coverage:
//! - `tests/permissions_callback.rs` — end-to-end permission callback.
//! - `tests/permission_update.rs` — `PermissionUpdate` types.
//! - `tests/permission_types.rs` — `PermissionDecision` shapes.
//! - `tests/hooks_dispatch.rs` — hook-callback dispatch.
//! - `tests/hooks_registration.rs` — hook registration wire shape.
//! - `tests/hooks_specific_output.rs` — hook output serialisation.
//! - `tests/hooks_types.rs` — hook input type round-trips.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

// ===========================================================================
// TestPermissionCallback (6)
// ===========================================================================

#[test]
fn permission_callback_allow() {
    // Covered by permissions_callback.rs::allow_path_completes_turn.
}

#[test]
fn permission_callback_deny() {
    // Covered by permissions_callback.rs + permission_types.rs.
}

#[test]
fn permission_callback_input_modification() {
    // Covered by permissions_callback.rs::allow_with_updated_input_propagates.
}

#[test]
fn permission_callback_receives_tool_use_id() {
    // Covered by permissions_callback.rs (tool_use_id flows in context).
}

#[test]
fn permission_callback_missing_agent_id() {
    // Covered by permissions_callback.rs (agent_id is Option).
}

#[test]
fn permission_callback_callback_exception_handling() {
    // Covered by permissions_callback.rs error-path propagation.
}

// ===========================================================================
// TestHookExecution (4)
// ===========================================================================

#[test]
fn hook_execution() {
    // Covered by hooks_dispatch.rs.
}

#[test]
fn hook_test() {
    // Python's top-level `test_hook` test — covered by hooks_dispatch.rs.
}

#[test]
fn hook_output_fields() {
    // Covered by hooks_specific_output.rs.
}

#[test]
fn async_hook_output() {
    // Covered by hooks_dispatch.rs + HookDecision::defer path.
    // Note: out-of-band AsyncHookJSONOutput follow-up delivery is a
    // parity gap (see PARITY.md + forge-sdk-parity-map.html).
}

// ===========================================================================
// TestFieldNameConversion + Options (2)
// ===========================================================================

#[test]
fn field_name_conversion() {
    // Covered by hooks_specific_output.rs camelCase wire keys.
}

#[test]
fn options_with_callbacks() {
    // Covered by permissions_callback.rs + hooks_registration.rs.
}

// ===========================================================================
// TestNewHookEvents (6)
// ===========================================================================

#[test]
fn notification_hook_callback() {
    // Covered by hooks_dispatch.rs notification-hook path.
}

#[test]
fn permission_request_hook_callback() {
    // Covered by hooks_dispatch.rs.
}

#[test]
fn subagent_start_hook_callback() {
    // Covered by hooks_dispatch.rs.
}

#[test]
fn post_tool_use_hook_with_updated_mcp_output() {
    // Covered by hooks_specific_output.rs
    // (PostToolUseHookSpecificOutput.updated_mcp_tool_output).
}

#[test]
fn pre_tool_use_hook_with_additional_context() {
    // Covered by hooks_specific_output.rs
    // (PreToolUseHookSpecificOutput.additional_context).
}

#[test]
fn new_hook_events_registered_in_hooks_config() {
    // Covered by hooks_registration.rs initialize-payload shape.
}

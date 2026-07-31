//! Unified collapse-decision resolution for chat render sites.
//!
//! Every render-time "should this be collapsed?" decision routes
//! through these resolvers so the four collapse models share one
//! derivation rule. Per-item state wins when present; absent falls
//! through to the global directive (`tools_collapsed`). The carve-out
//! predicate names the kinds that bypass the directive entirely:
//! Execute / Diff / Monitor / Workflow render expanded regardless of
//! the directive.

use crate::agent::model::ToolCallContent;
use crate::app::{ToolCallInfo, is_monitor_tool_name};
use crate::ui::message::grouping::GroupCollapseLevel;

/// Resolver for 2-state items (loose tool-calls, peer/MCP blocks
/// inbound + outbound). Per-item override wins; absent follows the
/// global directive.
pub fn resolve_collapsed_bool(per_item: Option<bool>, global_collapsed: bool) -> bool {
    per_item.unwrap_or(global_collapsed)
}

/// Resolver for 3-state items (tool-call groups, future messaging
/// groups). Per-group level override wins (preserves the per-group
/// L2 -> L1 -> L0 mouse-click cycle); absent falls through to the
/// global directive's level-equivalent:
///
/// - `global_collapsed = true`  -> `L2Summary` (one-line summary)
/// - `global_collapsed = false` -> `L0Bodies`  (full bodies)
///
/// `L1Titles` is reachable only via the per-group click cycle, never
/// via the global toggle.
pub fn resolve_group_level(
    per_group: Option<GroupCollapseLevel>,
    global_collapsed: bool,
) -> GroupCollapseLevel {
    per_group.unwrap_or(if global_collapsed {
        GroupCollapseLevel::L2Summary
    } else {
        GroupCollapseLevel::L0Bodies
    })
}

/// Kinds carved out from the global collapse directive. These render
/// expanded regardless of `tools_collapsed`. Matches the existing
/// pre-unify carve-out (Execute live-streaming + diff short-circuit)
/// plus lifecycle blocks (Monitor + Workflow), whose render paths in
/// `render_lifecycle_one_liner` bypass the directive by construction.
pub fn is_carved_out_from_global_directive(tc: &ToolCallInfo) -> bool {
    if tc.is_execute_tool() {
        return true;
    }
    if tc.content.iter().any(|c| matches!(c, ToolCallContent::Diff(_))) {
        return true;
    }
    if is_monitor_tool_name(&tc.sdk_tool_name) {
        return true;
    }
    if is_workflow_tool_name(&tc.sdk_tool_name) {
        return true;
    }
    false
}

/// Workflow lifecycle-tool predicate. Mirrors `is_monitor_tool_name`'s
/// case-insensitive shape. Kept local because no upstream sibling
/// exists today; if a future feature needs it elsewhere, lift to
/// `crate::app::state::tool_call_info` alongside
/// `is_monitor_tool_name`.
fn is_workflow_tool_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("workflow")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::model;
    use crate::app::{BlockCache, TerminalSnapshotMode};

    fn make_tc(name: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: "tu-test".to_owned(),
            title: format!("tool {name}"),
            sdk_tool_name: name.to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: model::ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_command: None,
            terminal_output: None,
            terminal_output_len: 0,
            terminal_bytes_seen: 0,
            terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    #[test]
    fn bool_per_item_some_true_wins_over_global_false() {
        assert!(resolve_collapsed_bool(Some(true), false));
    }

    #[test]
    fn bool_per_item_some_false_wins_over_global_true() {
        assert!(!resolve_collapsed_bool(Some(false), true));
    }

    #[test]
    fn bool_per_item_none_follows_global_true() {
        assert!(resolve_collapsed_bool(None, true));
    }

    #[test]
    fn bool_per_item_none_follows_global_false() {
        assert!(!resolve_collapsed_bool(None, false));
    }

    #[test]
    fn group_per_item_some_wins_regardless_of_global() {
        assert_eq!(
            resolve_group_level(Some(GroupCollapseLevel::L1Titles), true),
            GroupCollapseLevel::L1Titles,
        );
        assert_eq!(
            resolve_group_level(Some(GroupCollapseLevel::L1Titles), false),
            GroupCollapseLevel::L1Titles,
        );
        assert_eq!(
            resolve_group_level(Some(GroupCollapseLevel::L0Bodies), true),
            GroupCollapseLevel::L0Bodies,
        );
    }

    #[test]
    fn group_per_item_none_returns_l2summary_when_global_collapsed() {
        assert_eq!(resolve_group_level(None, true), GroupCollapseLevel::L2Summary);
    }

    #[test]
    fn group_per_item_none_returns_l0bodies_when_global_expanded() {
        assert_eq!(resolve_group_level(None, false), GroupCollapseLevel::L0Bodies);
    }

    #[test]
    fn carve_out_true_for_execute() {
        let tc = make_tc("Bash");
        assert!(is_carved_out_from_global_directive(&tc));
    }

    #[test]
    fn carve_out_true_for_diff_content() {
        let mut tc = make_tc("Read");
        tc.content.push(model::ToolCallContent::Diff(model::Diff::new("/tmp/x.rs", "")));
        assert!(is_carved_out_from_global_directive(&tc));
    }

    #[test]
    fn carve_out_true_for_monitor() {
        let tc = make_tc("Monitor");
        assert!(is_carved_out_from_global_directive(&tc));
    }

    #[test]
    fn carve_out_true_for_workflow() {
        let tc = make_tc("Workflow");
        assert!(is_carved_out_from_global_directive(&tc));
    }

    #[test]
    fn carve_out_false_for_plain_read() {
        let tc = make_tc("Read");
        assert!(!is_carved_out_from_global_directive(&tc));
    }

    #[test]
    fn carve_out_false_for_grep() {
        let tc = make_tc("Grep");
        assert!(!is_carved_out_from_global_directive(&tc));
    }
}

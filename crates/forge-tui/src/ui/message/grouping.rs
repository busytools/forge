//! Render-time grouping of consecutive collapsed-by-default tool
//! calls into a single summary block with a 3-level expand cycle.
//!
//! See `docs/superpowers/specs/2026-06-01-chat-tool-grouping.md`.

use std::ops::Range;

use crate::app::MessageBlock;

/// True when `sdk_tool_name` should be grouped when it appears in a
/// run of >= 2 consecutive collapsed-by-default tool calls.
pub fn is_groupable_by_default(sdk_tool_name: &str) -> bool {
    matches!(sdk_tool_name, "Read" | "Grep" | "Glob" | "Bash")
}

/// True when `block` breaks a group-run when encountered. Run-breakers
/// split a run into group-above / breaker / group-below; each side
/// independently meets the `>= 2` threshold to form a group.
///
/// Run-breakers: any non-`ToolCall` block (Text, Notice, Welcome,
/// ImageAttachment) plus any `ToolCall` whose `sdk_tool_name` is not
/// groupable-by-default (Edit / Write / Monitor / Workflow / ...).
pub fn is_run_breaker(block: &MessageBlock) -> bool {
    match block {
        MessageBlock::ToolCall(tc) => !is_groupable_by_default(&tc.sdk_tool_name),
        _ => true,
    }
}

/// Per-kind tally for a group's summary line. Read counts as a read;
/// Grep / Glob count as searches; Bash counts as a command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct KindCount {
    pub reads: usize,
    pub searches: usize,
    pub commands: usize,
}

impl KindCount {
    pub fn tally(&mut self, sdk_tool_name: &str) {
        match sdk_tool_name {
            "Read" => self.reads += 1,
            "Grep" | "Glob" => self.searches += 1,
            "Bash" => self.commands += 1,
            _ => {}
        }
    }

    /// `<n> reads \u{b7} <m> searches \u{b7} <k> commands`. Kinds with
    /// count 0 are dropped. Order: reads, searches, commands. Empty
    /// when total is 0.
    pub fn format_summary(&self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(3);
        if self.reads > 0 {
            parts.push(format!("{} {}", self.reads, plural(self.reads, "read", "reads")));
        }
        if self.searches > 0 {
            parts.push(format!(
                "{} {}",
                self.searches,
                plural(self.searches, "search", "searches")
            ));
        }
        if self.commands > 0 {
            parts.push(format!(
                "{} {}",
                self.commands,
                plural(self.commands, "command", "commands")
            ));
        }
        parts.join(" \u{b7} ")
    }
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// Group expand level. ctrl+x cycles L2 -> L1 -> L0 -> L2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GroupCollapseLevel {
    /// Default: a single summary line.
    #[default]
    L2Summary,
    /// All individual tool TITLE rows; bodies stay closed.
    L1Titles,
    /// All individual rows fully expanded (titles + bodies).
    L0Bodies,
}

impl GroupCollapseLevel {
    pub fn next(self) -> Self {
        match self {
            Self::L2Summary => Self::L1Titles,
            Self::L1Titles => Self::L0Bodies,
            Self::L0Bodies => Self::L2Summary,
        }
    }
}

/// Stable identity for a group within a session, derived from the
/// leading tool call's `tool_use_id`. Tool ids are unique per session
/// (one wire envelope per tool) and the leading position is stable
/// across renders because `Vec<MessageBlock>` is append-only after
/// construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupId(pub String);

impl GroupId {
    pub fn from_leader_id(leader_tool_use_id: impl Into<String>) -> Self {
        Self(leader_tool_use_id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A render-time chunk: either an individual block (today's behaviour)
/// or a Group over a maximal run of groupable tool calls. Indices into
/// the underlying `Vec<MessageBlock>` keep the type non-borrowing so
/// callers can still mutably index back into the message.
#[derive(Debug, Clone)]
pub enum RenderUnit {
    Individual(usize),
    Group { range: Range<usize>, leader_id: GroupId, kind_count: KindCount },
}

/// Walk `blocks` and return the [`GroupId`] of the group whose
/// run starts at `block_idx`, if any. Used by the mouse handler to
/// classify a click on a tool-row position as either an in-group
/// click (when the level is L2 and the position is the leader's) or
/// a normal per-tool click.
pub fn group_leader_at(blocks: &[MessageBlock], block_idx: usize) -> Option<GroupId> {
    let units = partition_blocks_into_render_units(blocks);
    units.into_iter().find_map(|unit| match unit {
        RenderUnit::Group { range, leader_id, .. } if range.start == block_idx => Some(leader_id),
        _ => None,
    })
}

/// Walk `blocks` identifying maximal runs of >= 2 consecutive groupable
/// tool calls. Each qualifying run becomes a `RenderUnit::Group`; every
/// other block (including lone groupable tools between breakers) becomes
/// `RenderUnit::Individual`.
pub fn partition_blocks_into_render_units(blocks: &[MessageBlock]) -> Vec<RenderUnit> {
    const GROUP_THRESHOLD: usize = 2;
    let mut units = Vec::with_capacity(blocks.len());
    let mut i = 0;
    while i < blocks.len() {
        if is_run_breaker(&blocks[i]) {
            units.push(RenderUnit::Individual(i));
            i += 1;
            continue;
        }
        let run_start = i;
        let mut run_end_exclusive = i + 1;
        while run_end_exclusive < blocks.len() && !is_run_breaker(&blocks[run_end_exclusive]) {
            run_end_exclusive += 1;
        }
        let run_len = run_end_exclusive - run_start;
        if run_len < GROUP_THRESHOLD {
            for idx in run_start..run_end_exclusive {
                units.push(RenderUnit::Individual(idx));
            }
        } else {
            // `blocks[run_start]` is guaranteed to be a groupable ToolCall:
            // we only enter this branch from the outer `if is_run_breaker`
            // false branch, and `is_run_breaker` returns false only for
            // ToolCall(tc) with a groupable sdk_tool_name. Defensive
            // fallback path (the leading block somehow isn't a ToolCall)
            // emits the run as Individual rows so the renderer can't
            // panic on bad input.
            let MessageBlock::ToolCall(leader_tc) = &blocks[run_start] else {
                for idx in run_start..run_end_exclusive {
                    units.push(RenderUnit::Individual(idx));
                }
                i = run_end_exclusive;
                continue;
            };
            let leader_id = GroupId::from_leader_id(leader_tc.id.clone());
            let mut kind_count = KindCount::default();
            for block in &blocks[run_start..run_end_exclusive] {
                if let MessageBlock::ToolCall(tc) = block {
                    kind_count.tally(&tc.sdk_tool_name);
                }
            }
            units.push(RenderUnit::Group {
                range: run_start..run_end_exclusive,
                leader_id,
                kind_count,
            });
        }
        i = run_end_exclusive;
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::model;
    use crate::app::{BlockCache, TerminalSnapshotMode, TextBlock, ToolCallInfo};

    fn tool_call_block(id: &str, sdk_tool_name: &str) -> MessageBlock {
        MessageBlock::ToolCall(Box::new(ToolCallInfo {
            id: id.to_owned(),
            title: format!("tool {id}"),
            sdk_tool_name: sdk_tool_name.to_owned(),
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
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
        }))
    }

    fn text_block(text: &str) -> MessageBlock {
        MessageBlock::Text(TextBlock::from_complete(text))
    }

    fn make(spec: &[(&str, &str)]) -> Vec<MessageBlock> {
        spec.iter()
            .enumerate()
            .map(|(i, (kind, name))| match *kind {
                "tool" => tool_call_block(&format!("tu-{i}"), name),
                "text" => text_block(name),
                other => panic!("unknown block kind {other:?}"),
            })
            .collect()
    }

    #[test]
    fn groupable_by_default_covers_the_four_collapsed_kinds() {
        assert!(is_groupable_by_default("Read"));
        assert!(is_groupable_by_default("Grep"));
        assert!(is_groupable_by_default("Glob"));
        assert!(is_groupable_by_default("Bash"));
    }

    #[test]
    fn groupable_by_default_excludes_expanded_and_block_tools() {
        for n in ["Edit", "Write", "MultiEdit", "Monitor", "Workflow", "Task", "Agent", ""] {
            assert!(!is_groupable_by_default(n), "{n} must NOT be groupable");
        }
    }

    #[test]
    fn run_breaker_true_for_non_groupable_tools_and_text_blocks() {
        assert!(is_run_breaker(&tool_call_block("a", "Edit")));
        assert!(is_run_breaker(&tool_call_block("b", "Monitor")));
        assert!(is_run_breaker(&text_block("hi")));
    }

    #[test]
    fn run_breaker_false_for_groupable_tools() {
        for n in ["Read", "Grep", "Glob", "Bash"] {
            assert!(!is_run_breaker(&tool_call_block("a", n)));
        }
    }

    #[test]
    fn group_collapse_level_cycles_l2_l1_l0() {
        let l = GroupCollapseLevel::default();
        assert_eq!(l, GroupCollapseLevel::L2Summary);
        let l = l.next();
        assert_eq!(l, GroupCollapseLevel::L1Titles);
        let l = l.next();
        assert_eq!(l, GroupCollapseLevel::L0Bodies);
        let l = l.next();
        assert_eq!(l, GroupCollapseLevel::L2Summary);
    }

    #[test]
    fn kind_count_format_summary_drops_zero_kinds_and_handles_singulars() {
        let k = KindCount { reads: 5, searches: 3, commands: 2 };
        assert_eq!(k.format_summary(), "5 reads \u{b7} 3 searches \u{b7} 2 commands");

        let k = KindCount { reads: 1, commands: 1, ..KindCount::default() };
        assert_eq!(k.format_summary(), "1 read \u{b7} 1 command");

        assert_eq!(KindCount::default().format_summary(), "");
    }

    #[test]
    fn partition_lone_groupable_tool_stays_individual() {
        let blocks = make(&[("tool", "Read")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 1);
        assert!(matches!(units[0], RenderUnit::Individual(0)));
    }

    #[test]
    fn partition_two_consecutive_groupable_tools_form_a_group() {
        let blocks = make(&[("tool", "Read"), ("tool", "Read")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 1);
        match &units[0] {
            RenderUnit::Group { range, kind_count, leader_id } => {
                assert_eq!(*range, 0..2);
                assert_eq!(kind_count.reads, 2);
                assert_eq!(kind_count.reads + kind_count.searches + kind_count.commands, 2);
                assert_eq!(leader_id.as_str(), "tu-0");
            }
            RenderUnit::Individual(_) => panic!("expected Group"),
        }
    }

    #[test]
    fn partition_run_then_breaker_then_run_splits_into_three_units() {
        let blocks = make(&[
            ("tool", "Read"),
            ("tool", "Read"),
            ("tool", "Read"),
            ("tool", "Edit"),
            ("tool", "Bash"),
            ("tool", "Bash"),
        ]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 3);
        match &units[0] {
            RenderUnit::Group { kind_count, range, .. } => {
                assert_eq!(kind_count.reads, 3);
                assert_eq!(*range, 0..3);
            }
            RenderUnit::Individual(_) => panic!("expected first Group"),
        }
        assert!(matches!(units[1], RenderUnit::Individual(3)));
        match &units[2] {
            RenderUnit::Group { kind_count, range, .. } => {
                assert_eq!(kind_count.commands, 2);
                assert_eq!(*range, 4..6);
            }
            RenderUnit::Individual(_) => panic!("expected second Group"),
        }
    }

    #[test]
    fn partition_breaker_in_middle_keeps_lone_tools_individual() {
        let blocks = make(&[("tool", "Read"), ("tool", "Edit"), ("tool", "Read")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 3);
        assert!(units.iter().all(|u| matches!(u, RenderUnit::Individual(_))));
    }

    #[test]
    fn partition_mixed_kind_run_tallies_per_kind() {
        let blocks = make(&[
            ("tool", "Read"),
            ("tool", "Grep"),
            ("tool", "Glob"),
            ("tool", "Bash"),
            ("tool", "Bash"),
        ]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 1);
        match &units[0] {
            RenderUnit::Group { kind_count, .. } => {
                assert_eq!(kind_count.reads, 1);
                assert_eq!(kind_count.searches, 2);
                assert_eq!(kind_count.commands, 2);
                assert_eq!(kind_count.reads + kind_count.searches + kind_count.commands, 5);
            }
            RenderUnit::Individual(_) => panic!("expected Group"),
        }
    }

    #[test]
    fn partition_empty_blocks_returns_empty() {
        let units = partition_blocks_into_render_units(&[]);
        assert!(units.is_empty());
    }

    #[test]
    fn partition_all_breakers_returns_all_individuals() {
        let blocks = make(&[("tool", "Edit"), ("tool", "Write"), ("text", "hello")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 3);
        assert!(units.iter().all(|u| matches!(u, RenderUnit::Individual(_))));
    }
}

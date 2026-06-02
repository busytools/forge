//! Render-time grouping of consecutive collapsed-by-default tool
//! calls into a single summary block with a 3-level expand cycle.
//!
//! See `docs/superpowers/specs/2026-06-01-chat-tool-grouping.md`.

use std::ops::Range;

use crate::app::MessageBlock;

/// True when `block` breaks a group-run when encountered. Run-breakers
/// split a run into group-above / breaker / group-below; each side
/// independently meets the threshold (v2: 1+) to form a group.
///
/// A ToolCall is a run-breaker iff it has a bespoke chat render path
/// rather than the standard collapsible tool card. Classified by
/// RENDER CLASS (not a hardcoded name list) so any new tool that
/// renders as the standard card folds by default; only adding a new
/// bespoke renderer requires extending this predicate.
///
/// Run-breakers:
/// - Any non-`ToolCall` block (Text, Notice, Welcome, ImageAttachment).
/// - Edit / Write / MultiEdit / NotebookEdit by name (mutations the
///   user's hard requirement says NEVER fold, NEVER collapse by
///   default). The diff-content check below catches the same set
///   once the result lands; the name-based check covers the
///   in-flight window before the diff content arrives.
/// - Any tool whose `content` carries a `ToolCallContent::Diff`
///   entry (Edit / Write / MultiEdit / NotebookEdit post-result).
/// - Tools with a lifecycle-one-liner render path (Monitor / Workflow -
///   `ui/message.rs::render_lifecycle_one_liner` arms).
/// - Tools rendered as a peer block
///   (`ui/peer_block.rs::detect_outbound` match set:
///   peers__ask_agent / peers__tell_agent / workers__ask /
///   workers__tell).
///
/// `tc.hidden == true` (chat-suppressed: Task* / AskUserQuestion /
/// Schedule* / Cron*) is NOT a breaker - hidden tools render nothing
/// visible in the chat stream, so they pass through the run so
/// adjacent visible groups merge across them.
///
/// Everything else folds, including WebFetch / WebSearch / LSP /
/// plain MCP calls (all render as the standard collapsible tool
/// card).
pub fn is_run_breaker(block: &MessageBlock) -> bool {
    let MessageBlock::ToolCall(tc) = block else {
        return true;
    };
    if tc.hidden {
        return false;
    }
    if is_edit_tool(&tc.sdk_tool_name) {
        return true;
    }
    let has_diff =
        tc.content.iter().any(|c| matches!(c, crate::agent::model::ToolCallContent::Diff(_)));
    if has_diff {
        return true;
    }
    if is_lifecycle_render_tool(&tc.sdk_tool_name) {
        return true;
    }
    if is_peer_block_render_tool(&tc.sdk_tool_name) {
        return true;
    }
    false
}

/// Mutation tools by name. Always-break belt-and-suspenders covering
/// the in-flight window before the diff content arrives. The user's
/// hard requirement: mutations NEVER fold, NEVER collapse by default.
fn is_edit_tool(sdk_tool_name: &str) -> bool {
    matches!(sdk_tool_name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
}

/// Tools whose chat surface is the lifecycle one-liner / block render
/// in `ui/message.rs::render_lifecycle_one_liner` (rather than the
/// standard tool card). Name-based because the render fn matches by
/// `sdk_tool_name` literal.
fn is_lifecycle_render_tool(sdk_tool_name: &str) -> bool {
    matches!(sdk_tool_name, "Monitor" | "Workflow")
}

/// Tools whose chat surface is the peer-block render in
/// `ui/peer_block.rs::detect_outbound` (rather than the standard tool
/// card). Name-based because `detect_outbound` matches by
/// `sdk_tool_name` literal. Mirror its match set exactly.
fn is_peer_block_render_tool(sdk_tool_name: &str) -> bool {
    matches!(
        sdk_tool_name,
        "mcp__forge__peers__ask_agent"
            | "mcp__forge__peers__tell_agent"
            | "mcp__forge__workers__ask"
            | "mcp__forge__workers__tell",
    )
}

/// Per-kind tally for a group's summary line. `Read` counts as a
/// read; `Grep` / `Glob` / `WebSearch` count as searches; `Bash`
/// counts as a command. Everything else (`WebFetch`, `LSP`, plain
/// `mcp__*` calls) tallies into the generic `calls` bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct KindCount {
    pub reads: usize,
    pub searches: usize,
    pub commands: usize,
    /// Generic bucket for tools that fold but don't fit reads /
    /// searches / commands. Renders last in the summary as
    /// `<n> call` / `<n> calls`.
    pub calls: usize,
}

impl KindCount {
    pub fn tally(&mut self, sdk_tool_name: &str) {
        match sdk_tool_name {
            "Read" => self.reads += 1,
            "Grep" | "Glob" | "WebSearch" => self.searches += 1,
            "Bash" => self.commands += 1,
            _ => self.calls += 1,
        }
    }

    /// `<n> reads \u{b7} <m> searches \u{b7} <k> commands \u{b7} <l> calls`.
    /// Kinds with count 0 are dropped. Order: reads, searches,
    /// commands, calls. Empty when every bucket is 0.
    pub fn format_summary(&self) -> String {
        let mut parts: Vec<String> = Vec::with_capacity(4);
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
        if self.calls > 0 {
            parts.push(format!("{} {}", self.calls, plural(self.calls, "call", "calls")));
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

/// Aggregate status across a run's blocks, used by the L2 summary
/// line's status_icon (spec decision 6 / v2.1). Priority:
///
/// - `InProgress` wins if ANY tool is in flight (the summary animates
///   the spinner).
/// - Else `Failed` wins if ANY tool failed or was killed (red cross).
/// - Else `Pending` if any is still pending (hollow circle).
/// - Else `Completed` (all clean - green check).
///
/// Non-ToolCall blocks are skipped (the partitioner never emits a
/// Group containing them, but the helper stays defensive).
pub fn aggregate_run_status(blocks: &[MessageBlock]) -> crate::agent::model::ToolCallStatus {
    use crate::agent::model::ToolCallStatus;
    let mut any_failed = false;
    let mut any_pending = false;
    for block in blocks {
        if let MessageBlock::ToolCall(tc) = block {
            if tc.hidden {
                continue;
            }
            match tc.status {
                ToolCallStatus::InProgress => return ToolCallStatus::InProgress,
                ToolCallStatus::Failed | ToolCallStatus::Killed => any_failed = true,
                ToolCallStatus::Pending => any_pending = true,
                ToolCallStatus::Completed => {}
            }
        }
    }
    if any_failed {
        ToolCallStatus::Failed
    } else if any_pending {
        ToolCallStatus::Pending
    } else {
        ToolCallStatus::Completed
    }
}

/// First-N target names plus an `+N` overflow count for the remaining
/// targets that don't render by name. Per the chat-messaging-grouping
/// spec: render the first 2 targets by name; everything past that
/// counts into `overflow_n` (rendered as `+N` after the named list).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct MessagingDirectionTargets {
    /// First-N target names in order of first appearance.
    pub targets: Vec<String>,
    /// Count of remaining targets beyond `targets`; renders as `+N`.
    pub overflow_n: usize,
}

impl MessagingDirectionTargets {
    /// `true` when no targets at all (rendered direction clause is
    /// omitted entirely - no "0 inbound" filler).
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty() && self.overflow_n == 0
    }
}

/// One per-message slice of a messaging group. A messaging group can
/// span multiple messages (cross-turn merge) - each message's render
/// reads the segment whose `msg_idx` matches, and the segment carries
/// the per-message render data plus pointers to its position in the
/// parent group (above/below continuation markers + total count).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessagingGroupSegment {
    /// Index of the message this segment lives in.
    pub msg_idx: usize,
    /// Block range within that message (start..end exclusive).
    pub block_range: Range<usize>,
    /// Count of peer/worker blocks in THIS segment only.
    pub segment_count: usize,
    /// Outbound targets seen in THIS segment.
    pub segment_outbound_targets: MessagingDirectionTargets,
    /// Inbound targets seen in THIS segment.
    pub segment_inbound_targets: MessagingDirectionTargets,
    /// True if the parent group has segments in earlier messages.
    pub segment_continues_above: bool,
    /// True if the parent group has segments in later messages.
    pub segment_continues_below: bool,
    /// Aggregate run-status across THIS segment.
    pub aggregate_status: crate::agent::model::ToolCallStatus,
    /// Total count across all segments in the parent group.
    pub group_total_count: usize,
}

/// A render-time chunk: either an individual block (today's behaviour),
/// a Group over a maximal run of groupable tool calls, or a
/// MessagingGroup over a run of peer/worker messages (potentially
/// spanning multiple messages via the session-walking partitioner).
/// Indices into the underlying `Vec<MessageBlock>` keep the type
/// non-borrowing so callers can still mutably index back into the
/// message.
#[derive(Debug, Clone)]
pub enum RenderUnit {
    Individual(usize),
    Group {
        range: Range<usize>,
        leader_id: GroupId,
        kind_count: KindCount,
        /// Aggregate status across the run (see `aggregate_run_status`)
        /// so the L2 summary's status_icon stays in sync as tools flip
        /// through the InProgress -> Completed lifecycle.
        aggregate_status: crate::agent::model::ToolCallStatus,
    },
    /// A run of consecutive peer/worker MCP message blocks (outbound
    /// + inbound). Unlike `Group`, a messaging group can span multiple
    /// messages - each segment carries its per-message slice + the
    /// `segment_continues_above/below` flags for the continuation
    /// marker render. All segments share `group_leader_id` so a click
    /// or cycle on any segment maps to the same collapse level via
    /// `UiSession.messaging_group_collapse_levels`.
    MessagingGroup {
        segments: Vec<MessagingGroupSegment>,
        group_leader_id: GroupId,
    },
}

/// Walk `blocks` and return the [`GroupId`] plus the run length of
/// the group whose run starts at `block_idx`, if any. Used by the
/// mouse handler to classify a click on a tool-row position as
/// either an in-group click (multi-item group at L2 sets focus and
/// cycles to L1) or a normal per-tool click. Single-item groups
/// (run length 1) fall through to per-tool toggle even at L2
/// because the title row IS the click target the user is
/// interacting with; the group cycle is still reachable via ctrl+x.
pub fn group_leader_at(blocks: &[MessageBlock], block_idx: usize) -> Option<(GroupId, usize)> {
    let units = partition_blocks_into_render_units(blocks);
    units.into_iter().find_map(|unit| match unit {
        RenderUnit::Group { range, leader_id, .. } if range.start == block_idx => {
            Some((leader_id, range.len()))
        }
        _ => None,
    })
}

/// True when `block` is a hidden / chat-suppressed tool call.
/// Hidden tools render nothing in the chat stream; the partitioner
/// emits them as `RenderUnit::Individual` (to preserve their block
/// index for hit-test consistency) but never includes them as the
/// leader of a Group, and they neither tally into KindCount nor
/// contribute to the aggregate run status.
fn is_hidden_tool_call(block: &MessageBlock) -> bool {
    matches!(block, MessageBlock::ToolCall(tc) if tc.hidden)
}

/// Walk `blocks` identifying maximal runs of >= 2 consecutive groupable
/// tool calls. Each qualifying run becomes a `RenderUnit::Group`; every
/// other block (including lone groupable tools between breakers and
/// any hidden tool call) becomes `RenderUnit::Individual`.
pub fn partition_blocks_into_render_units(blocks: &[MessageBlock]) -> Vec<RenderUnit> {
    // v2 (PR following #300): every consecutive run of non-breaker
    // tool calls forms a `RenderUnit::Group`, even runs of length 1.
    // The single-item L2 render in `message.rs` short-circuits to the
    // L1 path so the call stays visible by default; the cycle still
    // walks L2 -> L1 -> L0.
    //
    // Hidden / chat-suppressed tool calls render nothing visible. They
    // pass THROUGH a run so adjacent visible groups merge across them,
    // but a lone hidden block between visible breakers is emitted as
    // Individual (never starts its own Group). Inside a run they
    // extend the range but never tally or set the leader.
    const GROUP_THRESHOLD: usize = 1;
    let mut units = Vec::with_capacity(blocks.len());
    let mut i = 0;
    while i < blocks.len() {
        if is_hidden_tool_call(&blocks[i]) {
            units.push(RenderUnit::Individual(i));
            i += 1;
            continue;
        }
        if is_run_breaker(&blocks[i]) {
            units.push(RenderUnit::Individual(i));
            i += 1;
            continue;
        }
        let run_start = i;
        let mut run_end_exclusive = i + 1;
        while run_end_exclusive < blocks.len() {
            let block = &blocks[run_end_exclusive];
            if is_hidden_tool_call(block) {
                run_end_exclusive += 1;
                continue;
            }
            if is_run_breaker(block) {
                break;
            }
            run_end_exclusive += 1;
        }
        let run_len = run_end_exclusive - run_start;
        if run_len < GROUP_THRESHOLD {
            for idx in run_start..run_end_exclusive {
                units.push(RenderUnit::Individual(idx));
            }
        } else {
            // `blocks[run_start]` is guaranteed to be a visible groupable
            // ToolCall: the outer loop's hidden-and-breaker checks both
            // return false for this slot. Defensive fallback (the leading
            // block somehow isn't a ToolCall) emits the run as Individual
            // rows so the renderer can't panic on bad input.
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
                if let MessageBlock::ToolCall(tc) = block
                    && !tc.hidden
                {
                    kind_count.tally(&tc.sdk_tool_name);
                }
            }
            let aggregate_status = aggregate_run_status(&blocks[run_start..run_end_exclusive]);
            units.push(RenderUnit::Group {
                range: run_start..run_end_exclusive,
                leader_id,
                kind_count,
                aggregate_status,
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

    fn diff_tool_call_block(id: &str, sdk_tool_name: &str) -> MessageBlock {
        // A tool call whose content carries a `ToolCallContent::Diff`
        // entry. The `is_run_breaker` predicate keys off this variant
        // to recognize diff-rendering tools (Edit / Write /
        // MultiEdit / NotebookEdit) and treat them as breakers.
        let mut block = tool_call_block(id, sdk_tool_name);
        if let MessageBlock::ToolCall(tc) = &mut block {
            tc.content = vec![model::ToolCallContent::Diff(model::Diff::new("/tmp/dummy.rs", ""))];
        }
        block
    }

    fn hidden_tool_call_block(id: &str, sdk_tool_name: &str) -> MessageBlock {
        let mut block = tool_call_block(id, sdk_tool_name);
        if let MessageBlock::ToolCall(tc) = &mut block {
            tc.hidden = true;
        }
        block
    }

    fn tool_call_block_with_status(
        id: &str,
        sdk_tool_name: &str,
        status: model::ToolCallStatus,
    ) -> MessageBlock {
        let mut block = tool_call_block(id, sdk_tool_name);
        if let MessageBlock::ToolCall(tc) = &mut block {
            tc.status = status;
        }
        block
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
    fn run_breaker_true_for_special_render_and_text_blocks() {
        assert!(is_run_breaker(&diff_tool_call_block("a", "Edit")));
        assert!(is_run_breaker(&diff_tool_call_block("b", "Write")));
        assert!(is_run_breaker(&tool_call_block("c", "Monitor")));
        assert!(is_run_breaker(&tool_call_block("d", "Workflow")));
        assert!(is_run_breaker(&text_block("hi")));
    }

    #[test]
    fn run_breaker_false_for_groupable_tools() {
        for n in ["Read", "Grep", "Glob", "Bash"] {
            assert!(!is_run_breaker(&tool_call_block("a", n)));
        }
    }

    /// Hidden / chat-suppressed tools (Task* / AskUserQuestion /
    /// Schedule* / Cron*) render nothing visible in the chat stream;
    /// they pass through the run so adjacent visible groups merge
    /// across them.
    #[test]
    fn run_breaker_false_for_hidden_chat_suppressed_tools() {
        for n in [
            "TaskCreate",
            "TaskUpdate",
            "TaskList",
            "TaskGet",
            "TaskOutput",
            "TaskStop",
            "AskUserQuestion",
            "ScheduleWakeup",
            "CronCreate",
            "CronDelete",
        ] {
            assert!(
                !is_run_breaker(&hidden_tool_call_block("x", n)),
                "{n} is chat-suppressed and must pass through the run",
            );
        }
    }

    #[test]
    fn run_breaker_false_for_v2_newly_grouped_tools() {
        // Tools that v1 broke runs on (allow-list miss) but v2 folds
        // because they render as the standard tool card.
        for n in ["WebSearch", "WebFetch", "LSP", "mcp__forge__some_other_tool"] {
            assert!(
                !is_run_breaker(&tool_call_block("x", n)),
                "{n} should fold in v2 (standard tool card render)",
            );
        }
    }

    /// Mandatory invariant: every tool with a bespoke visible chat
    /// render path (diff view, lifecycle one-liner, peer block) MUST
    /// be a run-breaker so its render can't be silently folded away.
    /// Chat-suppressed tools (Task* / AskUserQuestion / Schedule* /
    /// Cron*) render nothing visible and intentionally pass through
    /// the run; they are covered by `run_breaker_false_for_hidden_
    /// chat_suppressed_tools`.
    ///
    /// Adding a new bespoke visible renderer requires extending BOTH
    /// this test's enumeration AND `is_run_breaker`'s predicate in
    /// the same change. Otherwise the next group containing the new
    /// tool folds and the bespoke render never fires.
    #[test]
    fn every_special_render_tool_is_a_run_breaker() {
        // Mutations: assert breaker behaviour BOTH with diff content
        // present (post-result) AND without (in-flight window). The
        // name-based `is_edit_tool` check is belt-and-suspenders to
        // the `has_diff` content check.
        for name in ["Edit", "Write", "MultiEdit", "NotebookEdit"] {
            assert!(
                is_run_breaker(&diff_tool_call_block("x", name)),
                "{name} with diff content MUST break runs",
            );
            assert!(
                is_run_breaker(&tool_call_block("x", name)),
                "{name} without diff content (in-flight) MUST still break runs",
            );
        }
        for name in ["Monitor", "Workflow"] {
            assert!(
                is_run_breaker(&tool_call_block("x", name)),
                "{name} renders a lifecycle block and MUST break runs",
            );
        }
        for name in [
            "mcp__forge__peers__ask_agent",
            "mcp__forge__peers__tell_agent",
            "mcp__forge__workers__ask",
            "mcp__forge__workers__tell",
        ] {
            assert!(
                is_run_breaker(&tool_call_block("x", name)),
                "{name} renders as a peer block and MUST break runs",
            );
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
        let k = KindCount { reads: 5, searches: 3, commands: 2, calls: 0 };
        assert_eq!(k.format_summary(), "5 reads \u{b7} 3 searches \u{b7} 2 commands");

        let k = KindCount { reads: 1, commands: 1, ..KindCount::default() };
        assert_eq!(k.format_summary(), "1 read \u{b7} 1 command");

        assert_eq!(KindCount::default().format_summary(), "");
    }

    #[test]
    fn kind_count_tallies_websearch_as_search_and_other_tools_as_calls() {
        let mut k = KindCount::default();
        k.tally("Read");
        k.tally("Grep");
        k.tally("Glob");
        k.tally("WebSearch");
        k.tally("Bash");
        k.tally("WebFetch");
        k.tally("LSP");
        k.tally("mcp__forge__some_other_tool");
        assert_eq!(k.reads, 1);
        assert_eq!(k.searches, 3); // Grep + Glob + WebSearch
        assert_eq!(k.commands, 1);
        assert_eq!(k.calls, 3); // WebFetch + LSP + mcp__*
    }

    #[test]
    fn kind_count_format_summary_includes_calls_bucket() {
        let k = KindCount { reads: 3, searches: 2, commands: 1, calls: 4 };
        assert_eq!(k.format_summary(), "3 reads \u{b7} 2 searches \u{b7} 1 command \u{b7} 4 calls",);

        let k = KindCount { calls: 1, ..KindCount::default() };
        assert_eq!(k.format_summary(), "1 call");
    }

    #[test]
    fn partition_mixed_kind_run_with_v2_tools_tallies_calls_bucket() {
        let blocks =
            make(&[("tool", "Read"), ("tool", "WebSearch"), ("tool", "WebFetch"), ("tool", "LSP")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 1);
        match &units[0] {
            RenderUnit::Group { kind_count, .. } => {
                assert_eq!(kind_count.reads, 1);
                assert_eq!(kind_count.searches, 1); // WebSearch
                assert_eq!(kind_count.calls, 2); // WebFetch + LSP
            }
            RenderUnit::Individual(_) => panic!("expected Group"),
        }
    }

    #[test]
    fn partition_lone_groupable_tool_forms_single_item_group() {
        let blocks = make(&[("tool", "Read")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 1);
        match &units[0] {
            RenderUnit::Group { range, kind_count, leader_id, .. } => {
                assert_eq!(*range, 0..1);
                assert_eq!(kind_count.reads, 1);
                assert_eq!(leader_id.as_str(), "tu-0");
            }
            RenderUnit::Individual(_) => panic!("expected Group, got Individual"),
        }
    }

    #[test]
    fn partition_two_consecutive_groupable_tools_form_a_group() {
        let blocks = make(&[("tool", "Read"), ("tool", "Read")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 1);
        match &units[0] {
            RenderUnit::Group { range, kind_count, leader_id, .. } => {
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
        // v2: the Edit breaker carries a `Diff` content block so the
        // render-class predicate flags it. Without the Diff content
        // it'd render as the standard tool card and fold into the
        // surrounding run.
        let blocks = vec![
            tool_call_block("tu-0", "Read"),
            tool_call_block("tu-1", "Read"),
            tool_call_block("tu-2", "Read"),
            diff_tool_call_block("tu-3", "Edit"),
            tool_call_block("tu-4", "Bash"),
            tool_call_block("tu-5", "Bash"),
        ];
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
    fn partition_breaker_in_middle_splits_into_three_groups() {
        // v2: lone Read on each side of the Edit breaker forms a
        // single-item group instead of an Individual. The Edit
        // breaker itself stays Individual. The Edit fixture must
        // carry a `Diff` content block so the render-class predicate
        // recognises it as a breaker.
        let blocks = vec![
            tool_call_block("tu-0", "Read"),
            diff_tool_call_block("tu-1", "Edit"),
            tool_call_block("tu-2", "Read"),
        ];
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 3);
        match &units[0] {
            RenderUnit::Group { range, kind_count, .. } => {
                assert_eq!(*range, 0..1);
                assert_eq!(kind_count.reads, 1);
            }
            RenderUnit::Individual(_) => panic!("expected first Group"),
        }
        assert!(matches!(units[1], RenderUnit::Individual(1)));
        match &units[2] {
            RenderUnit::Group { range, kind_count, .. } => {
                assert_eq!(*range, 2..3);
                assert_eq!(kind_count.reads, 1);
            }
            RenderUnit::Individual(_) => panic!("expected third Group"),
        }
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
        // Mix of breaker shapes: Monitor (lifecycle), an Edit with a
        // Diff content (diff-class), and a plain text block.
        let blocks = vec![
            tool_call_block("tu-0", "Monitor"),
            diff_tool_call_block("tu-1", "Edit"),
            text_block("hello"),
        ];
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 3);
        assert!(units.iter().all(|u| matches!(u, RenderUnit::Individual(_))));
    }

    #[test]
    fn aggregate_run_status_in_progress_wins_over_completed_and_pending() {
        let blocks = vec![
            tool_call_block_with_status("a", "Read", model::ToolCallStatus::Completed),
            tool_call_block_with_status("b", "Read", model::ToolCallStatus::InProgress),
            tool_call_block_with_status("c", "Read", model::ToolCallStatus::Pending),
        ];
        assert_eq!(aggregate_run_status(&blocks), model::ToolCallStatus::InProgress);
    }

    #[test]
    fn aggregate_run_status_in_progress_wins_over_failed() {
        let blocks = vec![
            tool_call_block_with_status("a", "Bash", model::ToolCallStatus::Failed),
            tool_call_block_with_status("b", "Bash", model::ToolCallStatus::InProgress),
        ];
        assert_eq!(aggregate_run_status(&blocks), model::ToolCallStatus::InProgress);
    }

    #[test]
    fn aggregate_run_status_failed_wins_when_no_in_progress() {
        let blocks = vec![
            tool_call_block_with_status("a", "Read", model::ToolCallStatus::Completed),
            tool_call_block_with_status("b", "Bash", model::ToolCallStatus::Failed),
            tool_call_block_with_status("c", "Read", model::ToolCallStatus::Completed),
        ];
        assert_eq!(aggregate_run_status(&blocks), model::ToolCallStatus::Failed);
    }

    #[test]
    fn aggregate_run_status_killed_treated_as_failure() {
        let blocks = vec![
            tool_call_block_with_status("a", "Read", model::ToolCallStatus::Completed),
            tool_call_block_with_status("b", "Bash", model::ToolCallStatus::Killed),
        ];
        let agg = aggregate_run_status(&blocks);
        assert!(matches!(agg, model::ToolCallStatus::Failed | model::ToolCallStatus::Killed));
    }

    #[test]
    fn aggregate_run_status_pending_when_only_pending() {
        let blocks = vec![
            tool_call_block_with_status("a", "Read", model::ToolCallStatus::Pending),
            tool_call_block_with_status("b", "Read", model::ToolCallStatus::Pending),
        ];
        assert_eq!(aggregate_run_status(&blocks), model::ToolCallStatus::Pending);
    }

    #[test]
    fn aggregate_run_status_completed_when_all_completed() {
        let blocks = vec![
            tool_call_block_with_status("a", "Read", model::ToolCallStatus::Completed),
            tool_call_block_with_status("b", "Grep", model::ToolCallStatus::Completed),
        ];
        assert_eq!(aggregate_run_status(&blocks), model::ToolCallStatus::Completed);
    }

    #[test]
    fn aggregate_run_status_skips_non_tool_call_blocks() {
        let blocks = vec![
            text_block("hello"),
            tool_call_block_with_status("a", "Read", model::ToolCallStatus::InProgress),
        ];
        assert_eq!(aggregate_run_status(&blocks), model::ToolCallStatus::InProgress);
    }

    #[test]
    fn partition_carries_aggregate_status_on_group() {
        // Default tool_call_block status is InProgress.
        let blocks = make(&[("tool", "Read"), ("tool", "Read")]);
        let units = partition_blocks_into_render_units(&blocks);
        match &units[0] {
            RenderUnit::Group { aggregate_status, .. } => {
                assert_eq!(*aggregate_status, model::ToolCallStatus::InProgress);
            }
            RenderUnit::Individual(_) => panic!("expected Group"),
        }
    }

    #[test]
    fn partition_aggregate_completed_when_all_done() {
        let blocks = vec![
            tool_call_block_with_status("a", "Read", model::ToolCallStatus::Completed),
            tool_call_block_with_status("b", "Read", model::ToolCallStatus::Completed),
        ];
        let units = partition_blocks_into_render_units(&blocks);
        match &units[0] {
            RenderUnit::Group { aggregate_status, .. } => {
                assert_eq!(*aggregate_status, model::ToolCallStatus::Completed);
            }
            RenderUnit::Individual(_) => panic!("expected Group"),
        }
    }

    /// Two visible tool-call groups separated only by a single hidden
    /// (chat-suppressed) tool-call block merge into one render group.
    /// Hidden blocks render as nothing visible, so they must not
    /// phantom-split adjacent visible groupings.
    #[test]
    fn partition_merges_visible_groups_across_one_hidden_block() {
        let blocks = vec![
            tool_call_block("a", "Read"),
            tool_call_block("b", "Read"),
            hidden_tool_call_block("c", "TaskCreate"),
            tool_call_block("d", "Read"),
            tool_call_block("e", "Read"),
        ];
        let units = partition_blocks_into_render_units(&blocks);
        let groups: Vec<_> =
            units.iter().filter(|u| matches!(u, RenderUnit::Group { .. })).collect();
        assert_eq!(
            groups.len(),
            1,
            "5 visible Reads separated by one hidden TaskCreate must form a single group; got {} groups",
            groups.len(),
        );
    }

    /// Any consecutive run of hidden blocks passes through; multiple
    /// hidden blocks in a row do not split adjacent visible groups.
    #[test]
    fn partition_merges_visible_groups_across_multiple_hidden_blocks() {
        let blocks = vec![
            tool_call_block("a", "Read"),
            tool_call_block("b", "Read"),
            hidden_tool_call_block("c", "TaskCreate"),
            hidden_tool_call_block("d", "AskUserQuestion"),
            hidden_tool_call_block("e", "CronCreate"),
            tool_call_block("f", "Read"),
            tool_call_block("g", "Read"),
        ];
        let units = partition_blocks_into_render_units(&blocks);
        let groups: Vec<_> =
            units.iter().filter(|u| matches!(u, RenderUnit::Group { .. })).collect();
        assert_eq!(groups.len(), 1, "any consecutive run of hidden blocks must pass through");
    }

    /// Regression-lock: a peer/worker block is a visible breaker.
    /// Adjacent visible tool-call groups separated by it still split.
    #[test]
    fn partition_still_splits_across_peer_block() {
        let blocks = vec![
            tool_call_block("a", "Read"),
            tool_call_block("b", "Read"),
            tool_call_block("c", "mcp__forge__peers__ask_agent"),
            tool_call_block("d", "Read"),
            tool_call_block("e", "Read"),
        ];
        let units = partition_blocks_into_render_units(&blocks);
        let groups: Vec<_> =
            units.iter().filter(|u| matches!(u, RenderUnit::Group { .. })).collect();
        assert_eq!(groups.len(), 2, "peer block must continue to split adjacent tool-call groups");
    }

    /// Regression-lock: a Text block is a visible breaker. Adjacent
    /// visible tool-call groups separated by Text still split.
    #[test]
    fn partition_still_splits_across_text_block() {
        let blocks = vec![
            tool_call_block("a", "Read"),
            tool_call_block("b", "Read"),
            text_block("some assistant message"),
            tool_call_block("d", "Read"),
            tool_call_block("e", "Read"),
        ];
        let units = partition_blocks_into_render_units(&blocks);
        let groups: Vec<_> =
            units.iter().filter(|u| matches!(u, RenderUnit::Group { .. })).collect();
        assert_eq!(groups.len(), 2, "text block must continue to split adjacent tool-call groups");
    }

    /// A lone hidden block between visible breakers must not form a
    /// visible Group on its own. Hidden tools render nothing; emitting
    /// a single-item Group around one would surface a `1 call` L2
    /// summary line where the user expects nothing.
    #[test]
    fn lone_hidden_block_renders_nothing_visible() {
        let blocks = vec![
            text_block("before"),
            hidden_tool_call_block("a", "TaskCreate"),
            text_block("after"),
        ];
        let units = partition_blocks_into_render_units(&blocks);
        let groups: Vec<_> =
            units.iter().filter(|u| matches!(u, RenderUnit::Group { .. })).collect();
        assert_eq!(
            groups.len(),
            0,
            "a lone hidden block between text breakers must not form a visible group",
        );
    }

    /// Edge case: a visible group followed by a trailing hidden block
    /// at end-of-message renders cleanly with no spurious second
    /// group or empty group.
    #[test]
    fn partition_handles_trailing_hidden_block() {
        let blocks = vec![
            tool_call_block("a", "Read"),
            tool_call_block("b", "Read"),
            tool_call_block("c", "Read"),
            hidden_tool_call_block("d", "TaskCreate"),
        ];
        let units = partition_blocks_into_render_units(&blocks);
        let groups: Vec<_> =
            units.iter().filter(|u| matches!(u, RenderUnit::Group { .. })).collect();
        assert_eq!(
            groups.len(),
            1,
            "trailing hidden block must not produce a spurious second group",
        );
        for u in &groups {
            if let RenderUnit::Group { kind_count, .. } = u {
                let total =
                    kind_count.reads + kind_count.searches + kind_count.commands + kind_count.calls;
                assert!(total > 0, "no empty group should be produced");
            }
        }
    }
}

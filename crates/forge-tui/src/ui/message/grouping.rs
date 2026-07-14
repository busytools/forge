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
/// `tc.hidden == true` (chat-suppressed: Task* / AskUserQuestion while
/// unanswered / Schedule* / Cron*) is NOT a breaker - hidden tools
/// render nothing visible in the chat stream, so they pass through the
/// run so adjacent visible groups merge across them. (AskUserQuestion
/// un-hides once answered and then breaks, per the arm above.)
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
    // An answered AskUserQuestion un-hides (the record at answer time
    // flips it visible) and renders the question -> answer card, so it
    // breaks runs like any bespoke-render tool.
    if tc.is_ask_question_tool() {
        return true;
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

/// One kind-line in a group's L2 summary: a glyph-family (or MCP
/// server) with its count and one resolved target per call (uncapped -
/// the render nests one child row per target). Same-glyph tools (Grep /
/// Glob / LS) share one line; each `mcp__<server>__*` server gets its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KindLine {
    pub glyph: &'static str,
    pub label: String,
    pub count: usize,
    pub targets: Vec<String>,
}

/// Per-group L2 summary: one [`KindLine`] per glyph-family / MCP
/// server, in first-appearance order across the run. Replaces the old
/// four-bucket count so `WebFetch` / `LSP` / `mcp__*` read as their
/// own kinds instead of an opaque `N calls`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct KindSummary {
    pub lines: Vec<KindLine>,
}

/// L2 marker glyph for `mcp__<server>__*` lines - distinct from the
/// generic `\u{25cb}` so a server call reads apart from local tools.
pub const MCP_GLYPH: &str = "\u{25c8}";

impl KindSummary {
    /// Total tool calls across every kind line.
    pub fn total(&self) -> usize {
        self.lines.iter().map(|l| l.count).sum()
    }

    /// Fold one groupable tool call into its glyph-family / MCP-server
    /// line, creating the line on first appearance. Every kind keeps one
    /// resolved target per call (uncapped) so the render can nest one
    /// child row per instance.
    pub fn tally(&mut self, tc: &crate::app::ToolCallInfo) {
        let (glyph, label) = family_glyph_label(&tc.sdk_tool_name);
        let target = family_target(tc);
        if let Some(line) = self.lines.iter_mut().find(|l| l.glyph == glyph && l.label == label) {
            line.count += 1;
            push_target(&mut line.targets, target);
        } else {
            let mut targets = Vec::new();
            push_target(&mut targets, target);
            self.lines.push(KindLine { glyph, label, count: 1, targets });
        }
    }
}

/// The read glyph (`⬚`), keyed by the theme's `tool_name_label`. The
/// render special-cases read on this glyph: it relativizes each path
/// against the project root and clips with a middle-ellipsis (keeping
/// the filename), where every other kind clips end-first.
pub const READ_GLYPH: &str = "\u{2b1a}";

/// A tool's L2 glyph-family + label. Local tools key by the
/// [`crate::ui::theme::tool_name_label`] glyph so same-glyph tools
/// (Grep / Glob / LS) merge into one line; `mcp__<server>__*` keys by
/// server so each server gets its own line under [`MCP_GLYPH`].
fn family_glyph_label(sdk_tool_name: &str) -> (&'static str, String) {
    if let Some((server, _)) = mcp_parts(sdk_tool_name) {
        return (MCP_GLYPH, server.to_owned());
    }
    let (glyph, tool_label) = crate::ui::theme::tool_name_label(sdk_tool_name);
    let family = match glyph {
        "\u{2b1a}" => "read",
        "\u{2315}" => "search",
        "\u{25b6}" => "bash",
        "\u{2295}" => "web",
        "\u{2699}" => "lsp",
        "\u{2726}" => "skill",
        "\u{2316}" => "toolsearch",
        "\u{2299}" => "config",
        "\u{21c4}" => "worktree",
        "\u{25cb}" => "tool",
        _ => tool_label,
    };
    (glyph, family.to_owned())
}

/// Split an `mcp__<server>__<tool>` name into (server, tool). `None`
/// for non-MCP names or an empty server. Peer/worker MCP tools never
/// reach here - they are run-breakers.
fn mcp_parts(sdk_tool_name: &str) -> Option<(&str, &str)> {
    let rest = sdk_tool_name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty()).then_some((server, tool))
}

/// Representative target for a kind line: MCP → the tool sub-name;
/// local tools → their per-kind extractor, falling back to the title
/// with the kind-label prefix stripped.
fn family_target(tc: &crate::app::ToolCallInfo) -> Option<String> {
    if let Some((_, tool)) = mcp_parts(&tc.sdk_tool_name)
        && !tool.is_empty()
    {
        return Some(tool.to_owned());
    }
    let (glyph, _) = crate::ui::theme::tool_name_label(&tc.sdk_tool_name);
    let bespoke = match glyph {
        "\u{2b1a}" => read_target(tc),
        "\u{2315}" => search_target(tc),
        "\u{25b6}" => command_target(tc),
        "\u{2295}" => web_target(tc),
        "\u{2316}" => query_target(tc),
        _ => None,
    };
    bespoke.or_else(|| strip_title_prefix(tc))
}

fn push_target(targets: &mut Vec<String>, candidate: Option<String>) {
    if let Some(value) = candidate.filter(|s| !s.is_empty()) {
        targets.push(value);
    }
}

/// Read target: the full `file_path` (absolute as Claude sends it).
/// The render relativizes it against the session project root and shows
/// each file as a nested child, so the full path is kept here - not the
/// basename.
fn read_target(tc: &crate::app::ToolCallInfo) -> Option<String> {
    let raw = tc.raw_input.as_ref().and_then(|v| v.as_object());
    let path =
        raw.and_then(|r| r.get("file_path")).and_then(serde_json::Value::as_str).map(str::trim);
    if let Some(p) = path.filter(|s| !s.is_empty()) {
        return Some(p.to_owned());
    }
    // raw_input absent (a defensive code path the renderer can hit
    // pre-result; the test fixtures also pass raw_input: None).
    // Recover by trimming the leading kind-label from `tc.title` -
    // for Read it's `"Read /path/to/file"`, for Edit `"Edit ..."`.
    let title = tc.title.trim();
    let stripped = title.strip_prefix("Read ").or_else(|| title.strip_prefix("Edit "));
    stripped.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn search_target(tc: &crate::app::ToolCallInfo) -> Option<String> {
    let raw = tc.raw_input.as_ref().and_then(|v| v.as_object())?;
    let value = match tc.sdk_tool_name.as_str() {
        "Grep" | "Glob" => raw.get("pattern"),
        "LS" => raw.get("path"),
        _ => None,
    };
    value
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Web family (`⊕`): WebFetch shows its URL (scheme stripped),
/// WebSearch its query. The full value reaches the render, which clips
/// it per row.
fn web_target(tc: &crate::app::ToolCallInfo) -> Option<String> {
    let raw = tc.raw_input.as_ref().and_then(|v| v.as_object())?;
    let value = match tc.sdk_tool_name.as_str() {
        "WebFetch" | "web_fetch" => raw.get("url"),
        "WebSearch" | "web_search" => raw.get("query"),
        _ => None,
    };
    value
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| strip_scheme(s).to_owned())
}

/// ToolSearch (`⌖`): the search query. The full query reaches the
/// render, which clips it per row.
fn query_target(tc: &crate::app::ToolCallInfo) -> Option<String> {
    let raw = tc.raw_input.as_ref().and_then(|v| v.as_object())?;
    raw.get("query")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn strip_scheme(url: &str) -> &str {
    url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url)
}

/// Fallback target for a kind with no bespoke extractor: `tc.title`
/// with a leading kind-label prefix stripped (claude sends titles like
/// `"Skill code-review"` / `"LSP hover"`).
fn strip_title_prefix(tc: &crate::app::ToolCallInfo) -> Option<String> {
    let (_, label) = crate::ui::theme::tool_name_label(&tc.sdk_tool_name);
    let title = tc.title.trim();
    if title.is_empty() {
        return None;
    }
    let stripped =
        title.strip_prefix(label).and_then(|r| r.strip_prefix(' ')).unwrap_or(title).trim();
    (!stripped.is_empty()).then(|| stripped.to_owned())
}

fn command_target(tc: &crate::app::ToolCallInfo) -> Option<String> {
    let raw = tc.raw_input.as_ref().and_then(|v| v.as_object());
    // Prefer Claude's human-readable description (the collapsed
    // headline) - it rides the same raw_input object as the command,
    // and the raw command often starts with a long `cd <path>` that
    // reads as nothing useful. The full description reaches the render,
    // which clips it per row; falls back to the command when no
    // description was sent.
    let description = raw
        .and_then(|r| r.get("description"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Some(desc) = description {
        return Some(desc);
    }
    let command = raw
        .and_then(|r| r.get("command"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Some(cmd) = command {
        return Some(cmd);
    }
    // Defensive: when raw_input is missing, `tool_title("Bash", ...)`
    // emits the bare command as `tc.title`. Use it directly (without
    // a kind-label strip since Bash has no prefix).
    let title = tc.title.trim();
    if title.is_empty() { None } else { Some(title.to_owned()) }
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
        summary: KindSummary,
        /// Aggregate status across the run (see `aggregate_run_status`)
        /// so the L2 summary's status_icon stays in sync as tools flip
        /// through the InProgress -> Completed lifecycle.
        aggregate_status: crate::agent::model::ToolCallStatus,
    },
    /// A run of consecutive peer/worker MCP message blocks (outbound
    /// and inbound). Unlike `Group`, a messaging group can span
    /// multiple messages; each segment carries its per-message slice
    /// plus the `segment_continues_above/below` flags for the
    /// continuation marker render. All segments share `group_leader_id`
    /// so a click or cycle on any segment maps to the same collapse
    /// level via the `messaging_group_collapse_levels` map on
    /// `UiSession`.
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

/// Sibling of [`group_leader_at`] for messaging groups. Returns the
/// [`GroupId`] + segment-block-count when `block_idx` is the leading
/// block of a `RenderUnit::MessagingGroup` segment in this message.
/// `None` for non-leader blocks (so per-block click toggle wins).
pub fn messaging_group_leader_at(
    blocks: &[MessageBlock],
    block_idx: usize,
) -> Option<(GroupId, usize)> {
    let units = partition_blocks_into_render_units(blocks);
    units.into_iter().find_map(|unit| match unit {
        RenderUnit::MessagingGroup { segments, group_leader_id } => {
            let segment = segments.first()?;
            if segment.block_range.start == block_idx {
                Some((group_leader_id, segment.block_range.len()))
            } else {
                None
            }
        }
        _ => None,
    })
}

/// True when `block` is a hidden / chat-suppressed tool call.
/// Hidden tools render nothing in the chat stream; the partitioner
/// emits them as `RenderUnit::Individual` (to preserve their block
/// index for hit-test consistency) but never includes them as the
/// leader of a Group, and they neither tally into KindSummary nor
/// contribute to the aggregate run status.
fn is_hidden_tool_call(block: &MessageBlock) -> bool {
    matches!(block, MessageBlock::ToolCall(tc) if tc.hidden)
}

/// Walk `blocks` identifying maximal runs of >= 2 consecutive groupable
/// tool calls. Each qualifying run becomes a `RenderUnit::Group`; every
/// other block (including lone groupable tools between breakers and
/// any hidden tool call) becomes `RenderUnit::Individual`.
pub fn partition_blocks_into_render_units(blocks: &[MessageBlock]) -> Vec<RenderUnit> {
    let tool_call_units = partition_tool_call_groups(blocks);
    merge_messaging_groups(blocks, &tool_call_units)
}

/// True when `block` is a within-message messaging-class block:
/// either an outbound peer/worker tool call OR a Text block carrying
/// a peer envelope wrapper. Hidden tool calls return false (they're
/// neither messaging nor breakers; the partitioner passes them
/// through). The session-walking partitioner classifies more finely;
/// this per-message predicate is for the within-message post-pass.
fn is_messaging_block(block: &MessageBlock) -> bool {
    use crate::ui::peer_block;
    match block {
        MessageBlock::ToolCall(tc) if !tc.hidden => peer_block::detect_outbound(tc).is_some(),
        MessageBlock::Text(text) => peer_block::detect_inbound(&text.text)
            .is_some_and(|k| k.peer_sender_identity().is_some()),
        _ => false,
    }
}

/// Post-process the tool-call partition: replace maximal runs of
/// Individual units pointing to messaging-class blocks with a single
/// `RenderUnit::MessagingGroup` covering one within-message segment.
/// Hidden tool calls between messaging blocks pass through.
fn merge_messaging_groups(blocks: &[MessageBlock], tool_units: &[RenderUnit]) -> Vec<RenderUnit> {
    use crate::ui::peer_block::{self, PeerInboundKind, PeerOutboundKind};
    let mut output: Vec<RenderUnit> = Vec::with_capacity(tool_units.len());
    let mut i = 0;
    while i < tool_units.len() {
        // Only Individual units that index a messaging-class block
        // start a run.
        let is_messaging_start = matches!(
            &tool_units[i],
            RenderUnit::Individual(idx) if is_messaging_block(&blocks[*idx])
        );
        if !is_messaging_start {
            output.push(tool_units[i].clone());
            i += 1;
            continue;
        }
        // Scan forward: collect a run of consecutive Individual units
        // that point to messaging blocks OR hidden tool calls (pass
        // through). Stop at the first unit that doesn't fit.
        let run_start_pos = i;
        let mut run_end_pos = i + 1;
        while run_end_pos < tool_units.len() {
            match &tool_units[run_end_pos] {
                RenderUnit::Individual(idx)
                    if is_messaging_block(&blocks[*idx]) || is_hidden_tool_call(&blocks[*idx]) =>
                {
                    run_end_pos += 1;
                }
                _ => break,
            }
        }
        // Compute block_range from the first and last block-indices in
        // the run.
        let first_block_idx = match &tool_units[run_start_pos] {
            RenderUnit::Individual(idx) => *idx,
            _ => unreachable!("run_start_pos must be Individual"),
        };
        let last_block_idx = match &tool_units[run_end_pos - 1] {
            RenderUnit::Individual(idx) => *idx,
            _ => unreachable!("run_end_pos - 1 must be Individual"),
        };
        let block_range = first_block_idx..(last_block_idx + 1);

        // Walk the block range and accumulate per-direction targets,
        // segment_count, aggregate_status.
        let mut segment_outbound_targets = MessagingDirectionTargets::default();
        let mut segment_inbound_targets = MessagingDirectionTargets::default();
        let mut segment_count = 0_usize;
        let mut any_status: Option<crate::agent::model::ToolCallStatus> = None;
        let mut leader_id: Option<GroupId> = None;
        for (offset, block) in blocks[block_range.clone()].iter().enumerate() {
            let block_idx = first_block_idx + offset;
            match block {
                MessageBlock::ToolCall(tc) if !tc.hidden => {
                    if let Some(kind) = peer_block::detect_outbound(tc) {
                        let target = match kind {
                            PeerOutboundKind::Ask { target, .. }
                            | PeerOutboundKind::Tell { target, .. } => target,
                        };
                        append_target(&mut segment_outbound_targets, &target);
                        segment_count += 1;
                        update_aggregate(&mut any_status, tc.status);
                        if leader_id.is_none() {
                            leader_id = Some(GroupId::from_leader_id(tc.id.clone()));
                        }
                    }
                }
                MessageBlock::Text(text) => {
                    let kind = peer_block::detect_inbound(&text.text);
                    if let Some(from) =
                        kind.as_ref().and_then(PeerInboundKind::peer_sender_identity)
                    {
                        append_target(&mut segment_inbound_targets, from);
                        segment_count += 1;
                        if leader_id.is_none() {
                            leader_id = Some(GroupId::from_leader_id(format!(
                                "msg-block-{block_idx}-inbound"
                            )));
                        }
                    }
                }
                _ => {} // hidden tool call: pass through, doesn't tally
            }
        }
        // Threshold-2: a lone messaging block doesn't form an @
        // group. Push back the original Individual units so the
        // single block renders as the plain peer block via
        // `append_assistant_tool_block`'s peer-block arm. Hidden
        // pass-throughs in the run survive as Individuals too.
        if segment_count < 2 {
            for unit in &tool_units[run_start_pos..run_end_pos] {
                output.push(unit.clone());
            }
            i = run_end_pos;
            continue;
        }
        let leader_id = leader_id
            .unwrap_or_else(|| GroupId::from_leader_id(format!("block-{first_block_idx}")));
        let aggregate_status = any_status.unwrap_or(crate::agent::model::ToolCallStatus::Completed);
        let segment = MessagingGroupSegment {
            msg_idx: 0,
            block_range,
            segment_count,
            segment_outbound_targets,
            segment_inbound_targets,
            segment_continues_above: false,
            segment_continues_below: false,
            aggregate_status,
            group_total_count: segment_count,
        };
        output.push(RenderUnit::MessagingGroup {
            segments: vec![segment],
            group_leader_id: leader_id,
        });
        i = run_end_pos;
    }
    output
}

/// Internal: the original tool-call grouping pass over `blocks`.
/// Identifies maximal runs of >= 1 consecutive groupable tool calls.
/// Each qualifying run becomes a `RenderUnit::Group`; every other
/// block (including any hidden tool call) becomes
/// `RenderUnit::Individual`. The result is post-processed by
/// `merge_messaging_groups` to also emit `RenderUnit::MessagingGroup`
/// for within-message peer/worker runs.
fn partition_tool_call_groups(blocks: &[MessageBlock]) -> Vec<RenderUnit> {
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
        // A run is always >= 1 block. `blocks[run_start]` is a visible
        // groupable ToolCall: the outer loop's hidden-and-breaker checks
        // both returned false for this slot. Defensive fallback (the
        // leading block somehow isn't a ToolCall) emits the run as
        // Individual rows so the renderer can't panic on bad input.
        let MessageBlock::ToolCall(leader_tc) = &blocks[run_start] else {
            for idx in run_start..run_end_exclusive {
                units.push(RenderUnit::Individual(idx));
            }
            i = run_end_exclusive;
            continue;
        };
        let leader_id = GroupId::from_leader_id(leader_tc.id.clone());
        let mut summary = KindSummary::default();
        for block in &blocks[run_start..run_end_exclusive] {
            if let MessageBlock::ToolCall(tc) = block
                && !tc.hidden
            {
                summary.tally(tc);
            }
        }
        let aggregate_status = aggregate_run_status(&blocks[run_start..run_end_exclusive]);
        units.push(RenderUnit::Group {
            range: run_start..run_end_exclusive,
            leader_id,
            summary,
            aggregate_status,
        });
        i = run_end_exclusive;
    }
    units
}

/// Classification of a single block for session-walking partition.
/// `Outbound` and `Inbound` extend a messaging run; the pass-through
/// kinds extend a run without contributing to it; `Breaker` ends a
/// run. See [`partition_session_into_render_units`].
#[derive(Debug, Clone)]
enum SessionBlockClass {
    /// Peer/worker outbound tool call. Carries the target's display
    /// name and the block's tool-use id.
    Outbound { target: String, tool_use_id: String },
    /// User-turn text that carries a peer envelope wrapper. Carries
    /// the sender's display name.
    Inbound { from: String },
    /// Hidden tool call. Renders nothing visible; passes through
    /// without contributing.
    HiddenPassThrough,
    /// Plain text inside a User-role message (without a peer envelope
    /// wrapper). Treated as a turn-separator: passes through a
    /// messaging run rather than breaking it.
    UserTurnPassThrough,
    /// Any other visible block. Ends an in-flight run.
    Breaker,
}

/// Classify one block in the context of its enclosing message's role.
fn classify_session_block(
    role: &crate::app::MessageRole,
    block: &MessageBlock,
) -> SessionBlockClass {
    use crate::app::MessageRole;
    use crate::ui::peer_block::{self, PeerInboundKind, PeerOutboundKind};
    match block {
        MessageBlock::ToolCall(tc) => {
            if tc.hidden {
                return SessionBlockClass::HiddenPassThrough;
            }
            if let Some(kind) = peer_block::detect_outbound(tc) {
                let target = match kind {
                    PeerOutboundKind::Ask { target, .. }
                    | PeerOutboundKind::Tell { target, .. } => target,
                };
                return SessionBlockClass::Outbound { target, tool_use_id: tc.id.clone() };
            }
            SessionBlockClass::Breaker
        }
        MessageBlock::Text(text) => {
            // Gotify notifications parse as an inbound envelope but are
            // external events, not agent traffic (`peer_sender_identity`
            // returns None), so they fall through to the user-turn class
            // and never merge into a peer group.
            if let Some(from) = peer_block::detect_inbound(&text.text)
                .as_ref()
                .and_then(PeerInboundKind::peer_sender_identity)
            {
                return SessionBlockClass::Inbound { from: from.to_owned() };
            }
            if matches!(role, MessageRole::User) {
                SessionBlockClass::UserTurnPassThrough
            } else {
                SessionBlockClass::Breaker
            }
        }
        _ => SessionBlockClass::Breaker,
    }
}

/// Append a target name to the rolling first-N + overflow tally. The
/// first `TARGETS_NAMED_LIMIT` distinct targets render by name; every
/// additional distinct target counts into `overflow_n`. Repeat hits
/// of an already-named target are no-ops.
fn append_target(targets: &mut MessagingDirectionTargets, name: &str) {
    const TARGETS_NAMED_LIMIT: usize = 2;
    if targets.targets.iter().any(|t| t == name) {
        return;
    }
    if targets.targets.len() < TARGETS_NAMED_LIMIT {
        targets.targets.push(name.to_owned());
    } else {
        targets.overflow_n += 1;
    }
}

/// Walks a complete session's messages and produces per-message
/// render-unit lists, threading peer/worker run-state across message
/// boundaries so a messaging-group can span multiple turns. Non-peer
/// blocks are partitioned per-message via the existing
/// [`partition_blocks_into_render_units`] logic for tool-call
/// grouping.
///
/// Returns a `Vec<Vec<RenderUnit>>` parallel to `messages`: the inner
/// vec is what the per-message renderer dispatches over.
///
/// Implementation: two-pass walk. Pass 1 (this function's first half)
/// classifies every block, identifies maximal messaging runs across
/// messages, and emits draft segments. Pass 2 (second half) stamps
/// continuation flags and group_total_count onto each segment.
/// Finally per-message render-unit lists are assembled by merging
/// segment units back into the position the messaging blocks held in
/// each message and partitioning the gaps via the per-message
/// partitioner.
/// Draft segment captured during the session walk's first pass. The
/// second pass groups drafts by `leader_id` to stamp continuation
/// flags + `group_total_count` onto the final
/// [`MessagingGroupSegment`].
struct SessionDraftSegment {
    msg_idx: usize,
    block_range: Range<usize>,
    segment_count: usize,
    segment_outbound_targets: MessagingDirectionTargets,
    segment_inbound_targets: MessagingDirectionTargets,
    aggregate_status: crate::agent::model::ToolCallStatus,
    leader_id: GroupId,
}

pub fn partition_session_into_render_units(
    messages: &[crate::app::ChatMessage],
) -> Vec<Vec<RenderUnit>> {
    let classifications: Vec<Vec<SessionBlockClass>> = messages
        .iter()
        .map(|msg| msg.blocks.iter().map(|b| classify_session_block(&msg.role, b)).collect())
        .collect();

    // Per-message: ranges of indices that belong to a messaging run,
    // plus the run identifier each range belongs to. Same run id is
    // shared across messages when the run spans turn boundaries.
    let mut messaging_drafts: Vec<SessionDraftSegment> = Vec::new();

    let mut active_run_leader: Option<GroupId> = None;

    for (msg_idx, msg) in messages.iter().enumerate() {
        let classes = &classifications[msg_idx];
        let mut i = 0;
        while i < classes.len() {
            // Skip leading non-messaging breakers - they end the run.
            match &classes[i] {
                SessionBlockClass::Breaker => {
                    active_run_leader = None;
                    i += 1;
                    continue;
                }
                SessionBlockClass::HiddenPassThrough | SessionBlockClass::UserTurnPassThrough => {
                    // Pass-through without an active run: just skip.
                    i += 1;
                    continue;
                }
                _ => {}
            }

            // Found a messaging-class block. Open or extend a run.
            let segment_start = i;
            let mut segment_end = i;
            let mut outbound_targets = MessagingDirectionTargets::default();
            let mut inbound_targets = MessagingDirectionTargets::default();
            let mut segment_count = 0_usize;
            let mut segment_leader: Option<GroupId> = None;
            let mut any_status: Option<crate::agent::model::ToolCallStatus> = None;

            // Scan within this message until we hit a Breaker, ALWAYS
            // extending across HiddenPassThrough + UserTurnPassThrough.
            while segment_end < classes.len() {
                match &classes[segment_end] {
                    SessionBlockClass::Outbound { target, tool_use_id } => {
                        if segment_leader.is_none() && active_run_leader.is_none() {
                            segment_leader = Some(GroupId::from_leader_id(tool_use_id.clone()));
                        }
                        append_target(&mut outbound_targets, target);
                        segment_count += 1;
                        if let MessageBlock::ToolCall(tc) = &msg.blocks[segment_end] {
                            update_aggregate(&mut any_status, tc.status);
                        }
                    }
                    SessionBlockClass::Inbound { from } => {
                        if segment_leader.is_none() && active_run_leader.is_none() {
                            // Inbound blocks don't carry a tool-use id;
                            // use a stable synthetic leader keyed on
                            // (msg_idx, block_idx).
                            segment_leader = Some(GroupId::from_leader_id(format!(
                                "msg-{msg_idx}-block-{segment_end}-inbound"
                            )));
                        }
                        append_target(&mut inbound_targets, from);
                        segment_count += 1;
                    }
                    SessionBlockClass::HiddenPassThrough
                    | SessionBlockClass::UserTurnPassThrough => {
                        // Pass through without contributing.
                    }
                    SessionBlockClass::Breaker => break,
                }
                segment_end += 1;
            }

            if segment_count == 0 {
                // The scan only saw pass-through blocks - no real
                // messaging contribution. Treat as gap; advance.
                i = segment_end.max(i + 1);
                continue;
            }

            let leader_id = active_run_leader
                .clone()
                .or(segment_leader)
                .unwrap_or_else(|| GroupId::from_leader_id(format!("msg-{msg_idx}-fallback")));

            if active_run_leader.is_none() {
                active_run_leader = Some(leader_id.clone());
            }

            messaging_drafts.push(SessionDraftSegment {
                msg_idx,
                block_range: segment_start..segment_end,
                segment_count,
                segment_outbound_targets: outbound_targets,
                segment_inbound_targets: inbound_targets,
                aggregate_status: any_status
                    .unwrap_or(crate::agent::model::ToolCallStatus::Completed),
                leader_id,
            });

            // If a Breaker terminated this segment, the run ends here.
            if segment_end < classes.len()
                && matches!(classes[segment_end], SessionBlockClass::Breaker)
            {
                active_run_leader = None;
                i = segment_end + 1;
                continue;
            }
            // Otherwise the segment ran out at end-of-message; the run
            // stays open into the next message.
            i = segment_end;
        }
    }

    // Pass 2: group segments by leader_id, stamp continuation +
    // totals.
    let mut by_leader: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (idx, draft) in messaging_drafts.iter().enumerate() {
        by_leader.entry(draft.leader_id.as_str().to_owned()).or_default().push(idx);
    }

    let mut segment_by_draft_idx: Vec<MessagingGroupSegment> =
        Vec::with_capacity(messaging_drafts.len());
    for draft in &messaging_drafts {
        // Placeholder; overwritten in the per-leader stamp loop below.
        segment_by_draft_idx.push(MessagingGroupSegment {
            msg_idx: draft.msg_idx,
            block_range: draft.block_range.clone(),
            segment_count: draft.segment_count,
            segment_outbound_targets: draft.segment_outbound_targets.clone(),
            segment_inbound_targets: draft.segment_inbound_targets.clone(),
            segment_continues_above: false,
            segment_continues_below: false,
            aggregate_status: draft.aggregate_status,
            group_total_count: draft.segment_count,
        });
    }
    for indices in by_leader.values() {
        let total: usize = indices.iter().map(|&i| messaging_drafts[i].segment_count).sum();
        let last_pos = indices.len().saturating_sub(1);
        for (pos, &idx) in indices.iter().enumerate() {
            let segment = &mut segment_by_draft_idx[idx];
            segment.segment_continues_above = pos > 0;
            segment.segment_continues_below = pos < last_pos;
            segment.group_total_count = total;
        }
    }

    // Now assemble per-message render-unit lists. For each message:
    // - Walk the message's classifications.
    // - Wherever a messaging segment lives (per draft), emit a single
    //   MessagingGroup unit covering the segment's block_range.
    // - For runs of non-messaging blocks between/around segments,
    //   defer to per-message tool-call partition via
    //   `partition_blocks_into_render_units` restricted to the gap.
    let mut output: Vec<Vec<RenderUnit>> = Vec::with_capacity(messages.len());
    for (msg_idx, msg) in messages.iter().enumerate() {
        let segments_in_msg: Vec<MessagingGroupSegment> = messaging_drafts
            .iter()
            .enumerate()
            .filter_map(|(idx, draft)| {
                if draft.msg_idx == msg_idx {
                    Some(segment_by_draft_idx[idx].clone())
                } else {
                    None
                }
            })
            .collect();

        if segments_in_msg.is_empty() {
            // No messaging activity in this message; reuse the
            // existing per-message partitioner directly.
            output.push(partition_blocks_into_render_units(&msg.blocks));
            continue;
        }

        // Assemble interleaved units honouring original block order.
        let mut msg_units: Vec<RenderUnit> = Vec::new();
        let mut cursor = 0_usize;
        for segment in segments_in_msg {
            if segment.block_range.start > cursor {
                // Gap before this segment - partition it via the
                // per-message tool-call partitioner.
                let gap = &msg.blocks[cursor..segment.block_range.start];
                for unit in partition_blocks_into_render_units(gap) {
                    msg_units.push(shift_unit(unit, cursor));
                }
            }
            if segment.group_total_count < 2 {
                // Threshold-2: a lone messaging block (the entire
                // cross-turn run is size 1) renders as the plain
                // peer block. Emit Individual per block index in the
                // segment range; hidden pass-throughs in the span
                // become their own Individuals too.
                for idx in segment.block_range.clone() {
                    msg_units.push(RenderUnit::Individual(idx));
                }
                cursor = segment.block_range.end;
                continue;
            }
            let leader_id = find_segment_leader(&messaging_drafts, &segment);
            msg_units.push(RenderUnit::MessagingGroup {
                segments: vec![segment.clone()],
                group_leader_id: leader_id,
            });
            cursor = segment.block_range.end;
        }
        if cursor < msg.blocks.len() {
            let tail = &msg.blocks[cursor..];
            for unit in partition_blocks_into_render_units(tail) {
                msg_units.push(shift_unit(unit, cursor));
            }
        }
        output.push(msg_units);
    }
    output
}

/// Update an in-progress aggregate status with one tool-call's status.
/// Mirrors [`aggregate_run_status`]'s priority (InProgress > Failed >
/// Pending > Completed).
fn update_aggregate(
    aggregate: &mut Option<crate::agent::model::ToolCallStatus>,
    status: crate::agent::model::ToolCallStatus,
) {
    use crate::agent::model::ToolCallStatus;
    match (aggregate, status) {
        (slot @ None, s) => *slot = Some(s),
        (Some(ToolCallStatus::InProgress), _) => {}
        (slot, ToolCallStatus::InProgress) => *slot = Some(ToolCallStatus::InProgress),
        (Some(ToolCallStatus::Failed | ToolCallStatus::Killed), _) => {}
        (slot, ToolCallStatus::Failed | ToolCallStatus::Killed) => {
            *slot = Some(ToolCallStatus::Failed);
        }
        (Some(ToolCallStatus::Pending), _) => {}
        (slot, ToolCallStatus::Pending) => *slot = Some(ToolCallStatus::Pending),
        _ => {}
    }
}

/// Look up the leader id for a segment by scanning the draft list
/// for the matching `(msg_idx, block_range.start)` pair. The leader
/// is shared across cross-message segments of the same run.
fn find_segment_leader(drafts: &[SessionDraftSegment], segment: &MessagingGroupSegment) -> GroupId {
    for draft in drafts {
        if draft.msg_idx == segment.msg_idx && draft.block_range.start == segment.block_range.start
        {
            return draft.leader_id.clone();
        }
    }
    GroupId::from_leader_id(format!("msg-{}-fallback", segment.msg_idx))
}

/// Shift a [`RenderUnit`]'s block indices by `offset` so a partition
/// computed on a slice can be re-anchored to its position in the
/// full block list.
fn shift_unit(unit: RenderUnit, offset: usize) -> RenderUnit {
    match unit {
        RenderUnit::Individual(i) => RenderUnit::Individual(i + offset),
        RenderUnit::Group { range, leader_id, summary, aggregate_status } => RenderUnit::Group {
            range: (range.start + offset)..(range.end + offset),
            leader_id,
            summary,
            aggregate_status,
        },
        RenderUnit::MessagingGroup { segments, group_leader_id } => RenderUnit::MessagingGroup {
            segments: segments
                .into_iter()
                .map(|s| MessagingGroupSegment {
                    block_range: (s.block_range.start + offset)..(s.block_range.end + offset),
                    ..s
                })
                .collect(),
            group_leader_id,
        },
    }
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
            answered_questions: Vec::new(),
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
        // AskUserQuestion is hidden (pass-through) while unanswered;
        // once answered it un-hides and renders the answered-card, so a
        // visible (non-hidden) one MUST break.
        assert!(
            is_run_breaker(&tool_call_block("x", "AskUserQuestion")),
            "answered AskUserQuestion renders the answered-card and MUST break runs",
        );
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

    fn kind_line<'a>(k: &'a KindSummary, label: &str) -> Option<&'a KindLine> {
        k.lines.iter().find(|l| l.label == label)
    }

    fn count_of(k: &KindSummary, label: &str) -> usize {
        kind_line(k, label).map_or(0, |l| l.count)
    }

    #[test]
    fn tally_orders_kinds_by_first_appearance() {
        let mut k = KindSummary::default();
        tally_block(&mut k, &tool_call_block("a", "Bash"));
        tally_block(&mut k, &tool_call_block("b", "Read"));
        tally_block(&mut k, &tool_call_block("c", "Read"));
        let labels: Vec<&str> = k.lines.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, vec!["bash", "read"], "kinds keep first-appearance order");
        assert_eq!(count_of(&k, "read"), 2);
        assert_eq!(k.total(), 3);
    }

    fn tool_call_block_with_input(
        id: &str,
        sdk_tool_name: &str,
        title: &str,
        raw_input: Option<serde_json::Value>,
    ) -> MessageBlock {
        let mut block = tool_call_block(id, sdk_tool_name);
        if let MessageBlock::ToolCall(tc) = &mut block {
            tc.title = title.to_owned();
            tc.raw_input = raw_input;
        }
        block
    }

    fn tally_block(k: &mut KindSummary, block: &MessageBlock) {
        if let MessageBlock::ToolCall(tc) = block {
            k.tally(tc);
        }
    }

    /// Glyph-family grouping: Grep/Glob/LS collapse to one `search`
    /// line, WebFetch/WebSearch to one `web`, LSP to `lsp`, and each
    /// `mcp__<server>__*` to a per-server line - no opaque `calls`
    /// grab-bag. LS moving out of the old catch-all is the headline fix.
    #[test]
    fn tally_groups_by_glyph_family_and_mcp_by_server() {
        let mut k = KindSummary::default();
        for (id, name) in [
            ("a", "Read"),
            ("b", "Grep"),
            ("c", "Glob"),
            ("d", "LS"),
            ("e", "WebSearch"),
            ("f", "WebFetch"),
            ("g", "Bash"),
            ("h", "LSP"),
            ("i", "mcp__context7__query-docs"),
            ("j", "mcp__context7__resolve-library-id"),
            ("k", "mcp__playwright__browser_click"),
        ] {
            tally_block(&mut k, &tool_call_block(id, name));
        }
        assert_eq!(count_of(&k, "read"), 1);
        assert_eq!(count_of(&k, "search"), 3, "Grep + Glob + LS");
        assert_eq!(count_of(&k, "web"), 2, "WebFetch + WebSearch");
        assert_eq!(count_of(&k, "bash"), 1);
        assert_eq!(count_of(&k, "lsp"), 1);
        assert_eq!(count_of(&k, "context7"), 2, "same server merges");
        assert_eq!(count_of(&k, "playwright"), 1);
        assert!(kind_line(&k, "calls").is_none(), "no generic calls grab-bag");
        assert_eq!(kind_line(&k, "context7").unwrap().glyph, MCP_GLYPH);
    }

    #[test]
    fn tally_mcp_target_is_the_tool_subname() {
        let mut k = KindSummary::default();
        tally_block(&mut k, &tool_call_block("a", "mcp__context7__query-docs"));
        let line = kind_line(&k, "context7").expect("context7 line");
        assert_eq!(line.targets, vec!["query-docs".to_owned()]);
    }

    /// Each kind line collects representative targets: Reads pull the
    /// full file_path (relativized + nested at render), Bash its
    /// `command`, Grep/Glob the `pattern`, WebSearch its `query` (now on
    /// the `web` line). Targets render in order of appearance.
    #[test]
    fn tally_collects_representative_targets() {
        let mut k = KindSummary::default();
        tally_block(
            &mut k,
            &tool_call_block_with_input(
                "r1",
                "Read",
                "Read /repo/src/foo.rs",
                Some(serde_json::json!({"file_path": "/repo/src/foo.rs"})),
            ),
        );
        tally_block(
            &mut k,
            &tool_call_block_with_input(
                "r2",
                "Read",
                "Read /repo/src/bar.rs",
                Some(serde_json::json!({"file_path": "/repo/src/bar.rs"})),
            ),
        );
        tally_block(
            &mut k,
            &tool_call_block_with_input(
                "b1",
                "Bash",
                "cargo check",
                Some(serde_json::json!({"command": "cargo check"})),
            ),
        );
        tally_block(
            &mut k,
            &tool_call_block_with_input(
                "g1",
                "Grep",
                "Grep",
                Some(serde_json::json!({"pattern": "FooBar"})),
            ),
        );
        tally_block(
            &mut k,
            &tool_call_block_with_input(
                "ws1",
                "WebSearch",
                "WebSearch rust async",
                Some(serde_json::json!({"query": "rust async"})),
            ),
        );

        assert_eq!(
            kind_line(&k, "read").unwrap().targets,
            vec!["/repo/src/foo.rs", "/repo/src/bar.rs"]
        );
        assert_eq!(kind_line(&k, "bash").unwrap().targets, vec!["cargo check"]);
        assert_eq!(kind_line(&k, "search").unwrap().targets, vec!["FooBar"]);
        assert_eq!(kind_line(&k, "web").unwrap().targets, vec!["rust async"]);
    }

    /// A Bash call with a `description` uses the readable headline
    /// directly (the raw command often starts with a long `cd <path>`
    /// that clips to nothing useful).
    #[test]
    fn bash_line_uses_description() {
        let mut k = KindSummary::default();
        tally_block(
            &mut k,
            &tool_call_block_with_input(
                "b1",
                "Bash",
                "cd /Users/x/Projects/forge && gh pr merge 358 --squash",
                Some(serde_json::json!({
                    "command": "cd /Users/x/Projects/forge && gh pr merge 358 --squash",
                    "description": "Squash-merge PR and confirm on main"
                })),
            ),
        );
        assert_eq!(
            kind_line(&k, "bash").unwrap().targets,
            vec!["Squash-merge PR and confirm on main".to_owned()]
        );
    }

    /// No description -> falls back to the full raw command (kept in
    /// full; the render clips per row).
    #[test]
    fn bash_line_falls_back_to_command_without_description() {
        let mut k = KindSummary::default();
        tally_block(
            &mut k,
            &tool_call_block_with_input(
                "b1",
                "Bash",
                "cargo nextest run -p forge-tui",
                Some(serde_json::json!({"command": "cargo nextest run -p forge-tui"})),
            ),
        );
        assert_eq!(
            kind_line(&k, "bash").unwrap().targets,
            vec!["cargo nextest run -p forge-tui".to_owned()]
        );
    }

    /// A long description is kept in FULL by the tally - the render
    /// clips it per row rather than the tally truncating it.
    #[test]
    fn bash_line_keeps_full_description() {
        let long = "Regenerate every baseline fixture and re-run the conformance suite twice";
        let mut k = KindSummary::default();
        tally_block(
            &mut k,
            &tool_call_block_with_input(
                "b1",
                "Bash",
                "x",
                Some(serde_json::json!({"command": "x", "description": long})),
            ),
        );
        assert_eq!(kind_line(&k, "bash").unwrap().targets, vec![long.to_owned()]);
    }

    /// Every kind keeps EVERY target now (uncapped, like read) so the
    /// render can nest one child row per instance. Five Greps keep all
    /// five patterns - no cap, no overflow.
    #[test]
    fn tally_keeps_every_target_uncapped() {
        let mut k = KindSummary::default();
        for i in 0..5 {
            tally_block(
                &mut k,
                &tool_call_block_with_input(
                    &format!("g{i}"),
                    "Grep",
                    "Grep",
                    Some(serde_json::json!({ "pattern": format!("pat{i}") })),
                ),
            );
        }
        let search = kind_line(&k, "search").unwrap();
        assert_eq!(search.count, 5);
        assert_eq!(search.targets.len(), 5, "every kind keeps every target, uncapped");
    }

    /// Read keeps EVERY file (uncapped, like every kind now) so the
    /// render can show one nested child per file.
    #[test]
    fn tally_keeps_every_read_file() {
        let mut k = KindSummary::default();
        for (i, path) in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"].iter().enumerate() {
            tally_block(
                &mut k,
                &tool_call_block_with_input(
                    &format!("r{i}"),
                    "Read",
                    "Read",
                    Some(serde_json::json!({ "file_path": path })),
                ),
            );
        }
        let read = kind_line(&k, "read").unwrap();
        assert_eq!(read.count, 5);
        assert_eq!(read.targets.len(), 5, "read keeps every file, uncapped");
    }

    /// `read_target` guards an empty / whitespace `file_path`: the
    /// primary path yields None (so the render never nests a blank child
    /// row), while a real path resolves to the full string.
    #[test]
    fn read_target_guards_empty_file_path() {
        let read_target_of = |file_path: &str| {
            let block = tool_call_block_with_input(
                "r",
                "Read",
                "Read",
                Some(serde_json::json!({ "file_path": file_path })),
            );
            let MessageBlock::ToolCall(tc) = &block else { unreachable!() };
            read_target(tc)
        };
        assert_eq!(read_target_of(""), None, "empty path -> no target");
        assert_eq!(read_target_of("   "), None, "whitespace path -> no target");
        assert_eq!(
            read_target_of("/repo/src/main.rs"),
            Some("/repo/src/main.rs".to_owned()),
            "a real path resolves in full",
        );
    }

    #[test]
    fn partition_mixed_kind_run_tallies_per_family() {
        let blocks =
            make(&[("tool", "Read"), ("tool", "WebSearch"), ("tool", "WebFetch"), ("tool", "LSP")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 1);
        match &units[0] {
            RenderUnit::Group { summary, .. } => {
                assert_eq!(count_of(summary, "read"), 1);
                assert_eq!(count_of(summary, "web"), 2); // WebSearch + WebFetch
                assert_eq!(count_of(summary, "lsp"), 1);
                assert!(kind_line(summary, "calls").is_none(), "no generic calls grab-bag");
            }
            RenderUnit::Individual(_) => panic!("expected Group"),
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
        }
    }

    #[test]
    fn partition_lone_groupable_tool_forms_single_item_group() {
        let blocks = make(&[("tool", "Read")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 1);
        match &units[0] {
            RenderUnit::Group { range, summary, leader_id, .. } => {
                assert_eq!(*range, 0..1);
                assert_eq!(count_of(summary, "read"), 1);
                assert_eq!(leader_id.as_str(), "tu-0");
            }
            RenderUnit::Individual(_) => panic!("expected Group, got Individual"),
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
        }
    }

    #[test]
    fn partition_two_consecutive_groupable_tools_form_a_group() {
        let blocks = make(&[("tool", "Read"), ("tool", "Read")]);
        let units = partition_blocks_into_render_units(&blocks);
        assert_eq!(units.len(), 1);
        match &units[0] {
            RenderUnit::Group { range, summary, leader_id, .. } => {
                assert_eq!(*range, 0..2);
                assert_eq!(count_of(summary, "read"), 2);
                assert_eq!(summary.total(), 2);
                assert_eq!(leader_id.as_str(), "tu-0");
            }
            RenderUnit::Individual(_) => panic!("expected Group"),
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
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
            RenderUnit::Group { summary, range, .. } => {
                assert_eq!(count_of(summary, "read"), 3);
                assert_eq!(*range, 0..3);
            }
            RenderUnit::Individual(_) => panic!("expected first Group"),
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
        }
        assert!(matches!(units[1], RenderUnit::Individual(3)));
        match &units[2] {
            RenderUnit::Group { summary, range, .. } => {
                assert_eq!(count_of(summary, "bash"), 2);
                assert_eq!(*range, 4..6);
            }
            RenderUnit::Individual(_) => panic!("expected second Group"),
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
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
            RenderUnit::Group { range, summary, .. } => {
                assert_eq!(*range, 0..1);
                assert_eq!(count_of(summary, "read"), 1);
            }
            RenderUnit::Individual(_) => panic!("expected first Group"),
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
        }
        assert!(matches!(units[1], RenderUnit::Individual(1)));
        match &units[2] {
            RenderUnit::Group { range, summary, .. } => {
                assert_eq!(*range, 2..3);
                assert_eq!(count_of(summary, "read"), 1);
            }
            RenderUnit::Individual(_) => panic!("expected third Group"),
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
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
            RenderUnit::Group { summary, .. } => {
                assert_eq!(count_of(summary, "read"), 1);
                assert_eq!(count_of(summary, "search"), 2);
                assert_eq!(count_of(summary, "bash"), 2);
                assert_eq!(summary.total(), 5);
            }
            RenderUnit::Individual(_) => panic!("expected Group"),
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
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
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
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
            RenderUnit::MessagingGroup { .. } => {
                unreachable!("tool-call-only test input does not produce MessagingGroup")
            }
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
            if let RenderUnit::Group { summary, .. } = u {
                assert!(summary.total() > 0, "no empty group should be produced");
            }
        }
    }

    // ─── partition_session_into_render_units (MG T2) ────────────

    fn outbound_peer_block(target: &str, kind: &str) -> MessageBlock {
        // `kind` is "Tell" or "Ask"; map to the matching MCP tool name
        // + raw_input shape that `peer_block::detect_outbound` keys on.
        let (sdk_tool_name, body_key) = match kind {
            "Tell" => ("mcp__forge__peers__tell_agent", "message"),
            "Ask" => ("mcp__forge__peers__ask_agent", "prompt"),
            other => panic!("unknown outbound kind {other:?}; use Tell|Ask"),
        };
        let mut block = tool_call_block("tu-out", sdk_tool_name);
        if let MessageBlock::ToolCall(tc) = &mut block {
            tc.raw_input = Some(serde_json::json!({
                "target": target,
                body_key: "body",
            }));
        }
        block
    }

    fn inbound_peer_block(from: &str, kind: &str) -> MessageBlock {
        // `kind` is "Question" | "Message" | "Reply". Use the
        // wrapper-prose shape `peer_block::detect_inbound` matches.
        let id = "t-test1234";
        let header = match kind {
            "Question" => format!("[Question id={id} hop=1/10 from agent '{from}' (org 'forge')]"),
            "Message" => format!("[Message id={id} hop=1/10 from agent '{from}' (org 'forge')]"),
            "Reply" => {
                format!("[Reply id={id} from agent '{from}' (org 'forge')]")
            }
            other => panic!("unknown inbound kind {other:?}"),
        };
        let text = format!("{header}\n\nbody");
        MessageBlock::Text(TextBlock::from_complete(&text))
    }

    fn assistant_message_with_blocks(blocks: Vec<MessageBlock>) -> crate::app::ChatMessage {
        crate::app::ChatMessage::new(crate::app::MessageRole::Assistant, blocks, None)
    }

    fn user_text_message(text: &str) -> crate::app::ChatMessage {
        crate::app::ChatMessage::new(
            crate::app::MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete(text))],
            None,
        )
    }

    /// CRITICAL invariant: a peer/worker run extending from the end
    /// of message N into the start of message N+1 produces ONE
    /// MessagingGroup with two segments. `group_total_count` is the
    /// sum of `segment_count`s; continuation markers stamped.
    #[test]
    fn messaging_group_merges_across_turn_boundary() {
        let messages = vec![
            assistant_message_with_blocks(vec![
                outbound_peer_block("planner", "Tell"),
                outbound_peer_block("debugger", "Ask"),
                inbound_peer_block("tester", "Reply"),
            ]),
            user_text_message("any update?"),
            assistant_message_with_blocks(vec![
                outbound_peer_block("debugger", "Tell"),
                inbound_peer_block("reviewer", "Message"),
            ]),
        ];

        let per_message_units = partition_session_into_render_units(&messages);
        let messaging_groups: Vec<&RenderUnit> = per_message_units
            .iter()
            .flatten()
            .filter(|u| matches!(u, RenderUnit::MessagingGroup { .. }))
            .collect();

        assert_eq!(
            messaging_groups.len(),
            2,
            "cross-message peer/worker run produces ONE logical group with TWO per-message render units (one per segment); got {}",
            messaging_groups.len(),
        );

        // Both render units must share the same group_leader_id so a
        // click on either resolves to the same collapse state.
        let leaders: Vec<&GroupId> = messaging_groups
            .iter()
            .map(|u| match u {
                RenderUnit::MessagingGroup { group_leader_id, .. } => group_leader_id,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            leaders[0], leaders[1],
            "segments share a single leader_id across the turn boundary"
        );

        // Validate per-segment shapes.
        let segments: Vec<&MessagingGroupSegment> = messaging_groups
            .iter()
            .flat_map(|u| match u {
                RenderUnit::MessagingGroup { segments, .. } => segments.iter(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(segments.len(), 2);

        assert_eq!(segments[0].msg_idx, 0);
        assert_eq!(segments[0].segment_count, 3);
        assert!(segments[0].segment_continues_below);
        assert!(!segments[0].segment_continues_above);
        assert_eq!(segments[0].group_total_count, 5);

        assert_eq!(segments[1].msg_idx, 2);
        assert_eq!(segments[1].segment_count, 2);
        assert!(!segments[1].segment_continues_below);
        assert!(segments[1].segment_continues_above);
        assert_eq!(segments[1].group_total_count, 5);
    }

    /// Threshold-1: a single peer block folds into a messaging group.
    /// A lone outbound peer/worker block does NOT form an @ group;
    /// it renders as an `Individual` (plain peer block). The group
    /// minimum is 2.
    #[test]
    fn messaging_group_single_outbound_session_walk_renders_individual() {
        let messages =
            vec![assistant_message_with_blocks(vec![outbound_peer_block("planner", "Tell")])];
        let per_message_units = partition_session_into_render_units(&messages);
        let groups: Vec<&RenderUnit> = per_message_units
            .iter()
            .flatten()
            .filter(|u| matches!(u, RenderUnit::MessagingGroup { .. }))
            .collect();
        assert!(
            groups.is_empty(),
            "threshold-2: a lone outbound peer block must not form an @ group",
        );
        assert!(
            per_message_units[0].iter().any(|u| matches!(u, RenderUnit::Individual(0))),
            "lone outbound renders as Individual(0); got {:?}",
            per_message_units[0],
        );
    }

    /// Same threshold-2 invariant for inbound: a lone inbound peer
    /// text block renders as `Individual`, not a MessagingGroup.
    #[test]
    fn messaging_group_single_inbound_session_walk_renders_individual() {
        let messages = vec![crate::app::ChatMessage::new(
            crate::app::MessageRole::User,
            vec![inbound_peer_block("tester", "Message")],
            None,
        )];
        let per_message_units = partition_session_into_render_units(&messages);
        let groups: Vec<&RenderUnit> = per_message_units
            .iter()
            .flatten()
            .filter(|u| matches!(u, RenderUnit::MessagingGroup { .. }))
            .collect();
        assert!(
            groups.is_empty(),
            "threshold-2: a lone inbound peer block must not form an @ group",
        );
    }

    /// Same threshold-2 invariant on the per-message partition path
    /// (`merge_messaging_groups`): a within-message run of length 1
    /// stays Individual; runs of length 2+ still fold.
    #[test]
    fn messaging_group_within_message_threshold_two() {
        let one = vec![outbound_peer_block("planner", "Tell")];
        let units_one = partition_blocks_into_render_units(&one);
        assert!(
            !units_one.iter().any(|u| matches!(u, RenderUnit::MessagingGroup { .. })),
            "single peer block: per-message partition emits Individual, not MessagingGroup",
        );
        assert!(
            units_one.iter().any(|u| matches!(u, RenderUnit::Individual(0))),
            "single peer block renders as Individual(0); got {units_one:?}",
        );

        let two =
            vec![outbound_peer_block("planner", "Tell"), outbound_peer_block("debugger", "Ask")];
        let units_two = partition_blocks_into_render_units(&two);
        let groups: Vec<&RenderUnit> =
            units_two.iter().filter(|u| matches!(u, RenderUnit::MessagingGroup { .. })).collect();
        assert_eq!(groups.len(), 1, "two-block run still folds into one MessagingGroup");
    }

    /// Within a single message, a peer/worker run produces ONE
    /// MessagingGroup with ONE segment.
    #[test]
    fn messaging_group_partitions_within_message_run() {
        let messages = vec![assistant_message_with_blocks(vec![
            outbound_peer_block("planner", "Tell"),
            outbound_peer_block("debugger", "Ask"),
            inbound_peer_block("tester", "Reply"),
        ])];
        let per_message_units = partition_session_into_render_units(&messages);
        let groups: Vec<&RenderUnit> = per_message_units
            .iter()
            .flatten()
            .filter(|u| matches!(u, RenderUnit::MessagingGroup { .. }))
            .collect();
        assert_eq!(groups.len(), 1);
        let RenderUnit::MessagingGroup { segments, .. } = groups[0] else { unreachable!() };
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].segment_count, 3);
        assert_eq!(segments[0].group_total_count, 3);
    }

    /// Mixed direction lives in the same group; targets render in
    /// order of first appearance.
    #[test]
    fn messaging_group_mixed_direction_targets_ordered() {
        let messages = vec![assistant_message_with_blocks(vec![
            outbound_peer_block("planner", "Tell"),
            inbound_peer_block("tester", "Reply"),
            outbound_peer_block("debugger", "Ask"),
            inbound_peer_block("reviewer", "Message"),
        ])];
        let per_message_units = partition_session_into_render_units(&messages);
        let groups: Vec<&RenderUnit> = per_message_units
            .iter()
            .flatten()
            .filter(|u| matches!(u, RenderUnit::MessagingGroup { .. }))
            .collect();
        let RenderUnit::MessagingGroup { segments, .. } = groups[0] else { unreachable!() };
        assert_eq!(
            segments[0].segment_outbound_targets.targets,
            vec!["planner".to_owned(), "debugger".to_owned()],
        );
        assert_eq!(
            segments[0].segment_inbound_targets.targets,
            vec!["tester".to_owned(), "reviewer".to_owned()],
        );
    }

    /// `+N` overflow: first 2 targets render by name; the rest count
    /// into `overflow_n`.
    #[test]
    fn messaging_group_overflow_n_at_more_than_two_targets() {
        let messages = vec![assistant_message_with_blocks(vec![
            outbound_peer_block("a", "Tell"),
            outbound_peer_block("b", "Tell"),
            outbound_peer_block("c", "Tell"),
            outbound_peer_block("d", "Tell"),
            outbound_peer_block("e", "Tell"),
        ])];
        let per_message_units = partition_session_into_render_units(&messages);
        let groups: Vec<&RenderUnit> = per_message_units
            .iter()
            .flatten()
            .filter(|u| matches!(u, RenderUnit::MessagingGroup { .. }))
            .collect();
        let RenderUnit::MessagingGroup { segments, .. } = groups[0] else { unreachable!() };
        assert_eq!(
            segments[0].segment_outbound_targets.targets,
            vec!["a".to_owned(), "b".to_owned()],
        );
        assert_eq!(segments[0].segment_outbound_targets.overflow_n, 3);
    }

    /// Adjacent peer/worker + tool-call runs partition into TWO render
    /// units (one MessagingGroup, one tool Group), never merged.
    #[test]
    fn messaging_and_tool_groups_never_merge() {
        let messages = vec![assistant_message_with_blocks(vec![
            outbound_peer_block("planner", "Tell"),
            outbound_peer_block("debugger", "Ask"),
            tool_call_block("r1", "Read"),
            tool_call_block("r2", "Read"),
            tool_call_block("r3", "Read"),
        ])];
        let per_message_units = partition_session_into_render_units(&messages);
        let units = &per_message_units[0];
        let messaging_count =
            units.iter().filter(|u| matches!(u, RenderUnit::MessagingGroup { .. })).count();
        let tool_group_count =
            units.iter().filter(|u| matches!(u, RenderUnit::Group { .. })).count();
        assert_eq!(messaging_count, 1, "one messaging group");
        assert_eq!(tool_group_count, 1, "one tool group");
    }

    /// A visible text block (assistant role, non-peer) BREAKS the run.
    /// The two peer runs split into TWO messaging groups.
    #[test]
    fn messaging_group_splits_across_visible_block() {
        let messages = vec![
            assistant_message_with_blocks(vec![
                outbound_peer_block("planner", "Tell"),
                outbound_peer_block("debugger", "Ask"),
            ]),
            user_text_message("any update?"),
            assistant_message_with_blocks(vec![
                text_block("here's an update"),
                outbound_peer_block("reviewer", "Tell"),
            ]),
        ];
        let per_message_units = partition_session_into_render_units(&messages);
        let groups: Vec<&RenderUnit> = per_message_units
            .iter()
            .flatten()
            .filter(|u| matches!(u, RenderUnit::MessagingGroup { .. }))
            .collect();
        assert_eq!(
            groups.len(),
            2,
            "an assistant-role text block between peer runs MUST split into two groups",
        );
    }
}

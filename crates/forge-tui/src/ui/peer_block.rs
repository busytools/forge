//! Peer-coordination chat blocks (#114 v1).
//!
//! Two responsibilities:
//!
//! 1. **Inbound rendering**. Draw the envelope
//!    `forge_sessions::envelope::detect_inbound` parses out of the
//!    bracket-wrapped prose `forge_workspace` injects into user-turn text
//!    (e.g. `[Question id=q-... from agent 'forge' (org 'Personal') -
//!    reply with agents__tell in_reply_to=q-...]\n\n<body>`) as a
//!    styled block in place of the default user-message
//!    bubble. Catches the five agent kinds the workspace produces
//!    (`Question`, `Message`, `Reply`, `DeliveryFailure`,
//!    `WorkerSpawnFailed`) plus the `Gotify`, `Cron` and `Slack`
//!    blocks, which render with their own chrome (glyph + source
//!    label).
//!
//! 2. **Outbound rendering**. Replace the default tool_use card for
//!    `mcp__forge__agents__ask` / `agents__tell` with a one-line
//!    `▶ Verb name` row + a body preview pulled from the tool
//!    arguments. The other `agents__*` verbs are NOT handled here -
//!    they render as standard tool cards because they're lifecycle,
//!    roster and reply calls, not new outbound comms.
//!
//! Pure rendering - no I/O, no state. The outbound kind is resolved
//! fresh on each call and not cached (the arguments are small, and
//! render frames don't call this hot enough to need a cache).
//!
//! Visual reference: `docs/book/src/ui/agents.md`.

use crate::app::ToolCallInfo;
use crate::ui::chat_tree;
use crate::ui::theme;
use forge_sessions::envelope::{PeerInboundKind, is_slack_id, tidy_mrkdwn};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// One outbound agent block parsed from a `mcp__forge__agents__ask` or
/// `mcp__forge__agents__tell` tool_use card. Both render with the same
/// `▶ Verb name` shape; `target` is the addressed seat.
#[derive(Debug)]
pub(crate) enum PeerOutboundKind {
    Ask { target: String, body: String },
    Tell { target: String, body: String },
}

/// Modifier suffix surfaced inline after the `Verb name` header when
/// the envelope is a notice variant. Renders as ` - ⚠ <label>` in
/// `STATUS_WARNING`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoticeModifier {
    Undeliverable,
}

impl NoticeModifier {
    const fn label(self) -> &'static str {
        match self {
            Self::Undeliverable => "undeliverable",
        }
    }
}

/// Detect an agent outbound tool_use card. Returns `None` for every
/// other tool - the chat renderer falls through to the default
/// tool-card rendering - including `agents__spawn`, `agents__despawn`,
/// `agents__update`, `agents__capacity` and `agents__list`: those are
/// lifecycle and roster calls that render as standard tool cards
/// rather than as agent comms.
pub(crate) fn detect_outbound(tc: &ToolCallInfo) -> Option<PeerOutboundKind> {
    let raw = tc.raw_input.as_ref()?;
    match tc.sdk_tool_name.as_str() {
        "mcp__forge__agents__ask" => {
            let target = address(raw)?;
            let body = raw.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Ask { target, body })
        }
        "mcp__forge__agents__tell" => {
            let target = address(raw)?;
            let body = raw.get("message").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Tell { target, body })
        }
        // The four arms below read transcripts, not calls. A session
        // recorded before the agents family replaced the two it used to
        // have still holds these cards, and resuming feeds that history
        // through this same walker - so matching them keeps those rows
        // rendering as agent blocks instead of degrading to generic tool
        // cards. Nothing can call them; they are registered nowhere.
        // replay-only: peers__ask_agent
        "mcp__forge__peers__ask_agent" => {
            let target = raw.get("target")?.as_str()?.to_owned();
            let body = raw.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Ask { target, body })
        }
        // replay-only: workers__ask
        "mcp__forge__workers__ask" => {
            let target = raw.get("label")?.as_str()?.to_owned();
            let body = raw.get("question").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Ask { target, body })
        }
        // replay-only: peers__tell_agent
        "mcp__forge__peers__tell_agent" => {
            let target = raw.get("target")?.as_str()?.to_owned();
            let body = raw.get("message").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Tell { target, body })
        }
        // replay-only: workers__tell
        "mcp__forge__workers__tell" => {
            let target = raw.get("label")?.as_str()?.to_owned();
            let body = raw.get("message").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Tell { target, body })
        }
        _ => None,
    }
}

/// The `${project}` / `${project}/${label}` a header shows for a
/// call's target. `None` when the call carries no target at all, which
/// is a reply: it goes to whoever asked, and the header says so by
/// falling through to the default tool card.
fn address(raw: &serde_json::Value) -> Option<String> {
    let project = raw.get("project")?.as_str()?;
    match raw.get("label").and_then(|v| v.as_str()) {
        Some(label) if label != forge_workspace::LEAD_LABEL => Some(format!("{project}/{label}")),
        _ => Some(project.to_owned()),
    }
}

/// Build the styled lines for an inbound peer block.
///
/// `suppress_header = true` is the same-worker streak-follower case:
/// the `▶ Verb name` line is dropped and only the body lines render,
/// so consecutive messages from the same worker stack as one
/// paragraph.
#[cfg(test)]
pub(crate) fn render_inbound(
    kind: &PeerInboundKind,
    suppress_header: bool,
    collapsed: bool,
) -> Vec<Line<'static>> {
    let mut copy_rows = Vec::new();
    render_inbound_with_metas(kind, suppress_header, collapsed, &mut copy_rows)
}

/// As `render_inbound`, also emitting each row's copy provenance:
/// the header and collapsed-summary rows are chrome, the tree body rows
/// carry the 5-column connector prefix as chrome.
pub(crate) fn render_inbound_with_metas(
    kind: &PeerInboundKind,
    suppress_header: bool,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    match kind {
        PeerInboundKind::Question { from, body, .. } => render_block(
            "Question",
            from,
            None,
            body,
            INBOUND_GLYPH,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::Message { from, body, .. } => render_block(
            "Message",
            from,
            None,
            body,
            INBOUND_GLYPH,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::Reply { from, body, .. } => render_block(
            "Reply",
            from,
            None,
            body,
            INBOUND_GLYPH,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::DeliveryFailure { target, reason, .. } => render_block(
            "Ask",
            target,
            Some(NoticeModifier::Undeliverable),
            reason,
            INBOUND_GLYPH,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::WorkerSpawnFailed { label, reason } => {
            render_worker_spawn_failed(label, reason, copy_rows)
        }
        PeerInboundKind::Gotify { app, title, message, priority } => render_gotify_notification(
            app,
            *priority,
            title,
            message,
            suppress_header,
            collapsed,
            copy_rows,
        ),
        PeerInboundKind::Cron { prompt } => {
            render_cron_prompt(prompt, suppress_header, collapsed, copy_rows)
        }
        PeerInboundKind::Slack { workspace, channel, author, body } => render_slack_notification(
            workspace,
            channel,
            author.as_deref(),
            body,
            suppress_header,
            collapsed,
            copy_rows,
        ),
    }
}

/// Build the styled lines for an outbound peer / worker block.
#[cfg(test)]
pub(crate) fn render_outbound(kind: &PeerOutboundKind, collapsed: bool) -> Vec<Line<'static>> {
    let mut copy_rows = Vec::new();
    render_outbound_with_metas(kind, collapsed, &mut copy_rows)
}

/// As `render_outbound`, also emitting each row's copy provenance.
pub(crate) fn render_outbound_with_metas(
    kind: &PeerOutboundKind,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    match kind {
        PeerOutboundKind::Ask { target, body } => {
            render_block("Ask", target, None, body, OUTBOUND_GLYPH, false, collapsed, copy_rows)
        }
        PeerOutboundKind::Tell { target, body } => {
            render_block("Tell", target, None, body, OUTBOUND_GLYPH, false, collapsed, copy_rows)
        }
    }
}

/// Header glyph for every chat block. Distinct enough from the
/// standard tool-card glyphs (`✓` / `⚠` / `✗`) to read as "this is a
/// peer / worker row, not a tool call".
const ROW_GLYPH: &str = "\u{25B6}"; // ▶

/// Directional kind-icon for outbound rows (Ask / Tell). U+2934
/// CURVED ARROW POINTING RIGHTWARDS AND CURVING UPWARDS.
const OUTBOUND_GLYPH: &str = "\u{2934}";

/// Directional kind-icon for inbound rows (Question / Message /
/// Reply / DeliveryFailure). U+2935 CURVED ARROW POINTING RIGHTWARDS
/// AND CURVING DOWNWARDS.
const INBOUND_GLYPH: &str = "\u{2935}";

/// Kind-icon for an inbound Gotify notification. U+25C8 - the shared
/// gotify glyph (also the Inspector GOTIFY status line), so gotify reads
/// with one icon everywhere; distinct from the `ROW_GLYPH` peer / worker
/// rows use. Monochrome + width-1, matching the codebase glyph convention.
const GOTIFY_GLYPH: &str = "\u{25C8}";

/// Kind-icon for a fired-cron block. U+25F4 CIRCLE WITH UPPER LEFT
/// QUADRANT - the same glyph the Inspector SCHEDULES section uses for a
/// cron, so cron reads with one icon everywhere.
const CRON_GLYPH: &str = "\u{25f4}";

/// Kind-icon for an inbound Slack message. U+25C7 WHITE DIAMOND - already the
/// Task / Agent kind row's glyph, and the hollow companion to gotify's filled
/// ◈, distinct from cron's ◴. Monochrome + width-1, matching the codebase
/// glyph convention.
const SLACK_GLYPH: &str = "\u{25c7}";

/// A Gotify priority at or above this renders in `STATUS_WARNING` (a
/// severity cue); below it stays `DIM`. Gotify priorities run 0-10.
const GOTIFY_ELEVATED_PRIORITY: u8 = 5;

/// Unified renderer for the new chat-block shape:
///
/// ```text
///   ▶ ⤴ Verb name[ - ⚠ modifier]   (outbound)
///   ▶ ⤵ Verb name[ - ⚠ modifier]   (inbound)
///   │  body line 1
///   └─ body line 2
/// ```
///
/// `direction_glyph` is the leading kind-icon - `OUTBOUND_GLYPH` for
/// `render_outbound` callers, `INBOUND_GLYPH` for every `render_inbound`
/// arm. `suppress_header = true` drops the header row entirely (same-
/// worker streak follower) and the glyph goes with it.
fn render_block(
    verb: &str,
    name: &str,
    modifier: Option<NoticeModifier>,
    body: &str,
    direction_glyph: &str,
    suppress_header: bool,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !suppress_header {
        let mut header = Line::default();
        header.spans.push(Span::raw("  "));
        header.spans.push(Span::styled(
            ROW_GLYPH.to_owned(),
            Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
        ));
        header.spans.push(Span::raw(" "));
        header.spans.push(Span::styled(
            direction_glyph.to_owned(),
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD),
        ));
        header.spans.push(Span::raw(" "));
        header.spans.push(Span::styled(
            format!("{verb} {name}"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        if let Some(m) = modifier {
            header.spans.push(Span::styled(" - ".to_owned(), Style::default().fg(theme::DIM)));
            header.spans.push(Span::styled(
                format!("\u{26a0} {}", m.label()),
                Style::default().fg(theme::STATUS_WARNING),
            ));
        }
        lines.push(header);
        copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    }
    if collapsed {
        push_collapsed_summary(&mut lines, copy_rows, body);
    } else {
        push_tree_body_lines(&mut lines, copy_rows, body);
    }
    lines
}

/// One-off renderer for the `WorkerSpawnFailed` lifecycle notice.
/// Kept distinct from `render_block` because the spawn failure is a
/// workspace-generated system event, not a peer comm; carrying it
/// through the verb-row shape would force a non-fitting verb. The
/// ✗-glyph + plain prose treatment signals "system notice, not a
/// peer row" at a glance.
fn render_worker_spawn_failed(
    label: &str,
    reason: &str,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut header = Line::default();
    header.spans.push(Span::raw("  "));
    header.spans.push(Span::styled(
        "\u{2717}".to_owned(),
        Style::default().fg(theme::STATUS_ERROR).add_modifier(Modifier::BOLD),
    ));
    header.spans.push(Span::raw(" "));
    header.spans.push(Span::styled(
        format!("Worker '{label}' spawn failed"),
        Style::default().fg(theme::STATUS_ERROR).add_modifier(Modifier::BOLD),
    ));
    lines.push(header);
    copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    push_tree_body_lines(&mut lines, copy_rows, reason);
    lines
}

/// Render an inbound Gotify notification block. Distinct chrome from the
/// peer rows: the ◈ gotify glyph + `app 'X' - priority N` header (priority
/// in `STATUS_WARNING` at or above [`GOTIFY_ELEVATED_PRIORITY`], else
/// `DIM`), body = title then message under the standard tree connectors.
/// `suppress_header` drops the header line (streak follower); Gotify
/// turns stand alone in practice, so it's normally rendered in full.
fn render_gotify_notification(
    app: &str,
    priority: u8,
    title: &str,
    message: &str,
    suppress_header: bool,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !suppress_header {
        let priority_style = if priority >= GOTIFY_ELEVATED_PRIORITY {
            Style::default().fg(theme::STATUS_WARNING)
        } else {
            Style::default().fg(theme::DIM)
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                GOTIFY_GLYPH.to_owned(),
                Style::default().fg(theme::GOTIFY).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(format!("app '{app}'"), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(" - priority ".to_owned(), Style::default().fg(theme::DIM)),
            Span::styled(format!("{priority}"), priority_style),
        ]));
        copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    }
    let body = if message.is_empty() { title.to_owned() } else { format!("{title}\n{message}") };
    if collapsed {
        push_collapsed_summary(&mut lines, copy_rows, &body);
    } else {
        push_tree_body_lines(&mut lines, copy_rows, &body);
    }
    lines
}

/// Render a fired-cron block. Distinct chrome from peer rows: the ◴ cron
/// glyph + a `Cron` source label (the SCHEDULES-section cron glyph), body =
/// the fired prompt under the standard tree connectors. `suppress_header`
/// drops the header line (streak follower); cron turns stand alone in
/// practice, so it's normally rendered in full.
fn render_cron_prompt(
    prompt: &str,
    suppress_header: bool,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !suppress_header {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                CRON_GLYPH.to_owned(),
                Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled("Cron", Style::default().add_modifier(Modifier::BOLD)),
        ]));
        copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    }
    if collapsed {
        push_collapsed_summary(&mut lines, copy_rows, prompt);
    } else {
        push_tree_body_lines(&mut lines, copy_rows, prompt);
    }
    lines
}

/// Render an inbound Slack message block. Distinct chrome from the peer and
/// Gotify rows: the ◇ glyph and one header row carrying the channel, then the
/// workspace and author, over the message body. The author clause needs a
/// resolved handle, so it is dropped for a bot post (`unknown`) and for a raw
/// id alike.
fn render_slack_notification(
    workspace: &str,
    channel: &str,
    author: Option<&str>,
    body: &str,
    suppress_header: bool,
    collapsed: bool,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !suppress_header {
        let dim = Style::default().fg(theme::DIM);
        let mut header = Line::default();
        header.spans.push(Span::raw("  "));
        header.spans.push(Span::styled(
            SLACK_GLYPH.to_owned(),
            Style::default().fg(theme::SLACK).add_modifier(Modifier::BOLD),
        ));
        header.spans.push(Span::raw(" "));
        // An id is never dressed as a channel: a DM's label is the partner's
        // user id, and `#U0AE0CBJ77G` reads as a channel that does not exist.
        let label = if is_slack_id(channel) { channel.to_owned() } else { format!("#{channel}") };
        header.spans.push(Span::styled(label, Style::default().add_modifier(Modifier::BOLD)));
        header.spans.push(Span::styled(format!(" \u{b7} {workspace}"), dim));
        if let Some(author) = author {
            header.spans.push(Span::styled(format!(" \u{b7} {author}"), dim));
        }
        lines.push(header);
        copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
    }
    let body = tidy_mrkdwn(body);
    if collapsed {
        push_collapsed_summary(&mut lines, copy_rows, &body);
    } else {
        push_tree_body_lines(&mut lines, copy_rows, &body);
    }
    lines
}

/// Tree row data for one envelope: the direction glyph, the kind label,
/// and whether the kind is a failure (styled as a warning).
///
/// The KIND is the envelope kind, not the direction - a per-message group
/// is always single-direction, so direction would never discriminate.
pub(crate) fn inbound_kind_row(
    kind: &PeerInboundKind,
) -> Option<(&'static str, &'static str, bool)> {
    let row = match kind {
        PeerInboundKind::Message { .. } => (INBOUND_GLYPH, "message", false),
        PeerInboundKind::Question { .. } => (INBOUND_GLYPH, "question", false),
        PeerInboundKind::Reply { .. } => (INBOUND_GLYPH, "reply", false),
        PeerInboundKind::DeliveryFailure { .. } => (INBOUND_GLYPH, "failed", true),
        PeerInboundKind::WorkerSpawnFailed { .. } => (INBOUND_GLYPH, "spawn failed", true),
        // External events, never agent traffic - excluded from grouping.
        PeerInboundKind::Gotify { .. }
        | PeerInboundKind::Cron { .. }
        | PeerInboundKind::Slack { .. } => return None,
    };
    Some(row)
}

/// Sibling of [`inbound_kind_row`] for outbound calls.
pub(crate) fn outbound_kind_row(kind: &PeerOutboundKind) -> (&'static str, &'static str) {
    match kind {
        PeerOutboundKind::Ask { .. } => (OUTBOUND_GLYPH, "ask"),
        PeerOutboundKind::Tell { .. } => (OUTBOUND_GLYPH, "tell"),
    }
}

/// The body text a leaf row previews, per envelope kind. The external-event
/// arms - `Gotify`, `Cron` and `Slack` - are unreachable: `inbound_kind_row`
/// returns `None` for all three, so none ever becomes a leaf.
pub(crate) fn inbound_body(kind: &PeerInboundKind) -> &str {
    match kind {
        PeerInboundKind::Message { body, .. }
        | PeerInboundKind::Question { body, .. }
        | PeerInboundKind::Reply { body, .. }
        | PeerInboundKind::Slack { body, .. } => body,
        PeerInboundKind::DeliveryFailure { reason, .. }
        | PeerInboundKind::WorkerSpawnFailed { reason, .. } => reason,
        PeerInboundKind::Gotify { message, .. } => message,
        PeerInboundKind::Cron { prompt } => prompt,
    }
}

/// A leaf row's content: `<peer> · <first non-blank body line>`. The
/// renderer clips this to a computed budget, so no fixed length here -
/// end-ellipsis keeps the peer name, which is at the head.
pub(crate) fn kind_row_target(peer: &str, body: &str) -> String {
    let head = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if head.is_empty() { peer.to_owned() } else { format!("{peer} \u{b7} {head}") }
}

/// One-line collapsed summary shape: `  └─ <first line of body, truncated>  click or ctrl+x to expand`.
/// Skips entirely when the body is empty so notice variants (which
/// have no prose body) don't render an orphan `└─ click to expand`
/// row pointing at nothing.
fn push_collapsed_summary(
    lines: &mut Vec<Line<'static>>,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
    body: &str,
) {
    // First non-blank line, truncated to a short width so the summary
    // fits on one terminal row. Matches the standard tool card's
    // collapsed summary length (`DEFAULT_COLLAPSED_TEXT_SUMMARY_LIMIT`
    // = 60 chars).
    const SUMMARY_LIMIT: usize = 60;
    let body = body.trim();
    if body.is_empty() {
        return;
    }
    let head = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let summary: String = if head.chars().count() > SUMMARY_LIMIT {
        let mut s: String = head.chars().take(SUMMARY_LIMIT).collect();
        s.push('\u{2026}');
        s
    } else {
        head.to_owned()
    };

    let dim = Style::default().fg(theme::DIM);
    let mut line = Line::default();
    line.spans.push(Span::styled(format!("  {}", chat_tree::LAST), dim));
    line.spans.push(Span::styled(summary, dim));
    line.spans.push(Span::styled("  click or ctrl+x to expand".to_owned(), dim));
    lines.push(line);
    copy_rows.push(crate::ui::copy::CopyRowMeta::chrome(0));
}

/// Push the body lines under `│  ` / `└─ ` tree connectors - matches
/// `tool_call::standard::render_tool_content`'s pipe / corner glyph
/// pair. Renders the FULL body when expanded - no truncation; the
/// collapsed summary (see [`push_collapsed_summary`]) is the only
/// place we truncate, and only to fit a single summary row. When the
/// body is empty, pushes nothing so the header stands alone.
fn push_tree_body_lines(
    lines: &mut Vec<Line<'static>>,
    copy_rows: &mut Vec<crate::ui::copy::CopyRowMeta>,
    body: &str,
) {
    /// Display columns of the `  │  ` / `  └─ ` connector prefix.
    const CONNECTOR_COLS: u16 = 5;
    let body = body.trim();
    if body.is_empty() {
        return;
    }

    let pipe_style = Style::default().fg(theme::DIM);
    let body_text_style = Style::default().fg(Color::Gray);

    let body_lines: Vec<&str> = body.lines().collect();
    let last_idx = body_lines.len().saturating_sub(1);
    for (idx, raw_line) in body_lines.iter().enumerate() {
        let prefix = if idx == last_idx {
            format!("  {}", chat_tree::LAST)
        } else {
            format!("  {}  ", chat_tree::SPINE)
        };
        lines.push(Line::from(vec![
            Span::styled(prefix, pipe_style),
            Span::styled((*raw_line).to_owned(), body_text_style),
        ]));
        copy_rows.push(crate::ui::copy::CopyRowMeta::hard_line().offset_chrome(CONNECTOR_COLS));
    }
}

/// Render the L2 summary TREE for a messaging group: a parent count row
/// plus one kind row per envelope kind and one leaf per message, drawn
/// by the same renderer the tool groups use.
pub(crate) fn render_messaging_group_summary_line(
    segment: &crate::ui::message::grouping::MessagingGroupSegment,
    spinner_glyph: char,
    max_width: usize,
) -> Vec<Line<'static>> {
    crate::ui::tool_call::render_group_summary_line(
        &segment.summary,
        segment.aggregate_status,
        spinner_glyph,
        max_width,
        None,
        &crate::ui::tool_call::SummaryChrome::MESSAGING,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_sessions::envelope::detect_inbound;

    fn render_lines_to_strings(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone().into_owned()).collect::<String>())
            .collect()
    }

    #[test]
    fn render_inbound_question_full_shape() {
        let kind = PeerInboundKind::Question {
            from: "planner".into(),
            org: "Personal".into(),
            body: "Is the seam plan ready?".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains("\u{25B6}"), "header has ▶ glyph: {:?}", s[0]);
        assert!(s[0].contains("Question planner"), "verb + name: {:?}", s[0]);
        assert!(!s[0].contains("Personal"), "org suppressed: {:?}", s[0]);
        assert!(s.last().unwrap().contains("Is the seam plan ready?"));
    }

    #[test]
    fn render_inbound_suppress_header_drops_verb_line() {
        let kind = PeerInboundKind::Message {
            from: "implementer".into(),
            org: "Personal".into(),
            body: "PR #187 open.".into(),
        };
        let lines = render_inbound(&kind, true, false);
        let s = render_lines_to_strings(&lines);
        // No verb header, only body lines under tree connectors.
        assert!(!s.iter().any(|line| line.contains("\u{25B6}")), "no ▶ header: {s:?}");
        assert!(s.iter().any(|line| line.contains("PR #187 open.")), "body present: {s:?}");
    }

    #[test]
    fn render_inbound_collapsed_shows_summary_with_hint() {
        let kind = PeerInboundKind::Message {
            from: "planner".into(),
            org: "Personal".into(),
            body: "first line\nsecond line".into(),
        };
        let lines = render_inbound(&kind, false, true);
        let s = render_lines_to_strings(&lines);
        assert!(s.iter().any(|line| line.contains("click or ctrl+x to expand")));
        // Only the first line is surfaced in the summary, not the second.
        assert!(s.iter().any(|line| line.contains("first line")));
        assert!(!s.iter().any(|line| line.contains("second line")));
    }

    #[test]
    fn render_inbound_expanded_keeps_full_body() {
        let kind = PeerInboundKind::Message {
            from: "planner".into(),
            org: "Personal".into(),
            body: "one\ntwo\nthree".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(s.iter().any(|line| line.contains("one")));
        assert!(s.iter().any(|line| line.contains("two")));
        assert!(s.iter().any(|line| line.contains("three")));
    }

    #[test]
    fn render_outbound_ask_shape() {
        let kind = PeerOutboundKind::Ask {
            target: "planner".into(),
            body: "Is the seam plan ready?".into(),
        };
        let lines = render_outbound(&kind, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains("Ask planner"), "verb + target: {:?}", s[0]);
        assert!(s[0].contains("\u{25B6}"), "▶ glyph: {:?}", s[0]);
    }

    #[test]
    fn render_outbound_tell_shape() {
        let kind = PeerOutboundKind::Tell {
            target: "planner".into(),
            body: "FYI: PR #187 is open.".into(),
        };
        let lines = render_outbound(&kind, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains("Tell planner"));
    }

    #[test]
    fn render_outbound_ask_includes_outbound_directional_glyph() {
        let kind = PeerOutboundKind::Ask {
            target: "planner".into(),
            body: "Is the seam plan ready?".into(),
        };
        let lines = render_outbound(&kind, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2934}'),
            "outbound glyph ⤴ U+2934 must appear in header: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_outbound_tell_includes_outbound_directional_glyph() {
        let kind = PeerOutboundKind::Tell {
            target: "planner".into(),
            body: "FYI: PR #187 is open.".into(),
        };
        let lines = render_outbound(&kind, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2934}'),
            "outbound glyph ⤴ U+2934 must appear in header: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_inbound_question_includes_inbound_directional_glyph() {
        let kind = PeerInboundKind::Question {
            from: "alice".into(),
            org: "org".into(),
            body: "what?".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2935}'),
            "inbound glyph ⤵ U+2935 must appear in header: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_inbound_reply_includes_inbound_directional_glyph() {
        let kind = PeerInboundKind::Reply {
            from: "alice".into(),
            org: "org".into(),
            body: "answer".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2935}'),
            "inbound glyph ⤵ U+2935 must appear in header: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_inbound_delivery_failure_carries_inbound_glyph() {
        let kind = PeerInboundKind::DeliveryFailure {
            target: "planner".into(),
            org: "Personal".into(),
            reason: "target spawn failed".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            s[0].contains('\u{2935}'),
            "DeliveryFailure routes through render_inbound and carries ⤵: {:?}",
            s[0]
        );
    }

    #[test]
    fn render_inbound_suppress_header_drops_directional_glyph_too() {
        let kind = PeerInboundKind::Message {
            from: "implementer".into(),
            org: "Personal".into(),
            body: "PR ready.".into(),
        };
        let lines = render_inbound(&kind, true, false);
        let s = render_lines_to_strings(&lines);
        assert!(
            !s.iter().any(|line| line.contains('\u{2935}')),
            "suppress_header drops the whole header (including glyph): {s:?}"
        );
    }

    #[test]
    fn render_worker_spawn_failed_uses_distinct_chrome() {
        let kind = PeerInboundKind::WorkerSpawnFailed {
            label: "planner".into(),
            reason: "git not found".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        // Uses ✗ rather than ▶ to mark it as a system notice, not a peer row.
        assert!(s[0].contains("\u{2717}"), "✗ glyph: {:?}", s[0]);
        assert!(s[0].contains("Worker 'planner' spawn failed"));
        assert!(s.last().unwrap().contains("git not found"));
    }

    #[test]
    fn render_cron_inbound_uses_cron_glyph_and_label() {
        let kind = PeerInboundKind::Cron { prompt: "deploy the release".into() };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains('\u{25f4}'), "◴ cron glyph: {:?}", s[0]);
        assert!(s[0].contains("Cron"), "Cron label: {:?}", s[0]);
        assert!(s.last().unwrap().contains("deploy the release"), "prompt body: {:?}", s.last());
    }

    /// The block's shape: one header row carrying the glyph, the bold channel,
    /// then the dim workspace and author, with the body under the standard
    /// tree.
    ///
    /// ```text
    /// Slack
    ///   ◇ #granite-staging-alerts · Trust Machines · granite-bot
    ///   └─ Large STX Transfer: 233468.293536 STX (~$60434.82 USD)
    /// ```
    ///
    /// `granite-bot` is a resolved handle, which today's producer does not
    /// emit: `message.user` arrives as a raw id and the block drops it.
    #[test]
    fn render_slack_notification_header_carries_the_channel_workspace_and_author() {
        let kind = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "granite-staging-alerts".into(),
            author: Some("granite-bot".into()),
            body: "Large STX Transfer: 233468.293536 STX (~$60434.82 USD)".into(),
        };
        let lines = render_inbound(&kind, false, false);
        let s = render_lines_to_strings(&lines);
        assert!(s[0].contains('\u{25C7}'), "◇ slack glyph in header: {:?}", s[0]);
        assert!(s[0].contains("#granite-staging-alerts"), "channel in header: {:?}", s[0]);
        assert!(s[0].contains("Trust Machines"), "workspace in header: {:?}", s[0]);
        assert!(s[0].contains("granite-bot"), "author in header: {:?}", s[0]);
        assert!(!s[0].contains('\u{25B6}'), "no peer row glyph: {:?}", s[0]);
        assert!(s.last().unwrap().contains("Large STX Transfer"), "body: {:?}", s.last());
        let channel_span = lines[0]
            .spans
            .iter()
            .find(|sp| sp.content.contains("#granite-staging-alerts"))
            .expect("the channel span");
        assert!(
            channel_span.style.add_modifier.contains(Modifier::BOLD),
            "the channel reads bold on the header",
        );
        let glyph_span =
            lines[0].spans.iter().find(|sp| sp.content.contains('\u{25C7}')).expect("glyph span");
        assert_eq!(
            glyph_span.style.fg,
            Some(theme::SLACK),
            "the ◇ glyph wears the Slack accent, not the neighbouring gotify one",
        );
    }

    /// A DM's label is the partner's user id, and an id is never dressed as a
    /// channel: `#U0AE0CBJ77G` reads as a channel that does not exist. A name
    /// that merely starts uppercase still is one.
    #[test]
    fn render_slack_notification_prefixes_a_name_but_not_an_id() {
        let id_label = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "U0AE0CBJ77G".into(),
            author: None,
            body: "ping".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&id_label, false, false));
        assert!(s[0].contains("U0AE0CBJ77G"), "the label is still named: {:?}", s[0]);
        assert!(!s[0].contains('#'), "and never prefixed as a channel: {:?}", s[0]);

        let name_label = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "General".into(),
            author: None,
            body: "ping".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&name_label, false, false));
        assert!(s[0].contains("#General"), "an uppercase first letter is not an id: {:?}", s[0]);
    }

    /// The collapsed block keeps the header and shows one tidied line of the
    /// body with the expand hint, the same summary every other block collapses
    /// to.
    #[test]
    fn render_slack_collapsed_shows_a_tidied_body_summary() {
        let kind = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "granite-staging-alerts".into(),
            author: None,
            body: "_Large STX Transfer_\nAmount: 233468.293536 STX (~$60434.82 USD)".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, true));
        assert!(s[0].contains('\u{25C7}'), "header still renders when collapsed: {:?}", s[0]);
        assert!(
            s.iter().any(|line| line.contains("Large STX Transfer")),
            "the tidied first body line is the summary: {s:?}",
        );
        assert!(!s.iter().any(|line| line.contains('_')), "markers gone collapsed: {s:?}");
        assert!(
            !s.iter().any(|line| line.contains("Amount:")),
            "the second body line stays hidden: {s:?}",
        );
        assert!(s.iter().any(|line| line.contains("click or ctrl+x to expand")));
    }

    /// A bundle's body is one line per member, so three members render as
    /// three rows under the one header rather than three messages crowding
    /// a single row.
    #[test]
    fn a_bundle_renders_one_row_per_member_under_one_header() {
        let prose = "[Slack - workspace 'acme', general] id C1 ts 3.3 (3 messages)\n\
                     alice: one [ts 1.1]\n\
                     bob: two [ts 2.2]\n\
                     carol: three [ts 3.3]";
        let kind = detect_inbound(prose).expect("a bundle is a Slack envelope");
        let rendered = render_lines_to_strings(&render_inbound(&kind, false, false));

        assert!(rendered[0].contains("#general"), "one header: {rendered:?}");
        let members: Vec<&String> = rendered.iter().filter(|line| line.contains("[ts ")).collect();
        assert_eq!(members.len(), 3, "one row per member: {rendered:?}");
        assert!(members[0].contains("alice"), "the first keeps its author: {rendered:?}");
        assert!(members[2].contains("carol"), "and the last its own: {rendered:?}");
    }

    /// An id is never dressed as a name, and that holds on a bundle's member
    /// lines too: a member with no resolved name keeps only its text, the
    /// same way a single message's header drops the clause.
    #[test]
    fn a_bundle_member_with_no_resolved_name_keeps_only_its_text() {
        let unknown_author = "[Slack - workspace 'acme', general] id C1 ts 2.2 (2 messages)\n\
                              alice: one [ts 1.1]\n\
                              unknown: two [ts 2.2]";
        let rendered = render_lines_to_strings(&render_inbound(
            &detect_inbound(unknown_author).unwrap(),
            false,
            false,
        ));
        assert!(
            !rendered.iter().any(|line| line.contains("unknown")),
            "the placeholder is not printed at a reader: {rendered:?}",
        );
        assert!(
            rendered.iter().any(|line| line.contains("two [ts 2.2]")),
            "and the member's text survives: {rendered:?}",
        );

        let id_author = "[Slack - workspace 'acme', general] id C1 ts 2.2 (2 messages)\n\
                         alice: one [ts 1.1]\n\
                         U0ATEK2EAGP: two [ts 2.2]";
        let rendered = render_lines_to_strings(&render_inbound(
            &detect_inbound(id_author).unwrap(),
            false,
            false,
        ));
        assert!(
            !rendered.iter().any(|line| line.contains("U0ATEK2EAGP")),
            "an unresolved id is not a name either: {rendered:?}",
        );
    }

    /// The producer resolves a handle before the prose is written, so the
    /// author clause survives the id check and the reader sees a name.
    ///
    /// Only that half is worth asserting here: with a name resolved the
    /// prose carries no id at all, so "no id reaches the reader" holds
    /// whether the clause is kept or dropped. The unresolved case is
    /// `detect_slack_inbound_drops_an_id_shaped_author`.
    #[test]
    fn slack_inbound_keeps_a_resolved_author_on_the_header() {
        let text = "[Slack - workspace 'acme', general] id C1 ts 1.1\narchitect2: hi [ts 1.1]";
        let block = render_inbound(&detect_inbound(text).expect("slack"), false, false);
        let rendered = render_lines_to_strings(&block);
        assert!(
            rendered[0].contains("architect2"),
            "the resolved handle rides the header: {rendered:?}",
        );
    }

    /// A bot message has no author, and the header drops the whole clause
    /// rather than printing `unknown` at the reader.
    #[test]
    fn render_slack_notification_omits_an_absent_author() {
        let kind = PeerInboundKind::Slack {
            workspace: "Trust Machines".into(),
            channel: "granite-staging-alerts".into(),
            author: None,
            body: "Large STX Transfer".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, false));
        assert!(s[0].contains("#granite-staging-alerts"), "channel in header: {:?}", s[0]);
        assert!(s[0].contains("Trust Machines"), "workspace in header: {:?}", s[0]);
        assert_eq!(
            s[0].matches('\u{b7}').count(),
            1,
            "only the channel/workspace separator: {:?}",
            s[0]
        );
    }

    /// The block shows the tidied body, so the markers never reach the reader.
    #[test]
    fn render_slack_notification_tidies_the_body() {
        let kind = PeerInboundKind::Slack {
            workspace: "W".into(),
            channel: "c".into(),
            author: None,
            body: "_Large STX Transfer_".into(),
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, false));
        assert!(s.last().unwrap().contains("Large STX Transfer"), "body: {:?}", s.last());
        assert!(!s.iter().any(|l| l.contains('_')), "no markers survive: {s:?}");
    }

    #[test]
    fn render_gotify_notification_full_shape() {
        let kind = PeerInboundKind::Gotify {
            app: "Backups".into(),
            title: "Nightly backup".into(),
            message: "All volumes done".into(),
            priority: 3,
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, false));
        assert!(s[0].contains('\u{25C8}'), "gotify glyph in header: {:?}", s[0]);
        assert!(s[0].contains("app 'Backups'"), "app in header: {:?}", s[0]);
        assert!(s[0].contains("priority 3"), "priority in header: {:?}", s[0]);
        assert!(!s[0].contains('\u{25B6}'), "no peer row glyph: {:?}", s[0]);
        assert!(s.iter().any(|l| l.contains("Nightly backup")), "title in body: {s:?}");
        assert!(s.iter().any(|l| l.contains("All volumes done")), "message in body: {s:?}");
    }

    #[test]
    fn render_gotify_high_priority_uses_warning_color() {
        let kind = PeerInboundKind::Gotify {
            app: "Sec".into(),
            title: "Alert".into(),
            message: "bad".into(),
            priority: 8,
        };
        let lines = render_inbound(&kind, false, false);
        let priority_span =
            lines[0].spans.iter().find(|sp| sp.content.contains('8')).expect("priority span");
        assert_eq!(priority_span.style.fg, Some(theme::STATUS_WARNING));
    }

    #[test]
    fn render_gotify_low_priority_uses_dim_color() {
        let kind = PeerInboundKind::Gotify {
            app: "Backups".into(),
            title: "ok".into(),
            message: "done".into(),
            priority: 2,
        };
        let lines = render_inbound(&kind, false, false);
        let priority_span =
            lines[0].spans.iter().find(|sp| sp.content.contains('2')).expect("priority span");
        assert_eq!(priority_span.style.fg, Some(theme::DIM));
    }

    #[test]
    fn render_gotify_collapsed_shows_title_summary_only() {
        let kind = PeerInboundKind::Gotify {
            app: "Backups".into(),
            title: "Nightly backup".into(),
            message: "All volumes done".into(),
            priority: 3,
        };
        let s = render_lines_to_strings(&render_inbound(&kind, false, true));
        assert!(s[0].contains('\u{25C8}'), "header still renders when collapsed: {:?}", s[0]);
        assert!(s.iter().any(|l| l.contains("Nightly backup")), "title in summary: {s:?}");
        assert!(s.iter().any(|l| l.contains("click or ctrl+x to expand")));
        assert!(
            !s.iter().any(|l| l.contains("All volumes done")),
            "message hidden collapsed: {s:?}"
        );
    }

    fn make_tc(sdk_tool_name: &str, raw_input: serde_json::Value) -> crate::app::ToolCallInfo {
        crate::app::ToolCallInfo {
            id: "tc-1".into(),
            title: "tc-1".into(),
            sdk_tool_name: sdk_tool_name.into(),
            raw_input: Some(raw_input),
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: crate::agent::model::ToolCallStatus::InProgress,
            content: vec![],
            hidden: false,
            terminal_output: None,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
            cache: crate::app::BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    #[test]
    fn detect_outbound_recognises_an_agents_ask_at_another_projects_agent() {
        let tc = make_tc(
            "mcp__forge__agents__ask",
            serde_json::json!({ "org": "Gateway", "project": "gateway-backend", "prompt": "?" }),
        );
        match detect_outbound(&tc) {
            Some(PeerOutboundKind::Ask { target, body }) => {
                assert_eq!(target, "gateway-backend");
                assert_eq!(body, "?");
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn detect_outbound_shows_a_worker_target_as_a_seat_in_its_project() {
        let tc = make_tc(
            "mcp__forge__agents__ask",
            serde_json::json!({
                "org": "Personal",
                "project": "forge",
                "label": "planner",
                "prompt": "ready?",
            }),
        );
        match detect_outbound(&tc) {
            Some(PeerOutboundKind::Ask { target, body }) => {
                assert_eq!(target, "forge/planner");
                assert_eq!(body, "ready?");
            }
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn detect_outbound_recognises_an_agents_tell() {
        let tc = make_tc(
            "mcp__forge__agents__tell",
            serde_json::json!({
                "org": "Personal",
                "project": "forge",
                "label": "implementer",
                "message": "PR #199 ready",
            }),
        );
        match detect_outbound(&tc) {
            Some(PeerOutboundKind::Tell { target, body }) => {
                assert_eq!(target, "forge/implementer");
                assert_eq!(body, "PR #199 ready");
            }
            other => panic!("expected Tell, got {other:?}"),
        }
    }

    #[test]
    fn detect_outbound_ignores_the_lifecycle_verbs_and_a_reply() {
        for name in ["mcp__forge__agents__spawn", "mcp__forge__agents__list"] {
            let tc = make_tc(name, serde_json::json!({ "label": "planner", "charter": "..." }));
            assert!(detect_outbound(&tc).is_none(), "{name} falls through to a standard tool card");
        }

        // A reply names no target: it is routed to whoever asked, so
        // there is no seat for the header to show.
        let reply = make_tc(
            "mcp__forge__agents__tell",
            serde_json::json!({ "message": "answer", "in_reply_to": "q-7f3a92e0" }),
        );
        assert!(detect_outbound(&reply).is_none(), "a reply falls through to a standard tool card");
    }

    #[test]
    fn detect_outbound_still_reads_a_card_a_transcript_recorded_before_the_rename() {
        // Resume feeds recorded history through this same walker, so a
        // pre-rename card has to keep rendering as an agent block rather
        // than degrade to a generic tool card.
        let peer = make_tc(
            // replay-only: peers__tell_agent
            "mcp__forge__peers__tell_agent",
            serde_json::json!({ "target": "gateway-backend", "message": "landed" }),
        );
        match detect_outbound(&peer) {
            Some(PeerOutboundKind::Tell { target, body }) => {
                assert_eq!(target, "gateway-backend");
                assert_eq!(body, "landed");
            }
            other => panic!("a pre-rename tell card must still read as a Tell, got {other:?}"),
        }

        let worker = make_tc(
            // replay-only: workers__ask
            "mcp__forge__workers__ask",
            serde_json::json!({ "label": "planner", "question": "ready?" }),
        );
        match detect_outbound(&worker) {
            Some(PeerOutboundKind::Ask { target, body }) => {
                assert_eq!(target, "planner");
                assert_eq!(body, "ready?");
            }
            other => panic!("a pre-rename ask card must still read as an Ask, got {other:?}"),
        }
    }

    #[test]
    fn detect_outbound_ignores_other_tools() {
        let tc = make_tc("Bash", serde_json::json!({ "command": "ls" }));
        assert!(detect_outbound(&tc).is_none());
    }

    fn segment_of(
        blocks: &[crate::app::MessageBlock],
    ) -> crate::ui::message::grouping::MessagingGroupSegment {
        crate::ui::message::grouping::partition_blocks_into_render_units(blocks)
            .into_iter()
            .find_map(|u| match u {
                crate::ui::message::grouping::RenderUnit::MessagingGroup { segment, .. } => {
                    Some(segment)
                }
                _ => None,
            })
            .expect("a messaging group")
    }

    fn inbound_block(kind: &str, from: &str, body: &str) -> crate::app::MessageBlock {
        crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(&format!(
            "[{kind} id=t-{from} from agent '{from}' (org 'forge')]\n\n{body}"
        )))
    }

    /// The bundle summary cycles on click as well as ctrl+x, so it
    /// advertises both.
    #[test]
    fn messaging_group_summary_advertises_click_and_ctrl_x() {
        let blocks = vec![
            inbound_block("Message", "steward", "one"),
            inbound_block("Message", "planner", "two"),
        ];
        let rendered = render_lines_to_strings(&render_messaging_group_summary_line(
            &segment_of(&blocks),
            '\u{280B}',
            80,
        ));
        assert!(rendered[0].ends_with("click or ctrl+x to expand"), "got {rendered:?}");
    }

    /// The parent row is a bare count - every peer appears as a leaf, so
    /// a target clause in the heading would name each one twice.
    #[test]
    fn messaging_group_parent_row_carries_no_target_list() {
        let blocks = vec![
            inbound_block("Message", "steward", "one"),
            inbound_block("Message", "planner", "two"),
        ];
        let rendered = render_lines_to_strings(&render_messaging_group_summary_line(
            &segment_of(&blocks),
            '\u{280B}',
            80,
        ));
        assert!(rendered[0].contains("2 messages"), "got {rendered:?}");
        assert!(!rendered[0].contains("steward"), "no target list on the parent; got {rendered:?}");
        assert!(!rendered[0].contains("inbound from"), "got {rendered:?}");
    }

    /// Always nest, never inline: a kind with one message still gets its
    /// own leaf, so peer names share a column instead of sitting at
    /// ragged widths.
    #[test]
    fn a_single_message_kind_still_nests_its_leaf() {
        let blocks = vec![
            inbound_block("Message", "steward", "one"),
            inbound_block("Reply", "tester", "two"),
        ];
        let rendered = render_lines_to_strings(&render_messaging_group_summary_line(
            &segment_of(&blocks),
            '\u{280B}',
            80,
        ));
        let reply_row = rendered.iter().find(|l| l.contains("reply")).expect("a reply kind row");
        assert!(
            !reply_row.contains("tester"),
            "the peer belongs on its own leaf, not inline on the kind row; got {reply_row:?}",
        );
        assert!(
            rendered.iter().any(|l| l.contains("tester") && !l.contains("reply")),
            "and the leaf exists separately; got {rendered:?}",
        );
    }

    /// Failure kinds carry warning colour on their kind row, and the
    /// leaf keeps the reason - a bare peer name with no reason would
    /// tell the reader nothing about what went wrong.
    #[test]
    fn failure_kind_rows_are_styled_and_keep_their_reason() {
        for (prose, label, reason) in [
            (
                "[Ask id=q-7 to agent 'planner' (org 'forge') failed to deliver: peer asleep]\n\n",
                "failed",
                "peer asleep",
            ),
            (
                "[Worker 'runner' spawn failed id=w-5: worktree busy]",
                "spawn failed",
                "worktree busy",
            ),
        ] {
            let blocks = vec![
                inbound_block("Message", "steward", "one"),
                crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(prose)),
            ];
            let lines = render_messaging_group_summary_line(&segment_of(&blocks), '\u{280B}', 80);
            let kind_span = lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .find(|s| s.content.contains(label))
                .unwrap_or_else(|| panic!("a {label} kind row"));
            assert_eq!(
                kind_span.style.fg,
                Some(theme::STATUS_WARNING),
                "{label} must read as a warning",
            );
            let rendered = render_lines_to_strings(&lines);
            assert!(
                rendered.iter().any(|l| l.contains(reason)),
                "the leaf keeps the reason; got {rendered:?}",
            );
        }
    }

    /// The leaf rows clip to a computed budget, because the outer
    /// layout char-wraps without the tree gutter and an overflowing row
    /// shears the tree. Messaging is the tightest fixture for that, its
    /// expand hint being longer than the tool one.
    #[test]
    fn leaf_rows_fit_a_narrow_width() {
        let blocks = vec![
            inbound_block(
                "Message",
                "steward",
                "a body long enough that it must be clipped to fit",
            ),
            inbound_block("Reply", "planner", "another body that would overflow a narrow terminal"),
        ];
        let rendered = render_lines_to_strings(&render_messaging_group_summary_line(
            &segment_of(&blocks),
            '\u{280B}',
            40,
        ));
        // Row 0 is the parent, which carries the longer messaging hint and
        // can exceed a narrow width - it wraps rather than shearing, since
        // the connectors live on the child rows. The CHILD rows are the
        // shearing risk and must fit.
        for row in rendered.iter().skip(1) {
            assert!(
                unicode_width::UnicodeWidthStr::width(row.as_str()) <= 40,
                "row must fit width 40; got {}: {row:?}",
                unicode_width::UnicodeWidthStr::width(row.as_str()),
            );
        }
        assert!(rendered.iter().any(|l| l.contains("...")), "something clipped; got {rendered:?}");
    }

    /// A delivery failure inside a run must not render under a green
    /// check: the parent status has to learn about inbound failures.
    #[test]
    fn a_delivery_failure_drives_the_parent_status() {
        let blocks = vec![
            inbound_block("Message", "steward", "one"),
            crate::app::MessageBlock::Text(crate::app::TextBlock::from_complete(
                "[Ask id=q-7 to agent 'planner' (org 'forge') failed to deliver: gone]\n\n",
            )),
        ];
        assert_eq!(
            segment_of(&blocks).aggregate_status,
            crate::agent::model::ToolCallStatus::Failed,
            "an undelivered message must not report success",
        );
    }
}

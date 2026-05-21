//! Peer-coordination chat blocks (#114 v1).
//!
//! Two responsibilities:
//!
//! 1. **Inbound detection + rendering**. Pattern-match the bracket-
//!    wrapped prose `forge_workspace::deliver_peer_prompt` injects
//!    into user-turn text (e.g. `[Question id=q-... hop=1/10 from
//!    agent 'forge' (org 'Personal') - reply with tell_agent
//!    in_reply_to=q-...]\n\n<body>`) and render a styled block in
//!    place of the default user-message bubble. Catches all seven
//!    kinds the workspace produces (`Question`, `Message`, `Reply`,
//!    `Late reply`, `Ask ... timed out` (caller-side),
//!    `Ask ... has expired` (recipient-side),
//!    `Ask ... failed to deliver`).
//!
//! 2. **Outbound rendering**. Replace the default tool_use card for
//!    `mcp__forge__peers__ask_agent` / `peers__tell_agent` with a
//!    one-line "→ ask · target · q-id" / "→ tell · target · t-id"
//!    block + a body preview pulled from the tool arguments.
//!
//! Pure rendering — no I/O, no state. Each call parses the text
//! fresh; results aren't cached (text is small, render frames don't
//! call this hot enough to need a cache).
//!
//! Visual reference: `.superpowers/brainstorm/peer-mcp-v1-mockup.html`.

use crate::app::ToolCallInfo;
use crate::ui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Maximum chars of the prose body to inline in a styled block.
/// Longer bodies get truncated with `…`; the user clicks-to-expand
/// (TODO: not yet wired — for now the truncation is permanent).
const BODY_TRUNCATE: usize = 200;

/// Indentation applied to the body lines (3 spaces — matches the
/// mockup's `   ` prefix on the muted body rows).
const BODY_INDENT: &str = "   ";

/// One inbound peer block parsed from the user-turn text.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PeerInboundKind {
    Question { id: String, from: String, org: String, hop: u8, hop_max: u8, body: String },
    Message { id: String, from: String, org: String, hop: u8, hop_max: u8, body: String },
    Reply { id: String, from: String, org: String, body: String },
    LateReply { id: String, from: String, org: String, body: String },
    /// `[Ask id=... to agent 'X' (org 'Y') timed out after 30 minutes - no reply received.]`
    /// Caller-side notice that their own ask hit the 30-min timer.
    CallerTimeout { id: String, target: String, org: String, body: String },
    /// `[Ask id=... from agent 'A' (org 'O') has expired - any reply you produce will be tagged late.]`
    /// Recipient-side notice that the caller's ask has been abandoned.
    RecipientExpired { id: String, from: String, org: String, body: String },
    /// `[Ask id=... to agent 'X' (org 'Y') failed to deliver: <reason>]`
    /// Caller-side delivery failure (spawn / connection / channel).
    DeliveryFailure { id: String, target: String, org: String, reason: String },
}

/// One outbound peer block parsed from a `mcp__forge__peers__*`
/// tool_use card. The correlation id is `None` until the tool call's
/// result arrives.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PeerOutboundKind {
    Ask { target: String, body: String, correlation_id: Option<String> },
    Tell { target: String, body: String, correlation_id: Option<String> },
}

/// Detect a peer wrapper at the start of a user-message text. Returns
/// `None` for any text that isn't a bracket-prefixed peer wrapper
/// (the chat renderer falls through to the default text rendering
/// in that case).
pub(crate) fn detect_inbound(text: &str) -> Option<PeerInboundKind> {
    let bracketed = text.strip_prefix('[')?;
    let close_idx = bracketed.find(']')?;
    let header = &bracketed[..close_idx];
    let after_bracket = &bracketed[close_idx + 1..];
    // The wrapper formats land the body after `]\n\n`. Some notice
    // variants have an empty body — the bracket itself is the whole
    // message, possibly followed by `\n\n` + extra context. Both
    // are valid.
    let body = after_bracket.strip_prefix("\n\n").unwrap_or("").to_owned();

    if let Some(rest) = header.strip_prefix("Question id=") {
        let (id, rest) = take_until(rest, " hop=")?;
        let (hop_str, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        let (hop, hop_max) = parse_hop(hop_str).unwrap_or((1, 10));
        return Some(PeerInboundKind::Question {
            id: id.to_owned(),
            from,
            org,
            hop,
            hop_max,
            body,
        });
    }

    if let Some(rest) = header.strip_prefix("Message id=") {
        let (id, rest) = take_until(rest, " hop=")?;
        let (hop_str, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        let (hop, hop_max) = parse_hop(hop_str).unwrap_or((1, 10));
        return Some(PeerInboundKind::Message {
            id: id.to_owned(),
            from,
            org,
            hop,
            hop_max,
            body,
        });
    }

    if let Some(rest) = header.strip_prefix("Reply id=") {
        let (id, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        return Some(PeerInboundKind::Reply {
            id: id.to_owned(),
            from,
            org,
            body,
        });
    }

    if let Some(rest) = header.strip_prefix("Late reply id=") {
        let (id, rest) = take_until(rest, " from agent ")?;
        let (from, org) = extract_from_agent_after(rest)?;
        return Some(PeerInboundKind::LateReply {
            id: id.to_owned(),
            from,
            org,
            body,
        });
    }

    if let Some(rest) = header.strip_prefix("Ask id=") {
        // Caller-side timeout — `to agent 'X' (org 'Y') timed out ...`
        if let Some(rest_to) = rest_after_id(rest, " to agent ")
            && header.contains("timed out after 30 minutes")
        {
            let id = id_before(rest, " to agent ")?;
            let (target, org) = extract_from_agent_after(rest_to)?;
            return Some(PeerInboundKind::CallerTimeout {
                id: id.to_owned(),
                target,
                org,
                body,
            });
        }
        // Caller-side delivery failure — `to agent 'X' (org 'Y') failed to deliver: <reason>`
        if let Some(rest_to) = rest_after_id(rest, " to agent ")
            && header.contains("failed to deliver:")
        {
            let id = id_before(rest, " to agent ")?;
            let (target, org_and_reason) = extract_from_agent_after_with_trailer(rest_to)?;
            // org_and_reason is "<rest after closing paren>" i.e.
            // " failed to deliver: <reason>"
            let reason = org_and_reason
                .split_once("failed to deliver:")
                .map(|(_, after)| after.trim().to_owned())
                .unwrap_or_default();
            let org = org_and_reason.split('|').next().unwrap_or("?").to_owned();
            return Some(PeerInboundKind::DeliveryFailure {
                id: id.to_owned(),
                target,
                org,
                reason,
            });
        }
        // Recipient-side expired — `from agent 'A' (org 'O') has expired ...`
        if let Some(rest_from) = rest_after_id(rest, " from agent ")
            && header.contains("has expired")
        {
            let id = id_before(rest, " from agent ")?;
            let (from, org) = extract_from_agent_after(rest_from)?;
            return Some(PeerInboundKind::RecipientExpired {
                id: id.to_owned(),
                from,
                org,
                body,
            });
        }
    }

    None
}

/// Detect an outbound peer tool_use card. Returns `None` for any
/// tool that isn't a peer ask/tell.
pub(crate) fn detect_outbound(tc: &ToolCallInfo) -> Option<PeerOutboundKind> {
    let raw = tc.raw_input.as_ref()?;
    let target = raw.get("target")?.as_str()?.to_owned();
    let correlation_id = extract_correlation_id_from_result(tc);
    match tc.sdk_tool_name.as_str() {
        "mcp__forge__peers__ask_agent" => {
            let body = raw.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Ask { target, body, correlation_id })
        }
        "mcp__forge__peers__tell_agent" => {
            let body = raw.get("message").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            Some(PeerOutboundKind::Tell { target, body, correlation_id })
        }
        _ => None,
    }
}

/// Build the styled lines for an inbound peer block. The chat
/// renderer calls this after `detect_inbound` matches and SKIPS the
/// default user-message rendering for the same text.
pub(crate) fn render_inbound(kind: &PeerInboundKind) -> Vec<Line<'static>> {
    match kind {
        PeerInboundKind::Question { id, from, org, hop, hop_max, body } => render_inbound_block(
            "\u{25c0}",
            theme::RUST_ORANGE,
            "question",
            &[
                ("from", from.clone(), Some(theme::RUST_ORANGE), true),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("id", id.clone(), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("hop", format!("hop {hop}/{hop_max}"), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("org", format!("({org})"), Some(theme::DIM), false),
            ],
            body,
        ),
        PeerInboundKind::Message { id, from, org, body, .. } => render_inbound_block(
            "\u{25c0}",
            theme::SUBAGENT_TOKEN,
            "message",
            &[
                ("from", from.clone(), Some(theme::RUST_ORANGE), true),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("id", id.clone(), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("org", format!("({org})"), Some(theme::DIM), false),
            ],
            body,
        ),
        PeerInboundKind::Reply { id, from, org, body } => render_inbound_block(
            "\u{25c0}",
            Color::Green,
            "reply",
            &[
                ("from", from.clone(), Some(theme::RUST_ORANGE), true),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("id", id.clone(), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("org", format!("({org})"), Some(theme::DIM), false),
            ],
            body,
        ),
        PeerInboundKind::LateReply { id, from, org, body } => render_inbound_block(
            "\u{25c0}",
            Color::Green,
            "reply",
            &[
                ("from", from.clone(), Some(theme::RUST_ORANGE), true),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("id", id.clone(), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::STATUS_WARNING), false),
                ("late", "\u{231b} late".to_owned(), Some(theme::STATUS_WARNING), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("org", format!("({org})"), Some(theme::DIM), false),
            ],
            body,
        ),
        PeerInboundKind::CallerTimeout { id, target, org, body } => render_inbound_block(
            "\u{26a0}",
            theme::STATUS_ERROR,
            "timeout",
            &[
                ("to", format!("ask to {target}"), Some(theme::RUST_ORANGE), true),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("id", id.clone(), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("reason", "no reply after 30 min".to_owned(), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("org", format!("({org})"), Some(theme::DIM), false),
            ],
            body,
        ),
        PeerInboundKind::RecipientExpired { id, from, org, body } => render_inbound_block(
            "\u{23f1}",
            theme::STATUS_WARNING,
            "ask expired",
            &[
                ("from", from.clone(), Some(theme::RUST_ORANGE), true),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("id", id.clone(), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("note", "your reply will be tagged late".to_owned(), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("org", format!("({org})"), Some(theme::DIM), false),
            ],
            body,
        ),
        PeerInboundKind::DeliveryFailure { id, target, org, reason } => render_inbound_block(
            "\u{26a0}",
            theme::STATUS_ERROR,
            "failed to deliver",
            &[
                ("to", target.clone(), Some(theme::RUST_ORANGE), true),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("id", id.clone(), Some(theme::DIM), false),
                ("·", " · ".to_owned(), Some(theme::DIM), false),
                ("org", format!("({org})"), Some(theme::DIM), false),
            ],
            reason,
        ),
    }
}

/// Build the styled lines for an outbound peer block. Used by the
/// chat renderer for `mcp__forge__peers__*` tool_use cards instead
/// of the default tool-card rendering.
pub(crate) fn render_outbound(kind: &PeerOutboundKind) -> Vec<Line<'static>> {
    let (icon, accent, label, target, body, id) = match kind {
        PeerOutboundKind::Ask { target, body, correlation_id } => {
            ("\u{2192}", theme::RUST_ORANGE, "ask", target, body, correlation_id)
        }
        PeerOutboundKind::Tell { target, body, correlation_id } => {
            ("\u{2192}", theme::SUBAGENT_TOKEN, "tell", target, body, correlation_id)
        }
    };

    let mut header = Line::default();
    header.spans.push(Span::raw(BODY_INDENT));
    header.spans.push(Span::styled(icon.to_owned(), Style::default().fg(accent)));
    header.spans.push(Span::raw(" "));
    header
        .spans
        .push(Span::styled(label.to_owned(), Style::default().fg(accent).add_modifier(Modifier::BOLD)));
    header.spans.push(Span::styled(" · ".to_owned(), Style::default().fg(theme::DIM)));
    header.spans.push(Span::styled(
        target.clone(),
        Style::default().fg(theme::RUST_ORANGE).add_modifier(Modifier::BOLD),
    ));
    header.spans.push(Span::styled(" · ".to_owned(), Style::default().fg(theme::DIM)));
    let id_label = id.clone().unwrap_or_else(|| "q-…".to_owned());
    header.spans.push(Span::styled(id_label, Style::default().fg(theme::DIM)));

    let mut lines = vec![header];
    push_body_lines(&mut lines, body);
    lines
}

/// Internal renderer for inbound styled blocks. The mockup's pattern:
/// `<icon> <label> · <field1> · <field2> · …` on line 1, body
/// indented + dimmed on subsequent lines.
fn render_inbound_block(
    icon: &str,
    accent: Color,
    label: &str,
    fields: &[(&str, String, Option<Color>, bool)],
    body: &str,
) -> Vec<Line<'static>> {
    let mut header = Line::default();
    header.spans.push(Span::raw(BODY_INDENT));
    header.spans.push(Span::styled(icon.to_owned(), Style::default().fg(accent)));
    header.spans.push(Span::raw(" "));
    header.spans.push(Span::styled(
        label.to_owned(),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    ));
    header.spans.push(Span::styled(" · ".to_owned(), Style::default().fg(theme::DIM)));
    let mut first = true;
    for (_k, value, color, bold) in fields {
        if value.trim() == "·" {
            continue;
        }
        if !first && !value.starts_with('·') {
            // Caller already inserted explicit separator spans —
            // skip auto-separator insertion to keep things simple.
        }
        let mut style = Style::default();
        if let Some(c) = color {
            style = style.fg(*c);
        }
        if *bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        header.spans.push(Span::styled(value.clone(), style));
        first = false;
    }

    let mut lines = vec![header];
    push_body_lines(&mut lines, body);
    lines
}

/// Push the body lines (indented, muted). Truncates at
/// `BODY_TRUNCATE` chars with an ellipsis.
fn push_body_lines(lines: &mut Vec<Line<'static>>, body: &str) {
    let body = body.trim();
    if body.is_empty() {
        return;
    }
    let truncated: String = if body.chars().count() > BODY_TRUNCATE {
        let mut s: String = body.chars().take(BODY_TRUNCATE).collect();
        s.push('\u{2026}');
        s
    } else {
        body.to_owned()
    };

    // Push a blank for spacing then each body line. We respect any
    // existing newlines in the body so the recipient's LLM sees a
    // multi-line prompt as multi-line in the rendered block too.
    lines.push(Line::default());
    for raw_line in truncated.lines() {
        let mut line = Line::default();
        line.spans.push(Span::raw(BODY_INDENT));
        line.spans
            .push(Span::styled(raw_line.to_owned(), Style::default().fg(Color::Gray)));
        lines.push(line);
    }
}

// ---------- parsing helpers ----------

/// Split `s` at the first occurrence of `marker`. Returns
/// `(before, after_marker)` — the marker itself is consumed.
/// `None` when the marker isn't found.
fn take_until<'a>(s: &'a str, marker: &str) -> Option<(&'a str, &'a str)> {
    let idx = s.find(marker)?;
    Some((&s[..idx], &s[idx + marker.len()..]))
}

/// After the `id=` slice has been peeled off, look for the marker
/// that comes after the id (e.g. ` to agent ` or ` from agent `).
/// Returns the slice after the marker.
fn rest_after_id<'a>(after_id: &'a str, marker: &str) -> Option<&'a str> {
    let idx = after_id.find(marker)?;
    Some(&after_id[idx + marker.len()..])
}

/// After the `id=` slice has been peeled off, return the id portion
/// (everything before `marker`).
fn id_before<'a>(after_id: &'a str, marker: &str) -> Option<&'a str> {
    let idx = after_id.find(marker)?;
    Some(&after_id[..idx])
}

/// Given a slice starting at `'X' (org 'Y') ...`, extract `X` and
/// `Y`. Returns `None` when the format doesn't match.
fn extract_from_agent_after(rest: &str) -> Option<(String, String)> {
    let after_open = rest.strip_prefix('\'')?;
    let (name, after_name) = take_until(after_open, "' (org '")?;
    let (org, _trailing) = take_until(after_name, "')")?;
    Some((name.to_owned(), org.to_owned()))
}

/// Same as `extract_from_agent_after` but returns the trailer (what
/// comes after `')`) joined with the org via `|`. Used by the
/// delivery-failure parser which needs both fields.
fn extract_from_agent_after_with_trailer(rest: &str) -> Option<(String, String)> {
    let after_open = rest.strip_prefix('\'')?;
    let (name, after_name) = take_until(after_open, "' (org '")?;
    let (org, trailing) = take_until(after_name, "')")?;
    Some((name.to_owned(), format!("{org}|{trailing}")))
}

/// Parse `k/M` into `(k, M)`. Tolerates bad input.
fn parse_hop(s: &str) -> Option<(u8, u8)> {
    let (a, b) = take_until(s, "/")?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Pull a correlation id out of a tool result's text (the JSON the
/// peer tools emit). Returns `None` when the call hasn't completed
/// yet, or when the parse fails.
fn extract_correlation_id_from_result(tc: &ToolCallInfo) -> Option<String> {
    use crate::agent::model::{ContentBlock, ToolCallContent};
    for content in &tc.content {
        if let ToolCallContent::Content(chunk) = content
            && let ContentBlock::Text(text_content) = &chunk.content
        {
            let text = &text_content.text;
            // Body is pretty-printed JSON containing
            //   "correlation_id": "q-XXXXXXXX"
            // Find that field. Don't full-parse — tool result strings
            // can be noisy at the edges.
            if let Some(start) = text.find("\"correlation_id\": \"") {
                let rest = &text[start + "\"correlation_id\": \"".len()..];
                if let Some(end) = rest.find('"') {
                    return Some(rest[..end].to_owned());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_question_inbound() {
        let text = "[Question id=q-7f3a92e0 hop=1/10 from agent 'forge' (org 'Personal') - reply with tell_agent in_reply_to=q-7f3a92e0]\n\nWhat's the test setup look like?";
        let kind = detect_inbound(text).expect("question parsed");
        match kind {
            PeerInboundKind::Question { id, from, org, hop, hop_max, body } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(from, "forge");
                assert_eq!(org, "Personal");
                assert_eq!(hop, 1);
                assert_eq!(hop_max, 10);
                assert_eq!(body, "What's the test setup look like?");
            }
            other => panic!("expected Question, got {other:?}"),
        }
    }

    #[test]
    fn detect_message_inbound() {
        let text = "[Message id=t-c45a8f12 hop=1/10 from agent 'forge' (org 'Personal')]\n\nFYI I just pushed the rewriter cleanup.";
        let kind = detect_inbound(text).expect("message parsed");
        match kind {
            PeerInboundKind::Message { id, from, org, body, .. } => {
                assert_eq!(id, "t-c45a8f12");
                assert_eq!(from, "forge");
                assert_eq!(org, "Personal");
                assert_eq!(body, "FYI I just pushed the rewriter cleanup.");
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }

    #[test]
    fn detect_reply_inbound() {
        let text = "[Reply id=q-7f3a92e0 from agent 'granite-backend' (org 'Granite') to your earlier ask]\n\nWe use pgtemp.";
        let kind = detect_inbound(text).expect("reply parsed");
        match kind {
            PeerInboundKind::Reply { id, from, org, body } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(from, "granite-backend");
                assert_eq!(org, "Granite");
                assert_eq!(body, "We use pgtemp.");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn detect_late_reply_inbound() {
        let text = "[Late reply id=q-7f3a92e0 from agent 'granite-backend' (org 'Granite') - your ask expired before this reply was sent]\n\nSorry for the delay.";
        let kind = detect_inbound(text).expect("late reply parsed");
        assert!(matches!(kind, PeerInboundKind::LateReply { .. }));
    }

    #[test]
    fn detect_caller_timeout_inbound() {
        let text = "[Ask id=q-7f3a92e0 to agent 'granite-backend' (org 'Granite') timed out after 30 minutes - no reply received. Any reply after this point will be tagged late.]";
        let kind = detect_inbound(text).expect("caller timeout parsed");
        match kind {
            PeerInboundKind::CallerTimeout { id, target, org, .. } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(target, "granite-backend");
                assert_eq!(org, "Granite");
            }
            other => panic!("expected CallerTimeout, got {other:?}"),
        }
    }

    #[test]
    fn detect_recipient_expired_inbound() {
        let text = "[Ask id=q-7f3a92e0 from agent 'forge' (org 'Personal') has expired - any reply you produce will be tagged late.]";
        let kind = detect_inbound(text).expect("recipient expired parsed");
        match kind {
            PeerInboundKind::RecipientExpired { id, from, org, .. } => {
                assert_eq!(id, "q-7f3a92e0");
                assert_eq!(from, "forge");
                assert_eq!(org, "Personal");
            }
            other => panic!("expected RecipientExpired, got {other:?}"),
        }
    }

    #[test]
    fn detect_delivery_failure_inbound() {
        let text = "[Ask id=q-d31fa8a3 to agent 'granite-liq-bot' (org 'Granite') failed to deliver: target spawn failed: all pinned accounts are rate-limited]";
        let kind = detect_inbound(text).expect("delivery failure parsed");
        match kind {
            PeerInboundKind::DeliveryFailure { id, target, reason, .. } => {
                assert_eq!(id, "q-d31fa8a3");
                assert_eq!(target, "granite-liq-bot");
                assert!(reason.contains("rate-limited"), "got: {reason}");
            }
            other => panic!("expected DeliveryFailure, got {other:?}"),
        }
    }

    #[test]
    fn detect_inbound_rejects_non_peer_text() {
        assert!(detect_inbound("Hello, world!").is_none());
        assert!(detect_inbound("[Not a peer wrapper]").is_none());
        assert!(detect_inbound("[Question with no id]\n\nbody").is_none());
    }

    #[test]
    fn render_inbound_question_produces_styled_lines() {
        let kind = PeerInboundKind::Question {
            id: "q-7f3a92e0".to_owned(),
            from: "forge".to_owned(),
            org: "Personal".to_owned(),
            hop: 1,
            hop_max: 10,
            body: "What's the setup?".to_owned(),
        };
        let lines = render_inbound(&kind);
        assert!(!lines.is_empty(), "non-empty");
        // First line is the header with the question icon + label.
        let header_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("question"), "header has label: {header_text}");
        assert!(header_text.contains("forge"), "header has sender: {header_text}");
        assert!(header_text.contains("q-7f3a92e0"), "header has id: {header_text}");
    }

    #[test]
    fn render_outbound_ask_block() {
        let kind = PeerOutboundKind::Ask {
            target: "granite-backend".to_owned(),
            body: "What's the test setup?".to_owned(),
            correlation_id: Some("q-7f3a92e0".to_owned()),
        };
        let lines = render_outbound(&kind);
        assert!(!lines.is_empty());
        let header_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("ask"), "header has label: {header_text}");
        assert!(header_text.contains("granite-backend"), "header has target: {header_text}");
        assert!(header_text.contains("q-7f3a92e0"), "header has id: {header_text}");
    }

    #[test]
    fn render_outbound_with_pending_correlation_id() {
        let kind = PeerOutboundKind::Tell {
            target: "forge".to_owned(),
            body: "fyi".to_owned(),
            correlation_id: None,
        };
        let lines = render_outbound(&kind);
        let header_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(header_text.contains("tell"));
        // Pending id placeholder.
        assert!(header_text.contains("q-…"));
    }

    #[test]
    fn body_truncates_at_threshold() {
        let body = "x".repeat(BODY_TRUNCATE + 50);
        let kind = PeerInboundKind::Reply {
            id: "q-1".to_owned(),
            from: "a".to_owned(),
            org: "o".to_owned(),
            body,
        };
        let lines = render_inbound(&kind);
        // Concat all body lines (skip header + blank).
        let body_text: String = lines
            .iter()
            .skip(2)
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            body_text.contains('\u{2026}'),
            "truncation marker should appear in long body: {body_text:?}"
        );
    }
}

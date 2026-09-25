//! Outbound peer coordination: one `mcp__forge__agents__ask` or
//! `agents__tell` tool_use card, parsed into the seat it addresses and the
//! body it carries.
//!
//! The inbound half of the same protocol is [`crate::envelope`], which parses
//! the bracket-wrapped prose the workspace injects into user-turn text. This
//! half parses a tool call instead, which is why it is a separate module
//! rather than another arm there.
//!
//! Pure - no I/O, no state, no rendering. The kind is resolved fresh on each
//! call and not cached (the arguments are small, and render frames don't call
//! this hot enough to need a cache).

/// One outbound agent block parsed from a `mcp__forge__agents__ask` or
/// `mcp__forge__agents__tell` tool_use card. Both render with the same
/// `▶ Verb name` shape; `target` is the addressed seat.
#[derive(Debug)]
pub enum PeerOutboundKind {
    Ask { target: String, body: String },
    Tell { target: String, body: String },
}

/// Detect an agent outbound tool_use card. Returns `None` for every
/// other tool - the chat renderer falls through to the default
/// tool-card rendering - including `agents__spawn`, `agents__despawn`,
/// `agents__update`, `agents__capacity` and `agents__list`: those are
/// lifecycle and roster calls that render as standard tool cards
/// rather than as agent comms.
pub fn detect_outbound(tc: &crate::model::ToolCallInfo) -> Option<PeerOutboundKind> {
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

#[cfg(test)]
mod tests {
    use super::{PeerOutboundKind, detect_outbound};

    fn make_tc(sdk_tool_name: &str, raw_input: serde_json::Value) -> crate::model::ToolCallInfo {
        crate::model::ToolCallInfo {
            id: "tc-1".into(),
            title: "tc-1".into(),
            sdk_tool_name: sdk_tool_name.into(),
            raw_input: Some(raw_input),
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: crate::model::agent::ToolCallStatus::InProgress,
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
    fn detect_outbound_ignores_other_tools() {
        let tc = make_tc("Bash", serde_json::json!({ "command": "ls" }));
        assert!(detect_outbound(&tc).is_none());
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
}

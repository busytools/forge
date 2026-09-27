//! The conversation fold: wire messages in, the units a view draws out.
//!
//! The TUI partitions one message's blocks at a time, and a run of tool
//! calls spans messages - a call and its result are two of them - so this
//! walks the whole conversation and folds it in one pass. The rules it
//! folds by are not restated here: the run rule
//! ([`crate::grouping::is_run_breaker_tool`]), the status a run
//! summarises under ([`crate::grouping::aggregate_call_status`]), the
//! family table ([`crate::family`]) and the peer parsers
//! ([`crate::envelope`], [`crate::peer_outbound`]) are the ones the TUI
//! already groups by.

use std::collections::HashMap;

use forge_primitives::{ContentBlock, Message};

use crate::envelope::{PeerInboundKind, detect_inbound};
use crate::family::{ToolFamily, tool_family, tool_label};
use crate::grouping::{aggregate_call_status, is_run_breaker_tool};
use crate::model::ToolCallStatus;
use crate::peer_outbound::{PeerOutboundKind, detect_outbound_call};

/// One thing a view draws, in the order the conversation produced it.
#[derive(Debug, Clone)]
pub enum ChatUnit {
    /// A turn the user wrote.
    UserTurn { text: String },
    /// Prose the assistant wrote.
    AssistantText { text: String },
    /// One tool call drawn on its own, because it does not fold into a
    /// run: a mutation, a lifecycle block, or a question waiting on a
    /// person.
    ToolCall(ToolLeaf),
    /// A maximal run of consecutive tool calls, drawn as one group.
    ToolGroup {
        /// The families the run met, in first-appearance order, each with
        /// the calls under it.
        families: Vec<FamilyLeaves>,
        /// What the run's header reports.
        status: ToolCallStatus,
    },
    /// A peer message the conversation holds: one it sent, or one it
    /// received.
    PeerCard(PeerCard),
}

/// One family's calls inside a group.
#[derive(Debug, Clone)]
pub struct FamilyLeaves {
    pub family: ToolFamily,
    pub calls: Vec<ToolLeaf>,
}

/// One call inside a group: what its own row shows.
#[derive(Debug, Clone)]
pub struct ToolLeaf {
    /// The `tool_use` id the wire gave it.
    pub id: String,
    /// The tool's own label, for a row that names the tool rather than
    /// its family.
    pub label: &'static str,
    /// The tool's title: the file, command or query it names.
    pub title: String,
    pub status: ToolCallStatus,
}

/// A peer message, as the envelope it arrived in or the call that sent
/// it.
#[derive(Debug, Clone)]
pub struct PeerCard {
    /// The seat it went to, or came from.
    pub peer: String,
    pub body: String,
    /// True when it arrived rather than was sent.
    pub inbound: bool,
}

/// Fold a conversation into the units a view draws.
pub fn render_units(messages: &[Message]) -> Vec<ChatUnit> {
    let results = result_statuses(messages);
    let mut units: Vec<ChatUnit> = Vec::new();
    let mut run: Vec<(ToolFamily, ToolLeaf)> = Vec::new();
    for message in messages {
        let (assistant, content) = match message {
            Message::Assistant { message: envelope, .. } => (true, envelope.content.as_slice()),
            Message::User { message: envelope, .. } => (false, envelope.content.as_slice()),
            _ => continue,
        };
        for block in content {
            match block {
                ContentBlock::Text { text } => {
                    flush(&mut run, &mut units);
                    units.push(text_unit(assistant, text));
                }
                ContentBlock::QueuedCommand { prompt, .. } => {
                    flush(&mut run, &mut units);
                    units.push(ChatUnit::UserTurn { text: queued_text(prompt) });
                }
                ContentBlock::ToolUse { id, name, input }
                | ContentBlock::ServerToolUse { id, name, input } => {
                    push_call(id, name, input, &results, &mut run, &mut units);
                }
                // A result is not a unit of its own: it is what the call
                // it answers already carries.
                _ => {}
            }
        }
    }
    flush(&mut run, &mut units);
    units
}

/// Add one call to the conversation: a peer card, a run-breaker drawn on
/// its own, or a member of the run being built.
fn push_call(
    id: &str,
    name: &str,
    input: &serde_json::Value,
    results: &HashMap<String, ToolCallStatus>,
    run: &mut Vec<(ToolFamily, ToolLeaf)>,
    units: &mut Vec<ChatUnit>,
) {
    if let Some(card) = outbound_card(name, input) {
        flush(run, units);
        units.push(ChatUnit::PeerCard(card));
    } else if is_run_breaker_tool(name, Some(input)) {
        flush(run, units);
        units.push(ChatUnit::ToolCall(leaf(id, name, input, results)));
    } else {
        run.push((tool_family(name), leaf(id, name, input, results)));
    }
}

/// The text a queued prompt carries. The wire shape is a string for a
/// typed prompt and a content-block array for a multi-modal one, where
/// only the text blocks are the words the user typed.
fn queued_text(prompt: &serde_json::Value) -> String {
    if let Some(text) = prompt.as_str() {
        return text.to_owned();
    }
    let Some(blocks) = prompt.as_array() else {
        return serde_json::to_string(prompt).unwrap_or_else(|_| String::from("[unrenderable]"));
    };
    blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => {
                block.get("text").and_then(serde_json::Value::as_str).map(str::to_owned)
            }
            Some(other) => Some(format!("[{other}]")),
            None => None,
        })
        .collect::<Vec<String>>()
        .join("\n")
}

/// One text block: the user's own turn, or the assistant's - unless it is
/// a peer envelope, which arrives as the user turn's prose.
fn text_unit(assistant: bool, text: &str) -> ChatUnit {
    if !assistant && let Some(card) = inbound_card(text) {
        return ChatUnit::PeerCard(card);
    }
    let text = text.to_owned();
    if assistant { ChatUnit::AssistantText { text } } else { ChatUnit::UserTurn { text } }
}

/// The peer envelope a user turn carries, if it is one. The notice-shaped
/// kinds - a delivery failure, a failed worker spawn - are not peer comms
/// and stay turns.
fn inbound_card(text: &str) -> Option<PeerCard> {
    match detect_inbound(text)? {
        PeerInboundKind::Question { from, body, .. }
        | PeerInboundKind::Message { from, body, .. }
        | PeerInboundKind::Reply { from, body, .. } => {
            Some(PeerCard { peer: from, body, inbound: true })
        }
        _ => None,
    }
}

/// The card an outbound peer call draws, if it is one.
fn outbound_card(name: &str, input: &serde_json::Value) -> Option<PeerCard> {
    let (peer, body) = match detect_outbound_call(name, input)? {
        PeerOutboundKind::Ask { target, body } | PeerOutboundKind::Tell { target, body } => {
            (target, body)
        }
    };
    Some(PeerCard { peer, body, inbound: false })
}

/// One call as a transcript holds it: the wire's name and input, the
/// title the shared call builder resolves, and the status its result
/// recorded. A call with no result yet reads as `Pending`, which is what
/// the resume path hands the TUI for the same file.
fn leaf(
    id: &str,
    name: &str,
    input: &serde_json::Value,
    results: &HashMap<String, ToolCallStatus>,
) -> ToolLeaf {
    ToolLeaf {
        id: id.to_owned(),
        label: tool_label(name),
        title: forge_workspace::tooling::create_tool_call(id, name, input, None).title,
        status: results.get(id).copied().unwrap_or(ToolCallStatus::Pending),
    }
}

/// Every tool result the conversation holds, by the call it answers.
fn result_statuses(messages: &[Message]) -> HashMap<String, ToolCallStatus> {
    let mut out = HashMap::new();
    for message in messages {
        let Message::User { message: envelope, .. } = message else {
            continue;
        };
        for block in &envelope.content {
            if let ContentBlock::ToolResult { tool_use_id, is_error, .. } = block {
                let status =
                    if *is_error { ToolCallStatus::Failed } else { ToolCallStatus::Completed };
                out.insert(tool_use_id.clone(), status);
            }
        }
    }
    out
}

/// Close the run being built, if it has one, as one group: the families
/// it met in first-appearance order, and the status its calls summarise
/// under.
fn flush(run: &mut Vec<(ToolFamily, ToolLeaf)>, units: &mut Vec<ChatUnit>) {
    if run.is_empty() {
        return;
    }
    let calls = std::mem::take(run);
    let status = aggregate_call_status(calls.iter().map(|(_, leaf)| leaf.status));
    let mut families: Vec<FamilyLeaves> = Vec::new();
    for (family, leaf) in calls {
        match families.iter_mut().find(|row| row.family == family) {
            Some(row) => row.calls.push(leaf),
            None => families.push(FamilyLeaves { family, calls: vec![leaf] }),
        }
    }
    units.push(ChatUnit::ToolGroup { families, status });
}

#[cfg(test)]
mod tests {
    use forge_primitives::{AssistantEnvelope, ContentBlock, Message, UserEnvelope};

    use crate::model::ToolCallStatus;

    use super::{ChatUnit, render_units};

    /// An assistant message carrying `content`.
    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message::Assistant {
            message: AssistantEnvelope {
                id: "msg_01".to_owned(),
                role: "assistant".to_owned(),
                model: "claude-opus-4-5".to_owned(),
                content,
                stop_reason: None,
                stop_sequence: None,
                usage: None,
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            error: None,
            uuid: None,
        }
    }

    /// One assistant message carrying a single tool call. `family` is the
    /// word these tests speak in, mapped to the SDK name it stands for,
    /// and `n` numbers the id so two calls in one run never share one.
    fn tool_call_at(family: &str, n: usize) -> Message {
        let sdk_name = match family {
            "read" => "Read",
            "search" => "Grep",
            "bash" => "Bash",
            other => other,
        };
        assistant(vec![ContentBlock::ToolUse {
            id: format!("toolu_{family}_{n}"),
            name: sdk_name.to_owned(),
            input: serde_json::json!({"file_path": "src/lib.rs", "command": "just check"}),
        }])
    }

    fn tool_call(family: &str) -> Message {
        tool_call_at(family, 0)
    }

    fn tool_call_messages(families: &[&str]) -> Vec<Message> {
        families.iter().enumerate().map(|(n, family)| tool_call_at(family, n)).collect()
    }

    fn assistant_text(text: &str) -> Message {
        assistant(vec![ContentBlock::Text { text: text.to_owned() }])
    }

    /// Consecutive tool calls with nothing between them are ONE group.
    /// This is the rule the mockup was built on and the one a reader will
    /// get wrong first.
    #[test]
    fn consecutive_tool_calls_fold_into_one_group() {
        let messages = tool_call_messages(&["read", "search", "read"]);
        let units = render_units(&messages);
        assert_eq!(units.len(), 1, "three consecutive calls are one unit");
        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        let named: Vec<&'static str> = families.iter().map(|f| f.family.kind_label()).collect();
        assert_eq!(named, ["read", "search"], "read and search, in first-appearance order");
        assert_eq!(families[0].calls.len(), 2, "both reads sit under the row they belong to");
        assert_eq!(families[1].calls.len(), 1, "and the search under its own");
    }

    /// A non-tool block ends the run: the next call starts a NEW group.
    #[test]
    fn a_non_tool_block_breaks_the_run() {
        let messages = [tool_call("read"), assistant_text("here it is"), tool_call("bash")];
        let units = render_units(&messages);
        assert_eq!(units.len(), 3, "the prose splits the run in two");
    }

    /// A user turn carrying `content`.
    fn user(content: Vec<ContentBlock>) -> Message {
        Message::User {
            message: UserEnvelope { role: "user".to_owned(), content },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
        }
    }

    /// The user's mid-turn queued prompt, as the scan hoists it out of the
    /// transcript's attachment row.
    fn queued_prompt(text: &str) -> Message {
        user(vec![ContentBlock::QueuedCommand {
            prompt: serde_json::Value::String(text.to_owned()),
            command_mode: Some("prompt".to_owned()),
            source_uuid: None,
        }])
    }

    /// The result the tool at `tool_use_id` came back with.
    fn tool_result(tool_use_id: &str, is_error: bool) -> Message {
        user(vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.to_owned(),
            content: serde_json::Value::String("output".to_owned()),
            is_error,
        }])
    }

    /// A user's mid-turn prompt reached the page. On the read path it has
    /// no other shape: the scan hoists the queued row into a user envelope
    /// so a reader can rebuild the bubble, and a fold that ignores the
    /// block drops the question the assistant went on to answer.
    #[test]
    fn a_queued_prompt_is_a_user_turn() {
        let messages = [queued_prompt("and keep the multiSelect case too")];
        let units = render_units(&messages);
        assert_eq!(units.len(), 1, "the queued prompt is one unit");
        let ChatUnit::UserTurn { text } = &units[0] else {
            panic!("a user turn");
        };
        assert_eq!(text, "and keep the multiSelect case too", "carrying what the user typed");
    }

    /// Every call carries its own status, and the group summarises the
    /// run, so a failed call must not paint as a clean one.
    #[test]
    fn a_result_sets_the_status_of_the_call_it_answers() {
        let messages = [
            tool_call_at("read", 0),
            tool_result("toolu_read_0", true),
            tool_call_at("read", 1),
            tool_result("toolu_read_1", false),
        ];
        let units = render_units(&messages);
        let ChatUnit::ToolGroup { families, status } = &units[0] else {
            panic!("a tool group");
        };
        let statuses: Vec<ToolCallStatus> =
            families[0].calls.iter().map(|call| call.status).collect();
        assert_eq!(
            statuses,
            [ToolCallStatus::Failed, ToolCallStatus::Completed],
            "each call reads the status its own result recorded",
        );
        assert_eq!(*status, ToolCallStatus::Failed, "and the run reports the failure it holds");
    }

    /// Peer traffic is a card on both sides: an envelope that arrived as
    /// the user turn's prose, and a call the session made. Folded as a
    /// turn or a tool call, the page would show the protocol instead of
    /// the message.
    #[test]
    fn peer_traffic_is_a_card_and_not_a_turn_or_a_call() {
        let arrived = user(vec![ContentBlock::Text {
            text: "[Message id=t-1 from agent 'companies' (org 'Busytools')]\n\nis the cron issue filed?"
                .to_owned(),
        }]);
        let sent = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_ask".to_owned(),
            name: "mcp__forge__agents__ask".to_owned(),
            input: serde_json::json!({"project": "forge", "prompt": "did it land?"}),
        }]);

        let units = render_units(&[arrived, sent]);

        let ChatUnit::PeerCard(inbound) = &units[0] else {
            panic!("a peer card");
        };
        assert!(inbound.inbound, "the envelope that arrived reads as inbound");
        assert_eq!(inbound.peer, "companies", "and names who sent it");
        assert_eq!(inbound.body, "is the cron issue filed?", "with what it said");
        let ChatUnit::PeerCard(outbound) = &units[1] else {
            panic!("a peer card");
        };
        assert!(!outbound.inbound, "the call the session made reads as outbound");
        assert_eq!(outbound.peer, "forge", "and names the seat it went to");
        assert_eq!(outbound.body, "did it land?", "with what it asked");
    }
}

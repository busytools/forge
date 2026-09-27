//! The conversation fold: wire messages in, the units a view draws out.
//!
//! The TUI partitions one message's blocks at a time, and a run of tool
//! calls spans messages - a call and its result are two of them - so this
//! walks the whole conversation and folds it in one pass. The rules it
//! folds by are not restated here: the row a call summarises under (the
//! wire-level row policy in `grouping`), the status a run summarises under
//! ([`crate::grouping::aggregate_call_status`]) and the peer parsers
//! ([`crate::envelope`], [`crate::peer_outbound`]) are the ones the TUI
//! already groups by.
//!
//! Where it differs from the TUI it is because the mockup draws something
//! else: a mutation folds as an `edit` family instead of breaking the run,
//! and an envelope that is not agent traffic is a notice instead of a turn.

use std::collections::HashMap;

use forge_primitives::{ContentBlock, Message};

use crate::envelope::{PeerInboundKind, detect_inbound};
use crate::family::tool_label;
use crate::grouping::{
    KindRow, aggregate_call_status, is_peer_block_render_tool, renders_as_lifecycle_block_parts,
    wire_row,
};
use crate::model::ToolCallStatus;
use crate::model::tool_call_info::is_ask_question_tool_name;
use crate::peer_outbound::{PeerOutboundKind, detect_outbound_call};

/// One thing a view draws, in the order the conversation produced it.
#[derive(Debug, Clone)]
pub enum ChatUnit {
    /// A turn the user wrote.
    UserTurn { text: String },
    /// Prose the assistant wrote.
    AssistantText { text: String },
    /// One tool call drawn on its own, because it does not fold into a
    /// run: a peer block, a lifecycle row, or a question waiting on a
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
    /// A line the conversation carries that nobody typed: an external
    /// delivery, a scheduled fire, or a failure the workspace reported.
    Notice(Notice),
}

/// One family's calls inside a group.
#[derive(Debug, Clone)]
pub struct FamilyLeaves {
    /// The class the row belongs to, which is what a view picks its glyph
    /// from. A label alone cannot tell a server named `read` from the read
    /// family.
    pub row: KindRow,
    /// The word the row draws: a family word, an MCP server's own name, or
    /// `edit` for a mutation.
    pub label: String,
    pub calls: Vec<ToolLeaf>,
}

/// How loudly a notice reads: the mockup's three rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Info,
    Warning,
    Error,
}

/// A notice, as the envelope it arrived in.
#[derive(Debug, Clone)]
pub struct Notice {
    pub severity: NoticeSeverity,
    /// Where it came from, for a renderer that gives each source its own
    /// chrome: `gotify`, `cron`, `slack`, `peer` or `worker`.
    pub source: &'static str,
    pub text: String,
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
    let mut run: Vec<((KindRow, String), ToolLeaf)> = Vec::new();
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
                    units.push(ChatUnit::UserTurn { text: queued_command_text(prompt) });
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
    run: &mut Vec<((KindRow, String), ToolLeaf)>,
    units: &mut Vec<ChatUnit>,
) {
    if let Some(card) = outbound_card(name, input) {
        flush(run, units);
        units.push(ChatUnit::PeerCard(card));
    } else if is_standalone_call(name, Some(input)) {
        flush(run, units);
        units.push(ChatUnit::ToolCall(leaf(id, name, input, results)));
    } else {
        run.push((wire_row(name), leaf(id, name, input, results)));
    }
}

/// True when the fold draws a call on its own instead of folding it into a
/// run: a peer block, a question waiting on a person, or a Monitor's
/// lifecycle row.
///
/// This is the fold's own predicate rather than `grouping::is_run_breaker_tool`,
/// which is the TUI's: that one also breaks on a mutation, because the TUI
/// opens a diff on its own, while the mockup draws an `edit` family inside
/// the run with its leaves open.
fn is_standalone_call(sdk_tool_name: &str, input: Option<&serde_json::Value>) -> bool {
    is_ask_question_tool_name(sdk_tool_name)
        || is_peer_block_render_tool(sdk_tool_name)
        || renders_as_lifecycle_block_parts(sdk_tool_name, input)
}

/// The text a `queued_command` block carries: a plain string for a typed
/// prompt, or a content-block array for a multi-modal one, where only the
/// text blocks are the words the user typed and every other block renders
/// as a `[type]` placeholder so the reader sees something rather than a
/// blank.
pub fn queued_command_text(prompt: &serde_json::Value) -> String {
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
/// an envelope, which arrives as the user turn's prose.
fn text_unit(assistant: bool, text: &str) -> ChatUnit {
    if !assistant && let Some(unit) = inbound_unit(text) {
        return unit;
    }
    let text = text.to_owned();
    if assistant { ChatUnit::AssistantText { text } } else { ChatUnit::UserTurn { text } }
}

/// The unit an envelope carries. A peer comms envelope is a card; every
/// other kind the workspace injects is a notice - an external delivery, a
/// scheduled fire, or a failure - and rendering one as the user's turn
/// would put a protocol header inside a bubble nobody typed. The match is
/// exhaustive on purpose: a new envelope kind is a compile error here, not
/// a silent turn.
fn inbound_unit(text: &str) -> Option<ChatUnit> {
    match detect_inbound(text)? {
        PeerInboundKind::Question { from, body, .. }
        | PeerInboundKind::Message { from, body, .. }
        | PeerInboundKind::Reply { from, body, .. } => {
            Some(ChatUnit::PeerCard(PeerCard { peer: from, body, inbound: true }))
        }
        PeerInboundKind::Gotify { app, title, message, priority } => {
            let severity = if priority >= GOTIFY_ELEVATED_PRIORITY {
                NoticeSeverity::Warning
            } else {
                NoticeSeverity::Info
            };
            Some(notice(
                severity,
                "gotify",
                &format!("app '{app}' \u{b7} priority {priority}: {title}\n{message}"),
            ))
        }
        PeerInboundKind::Cron { prompt } => Some(notice(NoticeSeverity::Info, "cron", &prompt)),
        PeerInboundKind::Slack { workspace, channel, author, body } => {
            let head = match author {
                Some(author) => format!("{workspace} \u{b7} {channel} \u{b7} {author}"),
                None => format!("{workspace} \u{b7} {channel}"),
            };
            Some(notice(NoticeSeverity::Info, "slack", &format!("{head}: {body}")))
        }
        PeerInboundKind::DeliveryFailure { target, org, reason } => Some(notice(
            NoticeSeverity::Warning,
            "peer",
            &format!("'{target}' ({org}) failed to deliver: {reason}"),
        )),
        PeerInboundKind::WorkerSpawnFailed { label, reason } => Some(notice(
            NoticeSeverity::Warning,
            "worker",
            &format!("'{label}' failed to spawn: {reason}"),
        )),
    }
}

/// A Gotify delivery at or above this priority reads as a warning rather
/// than as information. The same number is the TUI's cue for the same row,
/// where it is private: a divergence here shows up as the two views giving
/// one delivery different severities.
const GOTIFY_ELEVATED_PRIORITY: u8 = 5;

/// A notice, with its text trimmed: a Gotify envelope's message is often
/// empty and would otherwise leave a bare newline under the title.
fn notice(severity: NoticeSeverity, source: &'static str, text: &str) -> ChatUnit {
    ChatUnit::Notice(Notice { severity, source, text: text.trim_end().to_owned() })
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

/// Close the run being built, if it has one, as one group: the rows it met
/// in first-appearance order, and the status its calls summarise under.
fn flush(run: &mut Vec<((KindRow, String), ToolLeaf)>, units: &mut Vec<ChatUnit>) {
    if run.is_empty() {
        return;
    }
    let calls = std::mem::take(run);
    let status = aggregate_call_status(calls.iter().map(|(_, leaf)| leaf.status));
    let mut families: Vec<FamilyLeaves> = Vec::new();
    for ((row, label), leaf) in calls {
        match families.iter_mut().find(|family| family.row == row && family.label == label) {
            Some(family) => family.calls.push(leaf),
            None => families.push(FamilyLeaves { row, label, calls: vec![leaf] }),
        }
    }
    units.push(ChatUnit::ToolGroup { families, status });
}

#[cfg(test)]
mod tests {
    use forge_primitives::{AssistantEnvelope, ContentBlock, Message, UserEnvelope};

    use crate::family::ToolFamily;
    use crate::grouping::KindRow;
    use crate::model::ToolCallStatus;

    use super::{ChatUnit, NoticeSeverity, render_units};

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
            "edit" => "Edit",
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

    /// An assistant message carrying a call by its exact SDK name, for the
    /// names a family word does not cover.
    fn tool_call_named(name: &str) -> Message {
        assistant(vec![ContentBlock::ToolUse {
            id: format!("toolu_{name}"),
            name: name.to_owned(),
            input: serde_json::json!({"file_path": "src/lib.rs"}),
        }])
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
        let named: Vec<&str> = families.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(named, ["read", "search"], "read and search, in first-appearance order");
        assert_eq!(families[0].calls.len(), 2, "both reads sit under the row they belong to");
        assert_eq!(families[1].calls.len(), 1, "and the search under its own");
    }

    /// An edit folds into the run as a family of its own. The TUI breaks a
    /// run on a mutation because it draws the diff open on its own; the
    /// mockup draws an `edit` family inside the group with its leaves open,
    /// so this fold cannot borrow the TUI's rule, and the row's word is
    /// `edit` rather than the tool's own `Edit`.
    #[test]
    fn an_edit_folds_into_the_run_as_its_own_family() {
        let messages = tool_call_messages(&["read", "edit", "read"]);
        let units = render_units(&messages);
        assert_eq!(units.len(), 1, "the edit does not break the run");
        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        let named: Vec<&str> = families.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(named, ["read", "edit"], "with the mockup's own word for a mutation");
        assert_eq!(families[1].calls.len(), 1, "and the edit under that row");
        assert_eq!(
            families[0].row,
            KindRow::Family(ToolFamily::Read),
            "each row carries the class a view picks its glyph from",
        );
        assert_eq!(
            families[1].row,
            KindRow::Family(ToolFamily::Own("Edit")),
            "so the edit row reads as a class of its own, not as the generic tool row",
        );
    }

    /// The whole standalone-call predicate, pinned from both sides: the
    /// three classes the fold draws on their own - a Monitor's lifecycle
    /// row, a question waiting on a person, a peer block - and the mutation
    /// it now folds. Re-borrowing the TUI's predicate, or dropping an arm,
    /// fails here rather than in a rendered page.
    #[test]
    fn only_the_calls_the_mockup_draws_alone_break_the_run() {
        let monitor = assistant(vec![ContentBlock::ToolUse {
            id: "toolu_monitor".to_owned(),
            name: "Monitor".to_owned(),
            input: serde_json::json!({"description": "watch the deploy", "command": "tail -f log"}),
        }]);
        let question = tool_call_named("AskUserQuestion");
        let peer = tool_call_named("mcp__forge__agents__tell");
        let edit = tool_call("edit");
        let read = tool_call_at("read", 9);

        // A breaker on either side of a run: three units, the middle one
        // drawn alone.
        for breaker in [monitor, question, peer] {
            let units = render_units(&[read.clone(), breaker, read.clone()]);
            assert_eq!(units.len(), 3, "the run splits around a call drawn on its own");
            assert!(
                matches!(&units[1], ChatUnit::ToolCall(_) | ChatUnit::PeerCard(_)),
                "and that call is the unit in the middle",
            );
        }

        // The mutation does not break it: one group, with the edit inside.
        let units = render_units(&[read.clone(), edit, read]);
        assert_eq!(units.len(), 1, "a mutation folds into the run instead");
    }

    /// Two MCP servers are two rows, not one `tool` row. The mockup draws
    /// each server as its own family, and a server's name is only known at
    /// runtime, which is why the row carries a label rather than a family.
    #[test]
    fn each_mcp_server_is_its_own_family_row() {
        let messages = [
            tool_call_named("mcp__playwright__browser_click"),
            tool_call_named("mcp__forge__agents__list"),
        ];
        let units = render_units(&messages);
        let ChatUnit::ToolGroup { families, .. } = &units[0] else {
            panic!("a tool group");
        };
        let named: Vec<&str> = families.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(named, ["playwright", "forge"], "one row per server, in first-appearance order");
        let classes: Vec<KindRow> = families.iter().map(|row| row.row).collect();
        assert_eq!(
            classes,
            [KindRow::Mcp, KindRow::Mcp],
            "and each says it is a server rather than a family, so a view tells them from a \
             family row that happens to share the word",
        );
    }

    /// An envelope that is not agent traffic is not the user's own turn: a
    /// Gotify delivery, a cron fire, a Slack bundle, a failed delivery and a
    /// failed worker spawn each arrive as a notice, which is what the mockup's
    /// notice rows are for. Folded as a turn, the page shows a protocol
    /// header inside a bubble the user never typed.
    #[test]
    fn an_external_envelope_is_a_notice_and_not_a_turn() {
        let quiet = user(vec![ContentBlock::Text {
            text: "[Gotify - app 'watcher', priority 3]\n\ndeploy finished".to_owned(),
        }]);
        let loud = user(vec![ContentBlock::Text {
            text: "[Gotify - app 'ci', priority 9]\n\nbuild failed".to_owned(),
        }]);
        let slack = user(vec![ContentBlock::Text {
            text: "[Slack - workspace 'Busytools', #forge] id ts\n\nsteward: the gate is green\n"
                .to_owned(),
        }]);
        let failed = user(vec![ContentBlock::Text {
            text: "[Ask id=q-1 to agent 'companies' (org 'Busytools') failed to deliver: channel closed]"
                .to_owned(),
        }]);

        let units = render_units(&[quiet, loud, slack, failed]);

        let ChatUnit::Notice(quiet) = &units[0] else {
            panic!("a notice");
        };
        assert_eq!(quiet.severity, NoticeSeverity::Info, "a quiet delivery is information");
        assert_eq!(quiet.source, "gotify", "and says where it came from");
        assert!(quiet.text.contains("deploy finished"), "carrying its own words: {}", quiet.text);
        assert!(
            quiet.text.contains("priority 3"),
            "and the priority it arrived at: {}",
            quiet.text
        );

        let ChatUnit::Notice(loud) = &units[1] else {
            panic!("a notice");
        };
        assert_eq!(loud.severity, NoticeSeverity::Warning, "an elevated priority is a warning");

        let ChatUnit::Notice(slack) = &units[2] else {
            panic!("a notice");
        };
        assert_eq!(slack.source, "slack", "a Slack bundle says so");
        assert!(
            slack.text.contains("Busytools") && slack.text.contains("steward"),
            "and keeps the workspace and author its header draws: {}",
            slack.text,
        );

        let ChatUnit::Notice(failed) = &units[3] else {
            panic!("a notice");
        };
        assert_eq!(failed.severity, NoticeSeverity::Warning, "a failed delivery is");
        assert_eq!(failed.source, "peer", "and names the seat it could not reach");
        assert!(failed.text.contains("channel closed"), "keeping the reason: {}", failed.text);
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

/// One policy for the text a `queued_command` block carries: the fold
/// above reads it, and so does the TUI's own message walker.
#[cfg(test)]
mod queued_command_tests {
    use super::queued_command_text;
    use serde_json::json;

    #[test]
    fn plain_string_prompt_round_trips() {
        let prompt = json!("Q1, let's give.");
        assert_eq!(queued_command_text(&prompt), "Q1, let's give.");
    }

    #[test]
    fn multi_block_prompt_concatenates_text_blocks() {
        let prompt = json!([
            {"type": "text", "text": "look at this"},
            {"type": "image", "source": {"type": "base64", "data": "..."}},
        ]);
        assert_eq!(queued_command_text(&prompt), "look at this\n[image]");
    }

    #[test]
    fn unknown_inner_block_type_renders_as_placeholder() {
        let prompt = json!([
            {"type": "text", "text": "hi"},
            {"type": "future_block_type", "payload": "..."},
        ]);
        assert_eq!(queued_command_text(&prompt), "hi\n[future_block_type]");
    }

    #[test]
    fn empty_array_returns_empty_string() {
        let prompt = json!([]);
        assert_eq!(queued_command_text(&prompt), "");
    }

    #[test]
    fn non_array_non_string_falls_back_to_json_literal() {
        let prompt = json!({"weird": "shape"});
        let out = queued_command_text(&prompt);
        assert!(out.contains("weird"), "an unrenderable shape still shows something: {out}");
    }
}

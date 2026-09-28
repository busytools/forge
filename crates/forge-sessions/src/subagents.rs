//! The session's sub-agent instances: one card per `Task`/`Agent`
//! dispatch, each with the tool calls that ran under it.
//!
//! The identity is the wire's own link. A dispatch is an assistant frame
//! carrying the `Task` tool call, and every frame the instance produces
//! names it in `parent_tool_use_id`, which is the same shape the terminal's
//! per-instance view derives from. The attribution map cannot answer this
//! question: it pairs a call with an agent TYPE, which is one card per type
//! rather than one per instance.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use forge_primitives::{ContentBlock, Message, TaskNotificationStatus};

use crate::model::ToolCallStatus;
use crate::transcript::{Recorded, ToolLeaf, leaf, result_statuses};

/// How many of an instance's calls its card draws. The terminal's own
/// per-instance tail caps at the same four, and the drawing this card was
/// transcribed from.
pub const TAIL_CAP: usize = 4;

/// One `Task`/`Agent` dispatch, and what the session saw run under it.
pub struct SubagentCard {
    /// What the instance is called: the description its dispatch carried,
    /// which is the name the CLI's own roster uses for it.
    pub name: String,
    /// Whether the instance is still working.
    pub running: bool,
    /// When it settled, from the `end_time` the frame that ended it
    /// carried. `None` while it runs and for any ending that stated no
    /// instant, so a view draws an age only when one is here.
    pub ended_at: Option<SystemTime>,
    /// Every call the instance fired, of which [`Self::tail`] holds the
    /// most recent.
    pub calls: usize,
    /// The instance's last [`TAIL_CAP`] calls, in the order it fired them.
    pub tail: Vec<ToolLeaf>,
}

/// One instance while it is being folded.
struct Instance {
    id: String,
    /// The name its dispatch carried, used when no roster frame names it
    /// better.
    dispatch_name: String,
    children: Vec<ToolLeaf>,
}

/// What the CLI's task roster says about one task, keyed by its task id.
#[derive(Default)]
struct Roster {
    /// The name the CLI's own row uses for the instance, which its frame
    /// states and the dispatch's input only repeats.
    name: Option<String>,
    started: bool,
    ended: bool,
    ended_at: Option<SystemTime>,
}

/// The session's sub-agent instances, in the order they were dispatched.
pub fn subagent_cards(messages: &[Message]) -> Vec<SubagentCard> {
    let results = result_statuses(messages);
    let mut instances: Vec<Instance> = Vec::new();
    let mut at: HashMap<&str, usize> = HashMap::new();
    let mut roster: HashMap<String, Roster> = HashMap::new();
    // The task id the CLI assigned to a dispatch's tool call, which is what
    // lets a later lifecycle frame find the card it belongs to.
    let mut task_ids: HashMap<String, String> = HashMap::new();

    for message in messages {
        match message {
            Message::Assistant { message: envelope, parent_tool_use_id, .. } => {
                let parent = parent_tool_use_id.as_deref().filter(|p| !p.trim().is_empty());
                for block in &envelope.content {
                    let ContentBlock::ToolUse { id, name, input } = block else { continue };
                    match parent {
                        // A dispatch the session itself made. A `Task` a
                        // sub-agent made is that instance's own call, drawn
                        // in its tail and not as a card of its own: the work
                        // the inner agent then does names THAT dispatch as
                        // its parent, and nothing in the conversation links
                        // that id to the outer instance, so a second card
                        // would stand for an agent whose own calls are
                        // nowhere. The same lookup drops a call whose parent
                        // the conversation has not shown yet, which dispatch
                        // order makes unreachable for a session's own work.
                        None if matches!(name.as_str(), "Task" | "Agent") => {
                            at.insert(id.as_str(), instances.len());
                            instances.push(Instance {
                                id: id.clone(),
                                dispatch_name: dispatch_name(input, name),
                                children: Vec::new(),
                            });
                        }
                        Some(parent) => {
                            if let Some(&slot) = at.get(parent) {
                                instances[slot].children.push(leaf(id, name, input, &results));
                            }
                        }
                        None => {}
                    }
                }
            }
            Message::TaskStarted { task_id, tool_use_id, description, .. } => {
                if let Some(id) = tool_use_id.as_deref().filter(|id| !id.trim().is_empty()) {
                    task_ids.entry(task_id.clone()).or_insert_with(|| id.to_owned());
                }
                let row = roster.entry(task_id.clone()).or_default();
                row.started = true;
                if !description.trim().is_empty() {
                    row.name = Some(description.clone());
                }
            }
            Message::TaskProgress { task_id, tool_use_id: Some(id), .. } => {
                task_ids.entry(task_id.clone()).or_insert_with(|| id.clone());
            }
            Message::TaskUpdated { task_id, patch, .. } => {
                let row = roster.entry(task_id.clone()).or_default();
                row.started = true;
                row.ended |= patch.status.as_deref().is_some_and(is_terminal_status);
                // An end time is a statement that the task ended, whether
                // or not the patch also names a status this build knows.
                if let Some(end_ms) = patch.end_time {
                    row.ended = true;
                    row.ended_at = Some(SystemTime::UNIX_EPOCH + Duration::from_millis(end_ms));
                }
            }
            Message::TaskNotification { task_id, status, tool_use_id, .. } => {
                if let Some(id) = tool_use_id.as_deref().filter(|id| !id.trim().is_empty()) {
                    task_ids.entry(task_id.clone()).or_insert_with(|| id.to_owned());
                }
                let row = roster.entry(task_id.clone()).or_default();
                row.ended |= !matches!(status, TaskNotificationStatus::Unknown);
            }
            _ => {}
        }
    }

    instances
        .into_iter()
        .filter_map(|instance| {
            let row = task_ids
                .iter()
                .find(|(_, id)| id.as_str() == instance.id)
                .and_then(|(task_id, _)| roster.get(task_id));
            let any_child_open = instance.children.iter().any(|leaf| {
                matches!(leaf.status, ToolCallStatus::InProgress | ToolCallStatus::Pending)
            });
            let answer = results.get(instance.id.as_str());
            let liveness = match row {
                Some(row) if row.ended => Liveness::Settled,
                Some(row) if row.started => Liveness::Running,
                // A sub-agent's dispatch answers with a launch
                // acknowledgement and nothing else (see `finished`), so an
                // open child and an unanswered dispatch are the two things
                // that say it is working.
                _ if any_child_open => Liveness::Running,
                _ if answer.is_none() => Liveness::Running,
                _ if finished(answer) => Liveness::Settled,
                // Nothing says either way, which is the state a page opened
                // after the instance ran is in: it has the dispatch and the
                // acknowledgement, and the rows that would report an end are
                // not in the transcript it read. A card is drawn for what the
                // session knows, so this one is not drawn at all.
                _ => return None,
            };
            let calls = instance.children.len();
            let tail = instance.children[calls.saturating_sub(TAIL_CAP)..].to_vec();
            Some(SubagentCard {
                name: row.and_then(|row| row.name.clone()).unwrap_or(instance.dispatch_name),
                running: matches!(liveness, Liveness::Running),
                ended_at: row.and_then(|row| row.ended_at),
                calls,
                tail,
            })
        })
        .collect()
}

/// What a session's own frames say about whether an instance is over.
enum Liveness {
    Running,
    Settled,
}

/// Whether the result answering a dispatch says the instance is over.
///
/// Usually it says nothing of the kind. The CLI acknowledges a dispatch the
/// moment it launches, and that acknowledgement is 1066 of the 1077 dispatch
/// results measured across 220 session transcripts, the rest being 4 real
/// hand-backs and 7 with no status at all. So the result's existence is not
/// an ending, and reading it as one draws a finished card, with a check mark
/// and a settled line, for work that is still running.
///
/// What does end one is a failure, or the hand-back the CLI writes when the
/// agent reports. The text is the only witness on this path: the structured
/// `tool_use_result` that also marks one is dropped before a read sees it
/// (the transcript scan keeps only the inner message and the synthesizer
/// hardcodes the field to `None`), and a read that re-enabled it would
/// change what the terminal draws on a resume.
fn finished(answer: Option<&Recorded>) -> bool {
    const HAND_BACK: &str = "[Subagent hand-back]";
    let Some(answer) = answer else {
        return false;
    };
    if answer.status == ToolCallStatus::Failed {
        return true;
    }
    answer.content.as_ref().is_some_and(|content| match content {
        serde_json::Value::String(text) => text.contains(HAND_BACK),
        other => other.to_string().contains(HAND_BACK),
    })
}

/// What the dispatch called this instance. The description is the name the
/// CLI's roster and its own rows use; the type is what is left when the
/// dispatch named none.
fn dispatch_name(input: &serde_json::Value, tool: &str) -> String {
    let read = |key: &str| {
        input
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    read("description").or_else(|| read("subagent_type")).unwrap_or_else(|| tool.to_owned())
}

/// Whether a wire status word says the task is over. The CLI's vocabulary
/// is free-form and forge does not classify all of it, so this names the
/// endings: a status nobody has seen is not evidence that work stopped.
fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "killed" | "stopped" | "timed_out" | "error")
}

#[cfg(test)]
mod tests {
    use forge_primitives::{AssistantEnvelope, ContentBlock, Message};

    use super::{SubagentCard, subagent_cards};

    /// An assistant frame carrying `content`, from the instance named by
    /// `parent` and from the session itself when it is `None`.
    fn assistant(content: Vec<ContentBlock>, parent: Option<&str>) -> Message {
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
            parent_tool_use_id: parent.map(str::to_owned),
            error: None,
            uuid: None,
            timestamp: None,
        }
    }

    /// A tool call frame the instance named by `parent` produced.
    fn call(id: &str, name: &str, input: serde_json::Value, parent: Option<&str>) -> Message {
        assistant(
            vec![ContentBlock::ToolUse { id: id.to_owned(), name: name.to_owned(), input }],
            parent,
        )
    }

    /// One `Task` dispatch: the call that opens an instance.
    fn dispatch(id: &str, description: &str, subagent_type: &str) -> Message {
        call(
            id,
            "Task",
            serde_json::json!({
                "description": description,
                "subagent_type": subagent_type,
                "prompt": "do the thing",
            }),
            None,
        )
    }

    /// The user frame that answers `id`, which is what settles a call.
    fn result(id: &str) -> Message {
        Message::User {
            message: forge_primitives::UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_owned(),
                    content: serde_json::json!("ok"),
                    is_error: false,
                }],
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
            timestamp: None,
        }
    }

    /// The result the CLI writes when it launches a dispatch. Measured 1066
    /// of 1077 dispatch results across 220 transcripts, and it is what a
    /// page opened after the instance ran sees.
    fn launch_ack(id: &str) -> Message {
        answer(
            id,
            "Async agent launched successfully. (This tool result is internal metadata, never \
             quote or paste any part of it, including the agentId below.)\nagentId: a5f83c2a9b88e4db5",
        )
    }

    /// The result the CLI writes when a sub-agent reports back.
    fn hand_back(id: &str) -> Message {
        answer(
            id,
            "[Subagent hand-back] The text below is the final report of a subagent this session \
             delegated to.",
        )
    }

    /// One result whose content is `text`, in the block shape the wire uses.
    fn answer(id: &str, text: &str) -> Message {
        Message::User {
            message: forge_primitives::UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_owned(),
                    content: serde_json::json!([{"type": "text", "text": text}]),
                    is_error: false,
                }],
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
            timestamp: None,
        }
    }

    /// The CLI's own roster row for a dispatch: the task id it assigned,
    /// the call that opened it, and what it calls the instance.
    fn task_started(task_id: &str, tool_use_id: &str, description: &str) -> Message {
        Message::TaskStarted {
            task_id: task_id.to_owned(),
            description: description.to_owned(),
            uuid: "u-start".to_owned(),
            session_id: "session".to_owned(),
            tool_use_id: Some(tool_use_id.to_owned()),
            task_type: Some("Explore".to_owned()),
        }
    }

    /// A progress report: the frame a page attaching mid-turn sees for a
    /// task whose `task_started` it missed, and the only one that then
    /// links the task to the dispatch it came from.
    fn task_progress(task_id: &str, tool_use_id: &str) -> Message {
        Message::TaskProgress {
            task_id: task_id.to_owned(),
            description: "cli-version".to_owned(),
            usage: forge_primitives::messages::TaskUsage {
                total_tokens: 0,
                tool_uses: 1,
                duration_ms: 0,
            },
            uuid: "u-progress".to_owned(),
            session_id: "session".to_owned(),
            tool_use_id: Some(tool_use_id.to_owned()),
            last_tool_name: Some("Read".to_owned()),
            workflow_progress: Vec::new(),
        }
    }

    /// The last frame a task sends, and the only one that ends a card when
    /// no `task_updated` arrives.
    fn task_notified(task_id: &str, tool_use_id: &str, status: &str) -> Message {
        Message::TaskNotification {
            task_id: task_id.to_owned(),
            status: if status == "completed" {
                forge_primitives::TaskNotificationStatus::Completed
            } else {
                forge_primitives::TaskNotificationStatus::Unknown
            },
            output_file: String::new(),
            summary: "the agent reported".to_owned(),
            uuid: "u-notified".to_owned(),
            session_id: "session".to_owned(),
            tool_use_id: Some(tool_use_id.to_owned()),
            usage: None,
        }
    }

    /// The roster saying how the task ended, with the instant it did.
    fn task_ended(task_id: &str, status: &str, end_ms: Option<u64>) -> Message {
        Message::TaskUpdated {
            task_id: task_id.to_owned(),
            patch: forge_primitives::messages::TaskUpdatePatch {
                status: Some(status.to_owned()),
                end_time: end_ms,
            },
            uuid: "u-upd".to_owned(),
            session_id: "session".to_owned(),
        }
    }

    fn names(cards: &[SubagentCard]) -> Vec<&str> {
        cards.iter().map(|card| card.name.as_str()).collect()
    }

    /// The drawing is one card per INSTANCE, and the map the page reads
    /// today cannot see instances at all: it pairs a call with a type, so
    /// two dispatches of one type collapse into a single row while two
    /// types of one dispatch split it.
    ///
    /// The dispatches come first and both instances' calls follow, because
    /// both are live at once in a real session: a fold that hung each call
    /// on the last dispatch it saw would answer the interleaved order the
    /// same way as this one, and only this order tells the two apart.
    #[test]
    fn each_dispatch_is_its_own_card_with_its_own_calls() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            dispatch("toolu_b", "web-session-review", "Explore"),
            call(
                "toolu_a1",
                "Read",
                serde_json::json!({"file_path": "src/lib.rs"}),
                Some("toolu_a"),
            ),
            call("toolu_a2", "Grep", serde_json::json!({"pattern": "presubmit"}), Some("toolu_a")),
            call(
                "toolu_a3",
                "Read",
                serde_json::json!({"file_path": "src/main.rs"}),
                Some("toolu_a"),
            ),
            call(
                "toolu_a4",
                "Read",
                serde_json::json!({"file_path": "src/fold.rs"}),
                Some("toolu_a"),
            ),
            call("toolu_a5", "Bash", serde_json::json!({"command": "just check"}), Some("toolu_a")),
            call("toolu_b1", "Bash", serde_json::json!({"command": "cargo test"}), Some("toolu_b")),
        ];

        let cards = subagent_cards(&messages);

        assert_eq!(names(&cards), ["cli-version", "web-session-review"], "one card per dispatch");
        assert_eq!(cards[0].calls, 5, "the first instance ran five calls");
        assert_eq!(
            cards[0].tail.iter().map(|leaf| leaf.id.as_str()).collect::<Vec<_>>(),
            ["toolu_a2", "toolu_a3", "toolu_a4", "toolu_a5"],
            "and its tail is the four it made most recently, not the first four, not all five",
        );
        assert_eq!(cards[1].calls, 1, "the second instance ran one");
        assert_eq!(
            cards[1].tail.iter().map(|leaf| leaf.id.as_str()).collect::<Vec<_>>(),
            ["toolu_b1"],
            "which is what its own card carries",
        );
    }

    /// A call nobody dispatched belongs to no instance. The session's own
    /// work is the conversation's, and a card for it would put the main
    /// agent's calls under a sub-agent that never ran.
    #[test]
    fn a_call_no_instance_dispatched_is_not_a_card() {
        let messages = [
            call("toolu_main", "Read", serde_json::json!({"file_path": "src/lib.rs"}), None),
            call(
                "toolu_orphan",
                "Read",
                serde_json::json!({"file_path": "src/main.rs"}),
                Some("toolu_absent"),
            ),
        ];

        let cards = subagent_cards(&messages);

        assert!(cards.is_empty(), "neither the session's own call nor an orphaned one is a card");
    }

    /// A dispatch the roster has opened and not closed is running, and the
    /// roster is the authority: the dispatch's own answer is a launch
    /// acknowledgement, so nothing about the call says whether the instance
    /// is still going.
    #[test]
    fn an_instance_the_roster_has_opened_is_running() {
        // The dispatch names nothing and the roster names the instance, so
        // the two sources are separable: the CLI's own row is what the card
        // is called, and a fold that read the dispatch alone would name it
        // `Explore`.
        let messages = [
            dispatch("toolu_a", "", "Explore"),
            result("toolu_a"),
            task_started("t-a", "toolu_a", "cli-version"),
        ];

        let cards = subagent_cards(&messages);

        assert_eq!(names(&cards), ["cli-version"], "the roster names the instance");
        assert!(cards[0].running, "an opened task with no terminal frame is still running");
        assert_eq!(cards[0].ended_at, None, "and nothing has stated an end for it");
    }

    /// A call still open under an instance is running work, which is the
    /// evidence a read has when no roster frame arrived for it at all. The
    /// launch acknowledgement is already back, so the open call is the only
    /// thing saying the instance is still working.
    #[test]
    fn an_instance_with_an_open_call_is_running() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            launch_ack("toolu_a"),
            call("toolu_a1", "Bash", serde_json::json!({"command": "just check"}), Some("toolu_a")),
        ];

        let cards = subagent_cards(&messages);

        assert!(cards[0].running, "a call that has not come back is work in flight");
    }

    /// The frame that ends a task is the frame that says when, so a
    /// settled card carries the instant the CLI stamped rather than an age
    /// counted from whenever the page happened to load.
    #[test]
    fn a_settled_instance_carries_the_instant_it_settled() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            result("toolu_a"),
            task_started("t-a", "toolu_a", "cli-version"),
            task_ended("t-a", "completed", Some(1_700_000_000_123)),
        ];

        let cards = subagent_cards(&messages);

        assert!(!cards[0].running, "a terminal frame ends the instance");
        assert_eq!(
            cards[0].ended_at,
            Some(
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_millis(1_700_000_000_123)
            ),
            "and the instant it ended comes with the frame that ended it",
        );
    }

    /// A dispatch the CLI launched and never reported on is not drawn. Its
    /// only answer is the launch acknowledgement, which says the instance
    /// started and not that it ended, and the rows that would report an end
    /// are not in the transcript a page reads. Drawing it would put a check
    /// mark and a settled line under an instance that may be running right
    /// now, so the section stays away instead.
    #[test]
    fn a_dispatch_answered_only_by_its_launch_ack_is_not_drawn() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            launch_ack("toolu_a"),
            dispatch("toolu_b", "web-session-review", "Explore"),
            launch_ack("toolu_b"),
        ];

        let cards = subagent_cards(&messages);

        assert!(
            cards.is_empty(),
            "a launch acknowledgement is not evidence the instance is over: {:?}",
            names(&cards),
        );
    }

    /// A dispatch answered by a real report is over, and drawn. It is the
    /// rarer shape, 4 of the 1077 measured results, and the one a cold load
    /// can call settled without guessing.
    #[test]
    fn a_dispatch_answered_by_the_hand_back_is_settled() {
        let messages = [dispatch("toolu_a", "cli-version", "Explore"), hand_back("toolu_a")];

        let cards = subagent_cards(&messages);

        assert_eq!(names(&cards), ["cli-version"], "the report says the instance is over");
        assert!(!cards[0].running, "and the card says so");
    }

    /// A progress report is the frame a page attaching mid-turn gets for a
    /// task whose start it missed, and it is what links that task to the
    /// dispatch it came from. Without it the close that follows names a
    /// task nothing can find.
    #[test]
    fn a_progress_report_links_a_task_the_page_never_saw_start() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            task_progress("t-a", "toolu_a"),
            task_ended("t-a", "completed", None),
        ];

        let cards = subagent_cards(&messages);

        assert_eq!(names(&cards), ["cli-version"], "the report links the task to its dispatch");
        assert!(!cards[0].running, "and the close that follows ends the card");
    }

    /// The notification is the last frame a task sends, and it is the only
    /// one that ends a card when no `task_updated` arrives.
    #[test]
    fn a_notification_ends_an_instance_nothing_else_closed() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            task_notified("t-a", "toolu_a", "completed"),
        ];

        let cards = subagent_cards(&messages);

        assert_eq!(names(&cards), ["cli-version"], "the notification names the instance");
        assert!(!cards[0].running, "and is what ends it");
    }

    /// A dispatch a sub-agent made is that instance's own call. Drawing it
    /// as a second card would put an inner agent's work beside the outer
    /// one's, and the inner agent's own calls name the inner dispatch, which
    /// nothing in the conversation links to.
    #[test]
    fn a_dispatch_a_subagent_made_is_a_call_and_not_a_card() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            call("toolu_a9", "Task", serde_json::json!({"description": "inner"}), Some("toolu_a")),
            call(
                "toolu_inner1",
                "Read",
                serde_json::json!({"file_path": "src/inner.rs"}),
                Some("toolu_a9"),
            ),
        ];

        let cards = subagent_cards(&messages);

        assert_eq!(names(&cards), ["cli-version"], "one card, for the session's own dispatch");
        assert_eq!(
            cards[0].tail.iter().map(|leaf| leaf.id.as_str()).collect::<Vec<_>>(),
            ["toolu_a9"],
            "and the inner dispatch is that card's own call",
        );
    }

    /// A dispatch that never came back is running: without a roster frame
    /// the call itself is the only evidence, and a call with no result has
    /// not finished.
    #[test]
    fn a_dispatch_with_no_result_yet_is_running() {
        let messages = [dispatch("toolu_a", "cli-version", "Explore")];

        let cards = subagent_cards(&messages);

        assert!(cards[0].running, "a call the wire has not answered is in flight");
    }
}

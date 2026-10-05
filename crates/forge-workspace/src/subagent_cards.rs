//! The session's sub-agent instances, folded as the frames arrive.
//!
//! One card per `Task`/`Agent` dispatch, joined with the frames that ran
//! under it. The identity is the wire's own link: a dispatch is an
//! assistant frame carrying the `Task` tool call, and every frame the
//! instance produces names it in `parent_tool_use_id`.
//!
//! **Two entry points, one fold.** [`subagent_cards`] drives
//! [`CardTracker`] over a conversation that already exists (a read, a
//! resumed page); the session task drives the same tracker one frame at a
//! time and emits when the list moves, so a live session never re-walks
//! its conversation to push a card.

use std::collections::HashMap;

use forge_primitives::runtime::{SubagentCall, SubagentCallStatus, SubagentCard};
use forge_primitives::{ContentBlock, Message, TaskNotificationStatus};

/// How many of an instance's calls its card draws. The terminal's own
/// per-instance tail caps at the same four, and the drawing the card was
/// transcribed from.
pub const TAIL_CAP: usize = 4;

/// Every instance `messages` holds, in dispatch order.
pub fn subagent_cards(messages: &[Message]) -> Vec<SubagentCard> {
    let mut tracker = CardTracker::default();
    for message in messages {
        tracker.apply(message);
    }
    tracker.cards()
}

/// One instance while it is being folded.
struct Instance {
    id: String,
    /// The name its dispatch carried, used when no roster frame names it
    /// better.
    dispatch_name: String,
    /// The agent type the dispatch named, which is the card head's own word
    /// for what ran.
    agent_type: Option<String>,
    /// Whether it runs in the background: the dispatch's own intent, which
    /// a roster frame later overrides with what the CLI actually did.
    backgrounded: bool,
    children: Vec<ChildCall>,
}

/// One call an instance fired.
struct ChildCall {
    /// The `tool_use` id, kept so the result row that answers it can settle
    /// the child when it arrives.
    id: String,
    name: String,
    title: String,
    status: SubagentCallStatus,
}

/// What the CLI's task roster says about one task, keyed by its task id.
#[derive(Default)]
struct Roster {
    /// The name the CLI's own row uses for the instance, which its frame
    /// states and the dispatch's input only repeats.
    name: Option<String>,
    started: bool,
    ended: bool,
    ended_at_ms: Option<u64>,
    /// Whether the word that ended it was a failure - the terminal set
    /// minus the clean endings.
    ended_badly: bool,
    /// The CLI's own backgrounding, when its frame stated one.
    backgrounded: Option<bool>,
    usage: Option<forge_primitives::messages::TaskUsage>,
}

/// What the rows answering one dispatch say about it.
#[derive(Default)]
struct Answer {
    failed: bool,
    hand_back: bool,
}

/// The instances as frames arrive, one at a time.
#[derive(Default)]
pub struct CardTracker {
    instances: Vec<Instance>,
    at: HashMap<String, usize>,
    roster: HashMap<String, Roster>,
    /// The task id the CLI assigned to a dispatch's tool call, which is
    /// what lets a later lifecycle frame find the card it belongs to.
    task_ids: HashMap<String, String>,
    answers: HashMap<String, Answer>,
}

impl CardTracker {
    /// Fold one frame in.
    ///
    /// Every arm is a per-frame lookup, never a walk of the conversation.
    /// The caller compares [`Self::cards`] against what it last held to
    /// decide whether to announce - the list is the object, so a frame
    /// that changed nothing private pushes nothing.
    pub fn apply(&mut self, message: &Message) {
        match message {
            Message::Assistant { message: envelope, parent_tool_use_id, .. } => {
                let parent = parent_tool_use_id.as_deref().filter(|p| !p.trim().is_empty());
                for block in &envelope.content {
                    match block {
                        ContentBlock::ToolUse { id, name, input, .. } => match parent {
                            // A dispatch the session itself made. A `Task` a
                            // sub-agent made is that instance's own call, drawn
                            // in its tail and not as a card of its own: the
                            // work the inner agent then does names THAT
                            // dispatch as its parent, and nothing in the
                            // conversation links that id to the outer
                            // instance.
                            None if matches!(name.as_str(), "Task" | "Agent") => {
                                self.at.insert(id.clone(), self.instances.len());
                                self.instances.push(Instance {
                                    id: id.clone(),
                                    dispatch_name: dispatch_name(input, name),
                                    agent_type: input
                                        .get("subagent_type")
                                        .and_then(serde_json::Value::as_str)
                                        .map(str::trim)
                                        .filter(|value| !value.is_empty())
                                        .map(str::to_owned),
                                    backgrounded: input
                                        .get("run_in_background")
                                        .and_then(serde_json::Value::as_bool)
                                        .unwrap_or(false),
                                    children: Vec::new(),
                                });
                            }
                            Some(parent) => {
                                if let Some(&slot) = self.at.get(parent) {
                                    self.instances[slot].children.push(child_of(id, name, input));
                                }
                            }
                            None => {}
                        },
                        // A server-side tool's result arrives inline in the
                        // assistant frame that made the call, so it settles a
                        // child here rather than on a user row.
                        ContentBlock::ServerToolResult { tool_use_id, .. } => {
                            self.settle_child(tool_use_id, false);
                        }
                        _ => {}
                    }
                }
            }
            Message::User { message: envelope, .. } => {
                for block in &envelope.content {
                    let ContentBlock::ToolResult { tool_use_id, content, is_error, .. } = block
                    else {
                        continue;
                    };
                    let hand_back = content_is_hand_back(content);
                    let answer = self.answers.entry(tool_use_id.clone()).or_default();
                    answer.failed |= *is_error;
                    answer.hand_back |= hand_back;
                    self.settle_child(tool_use_id, *is_error);
                }
            }
            Message::TaskStarted { task_id, tool_use_id, description, extras, .. } => {
                self.link_task(task_id, tool_use_id.as_deref());
                let row = self.roster.entry(task_id.clone()).or_default();
                row.started = true;
                if !description.trim().is_empty() {
                    row.name = Some(description.clone());
                }
                // The CLI's own backgrounding rides the frame's unmodelled
                // fields; the dispatch's intent is only a fallback.
                if let Some(flag) =
                    extras.get("is_backgrounded").and_then(serde_json::Value::as_bool)
                {
                    row.backgrounded = Some(flag);
                }
            }
            Message::TaskProgress { task_id, tool_use_id, usage, .. } => {
                self.link_task(task_id, tool_use_id.as_deref());
                let row = self.roster.entry(task_id.clone()).or_default();
                row.started = true;
                row.usage = Some(usage.clone());
            }
            Message::TaskUpdated { task_id, patch, .. } => {
                let row = self.roster.entry(task_id.clone()).or_default();
                row.started = true;
                if let Some(status) = patch.status.as_deref()
                    && is_terminal_status(status)
                {
                    row.ended = true;
                    row.ended_badly |= is_failure_status(status);
                }
                // An end time is a statement that the task ended, whether or
                // not the patch also names a status this build knows.
                if let Some(end_ms) = patch.end_time {
                    row.ended = true;
                    row.ended_at_ms = Some(end_ms);
                }
            }
            Message::TaskNotification { task_id, status, tool_use_id, usage, .. } => {
                self.link_task(task_id, tool_use_id.as_deref());
                let row = self.roster.entry(task_id.clone()).or_default();
                if !matches!(status, TaskNotificationStatus::Unknown) {
                    row.ended = true;
                }
                row.ended_badly |= matches!(
                    status,
                    TaskNotificationStatus::Failed | TaskNotificationStatus::Stopped
                );
                if usage.is_some() {
                    row.usage.clone_from(usage);
                }
            }
            _ => {}
        }
    }

    /// The instances as they stand, in dispatch order.
    pub fn cards(&self) -> Vec<SubagentCard> {
        self.instances
            .iter()
            .filter_map(|instance| {
                let row = self
                    .task_ids
                    .iter()
                    .find(|(_, id)| id.as_str() == instance.id)
                    .and_then(|(task_id, _)| self.roster.get(task_id));
                let any_child_open = instance.children.iter().any(|child| {
                    matches!(
                        child.status,
                        SubagentCallStatus::Pending | SubagentCallStatus::InProgress
                    )
                });
                let answer = self.answers.get(instance.id.as_str());
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
                    // Nothing says either way, which is the state a READ
                    // conversation is in: a resume re-read the transcript,
                    // which carries no sub-agent frames, so all it holds is
                    // the dispatch and the acknowledgement and the rows that
                    // would report an end are not there. A live session's
                    // held conversation carries the frames instead, so this
                    // arm stops at the read case. A card is drawn for what
                    // the session knows, so this one is not drawn at all.
                    _ => return None,
                };
                let calls = instance.children.len();
                let tail = instance.children[calls.saturating_sub(TAIL_CAP)..]
                    .iter()
                    .map(|child| SubagentCall {
                        name: child.name.clone(),
                        title: child.title.clone(),
                        status: child.status,
                    })
                    .collect();
                Some(SubagentCard {
                    name: row
                        .and_then(|row| row.name.clone())
                        .unwrap_or_else(|| instance.dispatch_name.clone()),
                    dispatch_id: instance.id.clone(),
                    agent_type: instance.agent_type.clone(),
                    running: matches!(liveness, Liveness::Running),
                    failed: answer.is_some_and(|answer| answer.failed)
                        || row.is_some_and(|row| row.ended_badly),
                    backgrounded: row
                        .and_then(|row| row.backgrounded)
                        .unwrap_or(instance.backgrounded),
                    ended_at_ms: row.and_then(|row| row.ended_at_ms),
                    calls,
                    tail,
                    usage: row.and_then(|row| row.usage.clone()),
                })
            })
            .collect()
    }

    /// Note which task a dispatch's tool call was assigned.
    fn link_task(&mut self, task_id: &str, tool_use_id: Option<&str>) {
        if let Some(id) = tool_use_id.filter(|id| !id.trim().is_empty()) {
            self.task_ids.entry(task_id.to_owned()).or_insert_with(|| id.to_owned());
        }
    }

    /// Settle the child call `tool_use_id` answers, when it is one of an
    /// instance's own calls.
    fn settle_child(&mut self, tool_use_id: &str, failed: bool) {
        let status =
            if failed { SubagentCallStatus::Failed } else { SubagentCallStatus::Completed };
        for instance in &mut self.instances {
            for child in &mut instance.children {
                if child.id == tool_use_id {
                    child.status = status;
                    return;
                }
            }
        }
    }
}

/// What a session's own frames say about whether an instance is over.
enum Liveness {
    Running,
    Settled,
}

/// Whether the rows answering a dispatch say the instance is over.
///
/// Usually they say nothing of the kind. The CLI acknowledges a dispatch the
/// moment it launches, and that acknowledgement is 1066 of the 1077 dispatch
/// results measured across 220 session transcripts, the rest being 4 real
/// hand-backs and 7 with no status at all. So the result's existence is not
/// an ending, and reading it as one draws a finished card, with a check mark
/// and a settled line, for work that is still running.
fn finished(answer: Option<&Answer>) -> bool {
    let Some(answer) = answer else {
        return false;
    };
    answer.failed || answer.hand_back
}

/// Whether a result's content carries the CLI's hand-back marker.
fn content_is_hand_back(content: &serde_json::Value) -> bool {
    const HAND_BACK: &str = "[Subagent hand-back]";
    match content {
        serde_json::Value::String(text) => text.contains(HAND_BACK),
        other => other.to_string().contains(HAND_BACK),
    }
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

/// Whether an ending's word is a failure. `killed` and `stopped` are in:
/// a cancel is not a clean finish, and the terminal draws its cross for one.
fn is_failure_status(status: &str) -> bool {
    matches!(status, "failed" | "killed" | "stopped" | "timed_out" | "error")
}

/// One call an instance fired, as it arrives: pending until its result
/// lands, named by the shared call builder so a card's tail and the chat's
/// rows resolve a title the same way.
fn child_of(id: &str, name: &str, input: &serde_json::Value) -> ChildCall {
    let call = crate::tooling::create_tool_call(id, name, input, None);
    ChildCall {
        id: id.to_owned(),
        name: name.to_owned(),
        title: call.title,
        status: SubagentCallStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use forge_primitives::runtime::SubagentCard;
    use forge_primitives::{
        AssistantEnvelope, ContentBlock, Message, TaskNotificationStatus, UserEnvelope,
    };

    use super::{CardTracker, TAIL_CAP, subagent_cards};

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
                extras: serde_json::Map::new(),
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: parent.map(str::to_owned),
            error: None,
            uuid: None,
            timestamp: None,
            extras: serde_json::Map::new(),
        }
    }

    /// A tool call frame the instance named by `parent` produced.
    fn call(id: &str, name: &str, input: serde_json::Value, parent: Option<&str>) -> Message {
        assistant(
            vec![ContentBlock::ToolUse {
                id: id.to_owned(),
                name: name.to_owned(),
                input,
                extras: serde_json::Map::new(),
            }],
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
        answer(id, "ok")
    }

    /// One result whose content is `text`, in the block shape the wire uses.
    fn answer(id: &str, text: &str) -> Message {
        Message::User {
            message: UserEnvelope {
                role: "user".to_owned(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_owned(),
                    content: serde_json::json!([{"type": "text", "text": text}]),
                    is_error: false,
                    extras: serde_json::Map::new(),
                }],
                extras: serde_json::Map::new(),
            },
            session_id: "session".to_owned(),
            parent_tool_use_id: None,
            uuid: None,
            tool_use_result: None,
            timestamp: None,
            synthetic: false,
            extras: serde_json::Map::new(),
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

    /// The CLI's own roster row for a dispatch: the task id it assigned,
    /// the call that opened it, and what it calls the instance.
    fn task_started(task_id: &str, tool_use_id: &str, description: &str) -> Message {
        Message::TaskStarted {
            task_id: task_id.to_owned(),
            description: description.to_owned(),
            uuid: "u-start".to_owned(),
            session_id: "session".to_owned(),
            tool_use_id: Some(tool_use_id.to_owned()),
            task_type: Some("local_agent".to_owned()),
            extras: serde_json::Map::new(),
        }
    }

    /// The roster saying how the task ended, with the instant it did.
    fn task_ended(task_id: &str, status: &str, end_ms: Option<u64>) -> Message {
        Message::TaskUpdated {
            task_id: task_id.to_owned(),
            patch: forge_primitives::messages::TaskUpdatePatch {
                status: Some(status.to_owned()),
                end_time: end_ms,
                extras: serde_json::Map::new(),
            },
            uuid: "u-upd".to_owned(),
            session_id: "session".to_owned(),
            extras: serde_json::Map::new(),
        }
    }

    /// One progress report, the frame a page attaching mid-turn gets for a
    /// task whose start it missed.
    fn task_progress(task_id: &str, tool_use_id: &str) -> Message {
        Message::TaskProgress {
            task_id: task_id.to_owned(),
            description: "Running something".to_owned(),
            usage: forge_primitives::messages::TaskUsage {
                total_tokens: 9714,
                tool_uses: 1,
                duration_ms: 2716,
                extras: serde_json::Map::new(),
            },
            uuid: "u-prog".to_owned(),
            session_id: "session".to_owned(),
            tool_use_id: Some(tool_use_id.to_owned()),
            last_tool_name: None,
            workflow_progress: Vec::new(),
            extras: serde_json::Map::new(),
        }
    }

    /// The terminal notification the CLI sends when a task ends.
    fn task_notified(task_id: &str, tool_use_id: &str, status: &str) -> Message {
        let status = match status {
            "completed" => TaskNotificationStatus::Completed,
            "failed" => TaskNotificationStatus::Failed,
            "stopped" => TaskNotificationStatus::Stopped,
            _ => TaskNotificationStatus::Unknown,
        };
        Message::TaskNotification {
            task_id: task_id.to_owned(),
            status,
            output_file: "/tmp/sub.output".to_owned(),
            summary: "done".to_owned(),
            uuid: "u-note".to_owned(),
            session_id: "session".to_owned(),
            tool_use_id: Some(tool_use_id.to_owned()),
            usage: None,
            extras: serde_json::Map::new(),
        }
    }

    /// A dispatch whose own input asked to run in the background.
    fn background_dispatch(id: &str, description: &str, subagent_type: &str) -> Message {
        call(
            id,
            "Task",
            serde_json::json!({
                "description": description,
                "subagent_type": subagent_type,
                "run_in_background": true,
                "prompt": "do the thing",
            }),
            None,
        )
    }

    /// The roster row carrying the CLI's own backgrounding flag on the
    /// frame's unmodelled fields, where the wire puts it.
    fn task_started_flagging(
        task_id: &str,
        tool_use_id: &str,
        description: &str,
        is_backgrounded: bool,
    ) -> Message {
        let mut started = task_started(task_id, tool_use_id, description);
        if let Message::TaskStarted { extras, .. } = &mut started {
            extras.insert("is_backgrounded".to_owned(), serde_json::Value::Bool(is_backgrounded));
        }
        started
    }

    /// The user frame that answers `id` with an error.
    fn errored_result(id: &str) -> Message {
        let mut answered = result(id);
        if let Message::User { message, .. } = &mut answered
            && let Some(ContentBlock::ToolResult { is_error, .. }) = message.content.first_mut()
        {
            *is_error = true;
        }
        answered
    }

    fn names(cards: &[SubagentCard]) -> Vec<&str> {
        cards.iter().map(|card| card.name.as_str()).collect()
    }

    /// The drawing is one card per INSTANCE: two dispatches of one type are
    /// two cards, and both instances' calls follow their own dispatch even
    /// when the two are interleaved live.
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
            call("toolu_a2", "Bash", serde_json::json!({"command": "just check"}), Some("toolu_a")),
            call("toolu_b1", "Bash", serde_json::json!({"command": "cargo test"}), Some("toolu_b")),
        ];

        let cards = subagent_cards(&messages);

        assert_eq!(names(&cards), ["cli-version", "web-session-review"], "one card per dispatch");
        assert_eq!(
            (cards[0].dispatch_id.as_str(), cards[1].dispatch_id.as_str()),
            ("toolu_a", "toolu_b"),
            "each card names the dispatch that opened it, which is how a view joins it to the row",
        );
        assert_eq!(cards[0].calls, 2, "the first instance ran two calls");
        assert_eq!(
            cards[0].tail.iter().map(|call| call.title.as_str()).collect::<Vec<_>>(),
            ["Read src/lib.rs", "just check"],
            "and its tail names what each call touched",
        );
        assert_eq!(cards[1].calls, 1, "the second instance ran one");
    }

    /// The tail caps at [`TAIL_CAP`] most recent calls while `calls` counts
    /// every one.
    #[test]
    fn the_tail_caps_at_the_last_four_calls() {
        let mut messages = vec![dispatch("toolu_a", "cli-version", "Explore")];
        for n in 0..6 {
            messages.push(call(
                &format!("toolu_a{n}"),
                "Read",
                serde_json::json!({"file_path": format!("src/{n}.rs")}),
                Some("toolu_a"),
            ));
        }

        let cards = subagent_cards(&messages);

        assert_eq!(cards[0].calls, 6, "every call counts");
        assert_eq!(
            cards[0].tail.iter().map(|call| call.title.as_str()).collect::<Vec<_>>(),
            ["Read src/2.rs", "Read src/3.rs", "Read src/4.rs", "Read src/5.rs"],
            "and the tail is the most recent {TAIL_CAP}",
        );
    }

    /// A call nobody dispatched belongs to no instance.
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

        assert!(subagent_cards(&messages).is_empty());
    }

    /// The roster is the authority once it has spoken: an opened task with
    /// no terminal frame is running, and a terminal one carries the instant
    /// it ended.
    #[test]
    fn the_roster_decides_liveness_and_names_the_card() {
        let running = [
            dispatch("toolu_a", "", "Explore"),
            result("toolu_a"),
            task_started("t-a", "toolu_a", "cli-version"),
        ];
        let cards = subagent_cards(&running);
        assert_eq!(names(&cards), ["cli-version"], "the roster names the instance");
        assert!(cards[0].running, "an opened task with no terminal frame is still running");

        let ended = [
            dispatch("toolu_a", "cli-version", "Explore"),
            result("toolu_a"),
            task_started("t-a", "toolu_a", "cli-version"),
            task_ended("t-a", "completed", Some(1_700_000_000_123)),
        ];
        let cards = subagent_cards(&ended);
        assert!(!cards[0].running, "a terminal frame ends the instance");
        assert_eq!(cards[0].ended_at_ms, Some(1_700_000_000_123));
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

    /// A call still open under an instance is running work, which is the
    /// evidence a read has when no roster frame arrived at all.
    #[test]
    fn an_instance_with_an_open_call_is_running() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            launch_ack("toolu_a"),
            call("toolu_a1", "Bash", serde_json::json!({"command": "just check"}), Some("toolu_a")),
        ];

        assert!(subagent_cards(&messages)[0].running, "a call that has not come back is work");
    }

    /// A dispatch the CLI launched and never reported on is not drawn.
    #[test]
    fn a_dispatch_answered_only_by_its_launch_ack_is_not_drawn() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            launch_ack("toolu_a"),
            dispatch("toolu_b", "web-session-review", "Explore"),
            launch_ack("toolu_b"),
        ];

        assert!(subagent_cards(&messages).is_empty(), "a launch ack is not an ending");
    }

    /// A dispatch answered by a real report is over, and drawn. It is the
    /// rarer shape, 4 of the 1077 measured results.
    #[test]
    fn a_dispatch_answered_by_the_hand_back_is_settled() {
        let cards =
            subagent_cards(&[dispatch("toolu_a", "cli-version", "Explore"), hand_back("toolu_a")]);

        assert_eq!(names(&cards), ["cli-version"]);
        assert!(!cards[0].running);
    }

    /// A dispatch whose answer errored is over, and over badly: the error
    /// is the ending the read has when no roster frame says how it went.
    #[test]
    fn an_errored_answer_settles_the_card_as_failed() {
        let cards = subagent_cards(&[
            dispatch("toolu_a", "cli-version", "Explore"),
            errored_result("toolu_a"),
        ]);

        assert_eq!(names(&cards), ["cli-version"], "the errored answer still names it");
        assert!(!cards[0].running, "an errored answer is an ending");
        assert!(cards[0].failed, "and the card carries its cross");
    }

    /// The CLI's own backgrounding word rides the roster frame's unmodelled
    /// fields, and the dispatch's own intent is only the fallback.
    #[test]
    fn the_backgrounded_flag_prefers_the_rosters_word() {
        let asked = subagent_cards(&[background_dispatch("toolu_a", "cli-version", "Explore")]);
        assert!(asked[0].backgrounded, "the dispatch's own intent is the fallback");

        let roster_says_no = subagent_cards(&[
            background_dispatch("toolu_a", "cli-version", "Explore"),
            task_started_flagging("t-a", "toolu_a", "cli-version", false),
        ]);
        assert!(
            !roster_says_no[0].backgrounded,
            "the roster's word supersedes the dispatch's, either way"
        );
    }

    /// A dispatch a sub-agent made is that instance's own call.
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
        assert_eq!(cards[0].tail.len(), 1, "and the inner dispatch is that card's own call");
    }

    /// A dispatch that never came back is running.
    #[test]
    fn a_dispatch_with_no_result_yet_is_running() {
        assert!(subagent_cards(&[dispatch("toolu_a", "cli-version", "Explore")])[0].running);
    }

    /// The usage its last progress or notification frame reported rides the
    /// card, which is what the card's note draws.
    #[test]
    fn the_usage_the_task_reported_rides_the_card() {
        let progress = Message::TaskProgress {
            task_id: "t-a".to_owned(),
            description: "Running Bash".to_owned(),
            usage: forge_primitives::messages::TaskUsage {
                total_tokens: 12_047,
                tool_uses: 2,
                duration_ms: 4_953,
                extras: serde_json::Map::new(),
            },
            uuid: "u-p".to_owned(),
            session_id: "session".to_owned(),
            tool_use_id: Some("toolu_a".to_owned()),
            last_tool_name: Some("Bash".to_owned()),
            workflow_progress: Vec::new(),
            extras: serde_json::Map::new(),
        };
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            task_started("t-a", "toolu_a", "cli-version"),
            progress,
        ];

        let cards = subagent_cards(&messages);

        let usage = cards[0].usage.as_ref().expect("the progress frame's usage");
        assert_eq!(usage.total_tokens, 12_047);
        assert_eq!(usage.tool_uses, 2);
        assert_eq!(usage.duration_ms, 4_953);
        assert_eq!(
            cards[0].agent_type.as_deref(),
            Some("Explore"),
            "and the head's own word for what ran rides the card",
        );
    }

    /// An inner call's own result settles that row.
    #[test]
    fn a_child_settles_on_its_own_result() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            call("toolu_a1", "Bash", serde_json::json!({"command": "just check"}), Some("toolu_a")),
            result("toolu_a1"),
        ];

        let cards = subagent_cards(&messages);

        assert_eq!(
            cards[0].tail[0].status,
            forge_primitives::runtime::SubagentCallStatus::Completed,
            "the child's result settles its row",
        );
    }

    /// A bad ending marks the card: the roster's own word decides, and a
    /// kill counts as one - the terminal draws its cross for a kill.
    #[test]
    fn a_bad_ending_marks_the_card_failed() {
        let of = |word: &str| {
            subagent_cards(&[
                dispatch("toolu_a", "cli-version", "Explore"),
                task_started("t-a", "toolu_a", "cli-version"),
                task_ended("t-a", word, None),
            ])
            .remove(0)
        };

        assert!(of("failed").failed, "a failed word marks the card");
        assert!(of("killed").failed, "and so does a kill");
        assert!(of("stopped").failed, "and a cancel, which the terminal draws as a kill");
        assert!(!of("completed").failed, "a clean finish does not");

        let notified = subagent_cards(&[
            dispatch("toolu_a", "cli-version", "Explore"),
            task_notified("t-a", "toolu_a", "stopped"),
        ]);
        assert!(notified[0].failed, "a stopped notification carries the same word");
    }

    /// The tracker folds two instances' frames interleaved: `t-b`'s roster
    /// frame links to its dispatch and its ending reaches its card, `t-a`'s
    /// has no terminal frame so it is still running, and a `cards()` read in
    /// the middle of the stream does not disturb the fold.
    ///
    /// **This does not compare the tracker against `subagent_cards`.** One
    /// implementation backs both, so a pure function over the same state
    /// cannot disagree with itself and an equality here could never fail.
    /// What it pins is the interleaving itself.
    #[test]
    fn the_incremental_tracker_folds_interleaved_rosters() {
        let messages = [
            dispatch("toolu_a", "cli-version", "Explore"),
            dispatch("toolu_b", "web-session-review", "Explore"),
            task_started("t-a", "toolu_a", "cli-version"),
            call(
                "toolu_a1",
                "Read",
                serde_json::json!({"file_path": "src/lib.rs"}),
                Some("toolu_a"),
            ),
            result("toolu_a1"),
            task_started("t-b", "toolu_b", "web-session-review"),
            call("toolu_b1", "Bash", serde_json::json!({"command": "cargo test"}), Some("toolu_b")),
            result("toolu_b"),
            task_ended("t-b", "completed", Some(1_700_000_000_000)),
        ];

        let mut tracker = CardTracker::default();
        for message in &messages {
            tracker.apply(message);
            let _ = tracker.cards();
        }

        let cards = tracker.cards();
        assert_eq!(names(&cards), ["cli-version", "web-session-review"]);
        assert!(cards[0].running, "t-a has no terminal frame, so it is still running");
        assert!(
            !cards[1].running && cards[1].ended_at_ms == Some(1_700_000_000_000),
            "t-b's roster frame links, so its ending reaches its card"
        );
    }
}

//! Where a conversation's turns open, in the shape the fold reads them.
//!
//! **One rule, two readers.** A page is cut on the fold's turn boundaries, and
//! so is the window a session keeps its conversation in: a window that starts
//! part-way through a turn serves its first turn with the head missing. The
//! fold lives above this crate (`forge_server::transcript`), so the row
//! predicates it reads live here and both readers share the one copy.
//!
//! **This answers no more than the fold would.** Every frame [`TurnScan`]
//! names is one the fold opens a turn at; it names none the fold would not.
//! Two shapes it cannot read the way the fold does are answered the safe way -
//! no turn - so what they cost is a head the window cannot start on, never a
//! cut inside a turn.
//!
//! A queued prompt opens a turn in the fold unless a question card takes it,
//! which is the fold's own unit state and not visible from here. That miss is
//! not only the prompt: the fold's turn opens there and resets the state the
//! next turn opens under, so a frame the fold then draws as a head of its own -
//! the continuation a compaction leaves - is left alone here too.
//!
//! And a row opening with a bracket is read as an envelope, because every
//! envelope this workspace writes opens with one and the parser that tells an
//! envelope from the reader's own words lives above this crate. That is broader
//! than the envelopes: a reader's own `[note] ...` row, and a bracket-led
//! notice the parser rejects, are heads here that this leaves alone. The shapes
//! read here are exempt from that, so an image note the fold opens a turn at is
//! named.

use forge_primitives::messages::names_a_dispatch;
use forge_primitives::{ContentBlock, Message};

/// Where the fold opens a conversation turn, walked frame by frame.
#[derive(Default)]
pub struct TurnScan {
    /// The skills the open turn holds a call for, oldest first: a skill's body
    /// belongs in the turn whose call loaded it.
    skills: Vec<String>,
    /// Whether a compaction boundary sits in the open turn. The continuation
    /// prompt that follows belongs beside it, so it opens no turn of its own
    /// while this holds.
    boundary: bool,
    /// Whether the open turn has taken a tool call. The harness's line about
    /// an image it read arrives right behind the call that read it, so it
    /// joins that turn rather than opening one.
    calls: bool,
}

impl TurnScan {
    /// Whether the fold opens a conversation turn at this frame, under the
    /// state the frames before it left.
    pub fn opens(&mut self, message: &Message) -> bool {
        // A sub-agent's frames are the SUBAGENTS surface's, not the chat's.
        if is_dispatched(message) {
            return false;
        }
        match message {
            Message::CompactBoundary { .. } => {
                self.boundary = true;
                false
            }
            // A call's own row opens no turn, but a `Skill` call is what makes
            // the body behind it belong to this turn rather than opening one.
            Message::Assistant { message: envelope, .. } => {
                self.calls_in(&envelope.content);
                false
            }
            Message::User { message: envelope, .. } => {
                let mut opens = false;
                for block in &envelope.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            if claims_skill_call(&mut self.skills, text) {
                                continue;
                            }
                            if self.boundary && is_continuation(text) {
                                self.boundary = false;
                                continue;
                            }
                            if self.calls && is_image_note(text) {
                                continue;
                            }
                            if is_task_notice(text) {
                                continue;
                            }
                            // Every envelope this workspace writes opens with a
                            // bracket, and the parser that tells one from the
                            // reader's own words lives above this crate - so a
                            // bracket-led row is left alone unless it is a
                            // shape read here.
                            if text.starts_with('[') && !is_image_note(text) {
                                continue;
                            }
                            self.skills.clear();
                            self.boundary = false;
                            self.calls = false;
                            opens = true;
                        }
                        other => self.calls_in(std::slice::from_ref(other)),
                    }
                }
                opens
            }
            _ => false,
        }
    }

    /// The blocks that move the state the next turn opens under rather than
    /// opening one: a call, and a `Skill` call's own name for what it loads.
    fn calls_in(&mut self, content: &[ContentBlock]) {
        for block in content {
            if let ContentBlock::ToolUse { name, input, .. }
            | ContentBlock::ServerToolUse { name, input, .. } = block
            {
                self.calls = true;
                if let Some(want) = input
                    .get("skill")
                    .and_then(serde_json::Value::as_str)
                    .filter(|_| name.eq_ignore_ascii_case("skill"))
                {
                    self.skills.push(want.to_owned());
                }
            }
        }
    }
}

/// A frame a sub-agent produced. What it narrates and calls belongs to the
/// SUBAGENTS surface, never to the session's own conversation.
pub fn is_dispatched(message: &Message) -> bool {
    let (Message::Assistant { parent_tool_use_id: parent, .. }
    | Message::User { parent_tool_use_id: parent, .. }
    | Message::StopHookSummary { parent_tool_use_id: parent, .. }) = message
    else {
        return false;
    };
    names_a_dispatch(parent.as_deref())
}

/// Whether a turn's text is the harness's own task notice rather than anything
/// a person said: the marker the CLI writes at the head of the XML.
///
/// Two carriers hold it - the `attachment` row the scan hoists into a
/// `queued_command` block, and a `user` row whose content string is the XML -
/// and the fold reads the marker out of both, so a third shape would arrive
/// here and not elsewhere.
pub fn is_task_notice(text: &str) -> bool {
    text.trim_start().starts_with("<task-notification>")
}

/// Whether `text` is the harness's own line about an image it just read.
///
/// The CLI sends it as the reader's own row right after the result that
/// carried the image; the client fold hangs it on the call that read the
/// picture (`imageNoteOf` in its `units.ts`), so it belongs in that call's
/// turn rather than in one of its own.
pub fn is_image_note(text: &str) -> bool {
    text.trim().starts_with("[Image: original ")
}

/// Whether `text` is the continuation prompt a compaction leaves behind.
///
/// The CLI sends it as the reader's own user row right after the boundary
/// frame, and it is the compaction's own account of what was cut. The client
/// fold hangs it on the boundary's row (`attachContinuation` in its
/// `units.ts`), and the two reads must agree.
pub fn is_continuation(text: &str) -> bool {
    text.starts_with("This session is being continued from a previous conversation")
}

/// The skill a body's own frame names, off its first line's directory.
///
/// The CLI injects a skill's body as a user row whose first line names the
/// skill's directory and whose remainder is the skill's markdown. The name is
/// the path's last segment that is not a version, so a plugin-cached skill
/// (`.../ui-ux-pro-max/2.13.0`) is named as its directory names it - the same
/// reading the web client's fold makes (`skillBody` in its `units.ts`).
pub fn skill_body_name(text: &str) -> Option<&str> {
    let lead = text.split('\n').next()?;
    if let Some(path) = lead.strip_prefix("Base directory for this skill:") {
        let path = path.trim();
        return path
            .split('/')
            .rev()
            .find(|part| part.chars().next().is_some_and(|c| !c.is_ascii_digit()));
    }
    // The carrier a tool-invoked skill uses: the skill's own markdown, opening
    // on its title heading (`# PR Review Loop` for `pr-review-loop`) with no
    // plumbing line. The heading is the whole of what names it, so that is the
    // name - normalized by `names_skill`, the same match the client makes.
    let trimmed = lead.trim();
    let title = trimmed.trim_start_matches('#');
    if title.len() == trimmed.len() || !title.starts_with(' ') {
        return None;
    }
    let title = title.trim();
    (!title.is_empty()).then_some(title)
}

/// Whether `text` is the body of a skill the open turn holds a call for,
/// consuming that claim.
///
/// A body belongs in the turn whose call loaded it: it is the same telling the
/// call's row already carries, and a turn of its own draws it a second time
/// under the reader's name. The claim is consumed so a later body for the same
/// skill lands on the call after it, and a body no call holds opens a turn as
/// it did before - nothing may be dropped.
pub fn claims_skill_call(turn_skills: &mut Vec<String>, text: &str) -> bool {
    let Some(name) = skill_body_name(text) else {
        return false;
    };
    let Some(at) = turn_skills.iter().position(|want| names_skill(want, name)) else {
        return false;
    };
    turn_skills.remove(at);
    true
}

/// Whether a `Skill` call's own input names the skill a body's path ended in.
///
/// The spellings differ two ways: a plugin skill is `ui-ux-pro-max:ui-ux-pro-max`
/// where the path ends `ui-ux-pro-max`, and a tool-invoked body's heading is
/// `PR Review Loop` where the call says `pr-review-loop`. The client fold
/// matches the same way (`namesSkill` in its `units.ts`); the two must agree,
/// or the same frame lands in one view and not the other.
pub fn names_skill(want: &str, name: &str) -> bool {
    if want == name || want.ends_with(&format!(":{name}")) || want.starts_with(&format!("{name}:"))
    {
        return true;
    }
    let held = normalized_skill(want);
    let wanted = normalized_skill(name);
    held == wanted || held.contains(&wanted) || wanted.contains(&held)
}

/// A skill name as its words, so `pr-review-loop` and `PR Review Loop` agree.
pub fn normalized_skill(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c == '-' || c == '_' || c == ':' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

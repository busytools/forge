//! The window a session's conversation is kept in.
//!
//! **One conversation, two copies, one window.** The session task accumulates
//! the frames it receives and appends every one, because a replay has to
//! answer with the conversation rather than with the history the connect
//! carried; the transport holds a copy of the same conversation, seeded from
//! that connect or replay, because a page and a fold read it long after the
//! session is gone. Both stop growing with the transcript here.
//!
//! One definition rather than two, because one copy is the other's source: a
//! replay kept shorter than the window its reader holds is a page that cannot
//! reach the cap it was sized for, and the two would drift apart silently.
//!
//! **And both cuts land on a turn's first frame.** A window starting part-way
//! through a turn serves its first turn with the head missing, so what the cap
//! sheds is counted to a frame a turn opens at - one cut, shared by both entry
//! points, and [`crate::conversation_turns`] is where a turn opens.

use forge_primitives::Message;

/// How many messages one copy of a conversation keeps.
///
/// **Sized from the read that consumes it.** Every page a client is handed is
/// the newest twenty turns, and across the eight largest transcripts measured
/// on a live machine that window spans 85 to 1,270 messages, with the heaviest
/// twenty-turn run at 2,799 - so the cap holds every window any of them would
/// serve. What it buys is a copy that stops tracking its transcript's length:
/// the live heap grew a pair of ~90MB contiguous buffers an hour through these
/// two structures, and one held in a cap-sized window cannot reach one.
pub const CONVERSATION_CAP: usize = 4_000;

/// A cap under the window it serves cannot reach back over it: 2,799 messages
/// is the heaviest twenty-turn run measured across the eight largest
/// transcripts on the author's machine (2026-10-05), and every subscribe
/// carries twenty turns. Checked at compile time, so a re-tune under it is a
/// build that never runs rather than a page that quietly loses turns.
const _: () = assert!(
    CONVERSATION_CAP >= 2_799,
    "the cap cannot reach back over the heaviest twenty-turn run measured",
);

/// How far past the cap a copy may grow before the drop runs.
///
/// The drop rebuilds what it keeps into an allocation of its own, so running
/// it on every append past the cap would pay that rebuild per frame instead of
/// once per slack - and the slack costs a proportion of the cap, not a second
/// conversation.
pub const CONVERSATION_SLACK: usize = 1_000;

/// Drop the oldest messages until the copy is back inside the cap, and report
/// how many went.
///
/// **A fresh allocation rather than a `drain`**, because a `Vec` keeps the
/// store it grew to: the point of the cap is that a copy stops holding the
/// backing store of a conversation it no longer has, and a drain would leave
/// it holding all of it with a window's worth of messages in front.
pub fn drop_past_cap(messages: &mut Vec<Message>) -> usize {
    let front = front_of(messages);
    if front == 0 {
        return 0;
    }
    let taken = std::mem::take(messages);
    // Sized for the slack as well, so the frames that refill it do not double
    // the store on the way back up.
    let mut kept: Vec<Message> = Vec::with_capacity(CONVERSATION_CAP + CONVERSATION_SLACK);
    kept.extend(taken.into_iter().skip(front));
    *messages = kept;
    front
}

/// The newest window of `history`, as a copy to keep or hand over.
///
/// **Taken from the slice rather than cloned whole and cut**, which is the
/// difference between a window-sized allocation and a transcript-sized one:
/// what a connect hands over is the whole conversation, and the copy that
/// keeps it wants the tail.
pub fn tail_of(history: &[Message]) -> Vec<Message> {
    // Sized for the slack as well, so the frames that refill it do not double
    // the store on the way back up.
    let mut kept: Vec<Message> = Vec::with_capacity(CONVERSATION_CAP + CONVERSATION_SLACK);
    kept.extend(history[front_of(history)..].iter().cloned());
    kept
}

/// How many messages [`drop_past_cap`] takes off `messages`' front, without
/// taking them.
///
/// A caller that has to know what it is losing - the frames a transcript
/// carries no row for, which a numbering below the floor has to account for -
/// asks this before the drop and reads the front it names.
pub fn frames_dropped(messages: &[Message]) -> usize {
    front_of(messages)
}

/// Where the window's front moves to in `messages`: the first frame of the
/// oldest turn it keeps.
///
/// **The window ends at the end of a turn, so its front is a turn's first
/// frame.** `line` is where a count-based cut would land - `len` less the cap -
/// and the front is the first turn open at or above it, which keeps the window
/// whole turns and can come in slightly under the cap.
///
/// **A turn crossing the line rides into the slack.** The newest turn holds
/// the newest frames, so a window cannot drop it: when it alone is longer than
/// the cap, the window keeps it whole up to the cap's slack rather than cutting
/// inside it - and only when that single turn alone would push the window past
/// the slack is the cut made inside it, the one case with no whole turn to land
/// on. [`crate::conversation_turns`] answers where the turns open, and the walk
/// stops at the first one at or above the line.
fn front_of(messages: &[Message]) -> usize {
    let line = messages.len().saturating_sub(CONVERSATION_CAP);
    if line == 0 {
        return 0;
    }
    let mut scan = crate::conversation_turns::TurnScan::default();
    let mut newest = None;
    for (at, message) in messages.iter().enumerate() {
        if scan.opens(message) {
            if at >= line {
                return at;
            }
            newest = Some(at);
        }
    }
    match newest {
        Some(at) if messages.len() - at <= CONVERSATION_CAP + CONVERSATION_SLACK => at,
        // No turn THE SCAN CAN NAME leaves the window inside the slack: either
        // none opens at or above the line, or the newest one it names is
        // further back than the slack. The fold can still hold a head there -
        // a queued prompt opens a turn at it and this cannot read one - and
        // the count's own cut is what is left, inside whichever turn holds it.
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_frame(at: usize) -> Message {
        a_said(&format!("frame {at}"))
    }

    /// A user row carrying `text`.
    fn a_said(text: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": text},
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        }))
        .expect("a user frame")
    }

    fn said(message: &Message) -> String {
        let Message::User { message, .. } = message else {
            return String::new();
        };
        message
            .content
            .iter()
            .find_map(|block| match block {
                forge_primitives::ContentBlock::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// A user row carrying a queued prompt, which the fold opens a turn at
    /// unless a question card takes it.
    fn a_queued(text: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "queued_command", "prompt": text}],
            },
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        }))
        .expect("a user frame")
    }

    /// A frame inside a turn: a result answering the turn's own call, which
    /// the fold draws no turn of its own from.
    fn a_work_frame(at: usize, within: usize) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("tu{at}-{within}"),
                    "content": "ok",
                }],
            },
            "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        }))
        .expect("a user frame")
    }

    /// `turns` turns of `frames_each` frames: the user row that opens one,
    /// then frames that belong to it and open none of their own.
    fn a_history_of(turns: usize, frames_each: usize) -> Vec<Message> {
        let mut messages = Vec::with_capacity(turns * frames_each);
        for at in 0..turns {
            messages.push(a_said(&format!("turn {at}")));
            for within in 1..frames_each {
                messages.push(a_work_frame(at, within));
            }
        }
        messages
    }

    fn opens_a_turn(message: &Message) -> bool {
        crate::conversation_turns::TurnScan::default().opens(message)
    }

    /// **The window's front is a turn's first frame.** The frame the cap's own
    /// count would land on is inside a turn, so the drop walks up to the next
    /// turn and the window comes in under the cap.
    #[test]
    fn the_drop_lands_on_the_first_turn_at_or_above_the_line() {
        let mut messages = a_history_of(1_666, 3);
        messages.extend((0..2).map(|within| a_work_frame(9_999, within)));
        assert_eq!(
            messages.len(),
            CONVERSATION_CAP + CONVERSATION_SLACK,
            "precondition: the list is at the slack's edge, where an append drops",
        );

        let dropped = drop_past_cap(&mut messages);

        assert_eq!(
            dropped, 1_002,
            "the drop passes the count it had to shed to land on a turn's first frame",
        );
        assert_eq!(
            messages.len(),
            3_998,
            "and the window it leaves is whole turns at or under the cap",
        );
        assert!(messages.len() <= CONVERSATION_CAP, "never over the cap");
        assert_eq!(
            said(&messages[0]),
            "turn 334",
            "the oldest kept opens the window's oldest turn"
        );
        assert!(
            opens_a_turn(&messages[0]),
            "the frame the window starts on opens a turn, not the one the count landed in",
        );
    }

    /// A single turn crossing the line rides into the slack: the newest turn
    /// holds the newest frames and cannot be dropped, so a window that would
    /// cut inside it keeps it whole up toward the cap's slack instead.
    #[test]
    fn a_turn_crossing_the_line_rides_into_the_slack() {
        let mut messages = a_history_of(125, 4);
        messages.extend(a_history_of(1, 4_501));
        assert_eq!(messages.len(), 5_001, "precondition: one turn alone reaches over the cap");

        let dropped = drop_past_cap(&mut messages);

        assert_eq!(dropped, 500, "the crossing turn is kept whole");
        assert_eq!(
            messages.len(),
            4_501,
            "so the window rides over the cap rather than being cut inside the turn",
        );
        assert!(messages.len() <= CONVERSATION_CAP + CONVERSATION_SLACK, "and stays in the slack");
        assert_eq!(said(&messages[0]), "turn 0", "the window is that one turn");
    }

    /// **And a turn larger than the slack is cut inside, which is the one case
    /// with no whole turn to land on.** Keeping it would push the window past
    /// the slack the cap bounds it by; the newest turn alone is longer than
    /// that, so the cut is made inside it.
    #[test]
    fn a_turn_larger_than_the_slack_is_cut_inside() {
        let mut messages = a_history_of(125, 4);
        messages.extend(a_history_of(1, 5_500));
        assert_eq!(messages.len(), 6_000, "precondition: one turn is longer than the cap's slack");

        let dropped = drop_past_cap(&mut messages);

        assert_eq!(dropped, 2_000, "the cut is the count the cap holds");
        assert_eq!(messages.len(), CONVERSATION_CAP, "and the window comes out at the cap");
        assert!(
            !opens_a_turn(&messages[0]),
            "inside the turn, which is the only option when a single turn alone exceeds the slack",
        );
    }

    /// **A head the scan cannot read leaves the count's own cut, deliberately.**
    /// A queued prompt opens a turn in the fold, and the window's scan cannot
    /// see the card that decides whether it does - so a queued prompt at or
    /// above the line is a head the fold has and the scan does not. When the
    /// newest frame the scan CAN name is further back than the slack, the cut
    /// falls at the count and starts inside whatever turn holds it, which is
    /// the one thing the window's front is otherwise never allowed to do. That
    /// is what the pre-window cut did everywhere, and no real conversation
    /// measured has reached it.
    #[test]
    fn a_head_the_scan_cannot_name_leaves_the_count_cut() {
        let mut messages = vec![a_said("the only head the scan can name")];
        messages.extend((0..1_999).map(|within| a_work_frame(9_999, within)));
        messages.push(a_queued("a head the scan cannot read"));
        messages.extend((2_001..5_001).map(|within| a_work_frame(9_998, within)));
        assert_eq!(messages.len(), 5_001, "precondition: one queued head sits above the line");

        let dropped = drop_past_cap(&mut messages);

        assert_eq!(dropped, 1_001, "the cut is the count the cap holds, not a turn's first frame");
        assert_eq!(messages.len(), CONVERSATION_CAP, "and the window comes out at the cap");
        assert!(
            !opens_a_turn(&messages[0]),
            "inside the turn the fold opened at the head the scan could name, because the head \
             above the line is a queued prompt it cannot read",
        );
    }

    /// A frame the fold draws as something other than a turn never takes the
    /// cut: the window would start with a notice that opens nothing, and the
    /// turn behind it would draw with its head missing.
    #[test]
    fn a_frame_that_opens_no_turn_is_never_the_cut() {
        let mut messages = a_history_of(333, 3);
        // The run of deliveries the line lands in. Each draws as a notice -
        // and a queued prompt, which the fold opens a turn for unless a
        // question card takes it, is left alone too.
        messages
            .extend((0..4).map(|at| a_said(&format!("[Gotify - app 'x', priority 1]\n\n{at}"))));
        messages.push(
            serde_json::from_value(serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type": "queued_command", "prompt": "and this one"}],
                },
                "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
            }))
            .expect("a user frame"),
        );
        messages.extend(a_history_of(1_332, 3));
        assert_eq!(
            messages.len(),
            CONVERSATION_CAP + CONVERSATION_SLACK,
            "precondition: the line lands inside the run of frames that open no turn",
        );

        let dropped = drop_past_cap(&mut messages);

        assert_eq!(dropped, 1_004, "the cut passes every frame that opens no turn");
        assert!(opens_a_turn(&messages[0]), "and the window starts on a turn's own first frame");
    }

    /// The cap keeps the newest and reports what it dropped.
    #[test]
    fn a_copy_past_the_cap_drops_its_oldest_messages() {
        let mut at_cap: Vec<Message> = (0..CONVERSATION_CAP).map(a_frame).collect();
        assert_eq!(drop_past_cap(&mut at_cap), 0, "a copy at the cap is left alone");

        let mut messages: Vec<Message> =
            (0..CONVERSATION_CAP + CONVERSATION_SLACK).map(a_frame).collect();
        let dropped = drop_past_cap(&mut messages);

        assert_eq!(dropped, CONVERSATION_SLACK, "the drop keeps the cap and reports the rest");
        assert_eq!(
            messages.len(),
            CONVERSATION_CAP,
            "and what it left is the cap's worth of messages",
        );
        assert_eq!(
            said(&messages[0]),
            format!("frame {CONVERSATION_SLACK}"),
            "the oldest kept is the one the cap reaches back to",
        );
        assert_eq!(
            said(messages.last().expect("a newest")),
            format!("frame {}", CONVERSATION_CAP + CONVERSATION_SLACK - 1),
            "and the newest is the frame the copy ended on",
        );
    }

    /// **The store the drop frees is the point of it**, so this pins the shape
    /// rather than the length: a `drain` trims to the cap and keeps the
    /// transcript's buffer under it.
    #[test]
    fn a_copy_over_the_cap_is_held_in_a_window_sized_store() {
        let mut messages: Vec<Message> = (0..CONVERSATION_CAP * 4).map(a_frame).collect();

        drop_past_cap(&mut messages);

        assert_eq!(messages.len(), CONVERSATION_CAP, "the drop still keeps the cap");
        assert!(
            messages.capacity() <= CONVERSATION_CAP + CONVERSATION_SLACK,
            "the store it left is the window's rather than the history's: capacity for {} \
             messages over a history of {}",
            messages.capacity(),
            CONVERSATION_CAP * 4,
        );
    }

    /// A history anywhere near the cap is taken whole, and one over it loses
    /// only its front.
    #[test]
    fn the_tail_is_the_newest_window_and_nothing_else() {
        let short: Vec<Message> = (0..10).map(a_frame).collect();
        assert_eq!(tail_of(&short).len(), 10, "a history under the cap is kept whole");

        let long: Vec<Message> = (0..CONVERSATION_CAP * 2).map(a_frame).collect();
        let kept = tail_of(&long);
        assert_eq!(kept.len(), CONVERSATION_CAP, "an over-long history is taken as a window");
        assert_eq!(
            said(&kept[0]),
            format!("frame {CONVERSATION_CAP}"),
            "and the window is the newest one",
        );
        assert_eq!(
            said(kept.last().expect("a newest")),
            format!("frame {}", CONVERSATION_CAP * 2 - 1),
            "ending on the history's own last frame",
        );
    }
}

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

/// How far past the cap a copy may grow before the drop runs.
///
/// The drop rebuilds what it keeps into an allocation of its own, so running
/// it on every append past the cap would pay that rebuild per frame instead of
/// once per slack - and the slack costs a proportion of the cap, not a second
/// conversation.
pub const CONVERSATION_SLACK: usize = 1_000;

/// Drop the oldest messages until `messages` is back to the cap, and report
/// how many went.
///
/// **A fresh allocation rather than a `drain`**, because a `Vec` keeps the
/// store it grew to: the point of the cap is that a copy stops holding the
/// backing store of a conversation it no longer has, and a drain would leave
/// it holding all of it with a window's worth of messages in front.
pub fn drop_past_cap(messages: &mut Vec<Message>) -> usize {
    let excess = messages.len().saturating_sub(CONVERSATION_CAP);
    if excess == 0 {
        return 0;
    }
    let taken = std::mem::take(messages);
    // Sized for the slack as well, so the frames that refill it do not double
    // the store on the way back up.
    let mut kept: Vec<Message> = Vec::with_capacity(CONVERSATION_CAP + CONVERSATION_SLACK);
    kept.extend(taken.into_iter().skip(excess));
    *messages = kept;
    excess
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
    kept.extend(history[history.len().saturating_sub(CONVERSATION_CAP)..].iter().cloned());
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_frame(at: usize) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": format!("frame {at}")},
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

    /// The cap keeps the newest and reports what it dropped.
    #[test]
    fn a_copy_past_the_cap_drops_its_oldest_messages() {
        let mut at_cap: Vec<Message> = (0..CONVERSATION_CAP).map(a_frame).collect();
        assert_eq!(drop_past_cap(&mut at_cap), 0, "a copy at the cap is left alone");

        let mut messages: Vec<Message> =
            (0..CONVERSATION_CAP + CONVERSATION_SLACK).map(a_frame).collect();
        let dropped = drop_past_cap(&mut messages);

        assert_eq!(dropped, CONVERSATION_SLACK, "the drop keeps the cap and reports the rest");
        assert_eq!(messages.len(), CONVERSATION_CAP);
        assert_eq!(
            said(&messages[0]),
            format!("frame {CONVERSATION_SLACK}"),
            "the oldest kept is the one the cap reaches back to",
        );
        assert_eq!(
            said(messages.last().expect("a newest")),
            format!("frame {}", CONVERSATION_CAP + CONVERSATION_SLACK - 1),
        );
    }

    /// **The store the drop frees is the point of it**, so this pins the shape
    /// rather than the length: a `drain` trims to the cap and keeps the
    /// transcript's buffer under it.
    #[test]
    fn a_copy_over_the_cap_is_held_in_a_window_sized_store() {
        let mut messages: Vec<Message> = (0..CONVERSATION_CAP * 4).map(a_frame).collect();

        drop_past_cap(&mut messages);

        assert_eq!(messages.len(), CONVERSATION_CAP);
        assert!(
            messages.capacity() <= CONVERSATION_CAP + CONVERSATION_SLACK,
            "the store it left is the window's rather than the history's: capacity for {} \
             messages over a history of {}",
            messages.capacity(),
            CONVERSATION_CAP * 4,
        );
    }

    /// **The cap has a floor, and it is the window's own worst case.** Every
    /// page a client is handed is the newest twenty turns, so a cap that
    /// cannot reach back over twenty turns answers a page shorter than the one
    /// the subscriber promised. The number is the heaviest twenty-turn run
    /// measured across the eight largest transcripts on the author's machine
    /// (2026-10-05, see the cap's doc); a re-tune under it is a page that
    /// silently loses turns, and this is what says so.
    #[test]
    fn the_cap_reaches_past_the_heaviest_twenty_turn_run_measured() {
        const HEAVIEST_MEASURED: usize = 2_799;
        assert!(
            CONVERSATION_CAP >= HEAVIEST_MEASURED,
            "the cap ({CONVERSATION_CAP}) cannot reach back over the heaviest twenty-turn run \
             measured ({HEAVIEST_MEASURED})",
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
        assert_eq!(kept.len(), CONVERSATION_CAP);
        assert_eq!(said(&kept[0]), format!("frame {CONVERSATION_CAP}"), "the newest window");
        assert_eq!(
            said(kept.last().expect("a newest")),
            format!("frame {}", CONVERSATION_CAP * 2 - 1)
        );
    }
}

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
//! **A window is bounded by frames and by bytes.** The count alone let a giant
//! seat's window weigh 319 MiB (measured 2026-10-17 on the seat #1637 was
//! filed from) - and it is not the window's own weight that bites: a page is
//! cut from the window, so a ceiling ABOVE the newest twenty turns changes
//! nothing a reader feels. [`CONVERSATION_CAP_BYTES`] carries where the byte
//! ceiling's number comes from and what it deliberately changes on the seats
//! that reach it: a page there is the window whole rather than the newest
//! twenty turns, and the history above it is reached the way any window's is,
//! a `more` below the floor answered from the transcript.
//!
//! **And both cuts land on a turn's first frame.** A window starting part-way
//! through a turn serves its first turn with the head missing, so what the cap
//! sheds is counted to a frame a turn opens at - one cut, shared by both entry
//! points, and [`crate::conversation_turns`] is where a turn opens. The one
//! case with no whole turn to land on is a turn heavier than both ceilings
//! and their slack: there the cut is made inside it, and the frames it keeps
//! are the newest ones.

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
///
/// **The count is one of two ceilings.** [`CONVERSATION_CAP_BYTES`] bounds the
/// same window by weight, because on the seats that made #1637 four thousand
/// frames is not a bound a reader can feel.
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

/// How many bytes one copy of a conversation keeps, as frames weigh on the
/// wire.
///
/// **Sized from the seats that made #1637, and it binds only them.** Across
/// the seven largest transcripts on the author's machine (2026-10-17) every
/// window outside the two giant seats weighs 8.7-23.4 MiB and serves a
/// twenty-turn page of 0.8-5.2 MiB, while the giants' windows weigh 319 and
/// 467 MiB and their pages 95 and 105 MiB - about 2x under the client's
/// 256 MiB frame cap rather than orders under it. 64 MiB leaves every ordinary
/// seat untouched and comes down on exactly the giants, where the count alone
/// could not: what those seats serve on open becomes the window whole rather
/// than the newest twenty turns, and the rest of the history is reached the
/// way any window's is - a `more` below the floor, answered from the
/// transcript.
pub const CONVERSATION_CAP_BYTES: usize = 64 << 20;

/// How far past the byte cap a copy may grow before the drop runs. A quarter
/// of the cap, as the count's slack is, and it is also the room a turn
/// crossing the byte line is kept whole in.
pub const CONVERSATION_SLACK_BYTES: usize = 16 << 20;

/// The two ceilings one cut answers to, as a value.
///
/// The shipped numbers are [`WINDOW`]; the cut is a parameter over them so a
/// test can drive it at a ceiling a fixture can reach.
#[derive(Clone, Copy)]
struct Budget {
    /// Frames the window keeps, and how far past that a drop may run at.
    count: usize,
    count_slack: usize,
    /// Wire bytes the window keeps, and how far past that a drop may run at.
    bytes: usize,
    byte_slack: usize,
}

/// The ceilings the shipped window answers to.
const WINDOW: Budget = Budget {
    count: CONVERSATION_CAP,
    count_slack: CONVERSATION_SLACK,
    bytes: CONVERSATION_CAP_BYTES,
    byte_slack: CONVERSATION_SLACK_BYTES,
};

/// One conversation's newest window, and what each frame in it weighs.
///
/// **The weights are taken once, where a frame enters.** A copy's byte ceiling
/// is only a ceiling if it is known per frame: measuring the window at each
/// drop would serialize what the drop is about to throw away, and measuring
/// the history at each read is the transcript's own cost. So a frame is
/// measured as it is pushed, [`CONVERSATION_CAP_BYTES`] past which the drop
/// runs before the push, and what a seed keeps is what its cut measured on the
/// way in.
#[derive(Default)]
pub struct Window {
    messages: Vec<Message>,
    /// How each message weighs on the wire, one entry per message.
    sizes: Vec<usize>,
    /// The running total of `sizes`, so a push never walks the window.
    bytes: usize,
}

impl Window {
    /// A window holding nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// One frame the session emitted, and how many the ceilings shed for it.
    ///
    /// **Before the push, so the drop pays for the room it makes.** After it,
    /// this one frame is what tips the store into a doubling the drop is about
    /// to throw away.
    pub fn push(&mut self, message: Message) -> usize {
        self.push_with(message, WINDOW)
    }

    /// Take everything `other` holds onto this window's end.
    pub fn append(&mut self, other: &mut Window) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.messages.append(&mut other.messages);
        self.sizes.append(&mut other.sizes);
        other.bytes = 0;
    }

    /// The newest window of `history`, as a copy to keep or hand over, and how
    /// many frames it left behind.
    ///
    /// **Taken from the slice rather than cloned whole and cut**, which is the
    /// difference between a window-sized allocation and a transcript-sized
    /// one: what a connect hands over is the whole conversation, and the copy
    /// that keeps it wants the tail. **And nothing older than the ceilings is
    /// ever weighed**: the walk starts at the newest frame and stops once
    /// either ceiling's slack is spent, so a 2.8 GB read pays a window's worth
    /// of measuring rather than a transcript's.
    pub fn tail_of(history: &[Message]) -> (Window, usize) {
        Self::tail_of_with(history, WINDOW)
    }

    /// A frame on the end, weighed once, dropping first when a ceiling is
    /// already passed - the whole cut, drivable at a smaller ceiling.
    fn push_with(&mut self, message: Message, budget: Budget) -> usize {
        let dropped = if self.messages.len() >= budget.count + budget.count_slack
            || self.bytes >= budget.bytes + budget.byte_slack
        {
            self.drop_with(budget)
        } else {
            0
        };
        let size = wire_bytes(&message);
        self.bytes = self.bytes.saturating_add(size);
        self.sizes.push(size);
        self.messages.push(message);
        dropped
    }

    /// Drop the oldest messages until the window is back inside the ceilings,
    /// and report how many went.
    ///
    /// **A fresh allocation rather than a `drain`**, because a `Vec` keeps the
    /// store it grew to: the point of the cap is that a copy stops holding the
    /// backing store of a conversation it no longer has, and a drain would
    /// leave it holding all of it with a window's worth of messages in front.
    fn drop_with(&mut self, budget: Budget) -> usize {
        let front = front_of(&self.messages, &self.sizes, 0, budget);
        if front == 0 {
            return 0;
        }
        let taken = std::mem::take(&mut self.messages);
        let taken_sizes = std::mem::take(&mut self.sizes);
        // Sized for the slack as well, so the frames that refill it do not
        // double the store on the way back up.
        let mut kept: Vec<Message> = Vec::with_capacity(CONVERSATION_CAP + CONVERSATION_SLACK);
        kept.extend(taken.into_iter().skip(front));
        let mut kept_sizes: Vec<usize> = Vec::with_capacity(CONVERSATION_CAP + CONVERSATION_SLACK);
        kept_sizes.extend(taken_sizes.into_iter().skip(front));
        self.bytes = kept_sizes.iter().sum();
        self.messages = kept;
        self.sizes = kept_sizes;
        front
    }

    /// [`Self::tail_of`]'s body, at a ceiling a caller names.
    ///
    /// `sizes` covers `history[measured_from..]`: what the walk weighed, which
    /// is the newest frames up to whichever ceiling stopped it.
    fn tail_of_with(history: &[Message], budget: Budget) -> (Window, usize) {
        let mut sizes_newest_first: Vec<usize> = Vec::new();
        let mut bytes = 0usize;
        for message in history.iter().rev() {
            if sizes_newest_first.len() >= budget.count + budget.count_slack {
                break;
            }
            let size = wire_bytes(message);
            bytes = bytes.saturating_add(size);
            sizes_newest_first.push(size);
            if bytes > budget.bytes + budget.byte_slack {
                break;
            }
        }
        let measured_from = history.len() - sizes_newest_first.len();
        let mut sizes = sizes_newest_first;
        sizes.reverse();
        let front = front_of(history, &sizes, measured_from, budget);
        let kept_sizes = sizes[(front - measured_from)..].to_vec();
        let window = Window {
            bytes: kept_sizes.iter().sum(),
            sizes: kept_sizes,
            messages: history[front..].to_vec(),
        };
        (window, front)
    }
}

/// What a frame weighs on the wire: its JSON, which is what a page carries.
///
/// A `Message` serializes by construction - every shape crossed to get here -
/// so a short count is a frame that cannot cross either way, and it stays as
/// the count it reached rather than failing the frame around it.
fn wire_bytes(message: &Message) -> usize {
    let mut sink = Counted(0);
    serde_json::to_writer(&mut sink, message).map_or(sink.0, |()| sink.0)
}

/// A writer that only counts, so weighing a frame does not allocate a copy of
/// it.
struct Counted(usize);

impl std::io::Write for Counted {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Where the window's front moves to in `messages`: the first frame of the
/// oldest turn it keeps.
///
/// **The window ends at the end of a turn, so its front is a turn's first
/// frame.** `line` is where a count-based cut would land - `len` less the cap -
/// and the front is the first turn open at or above it, which keeps the window
/// whole turns and can come in slightly under the cap. The byte ceiling lands
/// the same way: its own line is the first frame whose tail weighs no more
/// than the ceiling, and the cut is made at whichever line reaches later, so
/// the window is inside both ceilings or a turn short of the later one.
///
/// **A turn crossing the line rides into the slack.** The newest turn holds
/// the newest frames, so a window cannot drop it: when it alone is longer than
/// the cap, the window keeps it whole up to the cap's slack rather than cutting
/// inside it - and only when that single turn alone would push the window past
/// the slack is the cut made inside it, the one case with no whole turn to land
/// on. [`crate::conversation_turns`] answers where the turns open, and the walk
/// stops at the first one at or above the line.
///
/// **The window is never left empty.** A single frame heavier than the whole
/// byte budget keeps the window at that one frame rather than at none: the
/// newest frame cannot be dropped - a page would open on nothing - and the
/// appends that follow shed it as soon as the tail fits again.
///
/// `sizes` is what each frame of `history[measured_from..]` weighs; nothing
/// older than that was measured, and nothing older than it can be kept (the
/// ceilings that would reach further back are inside the measure's own stop).
fn front_of(messages: &[Message], sizes: &[usize], measured_from: usize, budget: Budget) -> usize {
    let count_line = messages.len().saturating_sub(budget.count);
    let byte_line = measured_from + line_by_bytes(sizes, budget.bytes);
    let line = count_line.max(byte_line).min(messages.len().saturating_sub(1));
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
        Some(at)
            if messages.len() - at <= budget.count + budget.count_slack
                && at >= measured_from
                && weighted_from(sizes, at - measured_from) <= budget.bytes + budget.byte_slack =>
        {
            at
        }
        // No turn THE SCAN CAN NAME leaves the window inside the slack: either
        // none opens at or above the line, or the newest one it names is
        // further back than the slack. The fold can still hold a head there -
        // a queued prompt opens a turn at it and this cannot read one - and
        // the count's own cut is what is left, inside whichever turn holds it.
        //
        // `at < measured_from` is the same answer: the newest turn opened
        // below everything the walk weighed, so it alone is heavier than the
        // ceiling's slack and cannot ride.
        _ => line,
    }
}

/// The first frame of `sizes` whose tail weighs no more than `cap`.
///
/// The walk runs from the end and stops at the frame that tips the cap, so a
/// frame's own size is never summed past what the cut could keep.
fn line_by_bytes(sizes: &[usize], cap: usize) -> usize {
    let mut acc = 0usize;
    for (at, size) in sizes.iter().enumerate().rev() {
        acc = acc.saturating_add(*size);
        if acc > cap {
            return at + 1;
        }
    }
    0
}

/// What `sizes[from..]` weighs together.
fn weighted_from(sizes: &[usize], from: usize) -> usize {
    sizes[from..].iter().sum()
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

    /// A user row carrying a queued prompt, which the fold opens a turn at.
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

    /// A ceiling a fixture can reach: the shape of the shipped pair, at
    /// numbers a test's history can cross.
    fn a_small_ceiling() -> Budget {
        Budget { count: 8, count_slack: 2, bytes: 300, byte_slack: 60 }
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

        let (window, dropped) = Window::tail_of(&messages);

        assert_eq!(
            dropped, 1_002,
            "the drop passes the count it had to shed to land on a turn's first frame",
        );
        assert_eq!(
            window.len(),
            3_998,
            "and the window it leaves is whole turns at or under the cap",
        );
        assert!(window.len() <= CONVERSATION_CAP, "never over the cap");
        assert_eq!(
            said(&window.messages()[0]),
            "turn 334",
            "the oldest kept opens the window's oldest turn"
        );
        assert!(
            opens_a_turn(&window.messages()[0]),
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

        let (window, dropped) = Window::tail_of(&messages);

        assert_eq!(dropped, 500, "the crossing turn is kept whole");
        assert_eq!(
            window.len(),
            4_501,
            "so the window rides over the cap rather than being cut inside the turn",
        );
        assert!(window.len() <= CONVERSATION_CAP + CONVERSATION_SLACK, "and stays in the slack");
        assert_eq!(said(&window.messages()[0]), "turn 0", "the window is that one turn");
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

        let (window, dropped) = Window::tail_of(&messages);

        assert_eq!(dropped, 2_000, "the cut is the count the cap holds");
        assert_eq!(window.len(), CONVERSATION_CAP, "and the window comes out at the cap");
        assert!(
            !opens_a_turn(&window.messages()[0]),
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

        let (window, dropped) = Window::tail_of(&messages);

        assert_eq!(dropped, 1_001, "the cut is the count the cap holds, not a turn's first frame");
        assert_eq!(window.len(), CONVERSATION_CAP, "and the window comes out at the cap");
        assert!(
            !opens_a_turn(&window.messages()[0]),
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
        // and the queued prompt, which the fold opens a turn for and the scan
        // leaves alone, is one too.
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

        let (window, dropped) = Window::tail_of(&messages);

        assert_eq!(dropped, 1_004, "the cut passes every frame that opens no turn");
        assert!(
            opens_a_turn(&window.messages()[0]),
            "and the window starts on a turn's own first frame"
        );
    }

    /// The cap keeps the newest and reports what it dropped.
    #[test]
    fn a_copy_past_the_cap_drops_its_oldest_messages() {
        let at_cap: Vec<Message> = (0..CONVERSATION_CAP).map(a_frame).collect();
        assert_eq!(Window::tail_of(&at_cap).1, 0, "a copy at the cap is left alone");

        let messages: Vec<Message> =
            (0..CONVERSATION_CAP + CONVERSATION_SLACK).map(a_frame).collect();
        let (window, dropped) = Window::tail_of(&messages);

        assert_eq!(dropped, CONVERSATION_SLACK, "the drop keeps the cap and reports the rest");
        assert_eq!(
            window.len(),
            CONVERSATION_CAP,
            "and what it left is the cap's worth of messages",
        );
        assert_eq!(
            said(&window.messages()[0]),
            format!("frame {CONVERSATION_SLACK}"),
            "the oldest kept is the one the cap reaches back to",
        );
        assert_eq!(
            said(window.messages().last().expect("a newest")),
            format!("frame {}", CONVERSATION_CAP + CONVERSATION_SLACK - 1),
            "and the newest is the frame the copy ended on",
        );
    }

    /// **The store the drop frees is the point of it**, so this pins the shape
    /// rather than the length: a `drain` trims to the cap and keeps the
    /// transcript's buffer under it.
    #[test]
    fn a_copy_over_the_cap_is_held_in_a_window_sized_store() {
        let messages: Vec<Message> = (0..CONVERSATION_CAP * 4).map(a_frame).collect();

        let (window, _) = Window::tail_of(&messages);

        assert_eq!(window.len(), CONVERSATION_CAP, "the drop still keeps the cap");
        assert!(
            window.messages.capacity() <= CONVERSATION_CAP + CONVERSATION_SLACK,
            "the store it left is the window's rather than the history's: capacity for {} \
             messages over a history of {}",
            window.messages.capacity(),
            CONVERSATION_CAP * 4,
        );
    }

    /// A history anywhere near the cap is taken whole, and one over it loses
    /// only its front.
    #[test]
    fn the_tail_is_the_newest_window_and_nothing_else() {
        let short: Vec<Message> = (0..10).map(a_frame).collect();
        assert_eq!(Window::tail_of(&short).0.len(), 10, "a history under the cap is kept whole");

        let long: Vec<Message> = (0..CONVERSATION_CAP * 2).map(a_frame).collect();
        let (window, dropped) = Window::tail_of(&long);
        assert_eq!(window.len(), CONVERSATION_CAP, "an over-long history is taken as a window");
        assert_eq!(dropped, CONVERSATION_CAP, "and the front it left is reported");
        assert_eq!(
            said(&window.messages()[0]),
            format!("frame {CONVERSATION_CAP}"),
            "and the window is the newest one",
        );
        assert_eq!(
            said(window.messages().last().expect("a newest")),
            format!("frame {}", CONVERSATION_CAP * 2 - 1),
            "ending on the history's own last frame",
        );
    }

    /// **A frame is weighed as the JSON a page carries it as**, which is what
    /// the byte ceiling counts: the measured weight of anything else (a
    /// `Value`, an estimate) would be a ceiling over a number nothing pays.
    #[test]
    fn a_frame_is_weighed_as_the_json_it_crosses_as() {
        for message in [a_said("hello"), a_work_frame(1, 1), a_queued("queued")] {
            assert_eq!(
                wire_bytes(&message),
                serde_json::to_vec(&message).expect("a frame encodes").len(),
                "the weight is the frame's own JSON: {message:?}",
            );
        }
    }

    /// The shipped ceilings are the budget the window actually consults - a
    /// wiring that named the count cap where the byte cap belongs would type
    /// check and silently uncap the bytes.
    #[test]
    fn the_shipped_budget_is_the_shipped_ceilings() {
        assert_eq!(WINDOW.count, CONVERSATION_CAP);
        assert_eq!(WINDOW.count_slack, CONVERSATION_SLACK);
        assert_eq!(WINDOW.bytes, CONVERSATION_CAP_BYTES);
        assert_eq!(WINDOW.byte_slack, CONVERSATION_SLACK_BYTES);
    }

    /// **The byte ceiling binds where the count never could.** A history well
    /// under the count cap but over the byte ceiling keeps only the newest
    /// frames that fit, landing on a turn's first frame like the count's own
    /// cut does.
    #[test]
    fn the_byte_ceiling_binds_under_the_count_cap() {
        let messages = a_history_of(10, 2);
        let sizes = vec![100usize; messages.len()];
        let ceiling = a_small_ceiling();

        let front = front_of(&messages, &sizes, 0, ceiling);

        assert_eq!(front, 18, "the newest two frames are what the byte ceiling fits");
        assert_eq!(
            weighted_from(&sizes, front),
            200,
            "and the count cap had nothing to say here: it would have kept eight",
        );
        assert!(opens_a_turn(&messages[front]), "landed on a turn's own first frame");
    }

    /// The count still takes the cut when it is the lighter ceiling, and the
    /// byte ceiling whatever a fixture makes it: the two lines compose, and
    /// the later one wins.
    #[test]
    fn the_later_ceiling_takes_the_cut() {
        let messages = a_history_of(50, 2);
        let ceiling = a_small_ceiling();

        let by_count = front_of(&messages, &vec![1usize; messages.len()], 0, ceiling);
        assert_eq!(by_count, messages.len() - 8, "small frames leave the count cap to bind");

        let by_bytes = front_of(&messages, &vec![100usize; messages.len()], 0, ceiling);
        assert_eq!(by_bytes, messages.len() - 2, "heavy ones leave the byte ceiling to bind");
        assert!(
            opens_a_turn(&messages[by_bytes]),
            "and the byte's cut lands on a turn's own first frame too",
        );
    }

    /// **A turn crossing the byte line rides into the byte slack**, the way a
    /// crossing turn rides the count's: the newest turn holds the newest
    /// frames, so a window that would cut inside it keeps it whole up to the
    /// ceiling's slack.
    #[test]
    fn a_turn_crossing_the_byte_line_rides_into_the_byte_slack() {
        let messages = a_history_of(3, 5);
        // The newest turn is frames 10..15; 350 bytes is over the 300 ceiling
        // but inside its slack.
        let mut sizes = vec![1usize; 10];
        sizes.extend([70usize; 5]);
        let ceiling = a_small_ceiling();

        let front = front_of(&messages, &sizes, 0, ceiling);

        assert_eq!(front, 10, "the crossing turn is kept whole, over the ceiling rather than cut");
        assert_eq!(weighted_from(&sizes, front), 350, "and the window is that one turn");
    }

    /// **A turn heavier than the byte ceiling's slack is cut inside**, and the
    /// count's own slack is not what decides it: a turn inside the count's
    /// slack but past the byte's cannot ride, or the byte ceiling would be
    /// defeated by exactly the turns the seats it exists for run.
    #[test]
    fn a_turn_past_the_byte_slack_is_cut_inside() {
        let messages = a_history_of(3, 5);
        // The newest turn is frames 10..15; 600 bytes is past the 300 ceiling
        // and past its slack.
        let mut sizes = vec![1usize; 10];
        sizes.extend([120usize; 5]);
        let ceiling = a_small_ceiling();

        let front = front_of(&messages, &sizes, 0, ceiling);

        assert_eq!(front, 13, "the cut is made inside the turn, at the byte ceiling's own line");
        assert!(
            !opens_a_turn(&messages[front]),
            "which the count's slack had nothing to say about",
        );
    }

    /// **A single frame heavier than the whole budget keeps the window at that
    /// one frame rather than at none.** A page opening on nothing is worse
    /// than a window one frame over: the appends that follow shed it as soon
    /// as the tail fits again.
    #[test]
    fn a_frame_heavier_than_the_whole_budget_stays_alone() {
        let messages = a_history_of(2, 2);
        let mut sizes = vec![10usize; messages.len()];
        *sizes.last_mut().expect("a newest") = 1_000;
        let ceiling = a_small_ceiling();

        let front = front_of(&messages, &sizes, 0, ceiling);

        assert_eq!(front, messages.len() - 1, "the newest frame is never dropped");
    }

    /// **A push past the byte ceiling drops before it, keeping the newest
    /// frames that fit.** The ceiling is consulted on the running total, so it
    /// binds between drops rather than only when the count's own slack runs
    /// out - which on a giant seat is where the window's weight lives.
    #[test]
    fn a_push_past_the_byte_ceiling_drops_the_oldest() {
        let ceiling = Budget { count: 1_000, count_slack: 100, bytes: 2_000, byte_slack: 500 };
        let mut window = Window::empty();
        for _ in 0..40 {
            window.push_with(a_said(&"x".repeat(200)), ceiling);
        }

        let held: usize = window.messages().iter().map(wire_bytes).sum();
        assert_eq!(window.bytes, held, "the running total is the window's own weight");
        assert!(
            window.bytes <= ceiling.bytes + ceiling.byte_slack,
            "a push never leaves the window past the ceiling's slack: {} bytes",
            window.bytes,
        );
        assert!(window.len() < 40, "so frames have been shed");
        assert_eq!(
            said(window.messages().last().expect("a newest")),
            "x".repeat(200),
            "and the newest frame is the one just pushed",
        );
    }

    /// **A seed weighs only what a cut could keep.** The walk starts at the
    /// newest frame and stops once a ceiling's slack is spent, so the frames
    /// above the stop are copied and sized while the ones below are neither -
    /// which is what makes a 2.8 GB read cost a window rather than a
    /// transcript.
    #[test]
    fn a_seed_keeps_what_the_ceilings_reach_and_no_more() {
        let messages = a_history_of(100, 1);
        let ceiling = Budget { count: 8, count_slack: 2, bytes: usize::MAX, byte_slack: 0 };

        let (window, dropped) = Window::tail_of_with(&messages, ceiling);

        assert_eq!(window.len(), 8, "the count cap is what the window keeps");
        assert_eq!(dropped, 92, "and everything below the measure's stop is reported gone");
        assert_eq!(
            said(&window.messages()[0]),
            "turn 92",
            "the window is the newest turns, sized by the ceiling rather than the history",
        );
        assert_eq!(window.sizes.len(), window.len(), "every kept frame carries its weight");
        assert_eq!(window.bytes, window.sizes.iter().sum::<usize>(), "and the total is theirs");
    }

    /// **The push path weighs a frame once and keeps the weights aligned.**
    /// The drop rebuilds the window from what was measured, so a sizes vec out
    /// of step with the messages would make every later cut land on the wrong
    /// frame - after an append, after a drop, and after a merge.
    #[test]
    fn a_window_keeps_the_weights_aligned_with_the_messages() {
        let ceiling = Budget { count: 6, count_slack: 2, bytes: usize::MAX, byte_slack: 0 };
        let mut window = Window::empty();
        for at in 0..20 {
            window.push_with(a_frame(at), ceiling);
        }
        assert!(window.len() <= 8, "precondition: the count ceiling bound the window");

        let mut arrivals = Window::empty();
        arrivals.push_with(a_frame(100), ceiling);
        arrivals.push_with(a_frame(101), ceiling);
        window.append(&mut arrivals);

        assert_eq!(window.sizes.len(), window.messages().len(), "one weight per message");
        assert_eq!(
            window.sizes.clone(),
            window.messages().iter().map(wire_bytes).collect::<Vec<_>>(),
            "each weight is that message's own",
        );
        assert_eq!(window.bytes, window.sizes.iter().sum::<usize>(), "and the total is theirs");
        assert!(arrivals.is_empty() && arrivals.bytes == 0, "the donor is left empty");
    }
}

//! When a connection's stream goes out.
//!
//! A turn's frames arrive in bursts, and most of a storm is one kind: the CLI
//! reports its thinking tokens about every 50 of them. A batch waits for the
//! shorter of two intervals, folds the token appends it holds into the single
//! frame they draw as, and writes the rest out as they came - so the frame
//! count a storm produces drops, and what any frame means does not.

use std::time::{Duration, Instant};

use axum::extract::ws::Message;
use futures_util::SinkExt;

use crate::SessionUpdate;
use crate::delivery::delivery_turn;
use crate::transport::envelope::ServerMessage;

/// How long a batch waits for the updates behind it before it goes out.
///
/// One paint: a view that draws at the frame rate sees a batch in its next
/// frame whether it waited or not.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(16);

/// The longest an update waits for company, however busy the stream is.
///
/// A turn emitting continuously never leaves the window quiet, so without
/// this a batch would be held for as long as the turn runs.
pub const FLUSH_CEILING: Duration = Duration::from_millis(100);

/// The updates waiting for one write, and when they have waited long enough.
#[derive(Default)]
pub struct Batch {
    held: Vec<SessionUpdate>,
    /// When the first held update landed, which the ceiling counts from.
    opened: Option<Instant>,
    /// When the window the last arrival opened closes.
    quiet: Option<Instant>,
}

impl Batch {
    /// Hold one update, and hold the window open from its arrival.
    pub fn push(&mut self, at: Instant, update: SessionUpdate) {
        if self.held.is_empty() {
            self.opened = Some(at);
        }
        self.quiet = Some(at + FLUSH_INTERVAL);
        self.held.push(update);
    }

    /// When the held updates go out: the window the last arrival opened,
    /// unless the ceiling is nearer.
    ///
    /// `None` with nothing held, which is what keeps a caller from waiting on
    /// a timer it has no reason to arm.
    pub fn due(&self) -> Option<Instant> {
        let opened = self.opened?;
        let quiet = self.quiet?;
        Some(quiet.min(opened + FLUSH_CEILING))
    }

    /// Take what is held, leaving the batch empty.
    pub fn take(&mut self) -> Vec<SessionUpdate> {
        self.opened = None;
        self.quiet = None;
        std::mem::take(&mut self.held)
    }
}

/// Fold a run of token appends into the one frame they draw as, leaving every
/// other update where it is.
///
/// Both views draw a turn's estimate as the SUM of the deltas, because
/// `estimated_tokens` restarts at each thinking block and the running value is
/// not a turn total - so one frame carrying the sum draws exactly what the run
/// drew, and the run itself is what a storm is made of.
fn coalesce(updates: Vec<SessionUpdate>) -> Vec<SessionUpdate> {
    let mut run: Vec<SessionUpdate> = Vec::with_capacity(updates.len());
    for update in updates {
        if !run.last_mut().is_some_and(|last| merge(last, &update)) {
            run.push(update);
        }
    }
    run
}

/// Fold `update` into `last` when the two are consecutive token appends for
/// one seat, and answer whether it did.
///
/// Everything but the growth is the arrival being folded in: the running
/// value, the id and the session are that frame's own, and only the counter is
/// the run's.
fn merge(last: &mut SessionUpdate, update: &SessionUpdate) -> bool {
    let SessionUpdate::ChatAppended { key, origin, msg } = update else {
        return false;
    };
    let forge_primitives::Message::ThinkingTokens {
        estimated_tokens,
        estimated_tokens_delta,
        uuid,
        session_id,
        extras,
    } = msg
    else {
        return false;
    };
    let SessionUpdate::ChatAppended { key: held_key, origin: held_origin, msg: held_msg } = last
    else {
        return false;
    };
    // The occupant needs no comparing beside the seat: a change of session
    // rides `SessionReplaced`, which is not a counter and breaks the run.
    if held_key != key || held_origin != origin {
        return false;
    }
    let forge_primitives::Message::ThinkingTokens { estimated_tokens_delta: held_delta, .. } =
        held_msg
    else {
        return false;
    };

    let summed = held_delta.saturating_add(*estimated_tokens_delta);
    // The id, the session and the extras are the surviving frame's own -
    // the newest is the one the fold keeps, and a fact it carried must
    // survive the fold rather than being rebuilt away.
    *held_msg = forge_primitives::Message::ThinkingTokens {
        estimated_tokens: *estimated_tokens,
        estimated_tokens_delta: summed,
        uuid: uuid.clone(),
        session_id: session_id.clone(),
        extras: extras.clone(),
    };
    true
}

/// Write a run of updates to a client: every one its own frame - the token
/// appends among them folded into the one frame they draw as - in the order
/// given, and ONE flush for the lot.
///
/// A run is a batch drained by its deadline, or the backlog a subscribe is
/// handed before its snapshot. Nothing held is nothing to write, which the
/// caller relies on when it clears the way for an answer.
pub async fn flush<S>(socket: &mut S, updates: Vec<SessionUpdate>) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    if updates.is_empty() {
        return Ok(());
    }
    for update in coalesce(updates) {
        // A delivery is drawn as the turn it reaches the model as, and the
        // CLI echoes no prompt back: a client drawing only frames would show
        // the assistant answering something nobody saw.
        if let Some(key) = update.slot().cloned()
            && let Some(msg) = delivery_turn(&update, &key)
        {
            feed(
                socket,
                ServerMessage::Update {
                    update: Box::new(SessionUpdate::ChatAppended { key, msg, origin: None }),
                },
            )
            .await?;
        }
        feed(socket, ServerMessage::Update { update: Box::new(update) }).await?;
    }
    socket.flush().await?;
    Ok(())
}

/// Put one message in the socket's buffer, where the batch's flush picks it
/// up.
async fn feed<S>(socket: &mut S, message: ServerMessage) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    let text = serde_json::to_string(&message)?;
    socket.feed(Message::Text(text.into())).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};

    use forge_primitives::SessionSlot;
    use forge_primitives::cloud::service_status::ServiceSeverity;
    use futures_util::Sink;

    use super::*;
    use crate::SessionUpdate;
    use crate::transport::envelope::ServerMessage;

    /// One update, told from another by the name it carries.
    fn status(message: &str) -> SessionUpdate {
        SessionUpdate::ServiceStatus {
            severity: ServiceSeverity::Warning,
            message: message.to_owned(),
        }
    }

    /// What a run of updates carries, in the order it was given.
    fn names(updates: &[SessionUpdate]) -> Vec<&str> {
        updates
            .iter()
            .map(|update| match update {
                SessionUpdate::ServiceStatus { message, .. } => message.as_str(),
                other => panic!("a batch carries what was put in it, and this is {other:?}"),
            })
            .collect()
    }

    /// A sink that keeps the frames it was fed and counts its flushes.
    ///
    /// The count is the property under test: one flush per batch rather than
    /// one per frame is all a client could measure of this.
    #[derive(Default)]
    struct Recording {
        fed: Vec<Message>,
        flushes: usize,
    }

    impl Sink<Message> for Recording {
        type Error = axum::Error;

        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            self.fed.push(item);
            Ok(())
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            self.flushes += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    /// What each fed frame carries, read through the shape a client reads.
    fn frames(sink: &Recording) -> Vec<SessionUpdate> {
        sink.fed
            .iter()
            .map(|frame| {
                let Message::Text(text) = frame else {
                    panic!("the stream crosses as text, and this is {frame:?}")
                };
                let ServerMessage::Update { update } =
                    serde_json::from_str(text.as_str()).expect("a frame a client decodes")
                else {
                    panic!("every frame a batch writes is an update");
                };
                *update
            })
            .collect()
    }

    /// The window: each arrival holds the batch open for another interval, so
    /// a burst leaves together rather than one update per interval.
    #[test]
    fn an_arrival_holds_the_batch_open_for_another_interval() {
        let base = Instant::now();
        let mut batch = Batch::default();

        batch.push(base, status("one"));
        assert_eq!(
            batch.due(),
            Some(base + FLUSH_INTERVAL),
            "a lone update goes out one interval after it lands",
        );

        let later = base + Duration::from_millis(5);
        batch.push(later, status("two"));
        assert_eq!(
            batch.due(),
            Some(later + FLUSH_INTERVAL),
            "and the update behind it holds the batch open from its own arrival",
        );
    }

    /// The ceiling: a stream that never leaves the window quiet - a turn
    /// emitting every few milliseconds - still goes out, and the ceiling is
    /// what decides when.
    #[test]
    fn a_stream_that_never_goes_quiet_goes_out_at_the_ceiling() {
        let base = Instant::now();
        let mut batch = Batch::default();

        let mut at = base;
        for step in 0..=20 {
            batch.push(at, status(&format!("frame {step}")));
            at += Duration::from_millis(5);
        }

        assert_eq!(
            batch.due(),
            Some(base + FLUSH_CEILING),
            "the ceiling decides, not the last arrival, so a busy batch is never held past it",
        );
    }

    /// Taking a batch hands back what it holds in the order it was given, and
    /// leaves nothing behind.
    #[test]
    fn taking_a_batch_hands_back_its_updates_in_order_and_clears_it() {
        let base = Instant::now();
        let mut batch = Batch::default();
        for name in ["one", "two", "three"] {
            batch.push(base, status(name));
        }

        let taken = batch.take();

        assert_eq!(
            names(&taken),
            ["one", "two", "three"],
            "the batch keeps the order the core emitted",
        );
        assert!(
            batch.take().is_empty(),
            "and taking it leaves nothing for a second flush to write",
        );
        assert_eq!(batch.due(), None, "with nothing held there is no deadline to keep");
    }

    /// The write itself: every update its own frame, in order, and one flush
    /// for the lot.
    #[tokio::test]
    async fn a_batch_goes_out_as_its_updates_and_one_flush() {
        let base = Instant::now();
        let mut batch = Batch::default();
        for name in ["one", "two", "three"] {
            batch.push(base, status(name));
        }
        let mut sink = Recording::default();

        flush(&mut sink, batch.take()).await.expect("the batch writes");

        assert_eq!(sink.flushes, 1, "a batch is one flush on the socket, not one per update");
        assert_eq!(
            names(&frames(&sink)),
            ["one", "two", "three"],
            "and every update crosses as its own frame, in the order it was emitted",
        );
    }

    /// One token append, as the CLI sends them: the running value the block
    /// has reached and the growth since the previous event. Both restart at
    /// each thinking block, so a run of them is a turn's estimate only as a
    /// sum of its deltas.
    fn token(seat: &SessionSlot, running: u64, delta: i64, uuid: &str) -> SessionUpdate {
        SessionUpdate::ChatAppended {
            key: seat.clone(),
            origin: None,
            msg: forge_primitives::Message::ThinkingTokens {
                estimated_tokens: running,
                estimated_tokens_delta: delta,
                uuid: uuid.to_owned(),
                session_id: "s".to_owned(),
                extras: serde_json::Map::new(),
            },
        }
    }

    /// The id a token frame carries, which the frame a run folds into has to
    /// name.
    fn naming(update: &SessionUpdate) -> (&str, &str) {
        match update {
            SessionUpdate::ChatAppended {
                msg: forge_primitives::Message::ThinkingTokens { uuid, session_id, .. },
                ..
            } => (uuid, session_id),
            other => panic!("a token frame names itself, and this is {other:?}"),
        }
    }

    /// One frame, said shortly enough to compare a run of them.
    fn describe(update: &SessionUpdate) -> String {
        match update {
            SessionUpdate::ChatAppended {
                key,
                msg:
                    forge_primitives::Message::ThinkingTokens {
                        estimated_tokens,
                        estimated_tokens_delta,
                        ..
                    },
                ..
            } => format!(
                "{}: {estimated_tokens} running, {estimated_tokens_delta} grown",
                key.label(),
            ),
            SessionUpdate::ChatAppended { key, .. } => {
                format!("{}: a frame that is not a counter", key.label())
            }
            SessionUpdate::TurnCancelled { key } => format!("{}: cancelled", key.label()),
            other => format!("{other:?}"),
        }
    }

    /// A run of token appends inside one batch leaves as ONE frame carrying
    /// the sum of what they grew by - the whole point of the item, and what
    /// makes the frames a client receives fewer than the frames the CLI sent.
    #[tokio::test]
    async fn a_run_of_token_appends_leaves_as_one_frame_carrying_their_sum() {
        let seat = SessionSlot::lead("Org", "forge");
        let mut sink = Recording::default();

        // Two blocks' worth, so the last running value is not the total: the
        // second block restarts at 30, where the deltas sum to 280.
        flush(
            &mut sink,
            vec![
                token(&seat, 200, 200, "a"),
                token(&seat, 250, 50, "b"),
                token(&seat, 30, 30, "c"),
            ],
        )
        .await
        .expect("the batch writes");

        assert_eq!(sink.flushes, 1, "the run is one flush, as any batch is");
        assert_eq!(sink.fed.len(), 1, "and one frame where the CLI sent three");
        assert_eq!(
            describe(&frames(&sink)[0]),
            "lead: 30 running, 280 grown",
            "carrying the last arrival's running value and the deltas summed",
        );
        assert_eq!(
            naming(&frames(&sink)[0]),
            ("c", "s"),
            "and the id of the arrival the run ended on, which is the one the frame stands for",
        );
    }

    /// The merge's guard, both halves: an update of another kind between two
    /// appends keeps them apart, and so does one for another seat.
    ///
    /// The first is what keeps everything else in order - nothing but a
    /// counter may be moved or absorbed. The second is what keeps two seats'
    /// estimates out of each other's rows.
    #[tokio::test]
    async fn only_consecutive_token_appends_for_one_seat_merge() {
        let seat = SessionSlot::lead("Org", "forge");
        let elsewhere = SessionSlot::lead("Org", "other");
        let mut sink = Recording::default();

        flush(
            &mut sink,
            vec![
                token(&seat, 200, 200, "a"),
                SessionUpdate::TurnCancelled { key: seat.clone() },
                token(&seat, 250, 50, "b"),
                token(&seat, 30, 30, "c"),
                token(&elsewhere, 10, 10, "d"),
            ],
        )
        .await
        .expect("the batch writes");

        let said: Vec<String> = frames(&sink).iter().map(describe).collect();
        assert_eq!(
            said,
            [
                "lead: 200 running, 200 grown",
                "lead: cancelled",
                "lead: 30 running, 80 grown",
                "lead: 10 running, 10 grown",
            ],
            "the run the cancelled frame interrupted, and another seat's counter, both stay whole",
        );
    }
}

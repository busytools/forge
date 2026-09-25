//! Fan-out for the workspace's [`SessionUpdate`] stream.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::protocol::SessionUpdate;

/// What a subscriber can do with what it is sent.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriberRole {
    /// Renders the workspace's prompts - permission, question, Slack
    /// draft - and answers them. The TUI's role, and the one
    /// [`UpdateFanout::send_answering`] reports on.
    Answering,
    /// Reads the stream only. A prompt delivered to observers alone is
    /// one nobody will answer, so a path that parks a turn on an answer
    /// must not count this subscriber.
    Observing,
}

/// One subscriber's end of the fan-out.
struct Registration {
    tx: mpsc::UnboundedSender<SessionUpdate>,
    role: SubscriberRole,
}

#[derive(Default)]
struct Shared {
    subscribers: Vec<Registration>,
    /// Updates emitted before the first subscriber attached, held for
    /// whoever attaches first so a notice raised during boot is not lost.
    pending: Vec<SessionUpdate>,
    /// Set by the first [`UpdateFanout::subscribe`]. After that a send
    /// that reaches nobody is dropped rather than held, so a subscriber
    /// attaching later inherits no backlog.
    attached: bool,
}

/// Cloneable sender half of the workspace's fan-out to its subscribers.
///
/// Every clone shares one registry. [`Self::send`] hands the update to
/// each subscriber and drops any whose receiver has gone, so the
/// registry never keeps a dead one.
#[derive(Clone, Default)]
pub(crate) struct UpdateFanout {
    shared: Arc<Mutex<Shared>>,
}

impl UpdateFanout {
    /// Mint a subscriber stream, declaring what the caller can do with
    /// it. The first caller also takes whatever was emitted before it
    /// attached; every later one carries what is emitted after its own
    /// call and nothing before it.
    pub(crate) fn subscribe(&self, role: SubscriberRole) -> mpsc::UnboundedReceiver<SessionUpdate> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut shared = self.shared.lock();
        for held in std::mem::take(&mut shared.pending) {
            let _ = tx.send(held);
        }
        shared.attached = true;
        shared.subscribers.push(Registration { tx, role });
        rx
    }

    /// Deliver `update` to every subscriber, and report whether one took
    /// it. `false` is the "no receiver" signal the single-take channel
    /// gave, which `spawn::try_emit` and `SessionTask::emit` log on; an
    /// update emitted before anything attached is held for the first
    /// subscriber and reports `false` too, since no subscriber has it.
    ///
    /// The last subscriber is taken out of the registry to receive
    /// `update` itself while the rest are sent a copy, so the common
    /// one-subscriber case clones nothing. It goes back only if it took
    /// the update, which is also where a dead one leaves.
    pub(crate) fn send(&self, update: SessionUpdate) -> bool {
        self.deliver(update, None)
    }

    /// Deliver `update` to every subscriber, and report whether one that
    /// can ANSWER it took it. A path that parks a turn on an answer uses
    /// this rather than [`Self::send`]: an observer takes the update and
    /// never replies, so counting it would leave the turn waiting on a
    /// response nobody will send.
    pub(crate) fn send_answering(&self, update: SessionUpdate) -> bool {
        self.deliver(update, Some(SubscriberRole::Answering))
    }

    fn deliver(&self, update: SessionUpdate, required: Option<SubscriberRole>) -> bool {
        let mut shared = self.shared.lock();
        let Some(tail) = shared.subscribers.pop() else {
            if !shared.attached {
                shared.pending.push(update);
            }
            return false;
        };
        let counts = |role: SubscriberRole| required.is_none_or(|want| want == role);
        let mut took_it = false;
        shared.subscribers.retain(|subscriber| {
            let live = subscriber.tx.send(update.clone()).is_ok();
            took_it |= live && counts(subscriber.role);
            live
        });
        if tail.tx.send(update).is_ok() {
            took_it |= counts(tail.role);
            shared.subscribers.push(tail);
        }
        took_it
    }
}

#[cfg(test)]
mod tests {
    use forge_primitives::cloud::service_status::ServiceSeverity;

    use super::*;

    fn status(message: &str) -> SessionUpdate {
        SessionUpdate::ServiceStatus {
            severity: ServiceSeverity::Warning,
            message: message.to_owned(),
        }
    }

    fn next(rx: &mut mpsc::UnboundedReceiver<SessionUpdate>, who: &str) -> String {
        match rx.try_recv() {
            Ok(SessionUpdate::ServiceStatus { message, .. }) => message,
            Ok(_) => panic!("expected the ServiceStatus envelope this test sends to {who}"),
            Err(err) => panic!("expected an update on the {who} subscriber's stream: {err}"),
        }
    }

    /// Catches `send` delivering to only the first subscriber, which is
    /// the single-take behaviour this replaces.
    #[test]
    fn each_subscriber_receives_every_update() {
        let fanout = UpdateFanout::default();
        let mut first = fanout.subscribe(SubscriberRole::Answering);
        let mut second = fanout.subscribe(SubscriberRole::Answering);

        assert!(fanout.send(status("one")), "a subscribed fan-out delivers");

        assert_eq!(next(&mut first, "first"), "one", "the first subscriber sees the update");
        assert_eq!(
            next(&mut second, "second"),
            "one",
            "the second subscriber sees the same update"
        );
    }

    /// Catches dropping the prune in `send`, which leaves a dead sender
    /// in the registry for the life of the workspace. The dropped
    /// subscriber is subscribed FIRST, so it is not the one `send` takes
    /// out as the tail and the prune is the only thing that can remove
    /// it.
    #[test]
    fn a_subscriber_that_drops_leaves_the_fan_out() {
        let fanout = UpdateFanout::default();
        let dropped = fanout.subscribe(SubscriberRole::Answering);
        let mut kept = fanout.subscribe(SubscriberRole::Answering);
        drop(dropped);

        assert!(fanout.send(status("one")), "the live subscriber still receives");

        assert_eq!(
            fanout.shared.lock().subscribers.len(),
            1,
            "the dropped subscriber is gone from the registry",
        );
        assert_eq!(next(&mut kept, "kept"), "one", "the live subscriber still receives");
    }

    /// Catches buffering emitted updates and replaying them to the next
    /// subscriber, which hands a view a backlog it did not ask for.
    #[test]
    fn a_late_subscriber_sees_only_what_follows_it() {
        let fanout = UpdateFanout::default();
        let mut first = fanout.subscribe(SubscriberRole::Answering);
        assert!(fanout.send(status("before")), "the first subscriber receives");

        let mut late = fanout.subscribe(SubscriberRole::Answering);
        assert!(fanout.send(status("after")), "both subscribers receive");

        assert_eq!(
            next(&mut first, "first"),
            "before",
            "the first subscriber keeps its earlier update"
        );
        assert_eq!(
            next(&mut first, "first"),
            "after",
            "the first subscriber also sees the later one"
        );
        assert_eq!(next(&mut late, "late"), "after", "the late subscriber misses the backlog");
        assert!(
            late.try_recv().is_err(),
            "the late subscriber is handed the backlog and nothing more",
        );
    }

    /// Catches dropping the hold for what was emitted before anything
    /// attached, which loses a notice the workspace raises during boot
    /// and leaves the first subscriber starting empty.
    #[test]
    fn the_first_subscriber_takes_what_was_emitted_before_it() {
        let fanout = UpdateFanout::default();
        assert!(!fanout.send(status("boot")), "nothing has attached to take it");

        let mut first = fanout.subscribe(SubscriberRole::Answering);
        let mut second = fanout.subscribe(SubscriberRole::Answering);

        assert_eq!(
            next(&mut first, "first"),
            "boot",
            "the first subscriber is handed the boot notice"
        );
        assert!(second.try_recv().is_err(), "the second subscriber is handed no boot backlog");
    }

    /// Catches pointing a guard at `send`, which counts an observer as an
    /// answer: the update reaches the observer, but the turn that raised
    /// it would wait forever on a reply an observer never sends.
    #[test]
    fn an_observer_takes_the_update_without_answering_it() {
        let fanout = UpdateFanout::default();
        let mut observer = fanout.subscribe(SubscriberRole::Observing);

        assert!(!fanout.send_answering(status("one")), "an observer does not answer");
        assert_eq!(next(&mut observer, "observer"), "one", "the observer still sees the update");

        let mut answering = fanout.subscribe(SubscriberRole::Answering);
        assert!(fanout.send_answering(status("two")), "an answering subscriber answers");
        assert_eq!(next(&mut answering, "answering"), "two", "it sees the update too");
    }

    /// Catches `send` swallowing the no-subscriber case, which silences
    /// the "receiver is gone" diagnostics at both emit sites.
    #[test]
    fn send_without_a_subscriber_reports_nothing_delivered() {
        let fanout = UpdateFanout::default();

        assert!(!fanout.send(status("one")), "an unsubscribed fan-out delivers to nobody");
    }
}

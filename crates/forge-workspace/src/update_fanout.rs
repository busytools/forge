//! Fan-out for the workspace's [`SessionUpdate`] stream.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::protocol::SessionUpdate;

/// Cloneable sender half of the workspace's fan-out to its subscribers.
///
/// Every clone shares one registry. [`Self::send`] hands the update to
/// each subscriber and drops any whose receiver has gone, so the
/// registry never keeps a dead one; [`Self::subscribe`] mints a stream
/// carrying what is emitted from that point on.
#[derive(Clone, Default)]
pub(crate) struct UpdateFanout {
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<SessionUpdate>>>>,
}

impl UpdateFanout {
    /// Mint a subscriber stream. It carries every update sent after this
    /// call and none sent before it.
    pub(crate) fn subscribe(&self) -> mpsc::UnboundedReceiver<SessionUpdate> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.lock().push(tx);
        rx
    }

    /// Deliver `update` to every subscriber, and report whether any took
    /// it. `false` is the "no receiver" signal the single-take channel
    /// gave, which `spawn::try_emit` and `SessionTask::emit` log on.
    ///
    /// The last subscriber is taken out of the registry to receive
    /// `update` itself while the rest are sent a copy, so the common
    /// one-subscriber case clones nothing. It goes back only if it took
    /// the update, which is also where a dead one leaves.
    pub(crate) fn send(&self, update: SessionUpdate) -> bool {
        let mut subscribers = self.subscribers.lock();
        let Some(tail) = subscribers.pop() else {
            return false;
        };
        let mut delivered = false;
        subscribers.retain(|tx| {
            let live = tx.send(update.clone()).is_ok();
            delivered |= live;
            live
        });
        if tail.send(update).is_ok() {
            subscribers.push(tail);
            return true;
        }
        delivered
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
        let mut first = fanout.subscribe();
        let mut second = fanout.subscribe();

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
        let dropped = fanout.subscribe();
        let mut kept = fanout.subscribe();
        drop(dropped);

        assert!(fanout.send(status("one")), "the live subscriber still receives");

        assert_eq!(
            fanout.subscribers.lock().len(),
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
        let mut first = fanout.subscribe();
        assert!(fanout.send(status("before")), "the first subscriber receives");

        let mut late = fanout.subscribe();
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

    /// Catches `send` swallowing the no-subscriber case, which silences
    /// the "receiver is gone" diagnostics at both emit sites.
    #[test]
    fn send_without_a_subscriber_reports_nothing_delivered() {
        let fanout = UpdateFanout::default();

        assert!(!fanout.send(status("one")), "an unsubscribed fan-out delivers to nobody");
    }
}

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

    /// Deliver `update` to every subscriber. `Err` when none took it -
    /// the "no receiver" signal the single-take channel gave, which
    /// `spawn::try_emit` and `SessionTask::emit` log on.
    pub(crate) fn send(
        &self,
        update: SessionUpdate,
    ) -> Result<(), mpsc::error::SendError<SessionUpdate>> {
        let mut subscribers = self.subscribers.lock();
        let mut delivered = false;
        subscribers.retain(|tx| {
            let live = tx.send(update.clone()).is_ok();
            delivered |= live;
            live
        });
        if delivered {
            Ok(())
        } else {
            Err(mpsc::error::SendError(update))
        }
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

        fanout.send(status("one")).expect("a subscribed fan-out delivers");

        assert_eq!(next(&mut first, "first"), "one", "the first subscriber sees the update");
        assert_eq!(next(&mut second, "second"), "one", "the second subscriber sees the same update");
    }

    /// Catches dropping the prune in `send`, which leaves a dead sender
    /// in the registry for the life of the workspace.
    #[test]
    fn a_subscriber_that_drops_leaves_the_fan_out() {
        let fanout = UpdateFanout::default();
        let mut kept = fanout.subscribe();
        let dropped = fanout.subscribe();
        drop(dropped);

        fanout.send(status("one")).expect("the live subscriber still receives");

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
        fanout.send(status("before")).expect("the first subscriber receives");

        let mut late = fanout.subscribe();
        fanout.send(status("after")).expect("both subscribers receive");

        assert_eq!(next(&mut first, "first"), "before", "the first subscriber keeps its earlier update");
        assert_eq!(next(&mut first, "first"), "after", "the first subscriber also sees the later one");
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

        assert!(fanout.send(status("one")).is_err(), "an unsubscribed fan-out delivers to nobody");
    }
}

//! The narrow surface the browser tools call: one ask, sent to whichever
//! client holds the browser role.
//!
//! Mirrors [`crate::mcp::review::facade`]: the `Tool` impls hold an
//! `Arc<dyn BrowserFacade>` so their tests drive a `MockBrowserFacade`
//! instead of a live relay.

use std::sync::Arc;

use async_trait::async_trait;
use forge_primitives::SessionSlot;
use forge_primitives::browser::{BrowserPart, HandOff, HandOffEnding};
use serde_json::Value;

use crate::browser::BrowserRelay;
use crate::workspace::Workspace;

/// The relay, as one tool call sees it.
#[async_trait]
pub trait BrowserFacade: Send + Sync {
    /// Run one browser tool through the registered host, answering with the
    /// parts it returned or the reason the call failed.
    async fn call(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String>;

    /// Ask the person at a client to act in a browser tab, and wait for their
    /// word.
    ///
    /// **No timeout.** The ask dock's own timelines say a held prompt sits for
    /// hours, and the session is meant to wait as long as it takes; the one
    /// refusal that does not wait is a stream nobody can answer on, which
    /// fails closed rather than parking the session on a prompt no view can
    /// draw.
    async fn hand_off(
        &self,
        seat: &SessionSlot,
        reason: &str,
        context: Option<&str>,
    ) -> Result<HandOffEnding, String>;
}

/// Production impl: the workspace's own relay, and the workspace itself for
/// the one tool that parks on the person rather than on the browser.
pub struct ProdBrowserFacade {
    relay: Arc<BrowserRelay>,
    workspace: std::sync::Weak<Workspace>,
}

impl ProdBrowserFacade {
    pub fn from_workspace(workspace: &Arc<Workspace>) -> Arc<dyn BrowserFacade> {
        Arc::new(Self { relay: workspace.browser_relay(), workspace: Arc::downgrade(workspace) })
    }
}

#[async_trait]
impl BrowserFacade for ProdBrowserFacade {
    async fn call(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String> {
        self.relay.ask(seat, tool, args).await
    }

    async fn hand_off(
        &self,
        seat: &SessionSlot,
        reason: &str,
        context: Option<&str>,
    ) -> Result<HandOffEnding, String> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Err("the workspace went away before the hand-off was staged".to_owned());
        };
        let handoff = HandOff {
            id: uuid::Uuid::new_v4(),
            reason: reason.to_owned(),
            context: context.map(str::to_owned),
        };
        let (id, answer) = workspace.register_browser_hand_off(seat, handoff);
        let guard =
            ResolveHandOffOnDrop { workspace: Arc::clone(&workspace), id, caller: seat.clone() };
        let ending = answer.await.map_err(|_| {
            // The fail-closed read: the stream could not take the update, so
            // no view can show the dock - and holding the session on a prompt
            // nobody can answer would park it forever.
            "no attached client can show the browser hand-off".to_owned()
        })?;
        drop(guard);
        Ok(ending)
    }
}

/// Resolves a parked hand-off when the awaiting handler goes - a session that
/// died mid-wait, never a decision. A resolve that already happened makes the
/// drop a no-op, which is why the approved path can just let it fall.
struct ResolveHandOffOnDrop {
    workspace: Arc<Workspace>,
    id: uuid::Uuid,
    caller: SessionSlot,
}

impl Drop for ResolveHandOffOnDrop {
    fn drop(&mut self) {
        self.workspace.resolve_browser_hand_off(self.id, &self.caller, HandOffEnding::Abandoned);
    }
}

/// Mock for the tools' unit tests: captures what a call carried and answers
/// with a preloaded outcome.
#[cfg(test)]
pub struct MockBrowserFacade {
    /// Captured `(tool, args)` calls.
    pub calls: parking_lot::Mutex<Vec<(String, Value)>>,
    /// The seat each call was made for, so a caller sending the wrong one is a
    /// failed assertion rather than a silent mismatch.
    pub seats: parking_lot::Mutex<Vec<SessionSlot>>,
    /// The outcome every call answers with.
    pub answer: parking_lot::Mutex<Result<Vec<BrowserPart>, String>>,
    /// What `hand_off` answers with, and what it was asked.
    pub hand_off_answer: parking_lot::Mutex<Result<HandOffEnding, String>>,
    pub hand_offs: parking_lot::Mutex<Vec<(String, Option<String>)>>,
}

#[cfg(test)]
impl MockBrowserFacade {
    pub fn new() -> Self {
        Self {
            calls: parking_lot::Mutex::new(Vec::new()),
            seats: parking_lot::Mutex::new(Vec::new()),
            answer: parking_lot::Mutex::new(Ok(vec![BrowserPart::Text {
                text: "done".to_owned(),
            }])),
            hand_off_answer: parking_lot::Mutex::new(Ok(HandOffEnding::Done)),
            hand_offs: parking_lot::Mutex::new(Vec::new()),
        }
    }

    pub fn into_arc(self) -> Arc<dyn BrowserFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[async_trait]
impl BrowserFacade for MockBrowserFacade {
    async fn call(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String> {
        self.seats.lock().push(seat.clone());
        self.calls.lock().push((tool.to_owned(), args));
        self.answer.lock().clone()
    }

    async fn hand_off(
        &self,
        seat: &SessionSlot,
        reason: &str,
        context: Option<&str>,
    ) -> Result<HandOffEnding, String> {
        self.seats.lock().push(seat.clone());
        self.hand_offs.lock().push((reason.to_owned(), context.map(str::to_owned)));
        self.hand_off_answer.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::BrowserRequest;
    use tokio::sync::mpsc;

    /// The production facade is the relay: one ask out, the host's answer
    /// back, with the tool and args it was called with.
    #[tokio::test]
    async fn the_production_facade_sends_the_ask_through_the_relay() {
        let (workspace, _rx) = Workspace::testing_stub();
        let (to_host, mut asks) = mpsc::unbounded_channel::<BrowserRequest>();
        let (notices, _notice_rx) = mpsc::unbounded_channel();
        assert!(workspace.browser_relay().register(3, to_host, notices));
        let facade = ProdBrowserFacade::from_workspace(&workspace);
        let seat = SessionSlot::lead("TestOrg", "proj");

        let host = tokio::spawn(async move {
            let request = asks.recv().await.expect("the ask arrives");
            assert_eq!(request.tool, "browser_click", "the host is told which tool");
            assert_eq!(request.args, serde_json::json!({ "target": "e5" }));
            request.reply.send(Ok(vec![BrowserPart::Text { text: "clicked".to_owned() }])).ok();
        });

        let answer =
            facade.call(&seat, "browser_click", serde_json::json!({ "target": "e5" })).await;

        host.await.expect("the host task ran");
        assert_eq!(answer, Ok(vec![BrowserPart::Text { text: "clicked".to_owned() }]));
    }

    /// **The waiter going away is its own ending.** A session that dies (or a
    /// handler dropped mid-wait) must clear the registry and tell the views
    /// which ending took the hand-off, rather than leaving a dock up for a
    /// prompt no answer can reach.
    #[tokio::test]
    async fn a_hand_off_waiter_that_dies_is_reported_as_abandoned() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let facade = ProdBrowserFacade::from_workspace(&workspace);
        let seat = SessionSlot::lead("TestOrg", "proj");
        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            let seat = seat.clone();
            async move { facade.hand_off(&seat, "solve the CAPTCHA", None).await }
        });
        // The hand-off lands in the registry, which is what the blocked call
        // just did.
        for _ in 0..200 {
            if workspace.browser_handoffs.lock().keys().next().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            workspace.browser_handoffs.lock().keys().next().is_some(),
            "the blocked call parked a hand-off",
        );

        task.abort();
        let _ = task.await;

        let mut resolved = Vec::new();
        while let Ok(update) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::BrowserHandOffResolved { ending, .. } = update {
                resolved.push(ending);
            }
        }
        assert_eq!(
            resolved,
            vec![HandOffEnding::Abandoned],
            "a dropped waiter clears the registry and says which ending took the hand-off",
        );
        assert!(workspace.browser_handoffs.lock().is_empty(), "and leaves nothing registered");
    }

    /// With no client holding the role, the facade answers the relay's own
    /// refusal rather than inventing one of its own.
    #[tokio::test]
    async fn the_production_facade_carries_the_relays_refusal() {
        let (workspace, _rx) = Workspace::testing_stub();
        let facade = ProdBrowserFacade::from_workspace(&workspace);
        let refused = facade
            .call(&SessionSlot::lead("TestOrg", "proj"), "browser_close", serde_json::json!({}))
            .await;
        assert_eq!(refused, Err(crate::browser::NO_BROWSER_CLIENT.to_owned()));
    }
}

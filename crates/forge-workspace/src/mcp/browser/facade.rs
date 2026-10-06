//! The narrow surface the browser tools call: one ask, sent to whichever
//! client holds the browser role.
//!
//! Mirrors [`crate::mcp::review::facade`]: the `Tool` impls hold an
//! `Arc<dyn BrowserFacade>` so their tests drive a `MockBrowserFacade`
//! instead of a live relay.

use std::sync::Arc;

use async_trait::async_trait;
use forge_primitives::SessionSlot;
use forge_primitives::browser::BrowserPart;
use serde_json::Value;

use crate::browser::BrowserRelay;

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
}

/// Production impl: the workspace's own relay.
pub struct ProdBrowserFacade(pub Arc<BrowserRelay>);

impl ProdBrowserFacade {
    pub fn from_relay(relay: Arc<BrowserRelay>) -> Arc<dyn BrowserFacade> {
        Arc::new(Self(relay))
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
        self.0.ask(seat, tool, args).await
    }
}

/// Mock for the tools' unit tests: captures what a call carried and answers
/// with a preloaded outcome.
#[cfg(test)]
pub struct MockBrowserFacade {
    /// Captured `(tool, args)` calls.
    pub calls: parking_lot::Mutex<Vec<(String, Value)>>,
    /// The outcome every call answers with.
    pub answer: parking_lot::Mutex<Result<Vec<BrowserPart>, String>>,
}

#[cfg(test)]
impl MockBrowserFacade {
    pub fn new() -> Self {
        Self {
            calls: parking_lot::Mutex::new(Vec::new()),
            answer: parking_lot::Mutex::new(Ok(vec![BrowserPart::Text {
                text: "done".to_owned(),
            }])),
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
        _seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String> {
        self.calls.lock().push((tool.to_owned(), args));
        self.answer.lock().clone()
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
        let relay = Arc::new(BrowserRelay::new());
        let (to_host, mut asks) = mpsc::unbounded_channel::<BrowserRequest>();
        assert!(relay.register(3, to_host));
        let facade = ProdBrowserFacade::from_relay(Arc::clone(&relay));
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

    /// With no client holding the role, the facade answers the relay's own
    /// refusal rather than inventing one of its own.
    #[tokio::test]
    async fn the_production_facade_carries_the_relays_refusal() {
        let facade = ProdBrowserFacade::from_relay(Arc::new(BrowserRelay::new()));
        let refused = facade
            .call(&SessionSlot::lead("TestOrg", "proj"), "browser_close", serde_json::json!({}))
            .await;
        assert_eq!(refused, Err(crate::browser::NO_BROWSER_CLIENT.to_owned()));
    }
}

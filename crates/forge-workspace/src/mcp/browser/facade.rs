//! The narrow surface the browser tools call: one call, sent to the server's
//! browser fleet.
//!
//! Mirrors [`crate::mcp::review::facade`]: the `Tool` impls hold an
//! `Arc<dyn BrowserFacade>` so their tests drive a `MockBrowserFacade`
//! instead of a live fleet.

use std::sync::Arc;

use async_trait::async_trait;
use forge_primitives::SessionSlot;
use forge_primitives::browser::BrowserPart;
use serde_json::Value;

use crate::browser::NO_BROWSER_HOST;
use crate::workspace::Workspace;

/// The fleet, as one tool call sees it.
#[async_trait]
pub trait BrowserFacade: Send + Sync {
    /// Run one browser tool through the server's fleet, answering with the
    /// parts it returned or the reason the call failed.
    async fn call(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String>;

    /// Ask the person at a client to act in one of the seat's browser tabs,
    /// and answer at once with the ask's id.
    ///
    /// **The person's word arrives later** as a follow-up turn, because the
    /// CLI aborts an MCP call after 70 seconds and a person may take longer.
    /// The one refusal that does not raise an ask is a stream nobody can
    /// answer on, which fails closed rather than parking a prompt no view can
    /// draw.
    async fn hand_off(
        &self,
        seat: &SessionSlot,
        reason: &str,
        tab: u16,
    ) -> Result<uuid::Uuid, String>;
}

/// Production impl: the server's fleet, and the workspace itself for the one
/// tool that raises an ask on the person rather than calling the browser.
pub struct ProdBrowserFacade {
    workspace: std::sync::Weak<Workspace>,
}

impl ProdBrowserFacade {
    pub fn from_workspace(workspace: &Arc<Workspace>) -> Arc<dyn BrowserFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
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
        // **Read at call time**: the fleet is installed onto the workspace
        // after boot wiring, and a facade holding the port from construction
        // would answer every call from the state before it.
        match self.workspace.upgrade().and_then(|workspace| workspace.browser_host()) {
            Some(host) => host.call(seat, tool, args).await,
            None => Err(NO_BROWSER_HOST.to_owned()),
        }
    }

    async fn hand_off(
        &self,
        seat: &SessionSlot,
        reason: &str,
        tab: u16,
    ) -> Result<uuid::Uuid, String> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Err("the workspace went away before the hand-off was staged".to_owned());
        };
        workspace.hand_off_browser(seat, reason, tab).await
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
    /// The ask id every hand-off answers with, and what it was asked.
    pub hand_off_answer: parking_lot::Mutex<Result<uuid::Uuid, String>>,
    pub hand_offs: parking_lot::Mutex<Vec<(String, u16)>>,
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
            hand_off_answer: parking_lot::Mutex::new(Ok(uuid::Uuid::new_v4())),
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
        tab: u16,
    ) -> Result<uuid::Uuid, String> {
        self.seats.lock().push(seat.clone());
        self.hand_offs.lock().push((reason.to_owned(), tab));
        self.hand_off_answer.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::testing::StubHost;
    use tokio::sync::mpsc;

    /// The production facade is the server's fleet: one call out, the host's
    /// answer back, with the tool and args it was called with.
    #[tokio::test]
    async fn the_production_facade_sends_the_call_to_the_installed_host() {
        let (workspace, _rx) = Workspace::testing_stub();
        let (calls, mut heard) = mpsc::unbounded_channel();
        workspace.install_browser_host(Arc::new(StubHost { calls }));
        let facade = ProdBrowserFacade::from_workspace(&workspace);
        let seat = SessionSlot::lead("TestOrg", "proj");

        let answer =
            facade.call(&seat, "browser_click", serde_json::json!({ "target": "e5" })).await;

        assert_eq!(
            heard.try_recv(),
            Ok(("browser_click".to_owned(), serde_json::json!({ "target": "e5" }))),
            "the host is told which tool, with its arguments verbatim",
        );
        assert_eq!(answer, Ok(vec![BrowserPart::Text { text: "clicked".to_owned() }]));
    }

    /// **The call answers the ask, not the person.** The CLI aborts an MCP
    /// call after 70 seconds, so a hand-off that waited on the person would
    /// die mid-ask; the return carries the id, and the person's word arrives
    /// as a follow-up turn naming it.
    #[tokio::test]
    async fn the_hand_off_answers_the_ask_before_the_person_does() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let (calls, _heard) = mpsc::unbounded_channel();
        workspace.install_browser_host(Arc::new(StubHost { calls }));
        workspace.enable_test_dispatch_intercept();
        let facade = ProdBrowserFacade::from_workspace(&workspace);
        let seat = SessionSlot::lead("TestOrg", "proj");

        let id = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            facade.hand_off(&seat, "solve the CAPTCHA", 2),
        )
        .await
        .expect("the call answers without waiting for the person")
        .expect("the ask is raised");
        assert!(
            workspace.browser_handoffs.lock().contains_key(&id),
            "the ask is still parked when the call has already answered",
        );

        assert!(workspace.resolve_browser_hand_off(
            id,
            &seat,
            forge_primitives::browser::HandOffAnswer {
                ending: forge_primitives::browser::HandOffEnding::Done,
                source: forge_primitives::browser::HandOffSource::Dock,
                client: None,
            },
        ));
        let follow_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match rx.recv().await.map(|frame| std::sync::Arc::unwrap_or_clone(frame.update)) {
                    Some(crate::protocol::SessionUpdate::ChatAppended { msg, source, .. }) => {
                        if let forge_primitives::Message::User { message, .. } = msg
                            && let Some(forge_primitives::ContentBlock::Text { text, .. }) =
                                message.content.first()
                        {
                            break (text.clone(), source);
                        }
                    }
                    Some(_) => {}
                    None => panic!("the stream ended before the follow-up"),
                }
            }
        })
        .await
        .expect("the person's word reaches the seat's stream");
        assert!(
            follow_up.0.contains(&format!("[browser_hand_off {id}]")),
            "the follow-up names the ask: {}",
            follow_up.0,
        );
        assert!(follow_up.0.contains("is done"), "and what the person did: {}", follow_up.0);
        assert_eq!(
            follow_up.1,
            Some(crate::protocol::PromptSource::Forge),
            "forge's own turn, not a person's",
        );
    }

    /// **A stream nobody can answer is refused at the call, not promised as
    /// a follow-up.** The tool's description says a hand-off with no client
    /// fails by name, and that failure is what tells the caller not to
    /// retry in a loop.
    #[tokio::test]
    async fn a_hand_off_with_no_answering_client_fails_by_name() {
        let (workspace, rx) = Workspace::testing_stub();
        let (calls, _heard) = mpsc::unbounded_channel();
        workspace.install_browser_host(Arc::new(StubHost { calls }));
        drop(rx);
        let facade = ProdBrowserFacade::from_workspace(&workspace);
        let seat = SessionSlot::lead("TestOrg", "proj");

        let refused = facade.hand_off(&seat, "look at this", 2).await;

        assert_eq!(refused, Err("no attached client can show the browser hand-off".to_owned()),);
        assert!(workspace.browser_handoffs.lock().is_empty(), "nothing is left parked");
    }

    /// With no fleet on the workspace, the facade answers the named refusal
    /// rather than inventing one of its own.
    #[tokio::test]
    async fn a_workspace_with_no_host_refuses_by_name() {
        let (workspace, _rx) = Workspace::testing_stub();
        let facade = ProdBrowserFacade::from_workspace(&workspace);
        let refused = facade
            .call(&SessionSlot::lead("TestOrg", "proj"), "browser_close", serde_json::json!({}))
            .await;
        assert_eq!(refused, Err(crate::browser::NO_BROWSER_HOST.to_owned()));
    }

    /// **A hand-off on a tab that is not the session's never reaches a
    /// person.** The fleet's refusal is the tool call's, and no ask is
    /// parked or drawn.
    #[tokio::test]
    async fn a_hand_off_on_an_unknown_tab_is_refused_by_name() {
        let (workspace, mut rx) = Workspace::testing_stub();
        let (calls, _heard) = mpsc::unbounded_channel();
        workspace.install_browser_host(Arc::new(StubHost { calls }));
        let facade = ProdBrowserFacade::from_workspace(&workspace);
        let seat = SessionSlot::lead("TestOrg", "proj");

        let refused = facade.hand_off(&seat, "look at this", 0).await;

        assert_eq!(refused, Err("tab 0 is not open".to_owned()));
        assert!(workspace.browser_handoffs.lock().is_empty(), "nothing is parked");
        assert!(rx.try_recv().is_err(), "and no view is told to draw a dock");
    }
}

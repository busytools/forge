//! The browser relay: the one client connection that drives the browser, and
//! the asks the browser MCP family sends through it.

use std::sync::{Mutex, MutexGuard};

use forge_primitives::SessionSlot;
use forge_primitives::browser::BrowserPart;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// The error a browser tool answers with when nothing can drive it.
pub const NO_BROWSER_CLIENT: &str = "no browser-capable client connected";

/// The error an ask answers with when the host it was sent to went away.
const HOST_GONE: &str = "the browser-capable client went away before answering";

/// One ask, on its way to the registered host.
#[derive(Debug)]
pub struct BrowserRequest {
    /// The id the answer comes back under, which the relay mints.
    pub id: u64,
    /// The seat whose session is asking, for a client that shows who drives
    /// what.
    pub seat: SessionSlot,
    pub tool: String,
    pub args: Value,
    /// The call's outcome: the parts a tool returned, or the reason it
    /// failed.
    pub reply: oneshot::Sender<Result<Vec<BrowserPart>, String>>,
}

/// The registered host, and the channel its asks go down.
#[derive(Debug)]
struct Host {
    /// The connection that registered. A drop tells the transport which
    /// connection is giving the role back, so a second one cannot take the
    /// first's.
    id: u64,
    to_host: mpsc::UnboundedSender<BrowserRequest>,
}

/// The one client connection that drives the browser.
///
/// **Exclusive, and the first capable connection wins.** A second capable
/// client does not run its own browser - two browsers would duplicate
/// profiles, logins and state for nothing - and it is not an error: it is
/// simply not the host. The role frees when its holder goes, and the next
/// capable client may take it then.
///
/// The relay is the workspace's, not the transport's: the tools that ask are
/// in the workspace, and the connection that answers is in the transport,
/// so the one object both can hold is the one the workspace owns.
#[derive(Debug, Default)]
pub struct BrowserRelay {
    host: Mutex<Option<Host>>,
}

impl BrowserRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// A panicking task must not take the role with it, the way
    /// [`crate::workspace`]'s other locks read.
    fn lock(&self) -> MutexGuard<'_, Option<Host>> {
        self.host.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Offer the role to the connection `id`; answers whether it holds it.
    ///
    /// `false` is the ordinary case of a second capable client: it stays a
    /// view like any other, and only the host is sent asks.
    pub fn register(&self, id: u64, to_host: mpsc::UnboundedSender<BrowserRequest>) -> bool {
        let mut host = self.lock();
        match host.as_ref() {
            Some(existing) if existing.id != id => false,
            _ => {
                *host = Some(Host { id, to_host });
                true
            }
        }
    }

    /// Give the role back, if `id` holds it.
    pub fn unregister(&self, id: u64) {
        let mut host = self.lock();
        if host.as_ref().is_some_and(|existing| existing.id == id) {
            *host = None;
        }
    }

    /// Send one ask to the host and wait for its answer.
    ///
    /// The failure arms are the named errors a tool returns: no host at all,
    /// or a host whose connection is gone. **The second one frees the role**,
    /// so a client that connects afterwards can take it rather than waiting
    /// for a restart.
    pub async fn ask(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String> {
        let (id, to_host) = {
            let host = self.lock();
            let Some(existing) = host.as_ref() else {
                return Err(NO_BROWSER_CLIENT.to_owned());
            };
            (mint_id(), existing.to_host.clone())
        };
        let (reply, answer) = oneshot::channel();
        let request = BrowserRequest { id, seat: seat.clone(), tool: tool.to_owned(), args, reply };
        if to_host.send(request).is_err() {
            self.unregister_if_dead();
            tracing::debug!(
                event_name = "browser_host_gone",
                tool = %tool,
                slot = %seat.display(),
                "a browser ask found the host's connection gone; the role is free again",
            );
            return Err(HOST_GONE.to_owned());
        }
        answer.await.unwrap_or_else(|_| Err(HOST_GONE.to_owned()))
    }

    /// Drop the host when its channel has no receiver left.
    fn unregister_if_dead(&self) {
        let mut host = self.lock();
        if host.as_ref().is_some_and(|existing| existing.to_host.is_closed()) {
            *host = None;
        }
    }
}

/// The next ask number, unique for the process's life.
///
/// Minted here rather than by the connection that carries the ask, so the id
/// stays unique across a host change: a second connection answering an id the
/// first half-used would answer the wrong call.
fn mint_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat() -> SessionSlot {
        SessionSlot::lead("TestOrg", "proj")
    }

    fn args() -> Value {
        serde_json::json!({ "url": "https://example.com" })
    }

    /// The role is exclusive and the first capable connection holds it - read
    /// off where the ask LANDS rather than off a flag: which connection a call
    /// reaches is the whole of what the role means.
    #[tokio::test]
    async fn the_first_capable_connection_holds_the_role() {
        let relay = BrowserRelay::new();
        let (first, mut first_rx) = mpsc::unbounded_channel();
        let (second, mut second_rx) = mpsc::unbounded_channel();

        assert!(relay.register(1, first), "the first capable connection holds the role");
        assert!(!relay.register(2, second), "and a second capable client does not take it");

        let seat = seat();
        let asked = relay.ask(&seat, "browser_close", args());
        let landed = tokio::spawn(async move {
            let request = first_rx.recv().await.expect("the ask reaches the first");
            request.reply.send(Ok(Vec::new())).ok();
        });
        assert!(asked.await.is_ok(), "the ask was answered by the role's holder");
        landed.await.expect("the landing task ran");
        assert!(
            second_rx.try_recv().is_err(),
            "and nothing was routed to the second capable client",
        );
    }

    /// The role frees when its holder goes, and the next capable client may
    /// take it - the reconnect case.
    #[tokio::test]
    async fn the_role_frees_when_its_holder_goes() {
        let relay = BrowserRelay::new();
        let (first, _first_rx) = mpsc::unbounded_channel();
        assert!(relay.register(1, first));

        relay.unregister(1);
        let (second, mut second_rx) = mpsc::unbounded_channel();
        assert!(relay.register(2, second), "the next capable client takes the freed role");

        // A late unregister from a connection that no longer holds it must
        // not take the role away from its new holder.
        relay.unregister(1);
        let seat = seat();
        let asked = relay.ask(&seat, "browser_close", args());
        let landed = tokio::spawn(async move {
            let request = second_rx.recv().await.expect("the ask reaches the role's holder");
            request.reply.send(Ok(Vec::new())).ok();
        });
        assert!(asked.await.is_ok(), "the role still belongs to whoever holds it");
        landed.await.expect("the landing task ran");
    }

    /// Nothing capable connected is the named error rather than a hang.
    #[tokio::test]
    async fn an_ask_with_no_host_names_the_error() {
        let relay = BrowserRelay::new();
        let refused = relay.ask(&seat(), "browser_navigate", args()).await;
        assert_eq!(
            refused,
            Err(NO_BROWSER_CLIENT.to_owned()),
            "the refusal names what is missing, so a session can act on it",
        );
    }

    /// An ask reaches the host and the host's answer is what the caller
    /// gets back.
    #[tokio::test]
    async fn an_ask_reaches_the_host_and_its_answer_comes_back() {
        let relay = BrowserRelay::new();
        let (to_host, mut asks) = mpsc::unbounded_channel();
        assert!(relay.register(7, to_host));

        let host = tokio::spawn(async move {
            let request = asks.recv().await.expect("the ask arrives");
            assert_eq!(request.tool, "browser_navigate", "the host is told which tool");
            assert_eq!(request.seat, seat(), "and which seat asked");
            assert_eq!(request.args, args(), "and with which arguments");
            request
                .reply
                .send(Ok(vec![BrowserPart::Text { text: "navigated".to_owned() }]))
                .expect("the answer goes back");
        });

        let answer = relay.ask(&seat(), "browser_navigate", args()).await;

        host.await.expect("the host task ran");
        assert_eq!(
            answer,
            Ok(vec![BrowserPart::Text { text: "navigated".to_owned() }]),
            "the host's parts are what the caller gets",
        );
    }

    /// A host whose connection is gone fails the call rather than hanging it,
    /// and frees the role so the next capable client can take it.
    #[tokio::test]
    async fn a_host_that_went_away_frees_the_role_and_the_call_fails() {
        let relay = BrowserRelay::new();
        let (to_host, asks) = mpsc::unbounded_channel();
        assert!(relay.register(7, to_host));
        drop(asks);

        let refused = relay.ask(&seat(), "browser_close", args()).await;
        assert_eq!(
            refused,
            Err(HOST_GONE.to_owned()),
            "the call fails naming what happened rather than waiting on a channel nobody reads",
        );

        let (next, _next_rx) = mpsc::unbounded_channel();
        assert!(relay.register(8, next), "the role is free for the next capable client");
    }
}

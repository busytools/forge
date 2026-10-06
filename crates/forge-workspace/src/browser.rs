//! The browser relay: the one client connection that drives the browser, and
//! the asks the browser MCP family sends through it.

use std::sync::{Mutex, MutexGuard};

use forge_primitives::SessionSlot;
use forge_primitives::browser::{BrowserPart, HandOff, HandOffEnding};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::workspace::Workspace;

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

/// What a role change tells the connection it happened to.
///
/// A promotion is told too, because it arrives without an ask to make it
/// obvious - a holder goes, the next in line is handed the role - and a
/// client that went on believing it was in line would draw "another client
/// drives the browser" while its own connection is the one being asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleNotice {
    /// This connection holds the role now, promoted from the waiting line.
    Granted,
    /// This connection held the role and lost it to a force-take. Its
    /// connection stops reading asks on this word - the same thing its own
    /// drop does - so the calls it was carrying fail loudly rather than
    /// half-answering into a role it no longer holds.
    Taken,
}

/// The registered connection, and the channels its asks and its role notices
/// go down.
#[derive(Debug)]
struct Client {
    /// The connection that registered. A drop tells the transport which
    /// connection is giving the role back, so a second one cannot take the
    /// first's.
    id: u64,
    to_host: mpsc::UnboundedSender<BrowserRequest>,
    notices: mpsc::UnboundedSender<RoleNotice>,
}

/// Who holds the role, and who is in line for it.
#[derive(Debug, Default)]
struct Role {
    host: Option<Client>,
    /// Capable connections that offered the role while another held it,
    /// oldest first: the one it goes to when the holder gives it back. A
    /// connection stays here until it goes or takes the role, so a restart of
    /// the holder hands over without the waiter being asked to declare
    /// anything again.
    waiting: Vec<Client>,
}

/// The one client connection that drives the browser.
///
/// **Exclusive, and the first capable connection wins.** A second capable
/// client does not run its own browser - two browsers would duplicate
/// profiles, logins and state for nothing - and it is not an error: it is
/// simply not the host yet.
///
/// **The role moves on by itself when its holder goes.** A waiter is
/// promoted then, oldest first, so the failure this avoids is a client
/// attached and capable while every browser tool answers "no browser-capable
/// client connected" - which is a lie about the machine, and the state a
/// second client used to sit in until it happened to subscribe again.
///
/// The relay is the workspace's, not the transport's: the tools that ask are
/// in the workspace, and the connection that answers is in the transport,
/// so the one object both can hold is the one the workspace owns.
#[derive(Debug, Default)]
pub struct BrowserRelay {
    role: Mutex<Role>,
}

impl BrowserRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// A panicking task must not take the role with it, the way
    /// [`crate::workspace`]'s other locks read.
    fn lock(&self) -> MutexGuard<'_, Role> {
        self.role.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Offer the role to the connection `id`; answers whether it holds it now.
    ///
    /// `false` is the ordinary case of a second capable client: it stays a
    /// view like any other, waits in line, and is sent asks once the role
    /// reaches it.
    pub fn register(
        &self,
        id: u64,
        to_host: mpsc::UnboundedSender<BrowserRequest>,
        notices: mpsc::UnboundedSender<RoleNotice>,
    ) -> bool {
        let mut role = self.lock();
        // The same connection offering again - a later subscribe - replaces
        // its own channel, and as a WAITER it goes to the back of the line:
        // the retain drops it and the push below re-adds it, which is the
        // honest reading of "offered just now".
        role.waiting.retain(|client| client.id != id);
        match role.host.as_ref() {
            Some(host) if host.id != id => {
                role.waiting.push(Client { id, to_host, notices });
                false
            }
            _ => {
                role.host = Some(Client { id, to_host, notices });
                true
            }
        }
    }

    /// Take the role by force from whoever holds it.
    ///
    /// The claimant must have offered first - a connection that never
    /// declared itself capable has no channel to be asked down, and `false`
    /// says so. The holder is TOLD ([`RoleNotice::Taken`]), which is what
    /// makes its connection stop reading asks: the calls it was carrying
    /// then fail with the relay's own host-gone failure, the same shape a
    /// holder dying has, taken early rather than at its own choosing.
    pub fn claim(&self, id: u64) -> bool {
        let mut role = self.lock();
        if role.host.as_ref().is_some_and(|host| host.id == id) {
            return true;
        }
        let Some(at) = role.waiting.iter().position(|client| client.id == id) else {
            return false;
        };
        if let Some(displaced) = role.host.take() {
            let _ = displaced.notices.send(RoleNotice::Taken);
        }
        let claimant = role.waiting.remove(at);
        role.host = Some(claimant);
        true
    }

    /// Give the role back, if `id` holds it - and hand it to whoever is next
    /// in line.
    pub fn unregister(&self, id: u64) {
        let mut role = self.lock();
        if role.host.as_ref().is_some_and(|host| host.id == id) {
            role.host = None;
            promote(&mut role);
        }
        role.waiting.retain(|client| client.id != id);
    }

    /// Send one ask to the host and wait for its answer.
    ///
    /// The failure arms are the named errors a tool returns: no host at all,
    /// or a host whose connection is gone. **The second one frees the role
    /// and promotes whoever is waiting**, so the NEXT ask finds a host where
    /// this one found none - the call that met the dead connection is not
    /// retried, because a call may already have run partway on the host that
    /// went.
    pub async fn ask(
        &self,
        seat: &SessionSlot,
        tool: &str,
        args: Value,
    ) -> Result<Vec<BrowserPart>, String> {
        let (id, to_host) = {
            let role = self.lock();
            let Some(host) = role.host.as_ref() else {
                return Err(NO_BROWSER_CLIENT.to_owned());
            };
            (mint_id(), host.to_host.clone())
        };
        let (reply, answer) = oneshot::channel();
        let request = BrowserRequest { id, seat: seat.clone(), tool: tool.to_owned(), args, reply };
        if to_host.send(request).is_err() {
            self.forget_the_dead();
            tracing::debug!(
                event_name = "browser_host_gone",
                tool = %tool,
                slot = %seat.display(),
                "a browser ask found the host's connection gone; the role moved on",
            );
            return Err(HOST_GONE.to_owned());
        }
        answer.await.unwrap_or_else(|_| Err(HOST_GONE.to_owned()))
    }

    /// Forget a host whose channel has no receiver left - and a waiter whose
    /// channel is dead too, so a role is never handed to a connection that
    /// has already gone.
    fn forget_the_dead(&self) {
        let mut role = self.lock();
        role.waiting.retain(|client| !client.to_host.is_closed());
        if role.host.as_ref().is_some_and(|host| host.to_host.is_closed()) {
            role.host = None;
            promote(&mut role);
        }
    }
}

/// Hand the role to the first connection in line, if the role is free and
/// anyone is waiting.
fn promote(role: &mut Role) {
    if role.host.is_some() {
        return;
    }
    while !role.waiting.is_empty() {
        let next = role.waiting.remove(0);
        if next.to_host.is_closed() {
            continue;
        }
        let _ = next.notices.send(RoleNotice::Granted);
        role.host = Some(next);
        return;
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

impl Workspace {
    /// Hold a browser hand-off for the person at a client, and hand back its
    /// id plus the receiver the caller awaits.
    ///
    /// **No timeout rides the wait.** The ask dock's own timelines say a held
    /// prompt sits for hours, so nothing here ends it but an answer or the
    /// caller's own drop. The one case answered up front is a stream nobody
    /// can answer on: holding a session on a prompt no view can draw would
    /// park it forever, which is the failure this path exists to refuse.
    pub(crate) fn register_browser_hand_off(
        &self,
        caller: &SessionSlot,
        handoff: HandOff,
    ) -> (uuid::Uuid, oneshot::Receiver<HandOffEnding>) {
        let (sender, receiver) = oneshot::channel();
        let id = handoff.id;
        self.browser_handoffs.lock().insert(id, (caller.clone(), handoff.clone(), sender));
        let answerable = self.update_sender().send_answering(
            crate::protocol::SessionUpdate::BrowserHandOffPending { key: caller.clone(), handoff },
        );
        if !answerable {
            self.browser_handoffs.lock().remove(&id);
            let (_ignored_sender, dead_receiver) = oneshot::channel();
            return (id, dead_receiver);
        }
        (id, receiver)
    }

    /// Whether a hand-off with this id is still registered to `caller`,
    /// without removing it - the read the dispatch guard makes before an
    /// answer.
    pub(crate) fn browser_handoff_waiting(&self, id: uuid::Uuid, caller: &SessionSlot) -> bool {
        self.browser_handoffs.lock().get(&id).is_some_and(|(owner, _, _)| owner == caller)
    }

    /// Remove a parked hand-off, answer its waiter, and tell every view it is
    /// gone. `false` when the id is not this caller's to resolve.
    pub(crate) fn resolve_browser_hand_off(
        &self,
        id: uuid::Uuid,
        caller: &SessionSlot,
        ending: HandOffEnding,
    ) -> bool {
        let mut parked = self.browser_handoffs.lock();
        if !parked.get(&id).is_some_and(|(owner, _, _)| owner == caller) {
            return false;
        }
        let Some((owner, _, sender)) = parked.remove(&id) else { return false };
        drop(parked);
        let _ = sender.send(ending);
        let _ = self.update_sender().send(crate::protocol::SessionUpdate::BrowserHandOffResolved {
            key: owner,
            id,
            ending,
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn seat() -> SessionSlot {
        SessionSlot::lead("TestOrg", "proj")
    }

    /// A notices sender whose receiver is gone: the connection registered
    /// and nobody is reading its role notices, which every test but the
    /// force-take's is free to do.
    fn notices() -> mpsc::UnboundedSender<RoleNotice> {
        mpsc::unbounded_channel().0
    }

    fn args() -> Value {
        serde_json::json!({ "url": "https://example.com" })
    }

    /// One ask, landed on `rx`: the ask is answered there and the caller is
    /// told the parts. Named so the tests below read as where the ask went.
    async fn lands_on(relay: &Arc<BrowserRelay>, rx: &mut mpsc::UnboundedReceiver<BrowserRequest>) {
        let relay = Arc::clone(relay);
        let asked = tokio::spawn(async move { relay.ask(&seat(), "browser_close", args()).await });
        let request = rx.recv().await.expect("the ask reaches this connection");
        request.reply.send(Ok(Vec::new())).ok();
        assert!(
            asked.await.expect("the asker task ran").is_ok(),
            "the ask was answered where it landed",
        );
    }

    /// The role is exclusive and the first capable connection holds it - read
    /// off where the ask LANDS rather than off a flag: which connection a call
    /// reaches is the whole of what the role means.
    #[tokio::test]
    async fn the_first_capable_connection_holds_the_role() {
        let relay = Arc::new(BrowserRelay::new());
        let (first, mut first_rx) = mpsc::unbounded_channel();
        let (second, mut second_rx) = mpsc::unbounded_channel();

        assert!(relay.register(1, first, notices()), "the first capable connection holds the role");
        assert!(
            !relay.register(2, second, notices()),
            "and a second capable client does not take it"
        );

        lands_on(&relay, &mut first_rx).await;
        assert!(
            second_rx.try_recv().is_err(),
            "and nothing was routed to the second capable client",
        );
    }

    /// **The role moves on when its holder goes, without the waiter being
    /// asked to declare anything again.** A client attached and capable while
    /// a dead holder kept the role is the state this exists to prevent: every
    /// browser tool would answer "no browser-capable client connected" with
    /// one sitting right there.
    #[tokio::test]
    async fn the_role_moves_to_the_next_in_line_when_the_holder_goes() {
        let relay = Arc::new(BrowserRelay::new());
        let (first, _first_rx) = mpsc::unbounded_channel();
        let (second, mut second_rx) = mpsc::unbounded_channel();
        assert!(relay.register(1, first, notices()), "the first connection holds it");
        assert!(
            !relay.register(2, second, notices()),
            "the second waits in line rather than taking it"
        );

        relay.unregister(1);
        lands_on(&relay, &mut second_rx).await;

        // A late unregister from a connection that no longer holds it must
        // not take the role away from its new holder.
        let (third, mut third_rx) = mpsc::unbounded_channel();
        relay.unregister(1);
        assert!(!relay.register(3, third, notices()), "the role is taken, so a third waits");
        lands_on(&relay, &mut second_rx).await;
        assert!(third_rx.try_recv().is_err(), "and the third is not asked");
    }

    /// A waiter whose connection has gone is passed over: the role is handed
    /// to a client that can answer, not to the first name on a list.
    #[tokio::test]
    async fn a_dead_waiter_is_passed_over() {
        let relay = Arc::new(BrowserRelay::new());
        let (first, _first_rx) = mpsc::unbounded_channel();
        let (second, second_rx) = mpsc::unbounded_channel();
        let (third, mut third_rx) = mpsc::unbounded_channel();
        assert!(relay.register(1, first, notices()));
        assert!(!relay.register(2, second, notices()));
        drop(second_rx);
        assert!(!relay.register(3, third, notices()));

        relay.unregister(1);
        lands_on(&relay, &mut third_rx).await;
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
        assert!(relay.register(7, to_host, notices()));

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
        assert!(relay.register(7, to_host, notices()));
        drop(asks);

        let refused = relay.ask(&seat(), "browser_close", args()).await;
        assert_eq!(
            refused,
            Err(HOST_GONE.to_owned()),
            "the call fails naming what happened rather than waiting on a channel nobody reads",
        );

        let (next, _next_rx) = mpsc::unbounded_channel();
        assert!(relay.register(8, next, notices()), "the role is free for the next capable client");
    }

    /// **A force-take moves the role: the claimant is what gets asked, and
    /// the holder is TOLD.** Read off where the ask lands, which is the whole
    /// of what holding means - and the notice is what makes the old holder's
    /// connection stop reading asks, so the calls it was carrying fail loudly
    /// rather than half-answering into a role it no longer holds.
    #[tokio::test]
    async fn a_force_claim_takes_the_role_and_the_holder_is_told() {
        let relay = Arc::new(BrowserRelay::new());
        let (first, mut first_rx) = mpsc::unbounded_channel();
        let (second, mut second_rx) = mpsc::unbounded_channel();
        let (first_notes, mut first_note_rx) = mpsc::unbounded_channel();
        assert!(relay.register(1, first, first_notes), "the first connection holds it");
        assert!(!relay.register(2, second, notices()), "and the second waits in line");

        assert!(relay.claim(2), "a capable client already in line can take it");

        // Bounded, so a claim that quietly kept the old host fails as a
        // named timeout rather than holding the run open.
        tokio::time::timeout(std::time::Duration::from_secs(5), lands_on(&relay, &mut second_rx))
            .await
            .expect("the ask reaches the claimant, not the displaced holder");
        assert!(first_rx.try_recv().is_err(), "and nothing reaches the old holder");
        assert_eq!(
            first_note_rx.try_recv(),
            Ok(RoleNotice::Taken),
            "which is told it lost the role",
        );
    }

    /// The role a force-take leaves is the claimant's: a third capable client
    /// still waits, a connection that never offered cannot claim at all, and
    /// when the claimant goes the role still promotes in line.
    #[tokio::test]
    async fn after_a_force_take_the_role_is_the_claimants() {
        let relay = Arc::new(BrowserRelay::new());
        let (first, _first_rx) = mpsc::unbounded_channel();
        let (second, _second_rx) = mpsc::unbounded_channel();
        let (third, mut third_rx) = mpsc::unbounded_channel();
        assert!(relay.register(1, first, notices()));
        assert!(!relay.register(2, second, notices()));
        assert!(!relay.register(3, third, notices()));

        assert!(relay.claim(2), "the claim moves the role to the claimant");
        assert!(!relay.claim(9), "and a connection that never offered cannot claim");

        let (again, mut again_rx) = mpsc::unbounded_channel();
        assert!(!relay.register(3, again, notices()), "the third still waits behind it");
        relay.unregister(2);
        tokio::time::timeout(std::time::Duration::from_secs(5), lands_on(&relay, &mut again_rx))
            .await
            .expect("the role promotes in line after the claimant goes");
        assert!(third_rx.try_recv().is_err(), "and the third's old channel is the dead one");
    }

    /// **A promotion is ANNOUNCED**: the waiter promoted when the holder goes
    /// hears `Granted`, so its own strip stops saying another client drives
    /// the browser while its connection is the one being asked.
    #[tokio::test]
    async fn a_promoted_waiter_is_told_it_holds_the_role() {
        let relay = Arc::new(BrowserRelay::new());
        let (first, _first_rx) = mpsc::unbounded_channel();
        let (second, mut second_rx) = mpsc::unbounded_channel();
        let (second_notes, mut second_note_rx) = mpsc::unbounded_channel();
        assert!(relay.register(1, first, notices()));
        assert!(!relay.register(2, second, second_notes));
        assert!(second_note_rx.try_recv().is_err(), "in line, nothing is said yet");

        relay.unregister(1);

        assert_eq!(
            second_note_rx.try_recv(),
            Ok(RoleNotice::Granted),
            "the promotion is said out loud",
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), lands_on(&relay, &mut second_rx))
            .await
            .expect("and the asks arrive with it");
    }

    fn handoff(reason: &str) -> HandOff {
        HandOff {
            id: uuid::Uuid::new_v4(),
            reason: reason.to_owned(),
            context: Some("hunt".to_owned()),
        }
    }

    /// **A hand-off is addressed to the session that asked**, so the dock the
    /// person answers belongs to the session waiting on it rather than to
    /// whichever one happens to be focused.
    #[tokio::test]
    async fn a_hand_off_is_addressed_to_the_session_that_asked() {
        let (ws, mut rx) = Workspace::testing_stub();
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let (_id, _answer) = ws.register_browser_hand_off(&caller, handoff("solve the CAPTCHA"));

        let mut addressed = Vec::new();
        while let Ok(update) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::BrowserHandOffPending { key, .. } = update {
                addressed.push(key);
            }
        }
        assert_eq!(addressed, vec![caller], "the hand-off is addressed to the asker");
    }

    /// Answering a hand-off nobody parked is refused rather than answered
    /// into the void.
    #[tokio::test]
    async fn answering_a_hand_off_that_is_not_pending_is_refused() {
        let (ws, _rx) = Workspace::testing_stub();
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        assert!(
            !ws.resolve_browser_hand_off(uuid::Uuid::new_v4(), &caller, HandOffEnding::NotNow),
            "a hand-off that is not parked cannot be resolved",
        );
    }

    /// **No view can draw the dock: the caller fails closed rather than
    /// parking forever.** This is the one case a wait does not hold for, and
    /// it must not hold the session on a prompt nothing can answer.
    #[tokio::test]
    async fn a_hand_off_with_no_ui_to_answer_it_fails_closed() {
        let (ws, rx) = Workspace::testing_stub();
        drop(rx);
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let (_id, answer) = ws.register_browser_hand_off(&caller, handoff("solve the CAPTCHA"));

        assert!(
            ws.browser_handoffs.lock().is_empty(),
            "an unanswerable hand-off is dropped, not held forever",
        );
        assert!(answer.await.is_err(), "a dead receiver is the fail-closed read");
    }

    /// The resolve answers the blocked waiter AND tells the views it is gone,
    /// so a dock standing elsewhere retires with the same ending the answer
    /// carried.
    #[tokio::test]
    async fn resolving_a_hand_off_answers_its_waiter_and_tells_the_views() {
        let (ws, mut rx) = Workspace::testing_stub();
        let caller = SessionSlot::from_str_for_test("caller-uuid");
        let handoff = handoff("solve the CAPTCHA");
        let id = handoff.id;
        let (_id, answer) = ws.register_browser_hand_off(&caller, handoff);
        while rx.try_recv().is_ok() {}

        assert!(ws.resolve_browser_hand_off(id, &caller, HandOffEnding::Done));

        assert_eq!(
            answer.await.expect("the waiter is answered"),
            HandOffEnding::Done,
            "the ending the resolve carried is what the blocked handler gets",
        );
        let resolved = rx.try_recv().expect("the views hear it left the registry");
        let crate::protocol::SessionUpdate::BrowserHandOffResolved { key, id: seen, ending } =
            resolved
        else {
            panic!("the update is the resolved one");
        };
        assert_eq!(key, caller);
        assert_eq!(seen, id);
        assert_eq!(ending, HandOffEnding::Done);
    }

    /// Only the session that asked may resolve it: an answer from another
    /// seat is refused by the owner read, not applied.
    #[tokio::test]
    async fn only_the_owner_resolves_a_hand_off() {
        let (ws, _rx) = Workspace::testing_stub();
        let owner = SessionSlot::from_str_for_test("owner-uuid");
        let other = SessionSlot::from_str_for_test("other-uuid");
        let handoff = handoff("solve the CAPTCHA");
        let id = handoff.id;
        let (_id, _answer) = ws.register_browser_hand_off(&owner, handoff);

        assert!(
            !ws.resolve_browser_hand_off(id, &other, HandOffEnding::Done),
            "another seat's answer is refused",
        );
        assert!(ws.browser_handoff_waiting(id, &owner), "and the hand-off stays parked");
    }
}

//! The socket, from the other side of it: a client opens one against a
//! server this test starts itself.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use forge_primitives::SessionSlot;
use forge_server::Command;
use forge_server::live::Live;
use forge_server::surface::SessionUpdate;
use forge_server::testing::Fleet;
use forge_server::transport::TransportState;
use forge_server::transport::envelope::{ClientMessage, ServerMessage, Subject};
use forge_server::transport::serve;
use forge_server::work::WorkCache;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// The client end of a socket, named so the helpers below read as one.
type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The seat the fixture fleet declares.
fn lead_seat() -> SessionSlot {
    SessionSlot::lead("TestOrg", "proj")
}

/// A server over a fresh fixture fleet, the URL to reach it, and the fleet
/// itself so a test can drive the core as well as the socket.
///
/// The config directory is kept rather than dropped with the fleet: the
/// workspace's store lives under it, and a surface whose files vanished
/// under it is not what a test means to exercise.
async fn a_server() -> (String, Fleet) {
    let dir = tempfile::tempdir().expect("tempdir").keep();
    let fleet = Fleet::in_dir(&dir, &[("TestOrg", &["proj"])]).expect("the fleet builds");
    let state = Arc::new(TransportState {
        surface: fleet.surface(),
        work: Arc::new(WorkCache::new()),
        live: Mutex::new(Live::new()),
        config: forge_primitives::WebConfig::default(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = serve(state, listener).await;
    });
    (format!("ws://{addr}/socket"), fleet)
}

/// A client connected to a server this test started, with the greeting
/// already read: every test below starts from a stream whose next message is
/// the answer to what it sends.
async fn connect(url: &str) -> Client {
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.expect("the socket opens");
    let msg = socket.next().await.expect("a greeting").expect("no error");
    let text = msg.to_text().expect("text").to_owned();
    let greeting: ServerMessage = serde_json::from_str(&text).expect("the greeting decodes");
    assert!(
        matches!(greeting, ServerMessage::Greeting { .. }),
        "the first thing the server says is its greeting, not {text}",
    );
    socket
}

/// A client on a server of its own, for a test that needs no fleet.
async fn connected() -> Client {
    let (url, _fleet) = a_server().await;
    connect(&url).await
}

/// Waits for the server to notice its client went away.
///
/// The socket closing is local to the client; the server finds out when its
/// own read fails, which is a scheduling hop away. Polling is honest because
/// the property IS "eventually", and it is bounded so a server that never
/// notices fails the test that waits rather than hanging it.
async fn wait_for_the_server_to_notice(fleet: &Fleet) -> bool {
    for _ in 0..200 {
        if !fleet.emit_and_report(SessionUpdate::CatalogLoaded) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

async fn send(socket: &mut Client, message: ClientMessage) {
    let text = serde_json::to_string(&message).expect("encode");
    socket.send(Message::Text(text.into())).await.expect("send");
}

/// The server's next message.
///
/// Bounded, so a server that answers nothing fails the test that is waiting
/// rather than hanging it: a socket left open with nothing said is the
/// failure this helper exists to name.
async fn next_server(socket: &mut Client) -> ServerMessage {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("the server answered rather than leaving the client waiting")
        .expect("a message")
        .expect("no error");
    let text = msg.to_text().expect("text");
    serde_json::from_str(text).expect("decode")
}

/// The socket opens, and the server speaks first.
///
/// The same property `connected` asserts for every test below, kept as its
/// own named test so a failure here reads as "the socket did not open or
/// said nothing" rather than as a bad answer to something.
#[tokio::test]
async fn a_client_can_open_the_socket() {
    let state = Arc::new(TransportState::for_test().expect("the fixture builds"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = serve(state, listener).await;
    });

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/socket"))
        .await
        .expect("the socket opens");
    // The first thing the server says is its greeting, so a client knows it
    // is speaking to a forge and not to something else on the port.
    let msg = socket.next().await.expect("a greeting").expect("no error");
    let text = msg.to_text().expect("text").to_owned();
    let greeting: ServerMessage = serde_json::from_str(&text).expect("the greeting decodes");
    assert!(
        matches!(greeting, ServerMessage::Greeting { .. }),
        "the first thing the server says is its greeting, not {text}",
    );
}

#[tokio::test]
async fn a_subscribe_is_answered_with_that_subjects_snapshot() {
    let mut socket = connected().await;
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home }).await;

    let msg = next_server(&mut socket).await;
    let ServerMessage::Snapshot { subject, data } = msg else { panic!("{msg:?}") };
    assert_eq!(subject, Subject::Home);
    // `Roster`'s field is `projects`, not `orgs` - `orgs` belongs to `AccountsView`,
    // which is the gateway's subject and a different record entirely. Asserting the
    // wrong key here would fail for a reason that looks like the encoder's fault.
    assert!(data.get("projects").is_some(), "the home snapshot carries its projects: {data}");
}

/// A prompt aimed at a seat.
fn a_prompt_for(org: &str, project: &str, label: &str) -> Command {
    Command::Prompt {
        key: SessionSlot::for_label(org, project, Some(label)),
        text: "hello".to_owned(),
        attachments: Vec::new(),
    }
}

/// A command aimed at a seat forge holds no session for is answered with an
/// error naming it - never a panic, and never a silent success.
#[tokio::test]
async fn a_command_for_a_seat_that_is_not_there_answers_with_an_error() {
    let mut socket = connected().await;
    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(a_prompt_for("Nowhere", "nothing", "lead")),
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Error { what, why } = next_server(&mut socket).await else {
        panic!("expected an error")
    };
    // Which refusal, not merely that one arrived: an arm that echoed the whole message back
    // would carry the seat's name in its `Debug` and satisfy the assertion below while
    // having dispatched nothing at all.
    assert_eq!(
        what, "dispatch",
        "the refusal comes from the core, not from this server declining the message: {why}",
    );
    // This rests on WHICH refusal the surface returns: `UnknownSession(slot)` renders the
    // slot through `{:?}` so the seat's name is in the sentence, while `NoActiveSession`
    // renders as "no active session" and carries no seat at all. If this assertion fails on
    // the seat's name, the question is which variant came back - not whether the error
    // reached the client, because the `let else` above already proved that.
    assert!(why.contains("Nowhere"), "the error names the seat: {why}");
}

/// A subscription hears the updates its subject receives and no others.
#[tokio::test]
async fn a_subscriber_hears_the_update_it_asked_for_and_not_another_seats() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home }).await;
    let ServerMessage::Snapshot { .. } = next_server(&mut socket).await else {
        panic!("the subscribe is answered with a snapshot first")
    };

    // An App-level update names no slot, which is what the home subscription is for.
    fleet.emit(SessionUpdate::CatalogLoaded);
    assert!(
        matches!(next_server(&mut socket).await, ServerMessage::Update { .. }),
        "a home subscriber hears an App-level update",
    );

    // And one that names a seat is not the home's business. The evidence is ORDER, never a
    // timeout: waiting for the update to NOT arrive would hang on the correct behaviour, so
    // the test asks for something whose answer must come next and asserts THAT is what it got.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home }).await;
    let msg = next_server(&mut socket).await;
    assert!(
        matches!(msg, ServerMessage::Snapshot { .. }),
        "a seat's update must not reach a home subscriber; the snapshot should be next: {msg:?}",
    );
}

/// Two clients on one seat both hear it: the second neither steals the
/// first's stream nor sees half of it.
#[tokio::test]
async fn two_sockets_on_one_seat_both_hear_it() {
    let (url, fleet) = a_server().await;
    let mut first = connect(&url).await;
    let mut second = connect(&url).await;
    for socket in [&mut first, &mut second] {
        send(socket, ClientMessage::Subscribe { what: Subject::Session(lead_seat()) }).await;
        let ServerMessage::Snapshot { .. } = next_server(socket).await else {
            panic!("a seat that exists is answered with its snapshot")
        };
    }

    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });

    assert!(
        matches!(next_server(&mut first).await, ServerMessage::Update { .. }),
        "the first client hears the seat it subscribed to",
    );
    assert!(
        matches!(next_server(&mut second).await, ServerMessage::Update { .. }),
        "and the second must not have stolen the first's stream",
    );
}

/// A subscription does not outlive its socket, which on a long-lived server
/// is a subscription-shaped leak.
#[tokio::test]
async fn a_dropped_socket_leaves_no_subscription_behind() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home }).await;
    let ServerMessage::Snapshot { .. } = next_server(&mut socket).await else {
        panic!("the subscribe is answered with a snapshot")
    };
    // Precondition, so the assertion below is about a subscription that went
    // away rather than about one that was never made.
    assert!(
        fleet.emit_and_report(SessionUpdate::CatalogLoaded),
        "precondition: the socket's own subscription is attached",
    );

    // Closed politely rather than dropped: the server is told the client is
    // going, which is the path a page navigating away takes.
    socket.close(None).await.expect("the client says goodbye");
    drop(socket);

    assert!(
        wait_for_the_server_to_notice(&fleet).await,
        "a subscription must not outlive its socket",
    );
}

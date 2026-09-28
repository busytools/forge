//! The socket, from the other side of it: a client opens one against a
//! server this test starts itself.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use forge_server::transport::TransportState;
use forge_server::transport::envelope::{ClientMessage, ServerMessage, Subject};
use forge_server::transport::serve;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// The client end of a socket, named so the helpers below read as one.
type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A client connected to a server this test started, with the greeting
/// already read: every test below starts from a stream whose next message is
/// the answer to what it sends.
async fn connected() -> Client {
    let state = Arc::new(TransportState::for_test().expect("the fixture builds"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = serve(state, listener).await;
    });

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/socket"))
        .await
        .expect("the socket opens");
    let msg = socket.next().await.expect("a greeting").expect("no error");
    let text = msg.to_text().expect("text").to_owned();
    let greeting: ServerMessage = serde_json::from_str(&text).expect("the greeting decodes");
    assert!(
        matches!(greeting, ServerMessage::Greeting { .. }),
        "the first thing the server says is its greeting, not {text}",
    );
    socket
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

//! The socket, from the other side of it: a client opens one against a
//! server this test starts itself.

use std::sync::Arc;

use forge_server::transport::{TransportState, serve};
use futures_util::StreamExt;

#[tokio::test]
async fn a_client_can_open_the_socket() {
    let state = Arc::new(TransportState::for_test().expect("the fixture builds"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { serve(state, listener).await });

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/socket"))
        .await
        .expect("the socket opens");
    // The first thing the server says is its greeting, so a client knows it is speaking
    // to a forge and not to something else on the port.
    let msg = socket.next().await.expect("a greeting").expect("no error");
    assert!(msg.to_text().expect("text").contains("forge"), "{msg:?}");
}

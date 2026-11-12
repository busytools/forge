//! The share upload over a real socket: begin, chunks, end, and the file in
//! the inbox.
//!
//! **The transport is what this test drives**, not the registry alone: the
//! binary frame kind, the command interception and the reply all cross a
//! socket a client opens, because a registry that works in a unit test and a
//! socket that never routes a chunk is the failure this file exists for.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use forge_server::live::Live;
use forge_server::testing::Fleet;
use forge_server::transport::serve;
use forge_server::transport::{Heartbeat, TransportState};
use forge_server::work::WorkCache;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const SEAT: &str = r#"{"org":"TestOrg","project":"proj","label":"lead"}"#;

/// A server over a fixture fleet in `root`, and the URL to reach it.
async fn serve_in(root: &Path) -> String {
    let fleet = Fleet::in_dir(root, &[("TestOrg", &["proj"])]).expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let state = Arc::new(TransportState {
        surface: fleet.surface(),
        work: Arc::new(WorkCache::new()),
        conversations: Arc::new(forge_server::transport::conversation::Conversations::new()),
        live: Mutex::new(Live::new()),
        settings: forge_server::transport::settings_channel(),
        browser_host: None,
        heartbeat: Heartbeat::default(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let updates = state.surface.subscribe_mirror();
    tokio::spawn(async move {
        let _ = serve(state, listener, updates).await;
    });
    // The fleet must outlive the server, and the surface holds the workspace
    // it was built from; the fleet itself is dropped, which is fine because
    // the surface is what the server reads.
    format!("ws://{addr}/socket")
}

async fn dial(url: &str) -> Socket {
    let (socket, _) = tokio_tungstenite::connect_async(url).await.expect("the socket opens");
    socket
}

/// Send one command and answer the reply that carries its id.
async fn command(socket: &mut Socket, id: u64, command: Value) -> Value {
    let frame = json!({ "kind": "command", "command": command, "reply_to": id });
    socket.send(Message::Text(frame.to_string().into())).await.expect("the command goes");
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("an answer inside the bound")
            .expect("the socket is open")
            .expect("a message");
        let Message::Text(text) = message else { continue };
        let value: Value = serde_json::from_str(&text).expect("a JSON frame");
        if value.get("kind").and_then(Value::as_str) == Some("reply")
            && value.get("reply_to").and_then(Value::as_u64) == Some(id)
        {
            return value.get("body").cloned().expect("a reply body");
        }
    }
}

/// One file chunk, as the wire carries it.
fn chunk_frame(upload_id: u64, file: u16, offset: u32, bytes: &[u8]) -> Message {
    let mut frame = Vec::with_capacity(15 + bytes.len());
    frame.push(6);
    frame.extend_from_slice(&upload_id.to_le_bytes());
    frame.extend_from_slice(&file.to_le_bytes());
    frame.extend_from_slice(&offset.to_le_bytes());
    frame.extend_from_slice(bytes);
    Message::Binary(frame.into())
}

/// The inbox root the fixture's workspace resolves: `<config>/app-support`.
fn inbox(root: &Path) -> PathBuf {
    root.join("app-support").join("inbox").join("TestOrg").join("proj")
}

/// **A share lands whole over the socket**: begin answers an id and a path,
/// the chunks write the bytes, and the end renames the partial into place.
#[tokio::test]
async fn a_share_lands_whole_over_the_socket() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let url = serve_in(dir.path()).await;
    let mut socket = dial(&url).await;

    let began = command(
        &mut socket,
        1,
        json!({
            "file_upload_begin": {
                "key": serde_json::from_str::<Value>(SEAT).unwrap(),
                "files": [{ "name": "notes.txt", "size": 11, "mime": "text/plain" }],
            }
        }),
    )
    .await;
    let upload_id = began.get("upload_id").and_then(Value::as_u64).expect("an upload id");
    let paths = began.get("paths").and_then(Value::as_array).expect("the paths");
    assert_eq!(paths.len(), 1);
    let path = PathBuf::from(paths[0].as_str().expect("a path"));
    assert!(path.starts_with(inbox(dir.path())), "{}", path.display());
    assert!(!path.exists(), "nothing has landed before the end");

    socket.send(chunk_frame(upload_id, 0, 0, b"hello ")).await.expect("the first chunk");
    socket.send(chunk_frame(upload_id, 0, 6, b"world")).await.expect("the second chunk");

    let ended = command(
        &mut socket,
        2,
        json!({ "file_upload_end": { "key": serde_json::from_str::<Value>(SEAT).unwrap(), "upload_id": upload_id } }),
    )
    .await;
    let ended_paths = ended.get("paths").and_then(Value::as_array).expect("the paths");
    assert_eq!(ended_paths.len(), 1);
    assert_eq!(std::fs::read(&path).expect("the file landed"), b"hello world");
    assert!(!PathBuf::from(format!("{}.partial", path.display())).exists());
}

/// **A dropped connection leaves the upload for the next one**: a reconnect
/// resumes from the offsets the server holds and finishes the file.
#[tokio::test]
async fn a_reconnected_client_resumes_the_upload() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let url = serve_in(dir.path()).await;

    let mut first = dial(&url).await;
    let began = command(
        &mut first,
        1,
        json!({
            "file_upload_begin": {
                "key": serde_json::from_str::<Value>(SEAT).unwrap(),
                "files": [{ "name": "a.bin", "size": 6 }],
            }
        }),
    )
    .await;
    let upload_id = began.get("upload_id").and_then(Value::as_u64).expect("an id");
    first.send(chunk_frame(upload_id, 0, 0, b"abc")).await.expect("half the file");
    // The phone loses its network: the socket goes without a close handshake.
    drop(first);

    let mut second = dial(&url).await;
    let resumed = command(
        &mut second,
        2,
        json!({ "file_upload_resume": { "key": serde_json::from_str::<Value>(SEAT).unwrap(), "upload_id": upload_id } }),
    )
    .await;
    assert_eq!(
        resumed
            .get("received")
            .and_then(Value::as_array)
            .map(|rows| rows.iter().map(|row| row.as_u64().unwrap_or(0)).collect::<Vec<_>>()),
        Some(vec![3]),
        "the server still holds the three bytes it took",
    );
    second.send(chunk_frame(upload_id, 0, 3, b"def")).await.expect("the rest");
    let ended = command(
        &mut second,
        3,
        json!({ "file_upload_end": { "key": serde_json::from_str::<Value>(SEAT).unwrap(), "upload_id": upload_id } }),
    )
    .await;
    let path = PathBuf::from(ended["paths"][0].as_str().expect("a path"));
    assert_eq!(std::fs::read(&path).expect("the file landed"), b"abcdef");
}

/// A resume for an id the server never minted is refused in words, which is
/// what tells a client to begin again.
#[tokio::test]
async fn a_resume_for_an_unknown_upload_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let url = serve_in(dir.path()).await;
    let mut socket = dial(&url).await;
    let body = command(
        &mut socket,
        1,
        json!({ "file_upload_resume": { "key": serde_json::from_str::<Value>(SEAT).unwrap(), "upload_id": 7 } }),
    )
    .await;
    let why = body.as_str().expect("a refusal is the core's own words");
    assert!(why.contains("unknown or expired"), "{why}");
}

/// A share past the cap is refused before anything is written.
#[tokio::test]
async fn an_over_cap_share_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let url = serve_in(dir.path()).await;
    let mut socket = dial(&url).await;
    let body = command(
        &mut socket,
        1,
        json!({
            "file_upload_begin": {
                "key": serde_json::from_str::<Value>(SEAT).unwrap(),
                "files": [{ "name": "big.bin", "size": 200 * 1024 * 1024 + 1 }],
            }
        }),
    )
    .await;
    let why = body.as_str().expect("a refusal");
    assert!(why.contains("at most"), "{why}");
    assert!(!inbox(dir.path()).exists(), "nothing was created");
}

//! **The `claude` child never waits on the server for an answer that may
//! never come.**
//!
//! A tool call makes the CLI send a control request and block on the
//! `control_response`; forge's bridge forwards it to the workspace, which
//! offers it to whichever client can answer. Nothing bounded that wait, and a
//! connection that went away with the ask in hand left it unresolved: the
//! child sat alive and idle with its transcript stopped at a `tool_use` and
//! no record anywhere.
//!
//! **The hole is latent rather than loud.** A seat nobody is watching never
//! hits it: with no answering subscriber the ask is cancelled the moment it
//! arrives, which is why only watched seats could park. These tests pin each
//! path separately so the two stay distinct.
//!
//! Every test here drives a real `claude` child (the SDK's mock fixture)
//! through a real socket, and reads the CLI's own receipt of the answer: the
//! mock writes the `control_response` it decoded to a file, so a passing
//! assertion is the child having been answered rather than the server having
//! decided it was.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use forge_primitives::SessionSlot;
use forge_server::Command;
use forge_server::surface::SessionUpdate;
use forge_server::transport::TransportState;
use forge_server::transport::envelope::{ClientMessage, ServerMessage, Subject};
use forge_server::transport::serve;
use forge_workspace::Workspace;
use futures_util::{SinkExt, StreamExt};

/// The client end of a socket, named so the helpers below read as one.
type Client =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The tool call the mock asks about, and the id the whole file addresses it
/// by. One id is enough: one process, one child.
const TOOL_ID: &str = "toolu_park_01";
const TOOL: &str = "Edit";

/// The seat the fixture declares.
fn seat() -> SessionSlot {
    SessionSlot::lead("TestOrg", "proj")
}

/// The mock's own receipt of the answer, and the dir the caller holds for as
/// long as it reads one.
fn answer_marker(dir: &Path) -> std::path::PathBuf {
    dir.join("cli-control-answer")
}

/// **The mock asks and then WAITS.** After the initialize handshake it reads
/// one user turn, emits a `can_use_tool` control request for [`TOOL`], and
/// blocks on stdin until the server's `control_response` arrives. What it
/// read goes to the marker file, so a test asserts on the CLI's evidence
/// rather than on the server's intent; a request nobody answers leaves the
/// child parked here, which is the defect the file reproduces.
///
/// The marker is a placeholder rather than a `format!` argument because the
/// JSON below is full of braces.
fn permission_mock(dir: &Path) -> String {
    use std::os::unix::fs::PermissionsExt as _;

    let marker = answer_marker(dir);
    let script = r#"#!/bin/bash
set -euo pipefail
if [[ ${1:-} == "--version" ]]; then printf '%s\n' "2.1.280 (anthropic-mock)"; exit 0; fi
printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-park-001","cwd":"/tmp","tools":["Edit"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'
IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"
IFS= read -r _user
printf '%s\n' '{"type":"control_request","request_id":"req_park_01","request":{"subtype":"can_use_tool","tool_name":"Edit","input":{"file_path":"/tmp/park.txt","old_string":"a","new_string":"b"},"permission_suggestions":[],"blocked_path":null,"tool_use_id":"toolu_park_01","agent_id":null}}'
IFS= read -r response
printf '%s\n' "$response" > "@MARKER@"
printf '%s\n' '{"type":"assistant","message":{"id":"msg_park_01","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"edit denied"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-park-001","parent_tool_use_id":null}'
printf '%s\n' '{"type":"result","subtype":"success","duration_ms":50,"duration_api_ms":40,"is_error":false,"num_turns":1,"session_id":"mock-park-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
"#;
    let path = dir.join("permission-park-mock.sh");
    std::fs::write(&path, script.replace("@MARKER@", &marker.to_string_lossy()))
        .expect("write the permission-park mock");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod the mock");
    path.to_string_lossy().into_owned()
}

/// A workspace over a config directory of its own, with the project its lead
/// spawns into already on disk and an account the gateway can walk.
///
/// **The project directory exists before the workspace opens.** A path that
/// appears after registration canonicalises differently from the one the
/// fixture registered, and every lookup then misses.
fn a_workspace(dir: &Path) -> Arc<Workspace> {
    let project = dir.join("proj");
    std::fs::create_dir_all(&project).expect("project dir");
    let forge = dir.join("forge");
    std::fs::create_dir_all(&forge).expect("forge dir");
    std::fs::write(
        forge.join("forge.toml"),
        format!(
            r#"
[[orgs]]
name = "TestOrg"
accounts = ["Acct"]

[[orgs.projects]]
name = "proj"
path = "{}"
model = "claude-sonnet-5"

[[accounts]]
display_name = "Acct"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#,
            project.display()
        ),
    )
    .expect("write forge.toml");
    let workspace = Arc::new(Workspace::new_for_test(dir.to_owned()).expect("workspace"));
    workspace.seed_test_ready_account("Acct");
    workspace.seed_test_gateway_ready(true);
    workspace.set_settings_version("test-build");
    workspace.seed_test_user_preferences(serde_json::json!({}));
    workspace
}

/// The socket a workspace answers on, and the state behind it so a test can
/// read what the core holds.
async fn serve_workspace(workspace: &Arc<Workspace>) -> (String, Arc<TransportState>) {
    let state = Arc::new(TransportState::for_workspace(workspace));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = serve(served, listener, updates).await;
    });
    (format!("ws://{addr}/socket"), state)
}

/// A client whose greeting is already read: every test below starts from a
/// stream whose next message is the answer to what it sends.
async fn connect(url: &str) -> Client {
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.expect("the socket opens");
    let msg = socket.next().await.expect("a greeting").expect("no error");
    let text = msg.to_text().expect("text").to_owned();
    let greeting: ServerMessage = serde_json::from_str(&text).expect("the greeting decodes");
    assert!(
        matches!(greeting, ServerMessage::Greeting { .. }),
        "the first thing the server says is its greeting, not {text}",
    );
    // The settings document follows in the same open.
    let msg = socket.next().await.expect("a settings frame").expect("no error");
    let text = msg.to_text().expect("text").to_owned();
    let settings: ServerMessage = serde_json::from_str(&text).expect("the settings frame decodes");
    assert!(
        matches!(settings, ServerMessage::Settings { .. }),
        "the second thing the server says is the effective config, not {text}",
    );
    socket
}

async fn send(socket: &mut Client, message: ClientMessage) {
    let text = serde_json::to_string(&message).expect("the message encodes");
    socket
        .send(tokio_tungstenite::tungstenite::Message::text(text))
        .await
        .expect("the message goes");
}

/// Watch the seat as an answering client does: a SUBSCRIBE that declares it
/// can answer a prompt, and the SHOWING that puts the seat on screen.
///
/// Both are what the story needs: the role is what makes the core park the
/// turn on this connection, and the watch is what makes it forward the ask.
/// The subscribe's snapshot is read so the stream is settled before the
/// prompt goes in.
async fn watch_the_seat(socket: &mut Client) {
    subscribe_the_seat(socket, true).await;
}

/// [`watch_the_seat`] with the role the connection declares. `answering` is
/// the client's own statement that it has a dock to reply from; a mirror
/// watches the same seat and can answer nothing.
async fn subscribe_the_seat(socket: &mut Client, answering: bool) {
    send(
        socket,
        ClientMessage::Subscribe { what: Subject::Session(seat()), answering, review: false },
    )
    .await;
    // The subscribe is answered with its snapshot; updates the core raised on
    // its own can arrive ahead of it, so everything before the answer is
    // passed over.
    loop {
        match next_server_within(socket, Duration::from_secs(20)).await {
            Some(ServerMessage::Snapshot { .. }) => break,
            Some(ServerMessage::Update { .. } | ServerMessage::Settings { .. }) => {}
            Some(other) => panic!("a subscribe is answered with a snapshot, not {other:?}"),
            None => panic!("the subscribe was never answered"),
        }
    }
    send(socket, ClientMessage::Showing { seat: Some(seat()) }).await;
}

/// The next message within `within`, or `None` when the server stayed quiet.
async fn next_server_within(socket: &mut Client, within: Duration) -> Option<ServerMessage> {
    let msg = tokio::time::timeout(within, socket.next()).await.ok()??.expect("no socket error");
    match msg {
        tokio_tungstenite::tungstenite::Message::Text(text) => {
            Some(serde_json::from_str(text.as_str()).expect("a server message decodes"))
        }
        other => panic!("the server sent a frame this test does not read: {other:?}"),
    }
}

/// The next update satisfying `satisfies`, with everything else passed over.
async fn update_until(
    socket: &mut Client,
    within: Duration,
    want: &str,
    mut satisfies: impl FnMut(&SessionUpdate) -> bool,
) -> SessionUpdate {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match next_server_within(socket, left).await {
            Some(ServerMessage::Update { update, .. }) => {
                if satisfies(&update) {
                    return *update;
                }
            }
            Some(ServerMessage::Settings { .. }) => {}
            Some(other) => {
                panic!("waited for {want}, and a message that is not an update arrived: {other:?}")
            }
            None => panic!("waited for {want}, and never heard it"),
        }
    }
}

/// Boot the seat over the mock child, and wait for the `Connected` that says
/// its session is live.
///
/// The stream is taken before the spawn, so a `Connected` emitted on the
/// spawn's own task cannot land before this is listening.
async fn boot_the_seat(workspace: &Arc<Workspace>) {
    let mut updates = workspace.subscribe_mirror();
    workspace
        .dispatch(Command::SpawnProject {
            project_name: "proj".to_owned(),
            launch_settings: forge_workspace::SessionLaunchSettings {
                force_new: true,
                ..Default::default()
            },
        })
        .expect("the spawn is taken");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(left, updates.recv())
            .await
            .expect("the seat connects within the bound")
            .expect("the update stream stays open until the seat connects");
        match &*frame.update {
            SessionUpdate::Connected { key, .. } if *key == seat() => return,
            SessionUpdate::ConnectionFailed { key, message, .. } if *key == seat() => {
                panic!("the seat's spawn failed: {message}");
            }
            _ => {}
        }
    }
}

/// Start the seat's turn: the mock answers its first user turn with the ask.
fn prompt_the_seat(workspace: &Arc<Workspace>) {
    workspace
        .dispatch(Command::Prompt {
            key: seat(),
            text: "edit the file".to_owned(),
            attachments: Vec::new(),
        })
        .expect("the prompt is taken");
}

/// Whether the mock wrote its receipt within `within`.
async fn waited_for_marker(path: &Path, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    path.exists()
}

/// **The defect, exactly as the live capture had it.** A client registers as
/// able to answer, is shown the seat, and the ask is delivered to it; then the
/// connection ends without an answer. Nobody is left who can answer, and the
/// property is that the child is still answered - with a denial - rather than
/// parked on a control request forever.
#[tokio::test]
async fn a_client_that_disconnects_without_answering_still_answers_the_cli() {
    let dir = tempfile::tempdir().expect("config tempdir");
    forge_agent::set_test_sdk_binary(Some(permission_mock(dir.path())));
    let workspace = a_workspace(dir.path());
    let (url, _state) = serve_workspace(&workspace).await;
    let mut socket = connect(&url).await;
    watch_the_seat(&mut socket).await;
    boot_the_seat(&workspace).await;

    prompt_the_seat(&workspace);
    let delivered =
        update_until(&mut socket, Duration::from_secs(30), "the permission ask", |update| {
            matches!(
                update,
                SessionUpdate::PermissionRequest { key, tool_id, .. }
                    if *key == seat() && tool_id == TOOL_ID
            )
        })
        .await;
    let SessionUpdate::PermissionRequest { request, .. } = delivered else {
        panic!("the ask was not a permission request");
    };
    assert_eq!(request.tool_call.title, TOOL, "the mock asked about the tool this test names");

    // The client goes away holding the ask, and answers nothing.
    drop(socket);

    let marker = answer_marker(dir.path());
    let answered = waited_for_marker(&marker, Duration::from_secs(10)).await;
    forge_agent::set_test_sdk_binary(None);
    assert!(
        answered,
        "the `claude` child never received an answer for {} ({TOOL_ID}) on {}: the only client \
         that could answer it disconnected, and the control request stays parked in the bridge \
         with the child waiting on it",
        TOOL,
        seat().display(),
    );
    let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
    assert!(
        receipt.contains("\"behavior\":\"deny\""),
        "the CLI was answered, and with a denial rather than an allow: {receipt}",
    );
}

/// **A mirror beside the answerer does not hold the ask.** A watching client
/// that never declared it can answer receives the same frame; if it counted
/// as a holder, the child would stay parked after the only surface that could
/// reply died - the same stall with an extra witness.
#[tokio::test]
async fn a_mirror_beside_the_answerer_does_not_keep_the_ask_alive() {
    let dir = tempfile::tempdir().expect("config tempdir");
    forge_agent::set_test_sdk_binary(Some(permission_mock(dir.path())));
    let workspace = a_workspace(dir.path());
    let (url, _state) = serve_workspace(&workspace).await;
    let mut answerer = connect(&url).await;
    watch_the_seat(&mut answerer).await;
    let mut mirror = connect(&url).await;
    subscribe_the_seat(&mut mirror, false).await;
    boot_the_seat(&workspace).await;

    prompt_the_seat(&workspace);
    let ask = |update: &SessionUpdate| {
        matches!(
            update,
            SessionUpdate::PermissionRequest { key, tool_id, .. }
                if *key == seat() && tool_id == TOOL_ID
        )
    };
    // Both connections are handed the ask; witnessing it is the whole point.
    update_until(&mut answerer, Duration::from_secs(30), "the permission ask", ask).await;
    update_until(&mut mirror, Duration::from_secs(30), "the permission ask on the mirror", ask)
        .await;

    drop(answerer);

    let marker = answer_marker(dir.path());
    let marker_written = waited_for_marker(&marker, Duration::from_secs(10)).await;
    forge_agent::set_test_sdk_binary(None);
    assert!(
        marker_written,
        "the answering client on {} went away while a mirror still watched: a client that \
         cannot answer must not hold the ask, or the child waits on a prompt only the dead \
         connection could have answered",
        seat().display(),
    );
    let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
    assert!(receipt.contains("\"behavior\":\"deny\""), "and answered with a denial: {receipt}");
    drop(mirror);
}

/// **The last resort, for a surface that stays and never answers.** A client
/// keeps its connection and its answering role, is handed the ask, and sends
/// nothing. No connection dies here, so only the bridge's own bound can
/// answer: at the bound the child is denied, the dock is withdrawn, and the
/// pending set no longer holds a prompt whose reply can reach nothing.
#[tokio::test]
async fn a_client_that_never_answers_is_denied_at_the_bound() {
    let dir = tempfile::tempdir().expect("config tempdir");
    forge_agent::set_test_sdk_binary(Some(permission_mock(dir.path())));
    forge_agent::set_test_control_request_bound(Some(Duration::from_secs(2)));
    let workspace = a_workspace(dir.path());
    let (url, _state) = serve_workspace(&workspace).await;
    let mut socket = connect(&url).await;
    watch_the_seat(&mut socket).await;
    boot_the_seat(&workspace).await;

    prompt_the_seat(&workspace);
    update_until(&mut socket, Duration::from_secs(30), "the permission ask", |update| {
        matches!(
            update,
            SessionUpdate::PermissionRequest { key, tool_id, .. }
                if *key == seat() && tool_id == TOOL_ID
        )
    })
    .await;
    // Deliberately unanswered: the connection stays, the prompt stays drawn.

    let marker = answer_marker(dir.path());
    let answered = waited_for_marker(&marker, Duration::from_secs(20)).await;
    let resolved = if answered {
        Some(
            update_until(&mut socket, Duration::from_secs(5), "the prompt resolving", |update| {
                matches!(
                    update,
                    SessionUpdate::PendingInteractionResolved { key, tool_id, .. }
                        if *key == seat() && tool_id == TOOL_ID
                )
            })
            .await,
        )
    } else {
        None
    };
    let pending_left = workspace.pending_asks(&seat()).len();
    forge_agent::set_test_control_request_bound(None);
    forge_agent::set_test_sdk_binary(None);

    assert!(
        answered,
        "the `claude` child was never answered on {} although the bound expired: a client that \
         stays and never answers parks the turn as surely as a connection that dies",
        seat().display(),
    );
    let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
    assert!(receipt.contains("\"behavior\":\"deny\""), "and answered with a denial: {receipt}");
    assert!(resolved.is_some(), "the dock is told the prompt is over");
    assert_eq!(pending_left, 0, "and the seat holds no ask whose answer can reach nothing");
}

/// **The path that already worked, pinned so the two stay distinct.** With no
/// answering client at all the core cancels the ask as it arrives, and the
/// child is denied at once - no connection has to die first. This is why only
/// watched seats were found stalled.
#[tokio::test]
async fn with_no_answering_client_the_cli_is_denied_at_once() {
    let dir = tempfile::tempdir().expect("config tempdir");
    forge_agent::set_test_sdk_binary(Some(permission_mock(dir.path())));
    let workspace = a_workspace(dir.path());
    // No client, and no socket: the only subscriber is the transport's own
    // fold, which is a mirror.
    let _ = serve_workspace(&workspace).await;
    boot_the_seat(&workspace).await;
    prompt_the_seat(&workspace);

    let marker = answer_marker(dir.path());
    let answered = waited_for_marker(&marker, Duration::from_secs(30)).await;
    forge_agent::set_test_sdk_binary(None);
    assert!(
        answered,
        "with nobody attached, the ask is cancelled at birth and the child is answered; it was \
         not, so the unwatched path has regressed too",
    );
    let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
    assert!(receipt.contains("\"behavior\":\"deny\""), "and answered with a denial: {receipt}");
}

//! **The `claude` child never waits on the server for an answer that may
//! never come.**
//!
//! A tool call makes the CLI send a control request and block on the
//! `control_response`; forge's bridge forwards it to the workspace, which
//! offers it to whichever client can answer. Nothing bounded that wait, and a
//! connection that went away with the ask in hand left it unresolved: the
//! child sat alive and idle with its transcript stopped at a `tool_use` and no
//! record anywhere.
//!
//! **The hole is latent rather than loud, and it is not "watched seats only".**
//! An ask is cancelled the moment it arrives only when no answering client is
//! attached anywhere; a forge with one answering client parks the ask of every
//! seat, including one nobody has on screen, until something answers it. What
//! makes a seat answerable is a connection that declared it can answer and
//! watches that seat.
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
use forge_primitives::permission_interaction::PermissionAction;
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

/// The permission ask the mock makes, and the id the tests address it by.
const PERMISSION_TOOL: &str = "Edit";
const PERMISSION_ID: &str = "toolu_park_01";

/// The question ask the mock makes, on its own id so the two never collide.
const QUESTION_TOOL: &str = "AskUserQuestion";
const QUESTION_ID: &str = "toolu_park_q1";

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
/// one user turn, emits a `can_use_tool` control request for `tool`, and
/// blocks on stdin until the server's `control_response` arrives. What it read
/// goes to the marker file, so a test asserts on the CLI's evidence rather
/// than on the server's intent; a request nobody answers leaves the child
/// parked here, which is the defect the file reproduces.
///
/// The placeholders are replaced rather than formatted because the JSON below
/// is full of braces.
fn a_cli_mock(dir: &Path, tool: &str, tool_use_id: &str, input: &str) -> String {
    use std::os::unix::fs::PermissionsExt as _;

    let script = r#"#!/bin/bash
set -euo pipefail
if [[ ${1:-} == "--version" ]]; then printf '%s\n' "2.1.280 (anthropic-mock)"; exit 0; fi
printf '%s\n' '{"type":"system","subtype":"init","session_id":"mock-park-001","cwd":"/tmp","tools":["Edit"],"mcp_servers":[],"model":"claude-opus-4-5","permissionMode":"default","apiKeySource":"ANTHROPIC_API_KEY"}'
IFS= read -r init_req
init_id=$(printf '%s' "$init_req" | python3 -c 'import sys, json; d=json.load(sys.stdin); print(d.get("request_id",""))' 2>/dev/null || echo "")
printf '%s\n' "{\"type\":\"control_response\",\"response\":{\"subtype\":\"success\",\"request_id\":\"$init_id\",\"response\":{}}}"
IFS= read -r _user
printf '%s\n' '{"type":"control_request","request_id":"req_park_01","request":{"subtype":"can_use_tool","tool_name":"@TOOL@","input":@INPUT@,"permission_suggestions":[],"blocked_path":null,"tool_use_id":"@TOOL_ID@","agent_id":null}}'
response=""
while IFS= read -r line; do
  printf '%s\n' "$line" >> "@MARKER@.lines"
  case "$line" in
    *'"request_id":"req_park_01"'*)
      response="$line"
      break
      ;;
  esac
done
printf '%s\n' "$response" > "@MARKER@.tmp"
mv "@MARKER@.tmp" "@MARKER@"
printf '%s\n' '{"type":"assistant","message":{"id":"msg_park_01","role":"assistant","model":"claude-opus-4-5","content":[{"type":"text","text":"edit denied"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"session_id":"mock-park-001","parent_tool_use_id":null}'
printf '%s\n' '{"type":"result","subtype":"success","duration_ms":50,"duration_api_ms":40,"is_error":false,"num_turns":1,"session_id":"mock-park-001","total_cost_usd":0.0001,"usage":{"input_tokens":5,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}'
"#;
    let script = script
        .replace("@TOOL_ID@", tool_use_id)
        .replace("@TOOL@", tool)
        .replace("@INPUT@", input)
        .replace("@MARKER@", &answer_marker(dir).to_string_lossy());
    let path = dir.join(format!("mock-{}.sh", tool.to_lowercase()));
    std::fs::write(&path, script).expect("write the control-request mock");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("chmod the mock");
    path.to_string_lossy().into_owned()
}

/// The mock's `can_use_tool` ask for a permission.
fn permission_mock(dir: &Path) -> String {
    a_cli_mock(
        dir,
        PERMISSION_TOOL,
        PERMISSION_ID,
        r#"{"file_path":"/tmp/park.txt","old_string":"a","new_string":"b"}"#,
    )
}

/// The mock's `can_use_tool` ask for an `AskUserQuestion`.
fn question_mock(dir: &Path) -> String {
    a_cli_mock(
        dir,
        QUESTION_TOOL,
        QUESTION_ID,
        r#"{"questions":[{"question":"Which environment?","header":"Environment","options":[{"label":"Staging","description":"staging"},{"label":"Prod","description":"prod"}]}]}"#,
    )
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

/// Watch the seat as an answering client does.
async fn watch_the_seat(socket: &mut Client) {
    subscribe_the_seat(socket, true).await;
}

/// [`watch_the_seat`] with the role the connection declares. `answering` is
/// the client's own statement that it has a dock to reply from; a mirror
/// watches the same seat and can answer nothing.
async fn subscribe_the_seat(socket: &mut Client, answering: bool) {
    let _ = subscribe_the_seat_with_snapshot(socket, answering).await;
}

/// [`subscribe_the_seat`], handing back the snapshot its subscribe is answered
/// with - the read a client attaching after an ask draws the dock from.
async fn subscribe_the_seat_with_snapshot(
    socket: &mut Client,
    answering: bool,
) -> serde_json::Value {
    send(
        socket,
        ClientMessage::Subscribe { what: Subject::Session(seat()), answering, review: false },
    )
    .await;
    // The subscribe is answered with its snapshot; updates the core raised on
    // its own can arrive ahead of it, so everything before the answer is
    // passed over.
    let data = loop {
        match next_server_within(socket, Duration::from_secs(20)).await {
            Some(ServerMessage::Snapshot { data, .. }) => break data,
            Some(ServerMessage::Update { .. } | ServerMessage::Settings { .. }) => {}
            Some(other) => panic!("a subscribe is answered with a snapshot, not {other:?}"),
            None => panic!("the subscribe was never answered"),
        }
    };
    send(socket, ClientMessage::Showing { seat: Some(seat()) }).await;
    data
}

/// The next message within `within`, or `None` when the server stayed quiet.
///
/// **WebSocket pings are passed over, not treated as an ending.** The socket
/// carries them on its own clock, and a loaded machine delivers one while a
/// test waits for something else.
async fn next_server_within(socket: &mut Client, within: Duration) -> Option<ServerMessage> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(left, socket.next()).await.ok()??.expect("no socket error");
        match msg {
            tokio_tungstenite::tungstenite::Message::Text(text) => {
                return Some(
                    serde_json::from_str(text.as_str()).expect("a server message decodes"),
                );
            }
            tokio_tungstenite::tungstenite::Message::Ping(_)
            | tokio_tungstenite::tungstenite::Message::Pong(_) => {}
            other => panic!("the server sent a frame this test does not read: {other:?}"),
        }
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
            text: "go on then".to_owned(),
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

/// The ask's own update, whichever kind the test's mock produced.
fn is_ask(tool_id: &'static str) -> impl FnMut(&SessionUpdate) -> bool {
    move |update| match update {
        SessionUpdate::PermissionRequest { key, tool_id: id, .. }
        | SessionUpdate::QuestionRequest { key, tool_id: id, .. } => {
            *key == seat() && id == tool_id
        }
        _ => false,
    }
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
    let delivered = update_until(
        &mut socket,
        Duration::from_secs(30),
        "the permission ask",
        is_ask(PERMISSION_ID),
    )
    .await;
    let SessionUpdate::PermissionRequest { request, .. } = delivered else {
        panic!("the ask was not a permission request");
    };
    assert_eq!(request.tool_call.title, PERMISSION_TOOL, "the mock asked about the tool named");

    // The client goes away holding the ask, and answers nothing.
    drop(socket);

    let marker = answer_marker(dir.path());
    let answered = waited_for_marker(&marker, Duration::from_secs(20)).await;
    forge_agent::set_test_sdk_binary(None);
    assert!(
        answered,
        "the `claude` child never received an answer for {} ({PERMISSION_ID}) on {}: the only \
         client that could answer it disconnected, and the control request stays parked in the \
         bridge with the child waiting on it",
        PERMISSION_TOOL,
        seat().display(),
    );
    let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
    assert!(
        receipt.contains("\"behavior\":\"deny\""),
        "the CLI was answered, and with a denial rather than an allow: {receipt}",
    );
}

/// **A mirror beside the answerer does not hold the ask.** A watching client
/// that never declared it can answer receives the same frame; if it counted as
/// an answerer, the child would stay parked after the only surface that could
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
    // Both connections are handed the ask; witnessing it is the whole point.
    update_until(
        &mut answerer,
        Duration::from_secs(30),
        "the permission ask",
        is_ask(PERMISSION_ID),
    )
    .await;
    update_until(
        &mut mirror,
        Duration::from_secs(30),
        "the ask on the mirror",
        is_ask(PERMISSION_ID),
    )
    .await;

    drop(answerer);

    let marker = answer_marker(dir.path());
    let marker_written = waited_for_marker(&marker, Duration::from_secs(20)).await;
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

/// **A second answering client keeps the ask alive when the first goes.**
/// The second attaches after the ask and learns it from its snapshot - a read,
/// not the live frame - and it answers through the same command. Denying under
/// it would be a prompt withdrawn from a surface that could still answer it.
#[tokio::test]
async fn a_second_answerer_keeps_the_ask_alive_when_the_first_goes() {
    let dir = tempfile::tempdir().expect("config tempdir");
    forge_agent::set_test_sdk_binary(Some(permission_mock(dir.path())));
    let workspace = a_workspace(dir.path());
    let (url, _state) = serve_workspace(&workspace).await;
    let mut first = connect(&url).await;
    watch_the_seat(&mut first).await;
    boot_the_seat(&workspace).await;

    prompt_the_seat(&workspace);
    update_until(&mut first, Duration::from_secs(30), "the permission ask", is_ask(PERMISSION_ID))
        .await;

    // The second client attaches AFTER the ask and reads it from the snapshot.
    let mut second = connect(&url).await;
    let snapshot = subscribe_the_seat_with_snapshot(&mut second, true).await;
    assert!(
        snapshot.to_string().contains(PERMISSION_ID),
        "the snapshot hands the second client the pending ask: {snapshot}",
    );

    drop(first);

    // Longer than the release grace: nothing may be denied while the second
    // connection can still answer.
    let marker = answer_marker(dir.path());
    let denied_early = waited_for_marker(&marker, Duration::from_secs(8)).await;
    if denied_early {
        forge_agent::set_test_sdk_binary(None);
        let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
        panic!(
            "the ask on {} was answered while a second answering client held it: {receipt}",
            seat().display(),
        );
    }

    send(
        &mut second,
        ClientMessage::Command {
            command: Box::new(Command::RespondPermission {
                key: seat(),
                tool_id: PERMISSION_ID.to_owned(),
                outcome: forge_primitives::PermissionOutcome::Selected {
                    option_id: "allow_once".to_owned(),
                    action: PermissionAction::Allow,
                    notes_text: None,
                    edited_input: None,
                },
            }),
            reply_to: None,
        },
    )
    .await;

    let answered = waited_for_marker(&marker, Duration::from_secs(20)).await;
    forge_agent::set_test_sdk_binary(None);
    assert!(
        answered,
        "the second client answered and the CLI was never told: the ask on {} was dropped when \
         the first connection went",
        seat().display(),
    );
    let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
    assert!(
        receipt.contains("\"behavior\":\"allow\""),
        "the second client's allow reached the CLI: {receipt}",
    );
}

/// **A watcher that raises its role holds every seat it already watched.**
/// A connection can watch a seat as a mirror and only later declare it can
/// answer, on a subscribe that names some other subject. The seats it already
/// watches must enter the registry then, or a disconnect by the other
/// answerer denies a prompt this connection can still reach.
#[tokio::test]
async fn a_role_raised_watcher_holds_the_seat_it_already_watched() {
    let dir = tempfile::tempdir().expect("config tempdir");
    forge_agent::set_test_sdk_binary(Some(permission_mock(dir.path())));
    let workspace = a_workspace(dir.path());
    let (url, _state) = serve_workspace(&workspace).await;
    let mut answerer = connect(&url).await;
    watch_the_seat(&mut answerer).await;
    let mut raised = connect(&url).await;
    // Watches the seat without being able to answer it.
    subscribe_the_seat(&mut raised, false).await;
    // Raises the role by subscribing a subject that is not the seat.
    send(
        &mut raised,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    loop {
        match next_server_within(&mut raised, Duration::from_secs(20)).await {
            Some(ServerMessage::Snapshot { .. }) => break,
            Some(ServerMessage::Update { .. } | ServerMessage::Settings { .. }) => {}
            Some(other) => panic!("the home subscribe is answered with a snapshot, not {other:?}"),
            None => panic!("the home subscribe was never answered"),
        }
    }
    boot_the_seat(&workspace).await;

    prompt_the_seat(&workspace);
    update_until(
        &mut answerer,
        Duration::from_secs(30),
        "the permission ask",
        is_ask(PERMISSION_ID),
    )
    .await;
    update_until(
        &mut raised,
        Duration::from_secs(30),
        "the ask on the raised watcher",
        is_ask(PERMISSION_ID),
    )
    .await;

    drop(answerer);

    // Longer than the release grace: the raised watcher was an answerer of the
    // seat before this disconnect, so nothing may be denied.
    let marker = answer_marker(dir.path());
    let denied_early = waited_for_marker(&marker, Duration::from_secs(8)).await;
    if denied_early {
        forge_agent::set_test_sdk_binary(None);
        let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
        panic!(
            "the ask on {} was denied although the role-raised watcher still held it: {receipt}",
            seat().display(),
        );
    }

    send(
        &mut raised,
        ClientMessage::Command {
            command: Box::new(Command::RespondPermission {
                key: seat(),
                tool_id: PERMISSION_ID.to_owned(),
                outcome: forge_primitives::PermissionOutcome::Selected {
                    option_id: "allow_once".to_owned(),
                    action: PermissionAction::Allow,
                    notes_text: None,
                    edited_input: None,
                },
            }),
            reply_to: None,
        },
    )
    .await;

    let marker_written = waited_for_marker(&marker, Duration::from_secs(20)).await;
    forge_agent::set_test_sdk_binary(None);
    assert!(
        marker_written,
        "the role-raised watcher answered and the CLI was never told: its seat left the registry \
         when the other answerer went",
    );
    let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
    assert!(
        receipt.contains("\"behavior\":\"allow\""),
        "the raised watcher's allow reached the CLI: {receipt}",
    );
}

/// **A reconnect inside the grace keeps its ask.** The page retries in 50 ms,
/// a reload closes the socket first, and the phone redials on foreground: a
/// client that comes back inside the grace must see the prompt it left,
/// exactly as on main, and only a client that stays away is denied.
#[tokio::test]
async fn a_reconnect_inside_the_grace_keeps_its_ask() {
    let dir = tempfile::tempdir().expect("config tempdir");
    forge_agent::set_test_sdk_binary(Some(permission_mock(dir.path())));
    let workspace = a_workspace(dir.path());
    let (url, _state) = serve_workspace(&workspace).await;
    let mut first = connect(&url).await;
    watch_the_seat(&mut first).await;
    boot_the_seat(&workspace).await;

    prompt_the_seat(&workspace);
    update_until(&mut first, Duration::from_secs(30), "the permission ask", is_ask(PERMISSION_ID))
        .await;
    drop(first);

    // The reconnect lands well inside the five second grace.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut again = connect(&url).await;
    let snapshot = subscribe_the_seat_with_snapshot(&mut again, true).await;
    assert!(
        snapshot.to_string().contains(PERMISSION_ID),
        "the reconnected client reads the pending ask from its snapshot: {snapshot}",
    );

    let marker = answer_marker(dir.path());
    let denied_early = waited_for_marker(&marker, Duration::from_secs(7)).await;
    if denied_early {
        forge_agent::set_test_sdk_binary(None);
        let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
        panic!(
            "the ask on {} was denied although the client reconnected inside the grace: {receipt}",
            seat().display(),
        );
    }

    send(
        &mut again,
        ClientMessage::Command {
            command: Box::new(Command::RespondPermission {
                key: seat(),
                tool_id: PERMISSION_ID.to_owned(),
                outcome: forge_primitives::PermissionOutcome::Selected {
                    option_id: "allow_once".to_owned(),
                    action: PermissionAction::Allow,
                    notes_text: None,
                    edited_input: None,
                },
            }),
            reply_to: None,
        },
    )
    .await;

    let answered = waited_for_marker(&marker, Duration::from_secs(20)).await;
    forge_agent::set_test_sdk_binary(None);
    assert!(answered, "the reconnected client answered and the CLI was never told");
    let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
    assert!(
        receipt.contains("\"behavior\":\"allow\""),
        "the reconnected client's allow reached the CLI: {receipt}",
    );
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
    update_until(&mut socket, Duration::from_secs(30), "the permission ask", is_ask(PERMISSION_ID))
        .await;
    // Deliberately unanswered: the connection stays, the prompt stays drawn.

    let marker = answer_marker(dir.path());
    let answered = waited_for_marker(&marker, Duration::from_secs(20)).await;
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

    // The dock goes with the denial: a resolved frame, and nothing pending.
    let resolved =
        update_until(&mut socket, Duration::from_secs(5), "the prompt resolving", |update| {
            matches!(
                update,
                SessionUpdate::PendingInteractionResolved { key, tool_id, .. }
                    if *key == seat() && tool_id == PERMISSION_ID
            )
        })
        .await;
    assert!(
        matches!(resolved, SessionUpdate::PendingInteractionResolved { .. }),
        "the dock is told the prompt is over",
    );
    assert_eq!(
        workspace.pending_asks(&seat()).len(),
        0,
        "and the seat holds no ask whose answer can reach nothing",
    );
}

/// **The question half takes the same bound.** An `AskUserQuestion` whose
/// client never answers must be denied at the bound too, and its dock
/// withdrawn, or a regression there ships green while every permission test
/// passes.
#[tokio::test]
async fn an_unanswered_question_is_denied_at_the_bound() {
    let dir = tempfile::tempdir().expect("config tempdir");
    forge_agent::set_test_sdk_binary(Some(question_mock(dir.path())));
    forge_agent::set_test_control_request_bound(Some(Duration::from_secs(2)));
    let workspace = a_workspace(dir.path());
    let (url, _state) = serve_workspace(&workspace).await;
    let mut socket = connect(&url).await;
    watch_the_seat(&mut socket).await;
    boot_the_seat(&workspace).await;

    prompt_the_seat(&workspace);
    update_until(&mut socket, Duration::from_secs(30), "the question ask", is_ask(QUESTION_ID))
        .await;

    let marker = answer_marker(dir.path());
    let answered = waited_for_marker(&marker, Duration::from_secs(20)).await;
    forge_agent::set_test_control_request_bound(None);
    forge_agent::set_test_sdk_binary(None);
    assert!(
        answered,
        "the `claude` child was never answered on {} although the question's bound expired",
        seat().display(),
    );
    let receipt = std::fs::read_to_string(&marker).expect("read the CLI's receipt");
    let lines = std::fs::read_to_string(marker.with_extension("lines")).unwrap_or_default();
    assert!(
        receipt.contains("\"behavior\":\"deny\""),
        "and answered with a denial: {receipt}; lines read: {lines}",
    );

    update_until(&mut socket, Duration::from_secs(5), "the question resolving", |update| {
        matches!(
            update,
            SessionUpdate::PendingInteractionResolved { key, tool_id, .. }
                if *key == seat() && tool_id == QUESTION_ID
        )
    })
    .await;
    assert_eq!(
        workspace.pending_asks(&seat()).len(),
        0,
        "the question's dock is withdrawn with its denial",
    );
}

/// **The path that already worked, pinned so the two stay distinct.** With no
/// answering client at all the core cancels the ask as it arrives, and the
/// child is denied at once - no connection has to die first.
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

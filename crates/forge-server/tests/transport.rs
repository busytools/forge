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
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;

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

/// What a composer is doing is announced once and retained nowhere, so a
/// client attaching to a running session cannot rebuild it: a take in flight,
/// the line a finished one left, a compaction, a sign-in, and the ask it is
/// answering - which rides `pending_ask` rather than being copied here.
#[tokio::test]
async fn a_running_take_is_on_the_seat_a_client_attaches_to() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
    )
    .await;
    let ServerMessage::Snapshot { .. } = next_server(&mut socket).await else {
        panic!("the subscribe is answered with a snapshot first")
    };

    fleet.emit(SessionUpdate::DictateStarted { key: lead_seat(), floor_db: -50.0, generation: 1 });
    assert!(
        matches!(next_server(&mut socket).await, ServerMessage::Update { .. }),
        "reading it back is what proves the fold ran before the next subscribe",
    );

    let mut fresh = connect(&url).await;
    send(
        &mut fresh,
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
    )
    .await;
    let ServerMessage::Snapshot { data, .. } = next_server(&mut fresh).await else {
        panic!("expected the session snapshot")
    };

    assert_eq!(
        data["composer"]["take"]["phase"], "recording",
        "the take a client never saw announced is on the record: {}",
        data["composer"],
    );
    assert_eq!(
        data["composer"]["take"]["floor_db"], -50.0,
        "with the floor its own meter measures against",
    );
}

/// A turn that finishes while nobody is showing the seat leaves a mark, and
/// the mark is exactly the fact a client cannot reconstruct: a transcript says
/// the turn ended, never that it ended unwatched. It rides the home snapshot,
/// so a client attaching afterwards learns it rather than waiting for the next
/// turn to end.
#[tokio::test]
async fn a_turn_that_finished_unwatched_marks_its_row() {
    let (url, fleet) = a_server().await;
    // A mark is drawn on a row, so the seat has to have one: the home lists a
    // project's agents, and a seat nothing has started is not among them.
    fleet.install_agent("TestOrg", "proj", "lead");
    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
    let ServerMessage::Snapshot { .. } = next_server(&mut socket).await else {
        panic!("the subscribe is answered with a snapshot first")
    };

    // A turn ends with this connection watching the home and not the seat, so
    // nobody is showing it. Reading the update back is what proves the fold
    // ran before the next subscription is served.
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "s",
        }))
        .expect("parse a result message"),
    });
    assert!(
        matches!(next_server(&mut socket).await, ServerMessage::Update { .. }),
        "the completion reaches the home as a row change",
    );

    let mut fresh = connect(&url).await;
    send(&mut fresh, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
    let ServerMessage::Snapshot { data, .. } = next_server(&mut fresh).await else {
        panic!("expected the home snapshot")
    };

    let unseen = data["unseen"].as_array().expect("the home carries the marks");
    assert!(
        unseen.iter().any(|slot| slot["label"] == "lead" && slot["project"] == "proj"),
        "the seat whose turn went unwatched is marked: {unseen:?}",
    );
}

/// Four commands report through `reply_to` and have no update behind them, so
/// a client that omits it is not opting out of a reply - it is opting out of
/// knowing whether the work happened. The refusal names the field rather than
/// leaving the client to work out why a fire-and-forget command is different.
#[tokio::test]
async fn a_command_that_answers_through_a_reply_requires_reply_to() {
    let mut socket = connected().await;
    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::DespawnWorker {
                project_key: forge_workspace::ProjectKey::new("whatever"),
                label: "w1".to_owned(),
                force: false,
                respond: None,
            }),
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Error { what, why } = next_server(&mut socket).await else {
        panic!("a silent dispatch here is the client unable to tell it from success");
    };
    assert_eq!(what, "reply_to", "the refusal names the field that is missing");
    assert!(why.contains("reply_to"), "and says which field in the sentence: {why}");
}

/// Answering a prompt that is not waiting - already answered on another
/// client, or a prompt of another kind - is a click on a dock that is gone.
/// The core used to log it and report `Ok(())`, so the socket sent nothing and
/// the reader had no way to learn why nothing happened.
#[tokio::test]
async fn an_answer_to_a_prompt_that_is_gone_is_refused() {
    let mut socket = connected().await;
    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::RespondPermission {
                key: lead_seat(),
                tool_id: "nothing-is-waiting".to_owned(),
                outcome: forge_primitives::permission_interaction::PermissionOutcome::Cancelled,
            }),
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Error { what, why } = next_server(&mut socket).await else {
        panic!("silence here is the click that did nothing");
    };
    assert_eq!(what, "dispatch", "the refusal is the core's own: {why}");
    assert!(why.contains("nothing-is-waiting"), "and it names the prompt: {why}");
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

/// The transcript rows one turn leaves: what the user wrote, what the
/// assistant said, and the result that closes it.
fn a_turns_rows(turn: usize) -> String {
    format!(
        r#"{{"type":"user","uuid":"u{turn}","message":{{"role":"user","content":"turn {turn}"}}}}
{{"type":"assistant","uuid":"a{turn}","message":{{"id":"m{turn}","role":"assistant","model":"claude-opus-5","content":[{{"type":"text","text":"reply {turn}"}}]}}}}
{{"type":"result","uuid":"r{turn}","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}}"#
    )
}

/// A client asking for history is handed whole turns over the socket: a page
/// opens on a turn the user wrote, and it carries the handle that asks for
/// the ones above it.
#[tokio::test]
async fn a_more_is_answered_with_a_page_of_whole_turns() {
    let (url, fleet) = a_server().await;
    let rows: Vec<String> = (0..20).map(a_turns_rows).collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 5 })
        .await;

    let ServerMessage::Page { rows, cursor, .. } = next_server(&mut socket).await else {
        panic!("a page is the answer to a request for more")
    };
    assert!(!rows.is_empty(), "the newest turns come back");
    assert_eq!(
        rows[0].get("kind").and_then(serde_json::Value::as_str),
        Some("user_turn"),
        "a page opens on a turn the user wrote rather than inside one: {rows:?}",
    );
    let kinds: Vec<&str> =
        rows.iter().filter_map(|row| row.get("kind").and_then(serde_json::Value::as_str)).collect();
    assert!(cursor.is_some(), "and it carries the handle that asks for the ones above: {kinds:?}");
}

/// A subscription hears the updates its subject receives and no others.
#[tokio::test]
async fn a_subscriber_hears_the_update_it_asked_for_and_not_another_seats() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
    let ServerMessage::Snapshot { .. } = next_server(&mut socket).await else {
        panic!("the subscribe is answered with a snapshot first")
    };

    // An App-level update names no slot, which is what the home subscription is for.
    fleet.emit(SessionUpdate::CatalogLoaded);
    assert!(
        matches!(next_server(&mut socket).await, ServerMessage::Update { .. }),
        "a home subscriber hears an App-level update",
    );

    // A seat's own news reaches the home, because a row states it: whether the
    // seat is running, waiting on an answer, or finished with a turn nobody
    // looked at. A home that heard only the App-level updates would be a still
    // photograph of a fleet changing underneath it.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });
    assert!(
        matches!(next_server(&mut socket).await, ServerMessage::Update { .. }),
        "a home subscriber hears a seat's row change",
    );

    // And the conversation does not. A token is the bulk of the stream and no
    // row draws one, so carrying it here would re-send the whole fleet for
    // every word of every seat in it. The evidence is ORDER, never a timeout:
    // waiting for the update NOT to arrive would hang on the correct
    // behaviour, so the test asks for something whose answer must come next
    // and asserts THAT is what it got.
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "message": {
                "id": "m1",
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{ "type": "text", "text": "a token" }],
            },
            "session_id": "s",
        }))
        .expect("parse an assistant message"),
    });
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
    let msg = next_server(&mut socket).await;
    assert!(
        matches!(msg, ServerMessage::Snapshot { .. }),
        "a chat token must not reach a home subscriber; the snapshot should be next: {msg:?}",
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
        send(
            socket,
            ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
        )
        .await;
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
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
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

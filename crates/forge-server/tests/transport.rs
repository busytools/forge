//! The socket, from the other side of it: a client opens one against a
//! server this test starts itself.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use forge_primitives::{AgentCommand, ImageAttachment, SessionId, SessionSlot};
use forge_server::Command;
use forge_server::surface::SessionUpdate;
use forge_server::surface::inspector::ContextUsage;
use forge_server::testing::{Fleet, ViewFacts};
use forge_server::transport::conversation::Conversation;
use forge_server::transport::envelope::{ClientMessage, ServerMessage, Subject};
use forge_server::transport::serve;
use forge_server::transport::wire::{self, OpenTimings};
use forge_server::transport::{Heartbeat, TransportState};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
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
/// The fleet carries the config directory its store lives under, so the
/// returned `Fleet` has to be held for as long as a test drives the socket.
async fn a_server() -> (String, Fleet) {
    let (url, fleet, _state) = a_server_with_state().await;
    (url, fleet)
}

/// [`a_server`], keeping the state a test needs to put a seat's conversation
/// where a `Connected` would have left it.
///
/// **A fixture that seeds a transcript has to say which seat it is for.** One
/// registered as a RUNNING session (`Fleet::seed_transcript`) is held over by
/// [`Fleet::hold_conversation`] where a test wants the window a `Connected`
/// carries; one registered in the store alone
/// (`Fleet::seed_asleep_transcript`) is read off the file by the transport
/// itself, which is what a sleeping seat's page gets.
async fn a_server_with_state() -> (String, Fleet, Arc<TransportState>) {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let (url, state) = serve_fleet(&fleet, Heartbeat::default()).await;
    (url, fleet, state)
}

/// The socket a fleet answers on, for a test that built its own fleet (and
/// so owns the config the server reads).
///
/// The keepalive is the caller's: the shipping pair is what every test but
/// the reap's wants, and that one shortens it so a silent peer can be watched
/// being reaped rather than waiting the minute out.
async fn serve_fleet(fleet: &Fleet, heartbeat: Heartbeat) -> (String, Arc<TransportState>) {
    let state =
        Arc::new(TransportState::for_workspace(&fleet.workspace()).with_heartbeat(heartbeat));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = serve(served, listener, updates).await;
    });
    (format!("ws://{addr}/socket"), state)
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
    // The settings document follows in the same open; every test below
    // starts from a stream whose next message is the answer to what it
    // sends.
    let msg = socket.next().await.expect("a settings frame").expect("no error");
    let text = msg.to_text().expect("text").to_owned();
    let settings: ServerMessage = serde_json::from_str(&text).expect("the settings frame decodes");
    assert!(
        matches!(settings, ServerMessage::Settings { .. }),
        "the second thing the server says is the effective config, not {text}",
    );
    socket
}

/// [`connect`] with an `Origin` header, for the socket's gate. The greeting
/// and the settings frame are already read, so the caller holds the same
/// stream every other test starts from.
async fn connect_with_origin(url: &str, origin: &str) -> Client {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = url.into_client_request().expect("a client request");
    request.headers_mut().insert("Origin", origin.parse().expect("an origin header value"));
    let (mut socket, _) =
        tokio_tungstenite::connect_async(request).await.expect("the socket opens");
    let msg = socket.next().await.expect("a greeting").expect("no error");
    let greeting: ServerMessage =
        serde_json::from_str(msg.to_text().expect("text")).expect("the greeting decodes");
    assert!(matches!(greeting, ServerMessage::Greeting { .. }), "the greeting came first");
    let msg = socket.next().await.expect("a settings frame").expect("no error");
    let settings: ServerMessage =
        serde_json::from_str(msg.to_text().expect("text")).expect("the frame decodes");
    assert!(matches!(settings, ServerMessage::Settings { .. }), "the settings frame followed");
    socket
}

/// A client on a server of its own, for a test that needs no fleet.
async fn connected() -> Client {
    let (url, _fleet) = a_server().await;
    connect(&url).await
}

/// **The fold is attached before the first `Connected`.**
///
/// Every seat's copy comes from the seal the fold applies. A subscription
/// taken inside the serve task rather than by its caller is a race the boot
/// order has no reason to win: the boot wave spawns sessions as soon as
/// `serve_socket` returns, and a `Connected` emitted before the fold is
/// listening is dropped - the transport would answer that seat from the
/// transcript alone, losing every frame a transcript never carries.
#[tokio::test]
async fn the_fold_is_attached_before_the_first_connected() {
    let (url, fleet, state) = a_server_with_state().await;
    // The serve path takes the subscription before it spawns, so the count is
    // already one here - with no client attached, and no wait.
    assert!(
        fleet.subscriber_count() >= 1,
        "the fold subscribes inside the serve task rather than before it",
    );

    let seat = lead_seat();
    // The notice is armed before the emit, so a fold that seeds between the
    // emit and the wait is still seen rather than waited out.
    let seeded = state.conversations.seeded_notice();
    tokio::pin!(seeded);
    seeded.as_mut().enable();
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: SessionId::from("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45"),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the seal's row")],
        dropped: 0,
        compaction_count: 0,
    });
    tokio::time::timeout(SEED_WAIT, seeded.as_mut())
        .await
        .expect("the fold was not listening when the first Connected was emitted");

    // A client that never saw the seat start reads that same copy.
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    assert_eq!(
        says_times(&data, "the seal's row"),
        1,
        "the copy the fold made is what a page reads",
    );
}

/// [`connect`], with no message cap of its own.
///
/// The rig for the one test about a frame the CLIENT's cap answers for: a
/// refusal from tungstenite's defaults (64MiB a message, 16MiB a frame) is
/// the test's own ceiling, not anything forge decided, and it would fail a
/// test whose point is what the SERVER emitted.
async fn connect_unbounded(url: &str) -> Client {
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(None)
        .max_frame_size(None);
    let (mut socket, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), false)
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

/// Subscribes to `subject` over and over until `want` holds of the snapshot,
/// or the attempts run out.
///
/// POLLING rather than one read, because the fold is the transport's own task:
/// a snapshot taken immediately after an emit can lag it by a scheduling hop.
/// A test that reads once and asserts is claiming an ordering the code does not
/// give, and would pass for a reason nobody would guess.
async fn snapshot_until(
    socket: &mut Client,
    subject: Subject,
    mut want: impl FnMut(&serde_json::Value) -> bool,
) -> bool {
    for _ in 0..200 {
        send(
            socket,
            ClientMessage::Subscribe { what: subject.clone(), answering: true, review: false },
        )
        .await;
        let (_, data, _) = snapshot_answering(socket).await;
        if want(&data) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

/// Waits for the server to notice a client went away: the subscription count
/// falls back to what it was before that client attached.
///
/// A COUNT rather than an emit. "Did anyone receive" stopped answering this
/// the moment the transport took a subscription of its own for the fold -
/// that listener is always there, so every emit lands whatever the clients do.
///
/// The socket closing is local to the client; the server finds out when its
/// own read fails, which is a scheduling hop away. Polling is honest because
/// the property IS "eventually", and it is bounded so a server that never
/// notices fails the test that waits rather than hanging it.
async fn wait_for_the_server_to_notice(fleet: &Fleet, attached: usize) -> bool {
    for _ in 0..200 {
        // The emit is what reaps: the fan-out drops a dead subscriber when a
        // send to it fails, so the count is read AFTER one rather than
        // instead of it.
        fleet.emit_and_report(SessionUpdate::CatalogLoaded);
        if fleet.subscriber_count() < attached {
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

/// The server's next message, or `None` if it said nothing inside `ms`.
///
/// **Control frames are the socket's, not the protocol's**: the server's
/// keepalive pings arrive on the same stream, and `to_text` reports a ping's
/// payload as text, so a reader that took the next frame as a message would
/// read one as an empty document.
async fn next_server_within(socket: &mut Client, ms: u64) -> Option<ServerMessage> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = tokio::time::timeout(left, socket.next()).await.ok()?.expect("a message");
        let msg = msg.expect("no error");
        let Message::Text(text) = msg else { continue };
        return Some(serde_json::from_str(&text).expect("decode"));
    }
}

/// The server's next message as raw text, or `None` inside `ms`.
///
/// For the messages the typed reader refuses: a `no_audio` outcome carries a
/// `peak_db` that a no-signal take crosses as `null`, and unlike the level's
/// own `peak_db` that field has no tolerance on the read side. Control frames
/// are passed over for the reason [`next_server_within`] gives.
async fn next_text_within(socket: &mut Client, ms: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = tokio::time::timeout(left, socket.next()).await.ok()?.expect("a message");
        let msg = msg.expect("no error");
        let Message::Text(text) = msg else { continue };
        return Some(text.to_string());
    }
}

/// The server's next message.
///
/// Bounded, so a server that answers nothing fails the test that is waiting
/// rather than hanging it: a socket left open with nothing said is the
/// failure this helper exists to name.
async fn next_server(socket: &mut Client) -> ServerMessage {
    next_server_within(socket, 5_000)
        .await
        .expect("the server answered rather than leaving the client waiting")
}

/// The snapshot that answers a subscribe, with the updates that arrived ahead
/// of it handed back beside it.
///
/// A subscribe IS answered with its subject's snapshot, but not necessarily by
/// the NEXT message. The core raises updates of its own that no test emits, and
/// a connection forwards whatever was already queued for it before the answer,
/// so a test that reads once and asserts is claiming an ordering nothing here
/// gives. It failed on CI for exactly that: the statuspage probe announced a
/// service status into the middle of the test and shifted every read after it.
async fn snapshot_answering(
    socket: &mut Client,
) -> (Subject, serde_json::Value, Vec<SessionUpdate>) {
    let mut passed = Vec::new();
    loop {
        match next_server(socket).await {
            ServerMessage::Update { update, .. } => passed.push(*update),
            // The effective config is server-wide and unprompted, so it can
            // arrive before the answer like any queued update.
            ServerMessage::Settings { .. } => {}
            ServerMessage::Snapshot { subject, data } => return (subject, data, passed),
            other => panic!("a subscribe is answered with a snapshot, not {other:?}"),
        }
    }
}

/// The ceiling on a direct answer, or an echo a test's own action caused: the
/// server answers from what it already holds, so this is a wedge ceiling rather
/// than a producer's budget.
const DIRECT_WAIT: Duration = Duration::from_secs(5);

/// The ceiling on the seat watch's push, derived from the window it runs on.
///
/// The correction rides the watch's next pass, and `forge_workspace::work`'s
/// own test for this property bounds the same wait at `SNAPSHOT_STALENESS - 2
/// s` "so the two cannot drift apart"; this reuses that expression rather than
/// restating the subtraction. Well under nextest's slow period (30 s) and its
/// 120 s kill, so a wedged watch reds by the assertion's own name.
const WATCH_WAIT: Duration =
    forge_workspace::work::SNAPSHOT_STALENESS.saturating_sub(Duration::from_secs(2));

/// The next update satisfying `want`, with everything that is not it passed
/// over, for the reason [`snapshot_answering`] gives.
///
/// The failure is the wait itself, named by its caller: a test that never hears
/// what it emitted fails here rather than reading somebody else's update as its
/// own. The reads are bounded here rather than through [`next_server`], whose
/// own panic does not know which update a step was waiting for.
async fn update_until(
    socket: &mut Client,
    want: &str,
    satisfies: impl FnMut(&SessionUpdate) -> bool,
) -> SessionUpdate {
    update_until_within(socket, DIRECT_WAIT, want, satisfies).await
}

/// [`update_until`] for a producer that answers on its own schedule rather than
/// the request's.
async fn update_until_within(
    socket: &mut Client,
    within: Duration,
    want: &str,
    mut satisfies: impl FnMut(&SessionUpdate) -> bool,
) -> SessionUpdate {
    let ms = u64::try_from(within.as_millis()).unwrap_or(u64::MAX);
    for _ in 0..64 {
        // Split rather than a `let-else`, so a message that arrived and was not
        // an update is not reported as silence.
        let update = match next_server_within(socket, ms).await {
            Some(ServerMessage::Update { update, .. }) => *update,
            Some(other) => {
                panic!("waited for {want}, and a message that is not an update arrived: {other:?}")
            }
            None => panic!("waited for {want}, and never heard it"),
        };
        if satisfies(&update) {
            return update;
        }
    }
    panic!("waited for {want}, and heard 64 other updates without it");
}

/// The socket opens, and the server speaks first.
///
/// The same property `connected` asserts for every test below, kept as its
/// own named test so a failure here reads as "the socket did not open or
/// said nothing" rather than as a bad answer to something.
#[tokio::test]
async fn a_client_can_open_the_socket() {
    let (url, _fleet, _state) = a_server_with_state().await;

    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.expect("the socket opens");
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
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;

    let (subject, data, _) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Home);
    // `Roster`'s field is `projects`, not `orgs` - the gateway's org walk is its
    // own record, and the home's account pool carries none of it. Asserting the
    // wrong key here would fail for a reason that looks like the encoder's fault.
    assert!(data.get("projects").is_some(), "the home snapshot carries its projects: {data}");
}

/// **The board's own subject, and the split it made in the home's hears**
/// (#2517): a cron set moves the board and not the home, while a task write
/// moves both - the home's fleet counts are derived from the same tasks.
#[tokio::test]
async fn a_board_subscriber_hears_what_the_home_no_longer_carries() {
    let (url, fleet) = a_server().await;
    let mut board = connect(&url).await;
    let mut home = connect(&url).await;
    for (socket, subject) in [(&mut board, Subject::Board), (&mut home, Subject::Home)] {
        send(
            socket,
            ClientMessage::Subscribe { what: subject.clone(), answering: true, review: false },
        )
        .await;
        let (answered, ..) = snapshot_answering(socket).await;
        assert_eq!(answered, subject, "the subscription's own answer");
    }

    // A schedule set moves: the board hears it, and the home is not sent it
    // at all - its snapshot carries no crons.
    fleet.emit(SessionUpdate::CronSchedulesChanged { key: lead_seat(), crons: Vec::new() });
    match next_server_within(&mut board, 5_000).await {
        Some(ServerMessage::Update { update, .. }) => assert!(
            matches!(*update, SessionUpdate::CronSchedulesChanged { .. }),
            "the board was sent something else: {update:?}",
        ),
        other => panic!("a schedule set did not reach the board: {other:?}"),
    }
    // The marker orders the home's read: everything above was emitted before
    // it, so a cron update that was going to arrive would have.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });
    match next_server_within(&mut home, 5_000).await {
        Some(ServerMessage::Update { update, .. }) => assert!(
            matches!(*update, SessionUpdate::TurnCancelled { .. }),
            "the home was sent an update its snapshot cannot carry: {update:?}",
        ),
        other => panic!("the home heard nothing where the marker was due: {other:?}"),
    }

    // A task write moves the fleet's counts with the board's rows, so both
    // subjects hear it.
    fleet.emit(SessionUpdate::TasksChanged { key: lead_seat(), tasks: Vec::new() });
    for (socket, name) in [(&mut board, "board"), (&mut home, "home")] {
        match next_server_within(socket, 5_000).await {
            Some(ServerMessage::Update { update, .. }) => assert!(
                matches!(*update, SessionUpdate::TasksChanged { .. }),
                "the {name} was sent something else: {update:?}",
            ),
            other => panic!("a task write did not reach the {name}: {other:?}"),
        }
    }
}

/// **A late subscriber sees the same world as one attached at boot** (#2517):
/// the board's rows and the home's total, on both subjects.
#[tokio::test]
async fn a_late_subscriber_sees_the_same_board_and_home() {
    let (url, fleet) = a_server().await;
    let mut early = connect(&url).await;
    for subject in [Subject::Board, Subject::Home] {
        send(
            &mut early,
            ClientMessage::Subscribe { what: subject, answering: true, review: false },
        )
        .await;
        snapshot_answering(&mut early).await;
    }

    fleet.add_task("TestOrg", "proj", "a live row", "w1", None).expect("the task is seeded");
    fleet.emit(SessionUpdate::TasksChanged { key: lead_seat(), tasks: Vec::new() });

    // The early subscriber's next board read, once the write has landed.
    let mut early_board = serde_json::Value::Null;
    assert!(
        snapshot_until(&mut early, Subject::Board, |data| {
            let rows = data["projects"][0]["rows"].as_array();
            if rows.is_some_and(|rows| !rows.is_empty()) {
                early_board = data.clone();
                true
            } else {
                false
            }
        })
        .await,
        "the early subscriber never read the seeded row",
    );

    // A connection that attaches after the write reads the same row first.
    let mut late = connect(&url).await;
    send(
        &mut late,
        ClientMessage::Subscribe { what: Subject::Board, answering: true, review: false },
    )
    .await;
    let (_, late_board, _) = snapshot_answering(&mut late).await;
    let task_ids = |data: &serde_json::Value| {
        data["projects"][0]["rows"]
            .as_array()
            .map(|rows| rows.iter().map(|row| row["task"]["id"].clone()).collect::<Vec<_>>())
            .unwrap_or_default()
    };
    assert_eq!(
        task_ids(&early_board),
        task_ids(&late_board),
        "a late board subscriber saw a different world",
    );

    // And the home's total on a late connection matches the board's rows.
    send(
        &mut late,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    let (_, late_home, _) = snapshot_answering(&mut late).await;
    assert_eq!(
        late_home["fleet"][0]["rows"].as_u64(),
        Some(task_ids(&late_board).len() as u64),
        "the late home's total does not match the board's rows",
    );
}

/// Showing a seat spends the marks the home carries for it - the diamond
/// and the failure mark both clear by showing the seat - so the home is
/// re-sent when the connection says it has the seat on screen. Without it
/// the row keeps a spent mark until the next unrelated redraw, which can be
/// half a minute away.
#[tokio::test]
async fn showing_a_seat_that_spent_a_mark_re_sends_the_home_the_connection_holds() {
    let (url, fleet) = a_server().await;
    // A mark is drawn on a row, so the seat has to have one: the home lists a
    // project's agents, and a seat nothing has started is not among them.
    fleet.install_agent("TestOrg", "proj", "lead");
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Home, "precondition: the connection holds the home");

    // A completion on the seat while nothing is showing it: the diamond the
    // home's row carries, which this attach is what spends. Polled until the
    // home itself carries it, because the fold is the transport's own task and
    // a snapshot taken straight after an emit can lag it by a hop.
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_finished_turn(),
        origin: None,
        source: None,
    });
    assert!(
        snapshot_until(&mut socket, Subject::Home, |data| {
            data.get("unseen").and_then(|held| held.as_array()).is_some_and(|held| !held.is_empty())
        })
        .await,
        "precondition: the completion armed the seat's diamond",
    );

    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Session(lead_seat()), "the seat's own read is answered first");
    send(&mut socket, ClientMessage::Showing { seat: Some(lead_seat()) }).await;
    // Read as an absence, for the reason the failure-mark case gives: the
    // re-send not coming is what this catches, and it should say so itself.
    let resent = match next_server_within(&mut socket, 700).await {
        Some(ServerMessage::Snapshot { subject, .. }) => Some(subject),
        _ => None,
    };
    assert_eq!(
        resent,
        Some(Subject::Home),
        "and showing the seat re-sends the home, because it spent the seat's marks",
    );
}

/// **The failure half of the same gate.** A seat whose newest turn failed
/// carries a mark until it is shown, and the attach that spends it owes the
/// connection holding the home a fresh one - so the read the gate is decided
/// by has to carry the failure, not just the diamond.
#[tokio::test]
async fn showing_a_seat_that_spent_a_failure_mark_re_sends_the_home() {
    let (url, fleet) = a_server().await;
    // The core's own record, seeded by hand: the fold that sets it runs in the
    // session task, which a fixture does not start.
    let domain = fleet.workspace().register_domain_session(lead_seat(), None);
    domain.lock().failed_turn_at = Some(std::time::SystemTime::now());

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Home, "precondition: the connection holds the home");

    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Session(lead_seat()), "the seat's own read is answered first");
    send(&mut socket, ClientMessage::Showing { seat: Some(lead_seat()) }).await;
    // Read as an absence: the failure this catches is the re-send NOT coming,
    // and the generic wait's own panic would name the wait instead of it.
    let resent = match next_server_within(&mut socket, 700).await {
        Some(ServerMessage::Snapshot { subject, .. }) => Some(subject),
        _ => None,
    };
    assert_eq!(
        resent,
        Some(Subject::Home),
        "showing the seat re-sends the home, because it spent the failure mark",
    );
}

/// **The death half of the same gate.** A crash nobody watched keeps its
/// failure on the row - that is the durable trace - and the attach that
/// shows the seat is what spends it. Until then the row carries `died`, and
/// the home the connection holds is re-sent because showing the seat
/// changed it.
#[tokio::test]
async fn showing_a_seat_that_spent_a_death_re_sends_the_home() {
    let (url, fleet) = a_server().await;
    // The row the recovering connect leaves: the seat is up, and the death
    // it has not been shown rides beside a lifecycle that reads the truth.
    // Seeded by hand: the fold that writes it runs in the session task,
    // which a fixture does not start.
    fleet.workspace().register_domain_session(lead_seat(), None);
    fleet.workspace().record_death_for_test(
        &lead_seat(),
        "dead-occupant",
        "the claude subprocess closed its output stream",
    );

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Home, "precondition: the connection holds the home");

    // The crash is readable on the row before anyone has shown the seat.
    assert!(
        snapshot_until(&mut socket, Subject::Home, |data| {
            data.get("agents")
                .and_then(|held| held.as_array())
                .and_then(|held| held.iter().find(|row| row["slot"]["label"] == "lead"))
                .and_then(|row| row.get("died"))
                .and_then(serde_json::Value::as_bool)
                .is_some_and(|died| died)
        })
        .await,
        "precondition: the death the seat left is on the home's row",
    );

    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Session(lead_seat()), "the seat's own read is answered first");
    send(&mut socket, ClientMessage::Showing { seat: Some(lead_seat()) }).await;
    // Read as an absence, for the reason the failure-mark case gives: the
    // re-send not coming is what this catches, and it should say so itself.
    let resent = match next_server_within(&mut socket, 700).await {
        Some(ServerMessage::Snapshot { subject, .. }) => Some(subject),
        _ => None,
    };
    assert_eq!(
        resent,
        Some(Subject::Home),
        "showing the seat re-sends the home, because it spent the death",
    );

    // The spent trace leaves a row that is still there and clean: the seat
    // is up and its lifecycle stands, with `died` false rather than the row
    // gone. Read as a shape, not an absence - "no row claims died" would
    // hold just as well for a row that vanished.
    assert!(
        snapshot_until(&mut socket, Subject::Home, |data| {
            data.get("agents")
                .and_then(|held| held.as_array())
                .and_then(|held| held.iter().find(|row| row["slot"]["label"] == "lead"))
                .is_some_and(|row| row["died"] == false)
        })
        .await,
        "the row the death was written on did not come back clean",
    );
}

/// **An attach with nothing to spend leaves the home alone.**
///
/// The re-send is owed exactly when showing the seat cleared a diamond or a
/// failure mark. With neither, it is 448 KB and 113-263 ms of server time to
/// hand a connection the home it already holds - measured 2026-10-09, and the
/// ordinary case for a seat whose completions the reader has already seen.
#[tokio::test]
async fn showing_a_seat_with_nothing_to_spend_leaves_the_home_alone() {
    let (url, _fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Home, "precondition: the connection holds the home");

    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Session(lead_seat()), "the seat's own read is answered first");
    send(&mut socket, ClientMessage::Showing { seat: Some(lead_seat()) }).await;
    assert!(
        next_server_within(&mut socket, 700).await.is_none(),
        "the home was re-sent for a showing that spent nothing",
    );
}

/// The queue is a fact about the seat, so the read has to carry it: a client
/// that attached mid-queue - a fresh load, a refresh, a seat switch - has
/// nothing else to draw the waiting prompts from, and two attached clients
/// have to agree about them.
///
/// The updates speak only on a change, which is why this asserts the SNAPSHOT
/// rather than the stream: a queue that crossed only as an update would draw
/// nothing here, and nothing would say so.
#[tokio::test]
async fn a_seats_queue_is_on_the_snapshot_a_client_attaches_to() {
    let (url, fleet) = a_server().await;
    // Seeded as a dispatch leaves the seat, because this asserts the READ:
    // that the row crosses the wire at all. The dispatch-to-record path has
    // its own test beside the session task.
    fleet.seed_queued_prompt(
        &lead_seat(),
        "p-1",
        forge_workspace::protocol::PromptSource::You,
        "hello",
    );

    let mut socket = connect(&url).await;
    let held = snapshot_until(&mut socket, Subject::Session(lead_seat()), |data| {
        data["state"]["queue"].as_array().is_some_and(|rows| {
            rows.len() == 1
                && rows[0]["text"] == "hello"
                && rows[0]["source"] == "you"
                && rows[0]["uuid"] == "p-1"
        })
    })
    .await;

    assert!(
        held,
        "a prompt queued before the client attached is on the seat's read, not only on the stream",
    );
}

/// A prompt aimed at a seat.
fn a_prompt_for(org: &str, project: &str, label: &str) -> Command {
    Command::Prompt {
        key: SessionSlot::for_label(org, project, Some(label)),
        text: "hello".to_owned(),
        attachments: Vec::new(),
    }
}

/// A take belongs to the connection that started it: that connection hears
/// its start and its meter, and every other subscriber to the same seat
/// hears none of them - an in-process take included, whose
/// updates carry no connection at all.
#[tokio::test]
async fn a_take_is_heard_by_its_own_connection_and_no_other() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");

    let mut owner = connect(&url).await;
    send(
        &mut owner,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut owner).await;
    let mut other = connect(&url).await;
    send(
        &mut other,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut other).await;

    send(
        &mut owner,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;

    // The owner hears its own start, then its own meter.
    update_until(&mut owner, "the take's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;
    update_until(&mut owner, "the take's meter", |update| {
        matches!(update, SessionUpdate::DictateLevel { .. })
    })
    .await;

    // Then the two updates the OTHER connection must not hear either: the
    // in-process take on the same seat, whose update carries no
    // connection, and the marker that closes the read below.
    fleet.emit(SessionUpdate::DictateLevel { key: lead_seat(), peak_db: -20.0, initiator: None });
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });

    // The other connection's stream, read one message at a time up to the
    // marker: neither the owner's take nor the in-process one may appear in
    // it. The marker orders the read - everything above was emitted before
    // it, so a take update that was going to arrive would have.
    loop {
        match next_server_within(&mut other, 5_000).await {
            Some(ServerMessage::Update { update, .. }) => {
                assert!(
                    !matches!(
                        *update,
                        SessionUpdate::DictateStarted { .. }
                            | SessionUpdate::DictateLevel { .. }
                            | SessionUpdate::DictateTranscribing { .. }
                            | SessionUpdate::DictateProgress { .. }
                            | SessionUpdate::DictateEnded { .. }
                    ),
                    "another view's take drew on this connection's stream: {update:?}",
                );
                if matches!(*update, SessionUpdate::TurnCancelled { .. }) {
                    break;
                }
            }
            other => panic!("waited for the marker, heard {other:?}"),
        }
    }
}

/// **The surface a client names on its take crosses the socket into its
/// end.** The transport rebuilds the command to stamp the connection, and a
/// field dropped there never reaches the record the words are handed back
/// from - the take itself still runs, so only the ending says so.
#[tokio::test]
async fn a_takes_surface_crosses_the_socket_into_its_end() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");

    let mut owner = connect(&url).await;
    send(
        &mut owner,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut owner).await;

    send(
        &mut owner,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: Some("review".to_owned()),
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut owner, "the take's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;

    send(
        &mut owner,
        ClientMessage::Command {
            command: Box::new(Command::DictateStop {
                key: lead_seat(),
                submit: false,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    let ended = update_until(&mut owner, "the take's end", |update| {
        matches!(update, SessionUpdate::DictateEnded { .. })
    })
    .await;
    let SessionUpdate::DictateEnded { surface, .. } = ended else {
        panic!("the take must end, got {ended:?}")
    };
    assert_eq!(
        surface.as_deref(),
        Some("review"),
        "the ending names the surface the client started the take with"
    );
}

/// One live take per connection, enforced at the socket: the same client
/// asking for a second take on another seat is refused by name, and the
/// first take is untouched - which is what "a frame carries no seat" costs,
/// since two takes on one connection would have no stream their audio could
/// belong to.
#[tokio::test]
async fn a_second_take_on_another_seat_is_refused_for_one_connection() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    fleet.add_worker("TestOrg", "proj", "worker").expect("a worker seat");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");
    let other_seat = SessionSlot::worker("TestOrg", "proj", "worker");

    let mut owner = connect(&url).await;
    for seat in [lead_seat(), other_seat.clone()] {
        send(
            &mut owner,
            ClientMessage::Subscribe {
                what: Subject::Session(seat),
                answering: true,
                review: false,
            },
        )
        .await;
        let _ = snapshot_answering(&mut owner).await;
    }

    send(
        &mut owner,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut owner, "the first take's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;

    send(
        &mut owner,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: other_seat.clone(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    let refused = update_until(&mut owner, "the second take's refusal", |update| {
        matches!(update, SessionUpdate::DictateEnded { .. })
    })
    .await;
    let SessionUpdate::DictateEnded {
        outcome: forge_workspace::DictateOutcome::Refused { message },
        ..
    } = refused
    else {
        panic!("the second take must be refused, got {refused:?}")
    };
    assert!(
        message.contains("another seat"),
        "the refusal names where the live take is, got: {message}"
    );

    // The first take is untouched - and this is the seat memory's own
    // case, because the refused start ran through the same dispatch: its
    // frames still feed the first take's meter.
    let mut frame = vec![forge_server::transport::frame::Kind::Dictation.tag()];
    for _ in 0..320 {
        frame.extend_from_slice(&16384i16.to_le_bytes());
    }
    owner.send(Message::Binary(frame.into())).await.expect("the frame goes");
    update_until(&mut owner, "the first take's meter", |update| {
        matches!(update, SessionUpdate::DictateLevel { peak_db, .. } if (*peak_db + 6.02).abs() < 0.5)
    })
    .await;

    // And the teardown closes the take: the seat is startable by the next
    // client, which is what a take left behind would have refused.
    let attached = fleet.subscriber_count();
    drop(owner);
    assert!(
        wait_for_the_server_to_notice(&fleet, attached).await,
        "the server must notice the streaming connection is gone",
    );
    let mut next = connect(&url).await;
    send(
        &mut next,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut next).await;
    send(
        &mut next,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut next, "the freed seat's next start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;
}

/// Decision 3's scenario at the frame level: a second client pressing
/// dictate on a seat it does not hold has its start refused - and the
/// microphone frames already on the wire from that attempt (the client
/// streams what its start held) must NOT land in the holder's take.
#[tokio::test]
async fn a_refused_clients_frames_never_land_in_the_holders_take() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");

    let mut holder = connect(&url).await;
    send(
        &mut holder,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut holder).await;
    send(
        &mut holder,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut holder, "the holder's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;

    // The second client: its start refuses, and the frame its already-open
    // microphone sends next rides the same socket the refusal does.
    let mut second = connect(&url).await;
    send(
        &mut second,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut second).await;
    send(
        &mut second,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    let mut probe = vec![forge_server::transport::frame::Kind::Dictation.tag()];
    for _ in 0..320 {
        probe.extend_from_slice(&16384i16.to_le_bytes());
    }
    second.send(Message::Binary(probe.into())).await.expect("the probe frame goes");
    let refused = update_until(&mut second, "the refusal", |update| {
        matches!(update, SessionUpdate::DictateEnded { .. })
    })
    .await;
    let SessionUpdate::DictateEnded {
        outcome: forge_workspace::DictateOutcome::Refused { message },
        ..
    } = refused
    else {
        panic!("the second start must be refused, got {refused:?}")
    };
    assert!(
        message.contains("already dictating"),
        "the refusal says the seat is held, got: {message}"
    );

    // The holder's meter: every window is silence until its OWN frame - a
    // quarter of full scale, about -12 dB - arrives. The probe was half
    // scale (-6 dB), so any window between tells whose microphone that was.
    let mut own = vec![forge_server::transport::frame::Kind::Dictation.tag()];
    for _ in 0..320 {
        own.extend_from_slice(&8192i16.to_le_bytes());
    }
    holder.send(Message::Binary(own.into())).await.expect("the holder's frame goes");
    let mut heard_its_own = false;
    let mut peaks: Vec<f32> = Vec::new();
    for _ in 0..48 {
        match next_server_within(&mut holder, 5_000).await {
            Some(ServerMessage::Update { update, .. }) => {
                if let SessionUpdate::DictateLevel { peak_db, .. } = *update {
                    peaks.push(peak_db);
                    if (peak_db + 12.04).abs() < 0.5 {
                        heard_its_own = true;
                        break;
                    }
                }
            }
            other => panic!("waited for the holder's own frame, heard {other:?}"),
        }
    }
    assert!(heard_its_own, "the holder's own frame must reach its meter");
    // Read after the run rather than per window: the probe's window and the
    // holder's own can share one, and a reading checked one at a time would
    // depend on the scheduler for which frame it witnesses.
    assert!(
        peaks.iter().all(|peak| !peak.is_finite() || (*peak + 12.04).abs() < 0.5),
        "a refused client's microphone landed in the holder's take: {peaks:?}"
    );
}

/// A client-captured take over the socket: the start names the seat, the
/// binary frames that follow on that same ordered connection feed the take
/// it registered, and the meter that reads them comes back on that same
/// connection alone - which is the whole reason a frame carries no seat of
/// its own.
#[tokio::test]
async fn a_clients_frames_feed_the_take_it_started() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut socket).await;

    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut socket, "the take's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;

    // Half-scale audio, at the wire's own shape.
    let mut frame = vec![forge_server::transport::frame::Kind::Dictation.tag()];
    for _ in 0..320 {
        frame.extend_from_slice(&16384i16.to_le_bytes());
    }
    socket.send(Message::Binary(frame.into())).await.expect("the frame goes");

    // The meter reports the pushed frame's own peak - 0.5 of full scale,
    // about -6 dB - and only a routed frame produces one: a connection
    // that had forgotten the take it started drops the frame silently,
    // and every window reads the silence floor instead.
    update_until(&mut socket, "the pushed frame's own peak", |update| {
        matches!(update, SessionUpdate::DictateLevel { peak_db, .. } if (*peak_db + 6.02).abs() < 0.5)
    })
    .await;
}

/// **A frame reaches the read-aloud recording, the destination with no seat.**
/// The recording and the takes are two places a frame can land rather than
/// one, and deleting the recording's half is silent: the seats path still
/// answers, so nothing in this crate notices that a recording stopped
/// hearing the microphone. The stop that keeps the set is what says so - a
/// recording with no frame in it has nothing to write, and the set stays
/// empty.
#[tokio::test]
async fn a_frame_lands_in_the_read_aloud_recording() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");
    let mut socket = connect(&url).await;
    for what in [Subject::Session(lead_seat()), Subject::DictateModels] {
        send(&mut socket, ClientMessage::Subscribe { what, answering: true, review: false }).await;
        let _ = snapshot_answering(&mut socket).await;
    }

    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::DictateReadAloudStart { initiator: None }),
            reply_to: None,
        },
    )
    .await;

    // Half-scale audio, at the wire's own shape.
    let mut frame = vec![forge_server::transport::frame::Kind::Dictation.tag()];
    for _ in 0..320 {
        frame.extend_from_slice(&16384i16.to_le_bytes());
    }
    socket.send(Message::Binary(frame.into())).await.expect("the frame goes");

    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::DictateReadAloudStop { keep: true, initiator: None }),
            reply_to: None,
        },
    )
    .await;

    // The keep writes the set off the runtime thread and re-reads the models
    // behind it; a recording in that read is the frame having arrived.
    update_until(&mut socket, "the read-aloud set", |update| {
        matches!(
            update,
            SessionUpdate::DictateModelsChanged { models }
                if !models.read_aloud.recordings.is_empty()
        )
    })
    .await;
}

/// A client that drops mid-take leaves nothing behind: the take is DROPPED
/// rather than submitted - its reader is gone, so nothing it streamed lands
/// anywhere - and the seat is free, so the same client reconnecting and
/// starting again lands rather than being refused by the take it left.
///
/// This is the shape the disconnect path exists for, and it was the shape
/// that broke: with no close handler the orphan held the seat and refused
/// the next start as busy.
#[tokio::test]
async fn a_dropped_connection_frees_the_seat_for_the_next_take() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");

    let mut first = connect(&url).await;
    send(
        &mut first,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut first).await;
    send(
        &mut first,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut first, "the take's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;

    let mut frame = vec![forge_server::transport::frame::Kind::Dictation.tag()];
    for _ in 0..320 {
        frame.extend_from_slice(&16384i16.to_le_bytes());
    }
    first.send(Message::Binary(frame.into())).await.expect("the frame goes");

    // The client vanishes mid-take, and the server notices by its own read
    // failing - a scheduling hop away, which the subscribed-count falling
    // is what waits for.
    let attached = fleet.subscriber_count();
    drop(first);
    assert!(
        wait_for_the_server_to_notice(&fleet, attached).await,
        "the server must notice the streaming connection is gone",
    );

    // And the next start lands at once, which is what the refusal broke.
    let mut next = connect(&url).await;
    send(
        &mut next,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut next).await;
    send(
        &mut next,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut next, "the next take's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;
}

/// A refused re-start must not orphan the take the connection already owns.
///
/// A refusal answers a start that never ran, so the take it names is not the
/// one live on the seat - and the connection that drops after one must still
/// free its seat. Read as an end and used to forget the seat, the refusal
/// leaves the live take registered with nobody left to close it: every later
/// client is refused until the capture cap, which is the hold Ved met after a
/// seat switch (#1880).
#[tokio::test]
async fn a_refused_restart_leaves_the_live_take_to_the_disconnect() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");

    let mut first = connect(&url).await;
    send(
        &mut first,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut first).await;
    send(
        &mut first,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut first, "the take's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;

    // The reader presses again while their take is live - the press a seat
    // switch leaves them making - and the same-seat rule refuses it: the
    // connection already dictates on this session.
    send(
        &mut first,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut first, "the refusal", |update| {
        matches!(
            update,
            SessionUpdate::DictateEnded {
                outcome: forge_workspace::DictateOutcome::Refused { .. },
                ..
            }
        )
    })
    .await;

    // The client vanishes mid-take, and the server notices by its own read
    // failing - a scheduling hop away, which the subscribed-count falling
    // is what waits for.
    let attached = fleet.subscriber_count();
    drop(first);
    assert!(
        wait_for_the_server_to_notice(&fleet, attached).await,
        "the server must notice the streaming connection is gone",
    );

    // And the next start lands at once: the take went with the connection
    // that owned it, the refusal notwithstanding.
    let mut next = connect(&url).await;
    send(
        &mut next,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut next).await;
    send(
        &mut next,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    next_start_or_refusal(&mut next, "the next take's start").await;
}

/// A take's own news reaches the connection that started it whether or not
/// the seat is showing.
///
/// The reader is on another seat while their recording runs, and the fold
/// that draws the take - and the end it must hear - is that connection's.
/// Filtered by the watch, the record goes on drawing a recording that is
/// over, and a press for it is answered by nothing at all (#1880).
#[tokio::test]
async fn a_take_ending_reaches_its_connection_with_the_seat_unsubscribed() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");

    let mut client = connect(&url).await;
    send(
        &mut client,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut client).await;
    send(
        &mut client,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut client, "the take's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;

    // The reader moves to another seat: this seat's subscription goes back.
    send(&mut client, ClientMessage::Unsubscribe { what: Subject::Session(lead_seat()) }).await;
    // A turnstile, because an unsubscribe is answered by silence: a subscribe
    // answers at once and in order, so hearing it proves the seat was let go
    // before anything below was emitted.
    send(
        &mut client,
        ClientMessage::Subscribe { what: Subject::Home, answering: false, review: false },
    )
    .await;
    let (_, _, _) = snapshot_answering(&mut client).await;

    // The whole family crosses the switch, not only the end: one frame in,
    // and a level for the unwatched seat comes back METERED - the reading a
    // reader returning to a live recording has to find, and what a narrowed
    // exemption would drop.
    let mut frame = vec![forge_server::transport::frame::Kind::Dictation.tag()];
    for _ in 0..320 {
        frame.extend_from_slice(&16384i16.to_le_bytes());
    }
    client.send(Message::Binary(frame.into())).await.expect("the frame goes");
    update_until(&mut client, "a level for the unwatched seat", |update| {
        matches!(
            update,
            SessionUpdate::DictateLevel { peak_db, .. } if peak_db.is_finite()
        )
    })
    .await;

    // A control on the way past, because the exemption must be NARROW: news
    // about the seat that is not the take's is still the watch's to filter.
    fleet.emit(SessionUpdate::ForgeAccountIdentity {
        key: lead_seat(),
        display_name: "the-lead".to_owned(),
    });

    // The take ends while nothing here shows the seat - the cap, or a stop
    // from whichever view can reach the seat.
    send(
        &mut client,
        ClientMessage::Command {
            command: Box::new(Command::DictateStop {
                key: lead_seat(),
                submit: false,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;

    // The end comes next: the take's news is its own, and the seat's other
    // news is not.
    loop {
        match next_server_within(&mut client, 5_000).await {
            Some(ServerMessage::Update { update, .. }) => match *update {
                SessionUpdate::DictateEnded { .. } => break,
                SessionUpdate::ForgeAccountIdentity { .. } => panic!(
                    "a non-take update for an unwatched seat arrived: the exemption is too wide"
                ),
                _ => {}
            },
            Some(other) => panic!("waited for the take's end, and got {other:?}"),
            None => panic!("waited for the take's end, and never heard it"),
        }
    }
}

/// Take after take on one seat: the second take streams, and the connection
/// going away closes the one it owns.
///
/// The stale-interleaving half of #1886 - an older take's end arriving after
/// a newer take claimed the seat - cannot be FORCED through a socket with
/// the stub engine (an empty take's transcription resolves inside a round
/// trip), so the generation rule itself is pinned by the unit tests over
/// `Streaming::retire` in `transport::connection`. What this keeps is the
/// real-socket scenario around it and its consequences: a second take
/// registers under the seat, its frames meter, and the teardown frees the
/// seat for the next client.
#[tokio::test]
async fn take_after_take_on_one_seat_leaves_it_free_for_the_next_client() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    let _models = fleet.arm_dictation(&lead_seat()).expect("dictation arms without weights");

    let mut client = connect(&url).await;
    send(
        &mut client,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut client).await;

    // Take 1, stopped and submitted: it finishes while the claim stands.
    send(
        &mut client,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut client, "take 1's start", |update| {
        matches!(update, SessionUpdate::DictateStarted { .. })
    })
    .await;
    send(
        &mut client,
        ClientMessage::Command {
            command: Box::new(Command::DictateStop {
                key: lead_seat(),
                submit: true,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    update_until(&mut client, "take 1's transcribing", |update| {
        matches!(update, SessionUpdate::DictateTranscribing { .. })
    })
    .await;

    // Take 2 registers on the same seat, same connection, and both its start
    // and take 1's end are read raw: the empty take's end is a `no_audio`
    // outcome whose no-signal `peak_db` crosses as `null`, which the typed
    // reader refuses.
    send(
        &mut client,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    let mut started = false;
    let mut ended = false;
    while !(started && ended) {
        let text = next_text_within(&mut client, 5_000)
            .await
            .unwrap_or_else(|| panic!("take 2 started={started}, take 1 ended={ended}"));
        started |= text.contains("\"dictate_started\"");
        ended |= text.contains("\"dictate_ended\"");
    }

    // The seat serves take 2: one frame in, and a METERED level comes back.
    let mut frame = vec![forge_server::transport::frame::Kind::Dictation.tag()];
    for _ in 0..320 {
        frame.extend_from_slice(&16384i16.to_le_bytes());
    }
    client.send(Message::Binary(frame.into())).await.expect("the frame goes");
    update_until(&mut client, "a metered level for take 2", |update| {
        matches!(
            update,
            SessionUpdate::DictateLevel { peak_db, .. } if peak_db.is_finite()
        )
    })
    .await;

    // And the disconnect closes take 2 with it: the next client starts.
    let attached = fleet.subscriber_count();
    drop(client);
    assert!(
        wait_for_the_server_to_notice(&fleet, attached).await,
        "the server must notice the connection is gone",
    );
    let mut next = connect(&url).await;
    send(
        &mut next,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut next).await;
    send(
        &mut next,
        ClientMessage::Command {
            command: Box::new(Command::DictateStream {
                key: lead_seat(),
                options: forge_workspace::DictateAxes::default(),
                surface: None,
                initiator: None,
            }),
            reply_to: None,
        },
    )
    .await;
    // The next client's take must register. A refusal here IS the bug (the
    // seat still held by a take nothing closed), so the failure carries the
    // refusal's own words rather than a bare wait.
    next_start_or_refusal(&mut next, "the next take's start").await;
}

/// The next take's start, or a panic naming the refusal that held the seat.
///
/// The failure this exists to catch says WHICH take is stuck - the refusal's
/// own sentence names it - rather than that a wait expired.
async fn next_start_or_refusal(socket: &mut Client, want: &str) -> SessionUpdate {
    for _ in 0..64 {
        let update = match next_server_within(socket, 5_000).await {
            Some(ServerMessage::Update { update, .. }) => *update,
            Some(_) => continue,
            None => panic!("waited for {want}, and heard nothing"),
        };
        match &update {
            SessionUpdate::DictateStarted { .. } => return update,
            SessionUpdate::DictateEnded {
                outcome: forge_workspace::DictateOutcome::Refused { message },
                ..
            } => panic!("waited for {want}, and the seat was refused: {message}"),
            _ => {}
        }
    }
    panic!("waited for {want}, and heard 64 other updates without it");
}

/// The axes `forge.toml` set reach a client in the greeting. They are what a
/// capturing client starts on and resets to, so a default standing in for the
/// config's value would be invisible everywhere else - and the fixture's
/// config sets a value that is not the crate's default.
#[tokio::test]
async fn the_greeting_carries_the_dictate_axes_the_config_set() {
    let (url, fleet) = a_server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.expect("the socket opens");
    let msg = socket.next().await.expect("a greeting").expect("no error");
    let text = msg.to_text().expect("text").to_owned();
    let ServerMessage::Greeting { settings, .. } =
        serde_json::from_str(&text).expect("the greeting decodes")
    else {
        panic!("the first thing the server says is its greeting, not {text}")
    };

    assert_eq!(
        settings.dictate.styling,
        forge_dictate::normalize::Styling::Formal,
        "the fixture's config sets styling = formal, and the greeting must carry it"
    );
    assert_eq!(
        settings.dictate,
        fleet.surface().dictate_axes(),
        "the greeting's axes are the workspace's own, not a default standing in for them"
    );
}

/// A `[client]` edit reaches an open connection as a new settings frame, and
/// the next connection's greeting carries the same values.
#[tokio::test]
async fn a_client_setting_reload_reaches_an_open_connection_and_the_next() {
    let (url, fleet) = a_server().await;
    let mut first = connect(&url).await;

    let path = fleet.workspace().config_path();
    let mut raw = std::fs::read_to_string(&path).expect("read");
    raw.push_str("\n[client]\nmark = \"klin\"\n");
    std::fs::write(&path, raw).expect("write");
    let reload = fleet.workspace().reload_config().expect("the new file is valid");
    assert!(
        reload.changed.iter().any(|key| key.starts_with("client.")),
        "the client table moved: {reload:?}",
    );

    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), first.next())
        .await
        .expect("the open connection is pushed a settings frame")
        .expect("a frame")
        .expect("no error");
    let text = msg.to_text().expect("text");
    let ServerMessage::Settings { document } =
        serde_json::from_str(text).expect("the settings frame decodes")
    else {
        panic!("an open connection hears the effective config, not {text}")
    };
    let client = document
        .areas
        .iter()
        .find(|area| area.id == "client")
        .expect("the document carries the client area");
    let mark = client
        .keys
        .iter()
        .find(|key| key.path == "client.mark")
        .expect("the client area carries the mark");
    assert_eq!(
        mark.value,
        serde_json::json!("klin"),
        "the open connection was re-themed without a reconnect",
    );

    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.expect("the socket opens");
    let msg = socket.next().await.expect("a greeting").expect("no error");
    let text = msg.to_text().expect("text");
    let ServerMessage::Greeting { settings, .. } =
        serde_json::from_str(text).expect("the greeting decodes")
    else {
        panic!("the next connection's first frame is its greeting, not {text}")
    };
    assert_eq!(
        settings.mark.as_deref(),
        Some("klin"),
        "a connection opened after the reload greets with its sets",
    );
}

/// The document a connection is handed after its greeting is the effective
/// config: the same source path the workspace loaded, and every key with the
/// class the page chips it with.
#[tokio::test]
async fn the_settings_frame_follows_the_greeting_and_carries_the_effective_config() {
    let (url, fleet) = a_server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.expect("the socket opens");
    let _greeting = socket.next().await.expect("a greeting").expect("no error");
    let msg = socket.next().await.expect("a settings frame").expect("no error");
    let text = msg.to_text().expect("text");
    let ServerMessage::Settings { document } =
        serde_json::from_str(text).expect("the settings frame decodes")
    else {
        panic!("the second thing the server says is the effective config, not {text}")
    };

    assert_eq!(
        document.source,
        fleet.workspace().config_path().display().to_string(),
        "the document names the file the workspace loaded",
    );
    assert_eq!(document.version, "test-build", "and the build that sent it");
    let server = document
        .areas
        .iter()
        .find(|area| area.id == "server")
        .expect("the document carries the server area");
    let port = server.keys.iter().find(|key| key.path == "server.port").expect("and the port key");
    assert_eq!(
        port.class,
        forge_primitives::EffectClass::Restart,
        "the port is drawn as a restart, the class the reload reports it under",
    );
}

/// **The socket turns away a browser page forge did not serve**, at the
/// upgrade and before the greeting or a settings frame exists for it.
#[tokio::test]
async fn a_foreign_origin_is_refused_at_the_upgrade() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let (url, _fleet) = a_server().await;
    let mut request = url.into_client_request().expect("a client request");
    request.headers_mut().insert("Origin", "https://evil.example".parse().expect("a header value"));
    let error =
        tokio_tungstenite::connect_async(request).await.expect_err("the upgrade is refused");
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 403, "a foreign origin is forbidden");
        }
        other => panic!("expected an HTTP refusal, got {other:?}"),
    }
}

/// **Two Origin headers are not one origin.** A client forge ships sends at
/// most one, so a request carrying two is refused rather than resolved to
/// whichever the gate happened to read first.
#[tokio::test]
async fn a_duplicate_origin_is_refused_at_the_upgrade() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let (url, _fleet) = a_server().await;
    let mut request = url.into_client_request().expect("a client request");
    request.headers_mut().append("Origin", "http://localhost:1420".parse().expect("a value"));
    request.headers_mut().append("Origin", "http://localhost:1420".parse().expect("a value"));
    let error =
        tokio_tungstenite::connect_async(request).await.expect_err("the upgrade is refused");
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 403, "a duplicate origin is forbidden");
        }
        other => panic!("expected an HTTP refusal, got {other:?}"),
    }
}

/// **An Origin that is not text is not an origin.** The gate reads the
/// header as text; bytes it cannot read are refused rather than ignored as
/// if the request were a native client's.
#[tokio::test]
async fn a_non_text_origin_is_refused_at_the_upgrade() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let (url, _fleet) = a_server().await;
    let mut request = url.into_client_request().expect("a client request");
    request
        .headers_mut()
        .insert("Origin", HeaderValue::from_bytes(b"\xff\xfe").expect("an opaque value"));
    let error =
        tokio_tungstenite::connect_async(request).await.expect_err("the upgrade is refused");
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), 403, "a non-text origin is forbidden");
        }
        other => panic!("expected an HTTP refusal, got {other:?}"),
    }
}

/// The shell's webview origin is allowed, and the settings frame follows the
/// greeting on that connection as on any other.
#[tokio::test]
async fn a_shell_origin_connects_and_reads_the_settings_frame() {
    let (url, _fleet) = a_server().await;
    let _socket = connect_with_origin(&url, "tauri://localhost").await;
}

/// An origin the operator listed in `[server] allowed_origins` is the web
/// client's own, and it connects.
#[tokio::test]
async fn a_configured_web_origin_connects() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir_with_server(
        dir.path(),
        &[("TestOrg", &["proj"])],
        "[server]\nallowed_origins = [\"http://localhost:8080\"]\n",
    )
    .expect("the fleet builds");
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let _socket = connect_with_origin(&url, "http://localhost:8080").await;
}

/// An unchanged save publishes nothing: the workspace compares the document
/// it would build with the one the socket holds.
#[tokio::test]
async fn an_unchanged_save_pushes_no_settings_frame() {
    let (url, fleet) = a_server().await;
    let mut first = connect(&url).await;

    let path = fleet.workspace().config_path();
    let raw = std::fs::read_to_string(&path).expect("read");
    std::fs::write(&path, raw).expect("write the same contents");
    let reload = fleet.workspace().reload_config().expect("the same file is valid");
    assert!(
        reload.changed.is_empty() && reload.restart_only.is_empty(),
        "an unchanged save moves nothing: {reload:?}",
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), first.next()).await.is_err(),
        "and no frame follows it",
    );
}

/// **The settings frame carries the file's own values, and none of them reach
/// the log.** The frame is the page's read, not a diagnostic: a token in it
/// must never be written to the tracing log by the push that carries it.
#[tokio::test]
async fn the_settings_frame_never_writes_its_values_to_the_log() {
    const SECRET: &str = "settings-secret-9f3a";
    let capture = install_capture();
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir_with_server_and_env(
        dir.path(),
        &[("TestOrg", &["proj"])],
        "",
        &[("SETTINGS_SECRET", SECRET)],
    )
    .expect("the fleet builds");
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    let path = fleet.workspace().config_path();
    let mut raw = std::fs::read_to_string(&path).expect("read");
    raw.push_str("\n[usage]\npoll_seconds = 120\n");
    std::fs::write(&path, raw).expect("write");
    fleet.workspace().reload_config().expect("the new file is valid");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .expect("the open connection is pushed the new document")
        .expect("a frame")
        .expect("no error");
    let text = msg.to_text().expect("text");
    assert!(text.contains(SECRET), "precondition: the frame carries the project's env value");
    // **The capture reads the shipped filter, not a narrower one.** A probe
    // on an always-on target proves the test would see a record written
    // beside the frame's push; without it, a filter that dropped that
    // target would make the assertion below prove nothing.
    tracing::debug!(
        target: "app.lifecycle",
        event_name = "capture_probe",
        "the capture reads the shipped log directives",
    );
    assert!(
        captured().contains("the capture reads the shipped log directives"),
        "the capture must see the targets the shipped log enables",
    );
    let _ = capture;
    assert!(!captured().contains(SECRET), "the frame's values never reach the tracing log");
}

/// **A large text frame crosses whole.** The inbound cap is the socket's
/// guard against abuse, not a ceiling on a prompt: a command carrying a
/// ~100 KiB prompt used to exceed a 16 KiB message cap and tear the
/// connection, and a prompt has no reply, so the loss was silent. The
/// subscription behind it is the proof the socket lived.
#[tokio::test]
async fn a_large_text_command_does_not_tear_the_connection() {
    let (url, _fleet) = a_server().await;
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::Prompt {
                key: lead_seat(),
                text: "x".repeat(100 * 1024),
                attachments: Vec::new(),
            }),
            reply_to: None,
        },
    )
    .await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: false, review: false },
    )
    .await;

    let mut answered = false;
    for _ in 0..20 {
        match next_server_within(&mut socket, 5_000).await {
            Some(ServerMessage::Snapshot { subject: Subject::Home, .. }) => {
                answered = true;
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    assert!(answered, "the socket survived the large command and answered the subscribe");
}

/// The greeting names the build that sent it.
///
/// The greeting is the only channel both halves are guaranteed to have - a
/// refused client never gets a snapshot - so the release identity has to
/// cross here or a protocol skew can name nothing but a number at either
/// end.
#[tokio::test]
async fn the_greeting_carries_the_builds_own_identity() {
    let (url, _fleet) = a_server().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.expect("the socket opens");
    let msg = socket.next().await.expect("a greeting").expect("no error");
    let text = msg.to_text().expect("text").to_owned();
    let ServerMessage::Greeting { forge_version, forge_version_short, .. } =
        serde_json::from_str(&text).expect("the greeting decodes")
    else {
        panic!("the first thing the server says is its greeting, not {text}")
    };

    assert_eq!(
        forge_version,
        forge_server::FORGE_VERSION,
        "the greeting must name the build with the same stamp the header draws"
    );
    assert_eq!(
        forge_version_short,
        forge_server::FORGE_VERSION_SHORT,
        "and the short stamp beside it, which is the one a tight line can carry"
    );
}

/// One command the core handed a seat's stub, or `None` if it said nothing
/// inside `ms`.
///
/// The ask rides the socket's own task, so this reads a channel rather than a
/// socket and is bounded by a wait: a test that read once would claim an
/// ordering the scheduling does not give.
async fn next_agent_command(
    commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
    ms: u64,
) -> Option<AgentCommand> {
    tokio::time::timeout(std::time::Duration::from_millis(ms), commands.recv()).await.ok().flatten()
}

/// The frame the CLI sends when a turn ends, which is what the socket reads a
/// finished turn off.
fn a_finished_turn() -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1,
        "duration_api_ms": 1,
        "is_error": false,
        "num_turns": 1,
        "session_id": "s",
    }))
    .expect("parse a result message")
}

/// A frame from the middle of a turn, which is everything a turn sends before
/// the result that ends it.
fn an_assistant_chunk() -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "assistant",
        "message": {
            "id": "mid-turn",
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [{ "type": "text", "text": "a chunk" }],
        },
        "session_id": "s",
    }))
    .expect("parse an assistant message")
}

/// The frame the CLI sends once a compaction settles, which is the only status
/// frame carrying the compaction's own result.
fn a_compaction_settle() -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "system",
        "subtype": "status",
        "status": null,
        "compact_result": "success",
        "session_id": "s",
    }))
    .expect("parse a status message")
}

/// The CLI's report of a permission-mode change, which arrives on the same
/// `status`/null shape the settle does and is why a bare null is not a settle.
fn a_permission_mode_change() -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "system",
        "subtype": "status",
        "status": null,
        "permissionMode": "plan",
        "session_id": "s",
    }))
    .expect("parse a status message")
}

/// The lead seat with an agent behind it, reporting `reading`, and the receiver
/// that stub records what it is asked.
///
/// The reading is the state the tests below have in common: a number the seat
/// already holds, which a turn makes stale.
fn a_reporting_seat(fleet: &Fleet, reading: ContextUsage) -> mpsc::UnboundedReceiver<AgentCommand> {
    let asked = fleet.install_agent("TestOrg", "proj", "lead");
    fleet.seed_view_facts(
        &lead_seat(),
        ViewFacts {
            session_id: Some(SessionId::new("reporting")),
            context: Some(reading),
            // A snapshot of none, so these cases read one ask apiece: the
            // unreported-MCP ask is a test's own subject where it appears,
            // and here it would ride every read as an extra command.
            mcp: Some(forge_workspace::McpServers { servers: Vec::new(), error: None }),
            ..ViewFacts::default()
        },
    );
    asked
}

/// A reading small enough to pay a probe for.
fn a_small_reading() -> ContextUsage {
    ContextUsage { percent: Some(12), max_tokens: Some(200_000) }
}

/// A `[1m]`-class window past the socket's token gate, which is where a
/// reading can only be lowered by a compaction.
fn a_reading_past_the_gate() -> ContextUsage {
    ContextUsage { percent: Some(80), max_tokens: Some(1_000_000) }
}

/// A connected client showing `what`, with the snapshot it was answered read
/// and handed back beside it.
async fn a_page_on(url: &str, what: Subject) -> (Client, serde_json::Value) {
    let mut socket = connect(url).await;
    // **A page on a seat SHOWS it** (Ved, 2026-10-25). The subscribe carries
    // the seat's frames; the showing is what spends its marks and what makes
    // a turn finishing on it a turn the reader watched. Sent BEFORE the
    // subscribe so the snapshot answering the subscribe proves both crossed -
    // a showing has no answer of its own.
    if let Subject::Session(slot) = &what {
        send(&mut socket, ClientMessage::Showing { seat: Some(slot.clone()) }).await;
    }
    send(&mut socket, ClientMessage::Subscribe { what, answering: true, review: false }).await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    (socket, data)
}

/// A seat a client opens with no usage to report asks the core for one, so the
/// header draws a bar rather than the dash an unasked seat carries for its
/// whole life.
///
/// A view asks for the seat it is addressing; a client subscribing to a
/// seat is that same act, and the socket takes the ask on the read that encodes
/// the subject.
#[tokio::test]
async fn a_seat_a_client_opens_with_no_usage_asks_the_core_for_one() {
    let (url, fleet) = a_server().await;
    let mut asked = fleet.install_agent("TestOrg", "proj", "lead");
    fleet.seed_view_facts(
        &lead_seat(),
        ViewFacts { session_id: Some(SessionId::new("no-usage-yet")), ..ViewFacts::default() },
    );
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    assert!(
        data["header"]["context"]["percent"].is_null(),
        "precondition: the seat reports no usage, which is the state that draws a dash: {data}",
    );

    let command = next_agent_command(&mut asked, 5_000).await;
    assert!(
        matches!(command, Some(AgentCommand::GetContextUsage { .. })),
        "a seat a client opened reports no usage, so the core is asked for one: {command:?}",
    );
}

/// The other half of the same rule: a seat that already reports a reading is
/// not asked again, so a page open, a reconnect or a second tab costs no probe
/// over the CLI's whole transcript.
#[tokio::test]
async fn a_seat_that_already_reports_usage_is_not_asked_again() {
    let (url, fleet) = a_server().await;
    let mut asked = fleet.install_agent("TestOrg", "proj", "lead");
    fleet.seed_view_facts(
        &lead_seat(),
        ViewFacts {
            session_id: Some(SessionId::new("already-reporting")),
            context: Some(ContextUsage { percent: Some(41), max_tokens: Some(200_000) }),
            // Reported too, so the ONLY silence this window can hear is the
            // reading's own: the MCP twin is the next test's subject.
            mcp: Some(forge_workspace::McpServers { servers: Vec::new(), error: None }),
            ..ViewFacts::default()
        },
    );
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    assert_eq!(
        data["header"]["context"]["percent"], 41,
        "precondition: the seat reports a reading, so there is nothing to ask for: {data}",
    );

    // The window is the assertion: nothing being sent can only be observed by
    // waiting, and a probe wrongly fired lands a scheduling hop after the ask.
    let command = next_agent_command(&mut asked, 250).await;
    assert!(command.is_none(), "a seat that reports a reading is not probed again: {command:?}");
}

/// The MCP snapshot is the context reading's twin (#1844): it exists only once
/// the bridge has answered one, a view's connect paths are what asked
/// before, and a seat only a client watches would otherwise report `mcp: None`
/// for the life of its session. The usage reading is seeded, so the ONLY ask
/// this read can fire is the MCP one.
#[tokio::test]
async fn a_seat_a_client_opens_with_no_mcp_snapshot_asks_the_core_for_one() {
    let (url, fleet) = a_server().await;
    let mut asked = fleet.install_agent("TestOrg", "proj", "lead");
    fleet.seed_view_facts(
        &lead_seat(),
        ViewFacts {
            session_id: Some(SessionId::new("no-mcp-yet")),
            context: Some(ContextUsage { percent: Some(41), max_tokens: Some(200_000) }),
            ..ViewFacts::default()
        },
    );
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    assert!(
        data["mcp"].is_null(),
        "precondition: the seat reports no MCP snapshot, which is the state that draws no row: {data}",
    );

    let command = next_agent_command(&mut asked, 5_000).await;
    assert!(
        matches!(command, Some(AgentCommand::GetMcpSnapshot { .. })),
        "a seat a client opened reports no MCP snapshot, so the bridge is asked for one: {command:?}",
    );
}

/// The other half, as for the reading: a seat that already reports a snapshot
/// is not asked again - an empty set included, which is a snapshot carrying
/// none rather than no snapshot.
#[tokio::test]
async fn a_seat_that_already_reports_an_mcp_snapshot_is_not_asked_again() {
    let (url, fleet) = a_server().await;
    let mut asked = fleet.install_agent("TestOrg", "proj", "lead");
    fleet.seed_view_facts(
        &lead_seat(),
        ViewFacts {
            session_id: Some(SessionId::new("already-reported")),
            context: Some(ContextUsage { percent: Some(41), max_tokens: Some(200_000) }),
            mcp: Some(forge_workspace::McpServers { servers: Vec::new(), error: None }),
            ..ViewFacts::default()
        },
    );
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    assert!(
        data["mcp"]["servers"].is_array(),
        "precondition: the seat reports a snapshot, so there is nothing to ask for: {data}",
    );

    let command = next_agent_command(&mut asked, 250).await;
    assert!(command.is_none(), "a seat that reports a snapshot is not asked again: {command:?}");
}

/// A turn finishing on a seat a page shows is the moment its reading
/// stops being true: the transcript grew by the turn, and the number the seat
/// holds was taken before it.
///
/// The other half of the pair above: that one keeps an open page from costing a
/// probe per read, and this one keeps a reading from standing for the life of
/// its occupant.
#[tokio::test]
async fn a_turn_finishing_on_a_seat_a_page_shows_asks_the_core_for_a_reading() {
    let (url, fleet) = a_server().await;
    let mut asked = a_reporting_seat(&fleet, a_small_reading());
    let (_socket, data) = a_page_on(&url, Subject::Session(lead_seat())).await;
    assert_eq!(
        data["header"]["context"]["percent"], 12,
        "precondition: the seat reports a reading, so there is nothing wrong yet: {data}",
    );
    // Every frame of a turn arrives before the one that ends it, so a trigger
    // that took any of them would spend the interval on the first assistant
    // chunk and decline the result - leaving the page with a mid-turn number
    // that stops moving exactly when the turn ends.
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: an_assistant_chunk(),
        origin: None,
        source: None,
    });
    // One window for both negatives: the read that opened the seat asks for
    // nothing, and a frame that is not the turn's end asks for nothing either.
    assert!(
        next_agent_command(&mut asked, 250).await.is_none(),
        "opening a seat that already reports asks nothing, and neither does a frame that is \
         not the turn's end",
    );

    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_finished_turn(),
        origin: None,
        source: None,
    });

    let command = next_agent_command(&mut asked, 5_000).await;
    assert!(
        matches!(command, Some(AgentCommand::GetContextUsage { .. })),
        "the turn that just finished grew the transcript, so the reading is asked for again: \
         {command:?}",
    );
}

/// The other half of that trigger: a seat no page is holding is asked for
/// nothing, because the probe is answered inline over the CLI's whole
/// transcript and a fleet finishing turns with nobody watching would spend that
/// computation for nobody.
///
/// **And being unwatched must not cost the seat its turn when a page does
/// open.** A trigger that admitted before it asked who was watching would spend
/// the interval on a seat nobody was reading, so the page that opened a moment
/// later would wait up to a minute for a reading - the staleness this whole
/// change is about, arrived at by its own path. The second half below is that
/// page opening, and it is also what keeps the silence above from being an
/// assertion about a channel that simply never fires.
#[tokio::test]
async fn a_turn_finishing_on_a_seat_no_page_holds_asks_for_nothing() {
    let (url, fleet) = a_server().await;
    let mut asked = a_reporting_seat(&fleet, a_small_reading());
    // The home is every row and no seat's page, so nothing here is holding the
    // lead's seat.
    let _socket = a_page_on(&url, Subject::Home).await;

    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_finished_turn(),
        origin: None,
        source: None,
    });

    // The window is the assertion: a probe that wrongly fired lands a
    // scheduling hop after the frame.
    let command = next_agent_command(&mut asked, 250).await;
    assert!(
        command.is_none(),
        "a turn on a seat with no page open on it costs no probe: {command:?}",
    );

    // The same seat, now with a page on it and a turn that ends inside the
    // interval the unwatched turn must not have spent.
    let _page = a_page_on(&url, Subject::Session(lead_seat())).await;
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_finished_turn(),
        origin: None,
        source: None,
    });

    let command = next_agent_command(&mut asked, 5_000).await;
    assert!(
        matches!(command, Some(AgentCommand::GetContextUsage { .. })),
        "and the turn after a page opens is asked for, so the silence above was the seat being \
         unwatched rather than an interval it spent: {command:?}",
    );
}

/// The bound cannot be what refuses the one ask whose whole point is to
/// replace the reading it would be applied to.
///
/// The token gate reads the seat's held reading, and the only writer of that
/// reading is the probe's own answer - so a seat past the gate refuses every
/// ask, and with no way past the bound its number is frozen at the high value
/// for the life of its occupant. A compaction is the one thing that lowers the
/// transcript under a high reading, and the post-compaction refresh is forced
/// for exactly this: the gate reads the same stale number.
#[tokio::test]
async fn a_compaction_settling_on_a_seat_a_page_holds_asks_past_the_bound() {
    let (url, fleet) = a_server().await;
    let mut asked = a_reporting_seat(&fleet, a_reading_past_the_gate());
    let _socket = a_page_on(&url, Subject::Session(lead_seat())).await;

    // Precondition, and the latch itself: an ordinary ask is refused at this
    // reading, so a fix that only widened the ask would not be one.
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_finished_turn(),
        origin: None,
        source: None,
    });
    let refused = next_agent_command(&mut asked, 250).await;
    assert!(
        refused.is_none(),
        "precondition: a reading past the gate refuses an ordinary ask: {refused:?}",
    );

    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_compaction_settle(),
        origin: None,
        source: None,
    });

    let command = next_agent_command(&mut asked, 5_000).await;
    assert!(
        matches!(command, Some(AgentCommand::GetContextUsage { .. })),
        "the compaction left the held reading wrong rather than merely old, so it is replaced \
         whatever the bound says: {command:?}",
    );
    let again = next_agent_command(&mut asked, 250).await;
    assert!(again.is_none(), "and one settle costs one ask, not one per frame: {again:?}");
}

/// A compaction is the one ask that does not wait for a page, because the
/// reading it invalidates is the core's and every view reads it.
///
/// The page opened afterwards is the case this closes: it reads the number the
/// seat already holds, the read path asks only for a seat that reports nothing,
/// and no turn of its own would ask either while the stale number stands past
/// the token gate.
#[tokio::test]
async fn a_compaction_settling_on_a_seat_no_page_holds_still_asks() {
    let (url, fleet) = a_server().await;
    let mut asked = a_reporting_seat(&fleet, a_reading_past_the_gate());
    // The home is every row and no seat's page, so nothing here is holding the
    // lead's seat.
    let _socket = a_page_on(&url, Subject::Home).await;

    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_compaction_settle(),
        origin: None,
        source: None,
    });

    let command = next_agent_command(&mut asked, 5_000).await;
    assert!(
        matches!(command, Some(AgentCommand::GetContextUsage { .. })),
        "a compaction leaves the reading wrong for whoever opens the seat next, so the ask does \
         not wait for a page: {command:?}",
    );
}

/// A permission-mode change is not a compaction, and the gate it would otherwise
/// step past is the expensive one: a page-held `[1m]` seat past the token gate
/// that fires on a mode toggle spends a whole-transcript walk on an ask whose
/// reading never stopped being true.
///
/// The frame shares the `status`/null shape with the settle and is told apart
/// only by carrying the mode rather than the compaction's result, which is what
/// this pins.
#[tokio::test]
async fn a_permission_mode_change_is_not_a_settled_compaction() {
    let (url, fleet) = a_server().await;
    let mut asked = a_reporting_seat(&fleet, a_reading_past_the_gate());
    let _socket = a_page_on(&url, Subject::Session(lead_seat())).await;

    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_permission_mode_change(),
        origin: None,
        source: None,
    });

    let command = next_agent_command(&mut asked, 250).await;
    assert!(
        command.is_none(),
        "a mode change is not a compaction settling, so it is not asked past the bound: \
         {command:?}",
    );
}

/// The bypass is past both bounds, and the interval is the one this can only
/// show end to end: an ordinary ask has already gone, so a forced ask that
/// respected the interval would be dropped.
///
/// That is the state the bypass exists for - a turn ends, the user compacts, and
/// the settle lands inside the minute - and the reading would stand at the
/// pre-compaction number until the next compaction if the interval refused it.
#[tokio::test]
async fn a_compaction_settling_inside_the_interval_still_asks() {
    let (url, fleet) = a_server().await;
    let mut asked = a_reporting_seat(&fleet, a_small_reading());
    let _socket = a_page_on(&url, Subject::Session(lead_seat())).await;

    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_finished_turn(),
        origin: None,
        source: None,
    });
    let first = next_agent_command(&mut asked, 5_000).await;
    assert!(
        matches!(first, Some(AgentCommand::GetContextUsage { .. })),
        "precondition: the turn's ask goes, which is what starts the interval: {first:?}",
    );

    // The compaction settles a scheduling hop later, which is as far inside the
    // interval as this can be driven.
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_compaction_settle(),
        origin: None,
        source: None,
    });

    let second = next_agent_command(&mut asked, 5_000).await;
    assert!(
        matches!(second, Some(AgentCommand::GetContextUsage { .. })),
        "the settle is asked for inside the interval the turn's ask opened: {second:?}",
    );
}

/// A sink for every record a site emits, with a guard that has to be HELD for
/// as long as a test means to catch them.
///
/// **A guard rather than the crate's `test_support::logged`, which runs a
/// closure to completion on the caller's thread.** The record this exists for is
/// emitted from the fold task `serve` spawns, and every test here is a
/// current-thread `#[tokio::test]` - so that task is polled on this test's own
/// thread, and the thread-local subscriber applies for as long as the guard is
/// alive across the awaits.
fn catch_records() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("the sink is not poisoned").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let written = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = Arc::clone(&written);
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || Sink(Arc::clone(&sink)))
        .with_ansi(false)
        .finish();
    (written, tracing::subscriber::set_default(subscriber))
}

/// Everything caught so far, one JSON record per line.
fn caught(written: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(written.lock().expect("the sink is not poisoned").clone())
        .expect("the sink holds utf-8")
}

/// A seat with no agent has the ask ISSUED and its refusal recorded, which is
/// what the socket page says happens: the socket issues the ask whether or not
/// an agent is behind the seat, and where there is none it is refused.
///
/// The record is the only product of that refusal - the fold holds no
/// connection, so nothing can be drawn for it - and it is emitted from the fold
/// task rather than from anything the test drives. That is why the capture is
/// installed across the awaits rather than around a call.
#[tokio::test]
async fn a_seat_with_no_agent_has_the_refused_ask_recorded() {
    let (url, fleet) = a_server().await;
    // A reading, so the ask is admitted and the refusal is the missing agent
    // rather than one of the bounds.
    fleet.seed_view_facts(
        &lead_seat(),
        ViewFacts { context: Some(a_small_reading()), ..ViewFacts::default() },
    );
    let (written, _guard) = catch_records();
    let _socket = a_page_on(&url, Subject::Session(lead_seat())).await;

    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: a_finished_turn(),
        origin: None,
        source: None,
    });

    // The record lands a scheduling hop after the frame, so the read is repeated
    // and bounded rather than taken once.
    for _ in 0..200 {
        if caught(&written).contains("context_usage_request_failed") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let records = caught(&written);
    let Some(refusal) = records.lines().find(|line| line.contains("context_usage_request_failed"))
    else {
        panic!("the ask is issued for a seat with no agent and the refusal is recorded: {records}")
    };
    assert!(
        refusal.contains("TestOrg/proj/lead"),
        "and the refusal names the seat it was for: {refusal}",
    );
}

/// What bounds that trigger, and the half of the seat-opened rule it must not
/// undo: refreshing on the frame must not become one probe per frame.
///
/// A turn is not the only thing that ends in a burst - a resumed session and a
/// run of quick turns both produce several at once - and the socket asks a
/// seat at most once a minute. The seat here reports the reading the tests
/// above seed, so the reads that opened its page are not what is being counted.
#[tokio::test]
async fn a_burst_of_finished_turns_on_a_reporting_seat_costs_one_probe() {
    let (url, fleet) = a_server().await;
    let mut asked = a_reporting_seat(&fleet, a_small_reading());
    let _socket = a_page_on(&url, Subject::Session(lead_seat())).await;

    for _ in 0..8 {
        fleet.emit(SessionUpdate::ChatAppended {
            key: lead_seat(),
            msg: a_finished_turn(),
            origin: None,
            source: None,
        });
    }

    let first = next_agent_command(&mut asked, 5_000).await;
    assert!(
        matches!(first, Some(AgentCommand::GetContextUsage { .. })),
        "the first turn of the burst is worth a probe: {first:?}",
    );
    let second = next_agent_command(&mut asked, 250).await;
    assert!(
        second.is_none(),
        "the rest of the burst is inside the minimum interval between asks, so eight frames \
         cost one probe: {second:?}",
    );
}

/// The role is decided by the client's first SUBSCRIBE, not by its first
/// message. Deciding on the first message locked a connection whose opening
/// word was a `more` or a command into observing for its whole life, so a
/// client that declared `answering` afterwards was never handed a prompt - and
/// the page told it the declaration on its first subscribe was the one that
/// counted.
#[tokio::test]
async fn a_client_declares_answering_on_a_subscribe_and_not_on_its_first_word() {
    let (url, fleet) = a_server().await;
    assert_eq!(fleet.answering_count(), 0, "precondition: nothing answers yet");
    let mut socket = connect(&url).await;

    // A first message that is not a subscribe, which used to fix the role.
    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 5 })
        .await;
    let _ = next_server(&mut socket).await;
    assert_eq!(fleet.answering_count(), 0, "a command or a page is not a declaration either way");

    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    snapshot_answering(&mut socket).await;
    assert_eq!(
        fleet.answering_count(),
        1,
        "the subscribe that declares it is the one the core registers",
    );
}

/// A client that goes away without unsubscribing is still a client that has
/// gone. The attachment count decides whether a completion was watched, so a
/// seat left attached by a dropped connection never arms a mark again - the
/// mark is dead for every seat anyone has ever looked at.
#[tokio::test]
async fn a_dropped_connection_lets_go_of_the_seats_it_attached() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");

    let attached = {
        let mut leaving = connect(&url).await;
        send(
            &mut leaving,
            ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
        )
        .await;
        snapshot_answering(&mut leaving).await;
        // A completion with nobody showing the seat arms its diamond. Showing
        // the seat spends it and answers with the home, which is the ack that
        // the seat was attached before the drop - a showing that spent
        // nothing sends no frame at all.
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
            origin: None,
            source: None,
        });
        assert!(
            snapshot_until(&mut leaving, Subject::Home, |data| {
                data.get("unseen")
                    .and_then(|held| held.as_array())
                    .is_some_and(|held| !held.is_empty())
            })
            .await,
            "precondition: the completion armed the seat's diamond",
        );
        send(
            &mut leaving,
            ClientMessage::Subscribe {
                what: Subject::Session(lead_seat()),
                answering: true,
                review: false,
            },
        )
        .await;
        snapshot_answering(&mut leaving).await;
        // The subscription alone spends nothing; the seat is attached by the
        // connection saying it has it on screen.
        send(&mut leaving, ClientMessage::Showing { seat: Some(lead_seat()) }).await;
        // **The answer must be the one this showing earned**: the diamond the
        // completion armed is gone from the home it re-sends.
        assert!(
            snapshot_until(&mut leaving, Subject::Home, |data| {
                data.get("unseen")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|unseen| !unseen.iter().any(|held| held["label"] == "lead"))
            })
            .await,
            "the showing's home answer: the seat was attached and its diamond spent before the drop",
        );
        // Counted WITH it attached, so the transport's own fold is in the
        // number either way.
        let attached = fleet.subscriber_count();
        // Dropped without an unsubscribe, which is what a client that crashed
        // or was closed does.
        attached
    };
    assert!(
        wait_for_the_server_to_notice(&fleet, attached).await,
        "the server notices a client that went away: {attached} attached, {} still",
        fleet.subscriber_count(),
    );

    // A turn now finishes with nobody showing the seat, so it has to mark it.
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
        origin: None,
        source: None,
    });

    // The fold is the transport's own task, so the mark lands a scheduling
    // hop after the emit. Polling is honest because the property IS
    // "eventually", and it is bounded so a fold that never runs fails this
    // test rather than hanging it.
    let mut fresh = connect(&url).await;
    let mut marked = false;
    let mut last = 0;
    for _ in 0..200 {
        send(
            &mut fresh,
            ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
        )
        .await;
        let (_, data, _) = snapshot_answering(&mut fresh).await;
        let unseen = data["unseen"].as_array().expect("the home carries the marks");
        if unseen.iter().any(|slot| slot["label"] == "lead" && slot["project"] == "proj") {
            marked = true;
            break;
        }
        last = unseen.len();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        marked,
        "the seat the dropped client was showing is unlooked again, not still attached; marks seen: {last}",
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
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    snapshot_answering(&mut socket).await;

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
        origin: None,
        source: None,
    });
    // That a completion reaches the home as a row change is a property of the
    // FILTER, and this connection watches the home. It says nothing about when
    // the fold ran: that is the transport's own task now, so the snapshot below
    // polls rather than reading once and claiming an ordering nothing gives.
    assert!(
        matches!(next_server(&mut socket).await, ServerMessage::Update { .. }),
        "the completion reaches the home as a row change",
    );

    let mut fresh = connect(&url).await;
    let marked = snapshot_until(&mut fresh, Subject::Home, |data| {
        data["unseen"].as_array().is_some_and(|slots| {
            slots.iter().any(|slot| slot["label"] == "lead" && slot["project"] == "proj")
        })
    })
    .await;

    assert!(marked, "the seat whose turn went unwatched is marked");
}

/// Five commands report through `reply_to` and have no update behind them, so
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

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("a silent dispatch here is the client unable to tell it from success");
    };
    assert_eq!(what, "reply_to", "the refusal names the field that is missing");
    assert!(why.contains("reply_to"), "and says which field in the sentence: {why}");
}

/// The other half of the same rule: a `reply_to` on a command that does not
/// answer through one. Nothing comes down the reply for it - the outcome
/// rides the subscription - so the field is refused rather than accepted and
/// ignored, and refused BEFORE dispatch, because a command that both acted
/// and answered nothing is the case a client cannot tell from success.
#[tokio::test]
async fn a_reply_to_on_a_command_that_has_no_reply_is_refused() {
    let (url, fleet) = a_server().await;
    fleet.intercept_dispatch();
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::Cancel { key: lead_seat() }),
            reply_to: Some(7),
        },
    )
    .await;

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("a client that asked for an answer has to hear one");
    };
    assert_eq!(what, "reply_to", "the refusal names the field: {why}");
    assert!(why.contains("reply_to"), "and says which field in the sentence: {why}");
    assert!(fleet.dispatched().is_empty(), "a refused command must not run behind the refusal");
}

/// **A pending reply does not hold the connection's read loop** (#2445).
///
/// The server used to answer a reply-carrying command inline: a despawn's
/// reply comes only when its detached worktree cleanup finishes (seconds to
/// minutes), and a read loop awaiting it read nothing behind it - a subscribe,
/// a spawn or a review save on the same connection waited, and its client
/// bound could fire on a command nothing was wrong with.
///
/// The core is intercepted, so the command is never answered: the reply stays
/// pending for the whole test, which holds the despawn's window still. **That
/// pending reply is the intercept's doing** - the buffered command keeps its
/// oneshot sender alive, so nothing answers it. An intercept that dropped
/// commands would answer this one, and the test would stop discriminating
/// while still passing.
#[tokio::test]
async fn a_pending_reply_does_not_hold_the_connections_read_loop() {
    let (url, fleet) = a_server().await;
    fleet.intercept_dispatch();
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::DespawnWorker {
                project_key: forge_workspace::ProjectKey::new("proj"),
                label: "w1".to_owned(),
                force: false,
                respond: None,
            }),
            reply_to: Some(1),
        },
    )
    .await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: false, review: false },
    )
    .await;

    let next = next_server_within(&mut socket, 5_000).await;
    assert!(
        matches!(&next, Some(ServerMessage::Snapshot { .. })),
        "the message behind a pending reply was never read: {next:?}",
    );
}

/// **An answer dispatched off the read loop still carries its `reply_to`**
/// (#2445): the client resolves a pending command by that field alone, so the
/// task that waits for the answer has to hand the right one back.
#[tokio::test]
async fn an_answer_off_the_read_loop_carries_its_reply_to() {
    let (url, fleet) = a_server().await;
    let key = fleet
        .workspace()
        .list_projects()
        .into_iter()
        .next()
        .expect("the fleet declares a project")
        .key;
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::DespawnWorker {
                project_key: key,
                label: "w1".to_owned(),
                force: false,
                respond: None,
            }),
            reply_to: Some(9),
        },
    )
    .await;

    let next = next_server_within(&mut socket, 5_000).await;
    let Some(ServerMessage::Reply { reply_to, .. }) = &next else {
        panic!("the answer to a reply-carrying command never came: {next:?}");
    };
    assert_eq!(*reply_to, 9, "the answer names the dispatch that asked for it");
}

/// **A refused dispatch answers the press that made it.** The connection
/// reads the dispatch id off the command and echoes it on the refusal, which
/// is the whole of the attribution: a page holding a draft until its own
/// dispatch answers compares this against the id it minted, so a sibling
/// action's refusal - naming its own or none - never disposes of that draft.
///
/// This is the protocol-mismatch arm (`reply_to` on a command that has no
/// reply). The arm a page's own add press travels is the one below.
#[tokio::test]
async fn a_refusal_echoes_the_dispatch_its_press_named() {
    let (url, fleet) = a_server().await;
    fleet.intercept_dispatch();
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::MarketplaceAdd {
                source: "owner/repo".to_owned(),
                dispatch_id: Some(41),
            }),
            reply_to: Some(7),
        },
    )
    .await;

    let ServerMessage::Error { what, dispatch_id, .. } = next_server(&mut socket).await else {
        panic!("a refused dispatch is answered with an error");
    };
    assert_eq!(what, "reply_to", "the refusal names what was wrong");
    assert_eq!(dispatch_id, Some(41), "and the dispatch the press named rides it back");
    assert!(fleet.dispatched().is_empty(), "the refused command did not run");
}

/// **A dispatch names the connection that sent it** (spam-control A5, issue
/// #2372): a parked hand-off's answer arrived over the socket with no way to
/// attribute it, so every dispatched command logs the client and the command
/// variant. The client id is the socket's own, minted per connection.
#[tokio::test]
async fn a_dispatched_command_is_logged_with_its_connection_and_variant() {
    let (url, fleet) = a_server().await;
    fleet.intercept_dispatch();
    let (mut socket, _home) = a_page_on(&url, Subject::Home).await;
    let (written, _guard) = catch_records();

    send(
        &mut socket,
        ClientMessage::Command { command: Box::new(Command::PluginUpdateAll), reply_to: None },
    )
    .await;
    // The dispatch runs behind the read that answers it, so the read is
    // repeated and bounded rather than taken once.
    for _ in 0..200 {
        if caught(&written).contains("command_dispatched") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let records = caught(&written);
    let Some(line) = records.lines().find(|line| line.contains("command_dispatched")) else {
        panic!("a dispatched command is logged with its connection: {records}");
    };
    assert!(line.contains("PluginUpdateAll"), "the command variant is named: {line}");
    assert!(line.contains("client="), "and the connection that sent it: {line}");
}

/// **A refused command never logs as dispatched** (spam-control B3f): the
/// attribution line must mean the command reached the core, so the
/// `reply_to` refusals - which run before any dispatch - leave it silent.
#[tokio::test]
async fn a_refused_command_is_not_logged_as_dispatched() {
    let (url, fleet) = a_server().await;
    fleet.intercept_dispatch();
    let (mut socket, _home) = a_page_on(&url, Subject::Home).await;
    let (written, _guard) = catch_records();

    // A fire-and-forget command sent WITH a reply id is refused before
    // dispatch.
    send(
        &mut socket,
        ClientMessage::Command { command: Box::new(Command::PluginUpdateAll), reply_to: Some(3) },
    )
    .await;
    let ServerMessage::Error { .. } = next_server(&mut socket).await else {
        panic!("a refused dispatch is answered with an error");
    };
    let records = caught(&written);
    assert!(
        !records.contains("command_dispatched"),
        "a command refused before dispatch never logs as dispatched: {records}",
    );
    assert!(fleet.dispatched().is_empty(), "and nothing was dispatched");
}

/// **The refusal a page's own add press actually gets.** The add is
/// fire-and-forget - sent without `reply_to`, the way the page sends it - so
/// its refusals travel the fire-and-forget arm, not the mismatch arm above: a
/// regression that dropped the echo from that arm alone would leave this file
/// green while the page lost its attribution.
#[tokio::test]
async fn a_fire_and_forget_refusal_echoes_the_dispatch_its_press_named() {
    let (url, fleet) = a_server().await;
    // Hold the single-action gate: a run whose scan blocks until this test
    // releases it, so the add below is refused while the gate is held. The
    // gate is taken at dispatch, before the scan starts, so the refusal does
    // not race the release.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = Mutex::new(held);
    fleet.workspace().set_plugin_scan_for_test(move || {
        let _ = held.lock().expect("the scan's lock").recv();
        None
    });
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::Command { command: Box::new(Command::PluginUpdateAll), reply_to: None },
    )
    .await;
    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::MarketplaceAdd {
                source: "owner/repo".to_owned(),
                dispatch_id: Some(41),
            }),
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Error { what, why, dispatch_id, .. } = next_server(&mut socket).await else {
        panic!("a refused dispatch is answered with an error");
    };
    assert_eq!(what, "dispatch", "the tag a page routes a refused press by");
    assert!(why.contains("already running"), "in the core's own words: {why}");
    assert_eq!(dispatch_id, Some(41), "and the dispatch the press named rides it back");

    // Let the held run finish rather than leaving its scan blocked for good.
    release.send(()).expect("the held scan is released");
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

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("silence here is the click that did nothing");
    };
    assert_eq!(what, "dispatch", "the refusal is the core's own: {why}");
    assert!(why.contains("nothing-is-waiting"), "and it names the prompt: {why}");
    assert!(why.contains("TestOrg/proj/lead"), "and the seat in its own words: {why}");
    assert!(!why.contains("SessionSlot {"), "the sentence is not a Debug dump: {why}");
}

/// A command to a seat whose session task has closed its command channel is
/// refused naming the seat in its own words - the sentence the composer's
/// not-sent row draws.
///
/// `install_agent`'s stub hands back the channel's receiver; dropping it is
/// what closes the channel, the way a session that ended leaves it.
#[tokio::test]
async fn a_command_to_a_closed_session_task_names_the_seat_in_words() {
    let (url, fleet) = a_server().await;
    drop(fleet.install_agent("TestOrg", "proj", "lead"));
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(a_prompt_for("TestOrg", "proj", "lead")),
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("expected an error")
    };
    assert_eq!(what, "dispatch", "the refusal is the core's own: {why}");
    assert!(why.contains("TestOrg/proj/lead"), "the seat is named in its own words: {why}");
    assert!(!why.contains("SessionSlot {"), "the sentence is not a Debug dump: {why}");
}

/// A Slack answer the core no longer holds is refused by its own operation's
/// name. The dock it came from is gone by the time a view reads the refusal,
/// so a generic `dispatch` would reach no row - and it is the tag, not the
/// sentence, that a client routes by.
#[tokio::test]
async fn a_refused_slack_answer_names_its_own_operation() {
    let mut socket = connected().await;
    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::RespondSlackPost {
                key: lead_seat(),
                id: uuid::Uuid::new_v4(),
                approved: true,
            }),
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("silence here is the click that did nothing");
    };
    assert_eq!(
        what, "respond_slack_post",
        "the refusal names the operation a view draws it for: {why}",
    );
    assert!(
        why.contains("no longer waiting") && why.contains("asking session"),
        "and carries the draft's own sentence, endings included: {why}",
    );
}

/// A hand-off answer the core no longer holds is refused by its own
/// operation's name, on the Slack draft's exact rule: the dock it came from is
/// gone by the time a view reads the refusal, so a generic `dispatch` would
/// reach no row - and it is the tag, not the sentence, that a client routes by.
#[tokio::test]
async fn a_refused_hand_off_answer_names_its_own_operation() {
    let mut socket = connected().await;
    send(
        &mut socket,
        ClientMessage::Command {
            command: Box::new(Command::RespondBrowserHandOff {
                key: lead_seat(),
                id: uuid::Uuid::new_v4(),
                answer: forge_primitives::browser::HandOffAnswer {
                    ending: forge_primitives::browser::HandOffEnding::Done,
                    source: forge_primitives::browser::HandOffSource::Takeover,
                    client: None,
                },
            }),
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("silence here is the click that did nothing");
    };
    assert_eq!(
        what, "respond_browser_hand_off",
        "the refusal names the operation a view draws it for: {why}",
    );
    assert!(
        why.contains("no longer waiting") && why.contains("asking session"),
        "and carries the hand-off's own sentence, endings included: {why}",
    );
}

/// **A command the server cannot read is answered where a refusal is drawn.**
/// A client older than the server sends a command in the shape it knows; the
/// decode fails, and a `client_message` refusal is one a client reports only
/// to its console - its dock has already stood down, so the person believes
/// the answer left while the prompt, and the tool behind it, wait for ever.
/// The frame's kind is readable even when its body is not, and that is what
/// the reply routes by.
#[tokio::test]
async fn an_unreadable_command_is_answered_as_a_dispatch_refusal() {
    let mut socket = connected().await;
    // The v12 answer: `done` in place of this build's `answer`.
    let sent = serde_json::json!({
        "kind": "command",
        "command": {
            "respond_browser_hand_off": {
                "key": lead_seat(),
                "id": "0192e1c0-0000-7000-8000-0000000000aa",
                "done": false,
            },
        },
        "reply_to": null,
    });
    socket.send(Message::Text(sent.to_string().into())).await.expect("send");

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("an unreadable command is answered, not dropped");
    };
    assert_eq!(what, "dispatch", "the refusal a client draws where the answer came from: {why}");
    assert!(why.contains("older"), "and the likely cause is named: {why}");
    assert!(
        why.contains("still waiting"),
        "with the fact that the prompt survives the failed answer: {why}",
    );
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

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("expected an error")
    };
    // Which refusal, not merely that one arrived: an arm that echoed the whole message
    // back would carry the seat's name only in its `Debug` form, which the assertion below
    // rejects - and the `what` says which arm composed the sentence.
    assert_eq!(
        what, "dispatch",
        "the refusal comes from the core, not from this server declining the message: {why}",
    );
    // This rests on WHICH refusal the surface returns: `UnknownSession(slot)` names the
    // seat in its own words, while `NoActiveSession` renders as "no active session" and
    // carries no seat at all. If these assertions fail on the seat's name, the question is
    // which variant came back - not whether the error reached the client, because the
    // `let else` above already proved that.
    assert!(why.contains("Nowhere/nothing/lead"), "the error names the seat: {why}");
    assert!(!why.contains("SessionSlot {"), "the sentence is not a Debug dump: {why}");
}

/// A subscribe for a seat forge holds no session for is answered with an
/// error, and the sentence names the seat in the slot's own words rather than
/// the `SessionSlot { .. }` Debug dump a reader used to be handed.
///
/// **The declared role is granted even under the refusal.** The role is taken
/// at register, before the seat can be encoded and refuse, so the refusal owes
/// the grant too - without it the connection the relay is already routing
/// asks to is one whose own client believes it never got the role, and its
/// strip says another client drives the browser while the asks land here.
#[tokio::test]
async fn a_subscribe_for_a_seat_that_is_not_there_names_it_in_words() {
    let mut socket = connected().await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(SessionSlot::for_label("Nowhere", "nothing", Some("lead"))),
            answering: true,
            review: false,
        },
    )
    .await;

    let ServerMessage::Error { what, why, seat, .. } = next_server(&mut socket).await else {
        panic!("expected an error")
    };
    assert_eq!(what, "subscribe", "the refusal comes from the subscribe arm: {why}");
    assert!(why.contains("Nowhere/nothing/lead"), "the seat is named in its own words: {why}");
    assert!(!why.contains("SessionSlot {"), "the sentence is not a Debug dump: {why}");
    // **The error names the seat it refused** (#2420): a client holding
    // several seats cannot tell its own refused read from another's without
    // the field, and the `more` refusal has always carried it.
    assert_eq!(
        seat,
        Some(SessionSlot::for_label("Nowhere", "nothing", Some("lead"))),
        "the refusal names the seat it belongs to",
    );
}

/// Paging a seat forge holds no session for is refused in the same words: the
/// seat's own, not its Debug form.
#[tokio::test]
async fn a_more_for_a_seat_that_is_not_there_names_it_in_words() {
    let mut socket = connected().await;
    send(
        &mut socket,
        ClientMessage::More {
            conversation: SessionSlot::for_label("Nowhere", "nothing", Some("lead")),
            before: None,
            turns: 5,
        },
    )
    .await;

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("expected an error")
    };
    assert_eq!(what, "more", "the refusal comes from the paging arm: {why}");
    assert!(why.contains("Nowhere/nothing/lead"), "the seat is named in its own words: {why}");
    assert!(!why.contains("SessionSlot {"), "the sentence is not a Debug dump: {why}");
}

/// A page asked for a seat forge has a directory for but holds nothing for is
/// refused by the arm that runs once the directory resolves, and its sentence
/// names the seat in its own words too - the Debug form is what the sweep dug
/// out here.
///
/// The path is the issue's own scenario: a project `forge.toml` declares with
/// nothing started, so the roster resolves the directory and the refusal that
/// answers is this arm rather than the no-session one above it.
#[tokio::test]
async fn a_more_for_a_seat_nothing_holds_names_it_in_words() {
    let mut socket = connected().await;
    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 5 })
        .await;

    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("expected an error")
    };
    assert_eq!(what, "more", "the refusal comes from the paging arm: {why}");
    assert!(why.contains("TestOrg/proj/lead"), "the seat is named in its own words: {why}");
    assert!(!why.contains("SessionSlot {"), "the sentence is not a Debug dump: {why}");
}

/// Both paging refusals a reader can be shown name the seat in its own words.
///
/// The fold-failure arm has no deterministic route through the socket - its
/// only failure is the fold's own task dying, and the held conversation's
/// locks are poison-tolerant - so its sentence is pinned at the source beside
/// the socket-level pin on the reachable one.
#[test]
fn the_paging_refusals_name_the_seat_in_words() {
    let source = include_str!("../src/transport/connection.rs");
    assert!(
        source.contains("\"the conversation for {} is not held yet"),
        "the not-held refusal still formats the seat through Debug",
    );
    assert!(
        source.contains("\"the fold over {} did not finish"),
        "the fold-failure refusal still formats the seat through Debug",
    );
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

/// [`a_turns_rows`] with the assistant's reply padded to `pad` bytes, so a
/// turn can be made larger than a first-frame budget. The user's words stay
/// exactly `turn N`, which is what a page's turns are checked by.
fn a_turns_rows_padded(turn: usize, pad: usize) -> String {
    format!(
        r#"{{"type":"user","uuid":"u{turn}","message":{{"role":"user","content":"turn {turn}"}}}}
{{"type":"assistant","uuid":"a{turn}","message":{{"id":"m{turn}","role":"assistant","model":"claude-opus-5","content":[{{"type":"text","text":"reply {turn} {}"}}]}}}}
{{"type":"result","uuid":"r{turn}","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}}"#,
        "x".repeat(pad),
    )
}

/// A turn whose structured tool result is `pad` bytes of payload, so a turn
/// can be made to carry a record the cap must bound.
fn a_big_result_rows(turn: usize, pad: usize) -> String {
    format!(
        r#"{{"type":"user","uuid":"u{turn}","message":{{"role":"user","content":"turn {turn}"}}}}
{{"type":"assistant","uuid":"a{turn}","message":{{"id":"m{turn}","role":"assistant","model":"claude-opus-5","content":[{{"type":"text","text":"reply {turn}"}}]}}}}
{{"type":"user","uuid":"t{turn}","session_id":"s","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"call-{turn}","content":"ok"}}]}},"toolUseResult":{{"payload":"{}"}}}}
{{"type":"result","uuid":"r{turn}","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}}"#,
        "x".repeat(pad),
    )
}

/// **Only the turns the cut keeps are logged as capped.** A candidate the
/// budget drops was capped so it could be measured, and a line for it would
/// name a record the client never received.
#[tokio::test]
async fn a_dropped_turn_is_not_logged_as_capped() {
    install_capture();
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir_with_server(
        dir.path(),
        &[("TestOrg", &["capped"])],
        "[server]\nfirst_frame_cap_kb = 1\ntool_record_cap_kb = 1\n",
    )
    .expect("the fleet builds");
    let (url, state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let rows: Vec<String> = (0..4).map(|turn| a_big_result_rows(turn, 20_000)).collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "capped", "lead", &borrowed).expect("the transcript seeds");
    fleet
        .hold_conversation(&state, "TestOrg", "capped", "lead")
        .expect("the seat's conversation is held");
    let seat = SessionSlot::lead("TestOrg", "capped");

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Session(seat), answering: false, review: false },
    )
    .await;
    let ServerMessage::Snapshot { data, .. } = next_server(&mut socket).await else {
        panic!("a snapshot is the answer to a subscribe")
    };
    let kept = data["conversation"]["turns"].as_array().map_or(0, Vec::len);
    assert!(kept > 0 && kept < 4, "a 1 KB budget keeps some turns and not all: {kept}");

    let logged = captured()
        .lines()
        .filter(|line| line.contains("tool_record_capped") && line.contains("TestOrg/capped/lead"))
        .count();
    assert_eq!(logged, kept, "one line per served turn, none for a turn the cut dropped");
}

/// A client asking for history is handed whole turns over the socket: a page
/// opens on a turn the user wrote, and it carries the handle that asks for
/// the ones above it.
#[tokio::test]
async fn a_more_is_answered_with_a_page_of_whole_turns() {
    let (url, fleet, state) = a_server_with_state().await;
    let rows: Vec<String> = (0..20).map(a_turns_rows).collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");
    fleet
        .hold_conversation(&state, "TestOrg", "proj", "lead")
        .expect("the seat's conversation is held");

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 5 })
        .await;

    let ServerMessage::Page { turns, cursor, .. } = next_server(&mut socket).await else {
        panic!("a page is the answer to a request for more")
    };
    assert!(!turns.is_empty(), "the newest turns come back");
    assert_eq!(turns.len(), 5, "and a page is the turns the client asked for");
    // What crosses is each turn's MESSAGES: how a run of calls inside one
    // groups is a drawing decision, so a folded unit does not cross at all.
    assert_eq!(
        turns[0]
            .messages
            .first()
            .and_then(|frame| frame.get("type"))
            .and_then(|kind| kind.as_str()),
        Some("user"),
        "a page opens on a turn the user wrote rather than inside one: {turns:?}",
    );
    assert!(cursor.is_some(), "and it carries the handle that asks for the ones above");
}

/// **A page above the window's floor caps an oversized record** (#2374).
///
/// The held window is the other producer of a served page, and without this
/// the cap could be deleted from it with the suite green: the record would
/// cross whole on every `more` a client asks for.
#[tokio::test]
async fn a_more_above_the_floor_caps_the_record_it_serves() {
    let (url, fleet, state) = a_server_with_state().await;
    let rows: Vec<String> = (0..20)
        .map(|turn| {
            if turn == 19 {
                serde_json::json!({
                    "type": "user",
                    "uuid": format!("u{turn}"),
                    "session_id": "s",
                    "message": {"role": "user", "content": format!("turn {turn}")},
                    // A row the CLI wrote spells the record `toolUseResult`;
                    // the wire spells it `tool_use_result`.
                    "toolUseResult": {"payload": "x".repeat(100_000)},
                })
                .to_string()
            } else {
                a_turns_rows(turn)
            }
        })
        .collect();
    fleet
        .seed_transcript(
            "TestOrg",
            "proj",
            "lead",
            &rows.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .expect("the transcript seeds");
    fleet
        .hold_conversation(&state, "TestOrg", "proj", "lead")
        .expect("the seat's conversation is held");

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 5 })
        .await;

    let ServerMessage::Page { turns, .. } = next_server(&mut socket).await else {
        panic!("a page is the answer to a request for more")
    };
    let frame = turns
        .iter()
        .flat_map(|turn| turn.messages.iter())
        .find(|frame| frame["uuid"] == "u19")
        .expect("the oversized record's frame is in the page above the floor");
    assert_eq!(frame["tool_use_result"]["omitted"], true, "the record is marked: {frame}");
    assert!(
        frame["tool_use_result"]["bytes"].as_u64().is_some_and(|bytes| bytes > 100_000),
        "and says how much it weighed: {frame}",
    );
}

/// **The record's cursor carries the window's floor** (#2420). A byte-capped
/// giant serves its whole window, whose page names no handle of its own -
/// the history above it begins at the floor - so the `more` arm hands the
/// floor back. The record must hand the same handle: seeded without it, a
/// client reads the history above as ended and scrolling back is dead.
#[tokio::test]
async fn the_records_cursor_carries_the_windows_floor() {
    let (url, fleet, state) = a_server_with_state().await;
    let mut rows: Vec<String> = (0..20).map(a_turns_rows).collect();
    // One final turn that alone carries more frames than the window's cap:
    // the cut is turn-aligned and never empty, so the window keeps that turn
    // whole and drops the rows before it - its page opens on the window's
    // front while the history above begins at the floor.
    rows.push(
        serde_json::json!({
            "type": "user",
            "uuid": "u-giant",
            "session_id": "s",
            "message": {"role": "user", "content": "the giant turn"},
        })
        .to_string(),
    );
    for at in 0..5_100 {
        rows.push(
            serde_json::json!({
                "type": "assistant",
                "uuid": format!("a-giant-{at}"),
                "session_id": "s",
                "message": {
                    "id": format!("m-giant-{at}"),
                    "role": "assistant",
                    "model": "claude-opus-5",
                    "content": [{"type": "text", "text": "tick"}],
                },
            })
            .to_string(),
        );
    }
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");
    fleet
        .hold_conversation(&state, "TestOrg", "proj", "lead")
        .expect("the seat's conversation is held");

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: false,
            review: false,
        },
    )
    .await;
    let ServerMessage::Snapshot { data, .. } = next_server(&mut socket).await else {
        panic!("a snapshot is the answer to a subscribe")
    };
    let record = data["conversation"]["cursor"].as_str().map(str::to_owned);
    assert!(record.is_some(), "the record names the floor rather than the history's end: {data}");

    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 20 })
        .await;
    let ServerMessage::Page { cursor, .. } = next_server(&mut socket).await else {
        panic!("a page is the answer to a request for more")
    };
    assert_eq!(record, cursor, "the record's handle is the one `more` hands back");
}

/// **The configured cap reaches the page it serves** (#2374): the key is
/// pinned by EFFECT, not only by its parse. A fleet whose `forge.toml` says
/// `0` serves a record whole, and one whose cap is 1 KB marks the same
/// record.
#[tokio::test]
async fn the_configured_record_cap_reaches_the_served_page() {
    for (kb, capped) in [("0", false), ("1", true)] {
        let dir = tempfile::tempdir().expect("tempdir");
        let fleet = Fleet::in_dir_with_server(
            dir.path(),
            &[("TestOrg", &["proj"])],
            &format!("[server]\ntool_record_cap_kb = {kb}\n"),
        )
        .expect("the fleet builds");
        let (url, state) = serve_fleet(&fleet, Heartbeat::default()).await;
        let row = serde_json::json!({
            "type": "user",
            "uuid": "tool-result-1",
            "session_id": "s",
            "message": {"role": "user", "content": "the call"},
            "toolUseResult": {"payload": "x".repeat(100_000)},
        })
        .to_string();
        fleet
            .seed_transcript("TestOrg", "proj", "lead", &[row.as_str()])
            .expect("the transcript seeds");
        fleet
            .hold_conversation(&state, "TestOrg", "proj", "lead")
            .expect("the seat's conversation is held");

        let (_socket, data) = a_page_on(&url, Subject::Session(lead_seat())).await;
        let frame = data["conversation"]["turns"]
            .as_array()
            .expect("the page carries turns")
            .iter()
            .flat_map(|turn| turn["messages"].as_array().into_iter().flatten())
            .find(|frame| frame["uuid"] == "tool-result-1")
            .expect("the page carries the record's frame");
        let record = &frame["tool_use_result"];
        if capped {
            assert_eq!(record["omitted"], true, "a {kb} KB cap marks the record: {frame}");
        } else {
            assert_eq!(
                record["payload"].as_str().map(str::len),
                Some(100_000),
                "a 0 cap serves the record whole: {frame}",
            );
        }
    }
}

/// **The configured first-frame budget reaches the page a subscribe serves**,
/// and the walk above it still reaches every turn.
///
/// The key is pinned by EFFECT, like the record cap's: a fleet whose
/// `forge.toml` caps the first frame serves fewer turns than the count asks
/// for, and `more` from the handle the answer carries walks the whole
/// conversation. Nothing is dropped and the order does not move.
#[tokio::test]
async fn the_configured_first_frame_budget_is_completed_by_the_walk_above_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir_with_server(
        dir.path(),
        &[("TestOrg", &["proj"])],
        "[server]\nfirst_frame_cap_kb = 1\n",
    )
    .expect("the fleet builds");
    let (url, state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let rows: Vec<String> = (0..20).map(a_turns_rows).collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");
    fleet
        .hold_conversation(&state, "TestOrg", "proj", "lead")
        .expect("the seat's conversation is held");

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: false,
            review: false,
        },
    )
    .await;
    let ServerMessage::Snapshot { data, .. } = next_server(&mut socket).await else {
        panic!("a snapshot is the answer to a subscribe")
    };
    let first = said_words(&data["conversation"]["turns"]);
    assert!(first.len() < 20, "a 1 KB budget cuts the first frame: {} turns", first.len());
    assert_eq!(first.last().map(String::as_str), Some("turn 19"), "the newest turn rides it");
    assert!(
        first.windows(2).all(|pair| turn_number(&pair[0]) < turn_number(&pair[1])),
        "and it keeps the conversation's order: {first:?}",
    );

    let mut cursor = data["conversation"]["cursor"].as_str().map(str::to_owned);
    assert!(cursor.is_some(), "the cut page carries the handle above it");
    let mut reached = first;
    let mut walks = 0;
    while let Some(before) = cursor.take() {
        walks += 1;
        assert!(walks < 25, "the walk terminates rather than cycling");
        send(
            &mut socket,
            ClientMessage::More { conversation: lead_seat(), before: Some(before), turns: 20 },
        )
        .await;
        let ServerMessage::Page { turns, cursor: next, .. } = next_server(&mut socket).await else {
            panic!("a page is the answer to a request for more")
        };
        let words = page_words(&turns);
        assert!(
            words.windows(2).all(|pair| turn_number(&pair[0]) < turn_number(&pair[1])),
            "a page keeps the order too: {words:?}",
        );
        reached.extend(words);
        cursor = next;
    }

    assert_eq!(
        reached.len(),
        20,
        "each page is disjoint from the one it walks from, so no turn is served twice: {reached:?}",
    );
    reached.sort();
    let mut every: Vec<String> = (0..20).map(|turn| format!("turn {turn}")).collect();
    every.sort();
    assert_eq!(reached, every, "the first frame and its walk reach every turn");
}

/// **A newest turn larger than the whole budget is served whole.** A budget
/// that answered with no page would draw the seat empty, and one that split
/// the turn would hand a client half of it.
#[tokio::test]
async fn a_newest_turn_over_the_budget_is_served_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir_with_server(
        dir.path(),
        &[("TestOrg", &["proj"])],
        "[server]\nfirst_frame_cap_kb = 1\n",
    )
    .expect("the fleet builds");
    let (url, state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let rows: Vec<String> = (0..6)
        .map(|turn| if turn == 5 { a_turns_rows_padded(turn, 4_000) } else { a_turns_rows(turn) })
        .collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");
    fleet
        .hold_conversation(&state, "TestOrg", "proj", "lead")
        .expect("the seat's conversation is held");

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: false,
            review: false,
        },
    )
    .await;
    let ServerMessage::Snapshot { data, .. } = next_server(&mut socket).await else {
        panic!("a snapshot is the answer to a subscribe")
    };
    let first = said_words(&data["conversation"]["turns"]);
    assert_eq!(first, vec!["turn 5".to_owned()], "the oversized newest turn is the whole page");
    assert!(
        data["conversation"]["cursor"].as_str().is_some(),
        "with the history above it still askable",
    );
}

/// **`first_frame_cap_kb = 0` serves the count's page.** The opt-out is the
/// twenty turns a budget would cut, not the one turn its floor keeps.
#[tokio::test]
async fn a_zero_first_frame_cap_serves_the_twenty_turn_page() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir_with_server(
        dir.path(),
        &[("TestOrg", &["proj"])],
        "[server]\nfirst_frame_cap_kb = 0\n",
    )
    .expect("the fleet builds");
    let (url, state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let rows: Vec<String> = (0..20).map(a_turns_rows).collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");
    fleet
        .hold_conversation(&state, "TestOrg", "proj", "lead")
        .expect("the seat's conversation is held");

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: false,
            review: false,
        },
    )
    .await;
    let ServerMessage::Snapshot { data, .. } = next_server(&mut socket).await else {
        panic!("a snapshot is the answer to a subscribe")
    };
    let first = said_words(&data["conversation"]["turns"]);
    assert_eq!(first.len(), 20, "a 0 cap serves every turn the count asks for: {first:?}");
}

/// `turn N` as `N`, so an order check compares the numbers rather than the
/// words ("turn 10" sorts before "turn 9" as text).
fn turn_number(word: &str) -> u32 {
    word.trim_start_matches("turn ").parse().unwrap_or(u32::MAX)
}

/// The user words of a record's turns, in the order the page lists them.
fn said_words(turns: &serde_json::Value) -> Vec<String> {
    turns
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|turn| {
            turn["messages"][0]["message"]["content"][0]["text"].as_str().map(str::to_owned)
        })
        .collect()
}

/// The same, out of a page of typed turns.
fn page_words(turns: &[forge_server::transport::wire::TurnWire]) -> Vec<String> {
    turns
        .iter()
        .filter_map(|turn| {
            turn.messages[0]["message"]["content"][0]["text"].as_str().map(str::to_owned)
        })
        .collect()
}

/// **A sleeping seat's history answers from the transcript it left.**
///
/// The seat's last session wrote its file and the store still names its id,
/// and that is all a page needs: a column opening on an asleep seat draws the
/// conversation now rather than waiting for a spawn to carry the same history
/// back. Measured live 2026-10-12 on a sleeping lead: the first page waited on
/// the connect and the client's own retry cadence, ~2.6s for a history that
/// was already on disk.
#[tokio::test]
async fn a_sleeping_seats_history_answers_from_its_own_transcript() {
    let (url, fleet) = a_server().await;
    let rows: Vec<String> = (0..20).map(a_turns_rows).collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet
        .seed_asleep_transcript("TestOrg", "proj", "lead", &borrowed)
        .expect("the transcript seeds");

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 5 })
        .await;

    let ServerMessage::Page { turns, cursor, .. } = next_server(&mut socket).await else {
        panic!("an asleep seat's own transcript answers the page rather than refusing it")
    };
    assert_eq!(turns.len(), 5, "the newest turns come from the file the seat left");
    assert!(cursor.is_some(), "and the handle above them is the conversation's own");
}

/// The same read on the seat's own record, which is what a page opens on: a
/// subscribe to a sleeping seat is answered with the turns of its transcript
/// rather than with an empty conversation.
#[tokio::test]
async fn a_sleeping_seats_snapshot_carries_its_transcripts_turns() {
    let (url, fleet) = a_server().await;
    let rows: Vec<String> = (0..20).map(a_turns_rows).collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet
        .seed_asleep_transcript("TestOrg", "proj", "lead", &borrowed)
        .expect("the transcript seeds");

    let mut socket = connect(&url).await;
    let held = snapshot_until(&mut socket, Subject::Session(lead_seat()), |data| {
        data["conversation"]["turns"].as_array().is_some_and(|turns| {
            turns.len() == forge_server::transport::wire::SUBSCRIBE_TURNS as usize
        })
    })
    .await;

    assert!(
        held,
        "the record a client attaches to carries the seat's history, not an empty conversation",
    );
}

/// A user frame saying `text`, as a seat's own turn arrives.
fn a_said_frame(text: &str) -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
    }))
    .expect("a user frame")
}

/// A user frame carrying a structured record, as a tool result does.
fn a_recorded_frame(record: &serde_json::Value) -> forge_primitives::Message {
    serde_json::from_value(serde_json::json!({
        "type": "user",
        "uuid": "tool-result-1",
        "message": {"role": "user", "content": [{"type": "text", "text": "the call"}]},
        "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        "tool_use_result": record,
    }))
    .expect("a user frame")
}

/// Wait until the transport's own fold has attached to the core's stream.
///
/// It attaches at boot, one scheduling hop after the server starts; a frame
/// emitted before it is held for the first client instead, which is the boot
/// backlog's path rather than any of these tests'.
async fn wait_for_the_fold(fleet: &Fleet) {
    for _ in 0..200 {
        if fleet.subscriber_count() > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("the transport's own fold never attached");
}

/// How many times the answer's conversation carries a row saying `text` - the
/// count a duplicate hides behind a substring check.
fn says_times(data: &serde_json::Value, text: &str) -> usize {
    serde_json::to_string(&data["conversation"]).unwrap_or_default().matches(text).count()
}

/// The thinking tokens the snapshot's turns have spent, summed from the
/// deltas their frames carry.
fn thinking_spent(data: &serde_json::Value) -> i64 {
    data["conversation"]["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|turn| turn["messages"].as_array().into_iter().flatten())
        .filter(|msg| msg["subtype"] == "thinking_tokens")
        .filter_map(|msg| msg["estimated_tokens_delta"].as_i64())
        .sum()
}

/// A client attaching to a seat MID-TURN is handed the turn it is on: the
/// running state, and the thinking the running turn has already spent.
///
/// No frame it hears repeats either - the stream announces a turn once and
/// retains nothing - so a reconnect that dropped them would draw a seat
/// sitting idle on a turn that is running.
#[tokio::test]
async fn a_seats_reconnect_answer_carries_the_running_turn() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    let rows = [r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"go"}}"#];
    fleet.seed_transcript("TestOrg", "proj", "lead", &rows).expect("the transcript seeds");
    fleet
        .hold_conversation(&state, "TestOrg", "proj", "lead")
        .expect("the seat's conversation is held");
    // The running turn's own frames, as the task emits them: the window a
    // client reads is the transport's copy of exactly these.
    //
    // Emitted only once the transport's own fold is attached, or the frame is
    // held for the first client instead of reaching the copy this reads.
    wait_for_the_fold(&fleet).await;
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: forge_primitives::Message::ThinkingTokens {
            estimated_tokens: 24,
            estimated_tokens_delta: 24,
            uuid: "t1".to_owned(),
            session_id: "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45".to_owned(),
            extras: serde_json::Map::new(),
        },
    });
    fleet.seed_view_facts(&seat, ViewFacts::default());
    fleet
        .workspace()
        .domain_session_for(&seat)
        .expect("the seat is registered")
        .lock()
        .turn_pending = true;

    let mut socket = connect(&url).await;
    let running = snapshot_until(&mut socket, Subject::Session(seat.clone()), |data| {
        data["header"]["turn_in_flight"] == true
    })
    .await;
    assert!(
        running,
        "a client attaching mid-turn reads the turn it is on, not a seat that looks idle",
    );

    let spent = snapshot_until(&mut socket, Subject::Session(seat.clone()), |data| {
        thinking_spent(data) > 0
    })
    .await;
    assert!(
        spent,
        "and reads the thinking the running turn has spent, which no frame it hears repeats",
    );
}

/// A seat's answer carries the epoch its sequence belongs to and the number
/// the NEXT frame will bear, so a client that has just read the seat knows
/// what to expect rather than guessing - and knows what it did not miss.
///
/// **The number is the successor, not the last one minted.** A client that
/// read the last one would expect a frame it has already drawn and ask for a
/// range that is not missing.
#[tokio::test]
async fn a_seats_answer_carries_its_epoch_and_the_number_the_next_frame_will_bear() {
    let (url, fleet) = a_server().await;
    let seat = lead_seat();
    fleet.seed_view_facts(
        &seat,
        ViewFacts {
            session_id: Some(SessionId::from("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45")),
            ..Default::default()
        },
    );
    fleet.emit(SessionUpdate::TurnComplete { key: seat.clone(), terminal_reason: None });

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    assert_eq!(
        data["header"]["session_id"], "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
        "a client attaching after one frame reads the epoch its sequence belongs to",
    );
    // The hold starts the seat's watch (#2506) and the record's read marks it.
    // A still tree's first pass is seeded from that read and adds no frame, so
    // the count is the seat's one frame and nothing else - pinned absolutely
    // rather than waited out, so a pass that announces on open reds here.
    let baseline = next_frame_number(&mut socket, &seat).await;
    assert_eq!(baseline, 2, "the open adds no frame to the seat's one");

    // Another seat's traffic must not move this one's number, and this one's
    // own frame must: the counter is the seat's.
    let other = SessionSlot::worker("TestOrg", "proj", "reviewer");
    fleet.emit(SessionUpdate::TurnComplete { key: other.clone(), terminal_reason: None });
    assert_eq!(
        next_frame_number(&mut socket, &seat).await,
        baseline,
        "another seat's traffic does not move this one's number",
    );
    fleet.emit(SessionUpdate::TurnComplete { key: seat.clone(), terminal_reason: None });
    assert_eq!(
        next_frame_number(&mut socket, &seat).await,
        baseline + 1,
        "the seat's own frame is what moves its number",
    );
}

/// The number a seat's next frame will bear, read off a fresh subscribe's
/// answer.
async fn next_frame_number(socket: &mut Client, seat: &SessionSlot) -> u64 {
    send(
        socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(socket).await;
    data["header"]["seq"]
        .as_u64()
        .unwrap_or_else(|| panic!("the answer carries the number its next frame will bear: {data}"))
}

/// **A seat nobody has attached to yet is still at a number a client can
/// continue from.** The boot backlog is emitted before any subscriber exists;
/// a read that reported the last number, or zero, would have the first client
/// ask for frames it never missed or expect one that already went out.
#[tokio::test]
async fn a_seat_with_no_subscriber_yet_answers_with_the_next_number() {
    let (url, fleet) = a_server().await;
    let seat = lead_seat();
    for _ in 0..3 {
        fleet.emit(SessionUpdate::TurnComplete { key: seat.clone(), terminal_reason: None });
    }

    let mut socket = connect(&url).await;
    let held = snapshot_until(&mut socket, Subject::Session(seat.clone()), |data| {
        data["header"]["seq"] == 4
    })
    .await;
    assert!(held, "the first subscriber reads the number the seat's next frame will bear");
}

/// **A `/resume` opens a new epoch, and its sequence starts at 1.** The
/// transcript the new occupant replays is old, and none of it is a frame this
/// epoch missed: a client that read the counter as continuing would ask for a
/// range that can never arrive.
#[tokio::test]
async fn a_resumed_seats_answer_starts_its_new_epoch_at_one() {
    let (url, fleet) = a_server().await;
    let seat = lead_seat();
    fleet.emit(SessionUpdate::TurnComplete { key: seat.clone(), terminal_reason: None });
    fleet.emit(SessionUpdate::TurnComplete { key: seat.clone(), terminal_reason: None });
    fleet.emit(SessionUpdate::SessionReplaced {
        key: seat.clone(),
        session_id: SessionId::from("6c2d3e4f-5061-4b72-8839-ad1e2f3a4b56"),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: Vec::new(),
        dropped: 0,
        compaction_count: 0,
    });
    fleet.seed_view_facts(
        &seat,
        ViewFacts {
            session_id: Some(SessionId::from("6c2d3e4f-5061-4b72-8839-ad1e2f3a4b56")),
            ..Default::default()
        },
    );

    let mut socket = connect(&url).await;
    let held = snapshot_until(&mut socket, Subject::Session(seat.clone()), |data| {
        data["header"]["session_id"] == "6c2d3e4f-5061-4b72-8839-ad1e2f3a4b56"
            && data["header"]["seq"] == 1
    })
    .await;
    assert!(
        held,
        "a resumed seat's answer names the new occupant and the epoch's own first number",
    );
}

/// **The answer's copy covers every frame below the number it carries.**
///
/// The copy is filled by the transport's fold loop consuming the same stream
/// in a task of its own, so a frame minted before the answer's number is read
/// can reach the copy after it - and a client that adopts `seq - 1` then skips
/// the frame's live delivery as already drawn. A row lost in silence, on the
/// one path this cut exists for.
///
/// The fold is held to make the window deterministic rather than raced: the
/// frame below cannot reach the copy on its own, so only the answer's own
/// catch-up can put it there.
///
/// Two mutations die here, one per window:
/// - removing the catch-up leaves the row absent (the fold never took it);
/// - reading the answer's number AFTER the copy - the order this fixes -
///   catches up the copy but reads the turns before it, so the row is absent
///   again.
#[tokio::test]
async fn a_reconnect_answer_covers_every_frame_below_its_number() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;

    state.conversations.hold_the_fold(true);
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: a_said_frame("the frame the fold has not taken yet"),
    });

    // **The FIRST answer, not a poll.** Every subscribe tops the copy up, so a
    // second attempt would carry the row whatever the order was - the window
    // is only open for the answer that finds it.
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    state.conversations.hold_the_fold(false);

    let times = says_times(&data, "the frame the fold has not taken yet");
    assert_eq!(
        times, 1,
        "the row must cross exactly once: absent means the answer carries a number its copy \
         cannot cover and the client skips its live delivery, and twice means the catch-up \
         applied a frame the copy already had",
    );

    // **And the fold does not fold it again.** Releasing it lets it consume the
    // frame the catch-up already applied - which its own watermark must skip,
    // or the row lands in the copy a second time.
    state.conversations.hold_the_fold(false);
    for _ in 0..200 {
        if state.conversations.consumed_for(&seat) >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, after, _) = snapshot_answering(&mut socket).await;
    assert_eq!(
        says_times(&after, "the frame the fold has not taken yet"),
        1,
        "the fold applied a frame the catch-up had already put in the copy",
    );
}

/// **A caught-up copy is never refused.** A seat that has emitted more than a
/// window's worth of frames has a ring that shed its oldest, so the fallback's
/// coverage guard cannot reach back - and a watermark that never advanced
/// sends every such reconnect down that guard and refuses an ordinary long
/// seat. Here the fold is NOT held: the copy is caught up, so the answer is
/// simply the record.
#[tokio::test]
async fn a_caught_up_seat_answers_a_reconnect_with_a_shed_window() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    // One frame's worth of window, and more frames than it can hold. The seat
    // connects first, so the copy the fold fills is a real one - a seat with
    // no copy takes no frames at all, and this is about the copy being behind.
    forge_workspace::set_seat_ring_cap(&fleet.workspace(), 1);
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: SessionId::from("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45"),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: Vec::new(),
        dropped: 0,
        compaction_count: 0,
    });
    for at in 1..=3 {
        fleet.emit(SessionUpdate::ChatAppended {
            key: seat.clone(),
            origin: None,
            source: None,
            msg: a_said_frame(&format!("frame {at}")),
        });
    }
    for _ in 0..200 {
        if state.conversations.consumed_for(&seat) >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let message = next_server(&mut socket).await;

    let ServerMessage::Snapshot { data, .. } = message else {
        panic!("a caught-up copy is answered with the record, not refused: {message:?}")
    };
    for at in 1..=3 {
        assert_eq!(
            says_times(&data, &format!("frame {at}")),
            1,
            "the copy is the fold's own, and every frame of it crosses once",
        );
    }
}

/// **A copy no window can cover is refused, not answered with a hole.**
///
/// The fold is stuck and the seat's own window has shed the frames below the
/// number an answer would carry, so the only honest answer is none: the read
/// names the seat and the reason, and the client reads it as ask-again rather
/// than as a record with a row silently missing from it.
#[tokio::test]
async fn a_copy_no_window_can_cover_is_refused_rather_than_answered() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    // One frame's worth of window, so the stretch below cannot all be in it.
    forge_workspace::set_seat_ring_cap(&fleet.workspace(), 1);

    state.conversations.hold_the_fold(true);
    for at in 1..=2 {
        fleet.emit(SessionUpdate::ChatAppended {
            key: seat.clone(),
            origin: None,
            source: None,
            msg: a_said_frame(&format!("frame {at}")),
        });
    }

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let message = next_server(&mut socket).await;
    state.conversations.hold_the_fold(false);

    let ServerMessage::Error { what, why, .. } = message else {
        panic!("an answer whose copy cannot be covered was answered anyway: {message:?}")
    };
    assert_eq!(what, "subscribe", "the refusal names what was asked for");
    assert!(
        why.contains(&seat.display())
            && why.contains("has not settled")
            && why.contains("behind the number an answer would carry"),
        "and names the seat and this reason rather than the seal one, so a stuck fold is forge's \
         problem and not a row lost in silence: {why}",
    );
}

/// **A seal the watermark has jumped past still applies its history.**
///
/// A seal's number can equal the first frame of the run it opens - the fresh
/// seat mints 1, then counts from 1 again - so a fill that took that frame
/// leaves a watermark the seal's own number is not above. A skip-guard blind
/// to that drops the seal, and the reseed it carries - the only path the new
/// run's history has into the copy - never runs: the rows are gone for good.
#[tokio::test]
async fn a_seal_the_watermark_jumped_past_still_applies_its_history() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    let session = SessionId::from("9d4e5f60-6172-4a83-c940-a1b2c3d4e5f6");
    fleet.seed_view_facts(
        &seat,
        ViewFacts { session_id: Some(session.clone()), ..Default::default() },
    );
    // **This empty copy stands in for the occupant-less one a replay leaves**:
    // the occupant check takes it as the seat's, so the first answer fills it
    // from the window. A copy materialised from the ring would be refused
    // instead (#2446); the skip-guard this pins is the fold's own.
    state.conversations.insert(&seat, Conversation::empty());

    state.conversations.hold_the_fold(true);
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: session,
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the history row")],
        dropped: 0,
        compaction_count: 0,
    });
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: a_said_frame("the live row"),
    });

    // The first answer fills from the window: the live row lands and the
    // watermark is moved to its number - the seal's own.
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut socket).await;

    // **The fold's own mark, not a clock, says the copy is ready.** The first
    // answer's catch-up marked the live frame's number, and the seal re-mints
    // that same number, so a wait on it cannot tell the stale mark from the
    // one the reseed leaves behind - and a read landing between the reseed's
    // two applies sees the history without the live row. One more frame, past
    // the live row and carrying no row of its own, can only be marked by the
    // fold after it has taken both: the test issues no subscribe while it
    // waits, so an encode's catch-up cannot mark the number from the ring
    // alone and leave the unringed seal's reseed still pending.
    fleet.emit(SessionUpdate::TurnComplete { key: seat.clone(), terminal_reason: None });
    let settled = fleet.workspace().seat_next_seq(&seat).saturating_sub(1);
    state.conversations.hold_the_fold(false);
    assert!(
        state.conversations.await_consumed(&seat, settled, SEED_WAIT).await,
        "waited for the fold to take the seal and the live row, and it never marked them: the \
         watermark is {}",
        state.conversations.consumed_for(&seat),
    );

    // One answer, with the fold past both frames: the seal must have applied
    // despite the watermark, or the history it carries never reaches the copy.
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;

    assert_eq!(
        says_times(&data, "the history row"),
        1,
        "the seal's history must cross exactly once: absent means the skip-guard took the seal \
         and the new run's rows are gone from the record for good; the answer carried {}",
        serde_json::to_string(&data["conversation"]).unwrap_or_default(),
    );
    assert_eq!(says_times(&data, "the live row"), 1, "and the live row exactly once");
}

/// The window #2446 measured: a `Connected` carrying one history row, one
/// `ChatAppended`, the header already naming the new occupant, and the fold
/// held short of both.
fn a_seat_sealed_under_a_held_fold(fleet: &Fleet, state: &Arc<TransportState>, seat: &SessionSlot) {
    let session = SessionId::from("3b4c5d6e-7f80-4a91-b0c1-d2e3f4a5b6c7");
    // The header names the new occupant before the seal reaches the fold,
    // which is what makes a synthetic copy read as the seat's own.
    fleet.seed_view_facts(
        seat,
        ViewFacts { session_id: Some(session.clone()), ..Default::default() },
    );

    state.conversations.hold_the_fold(true);
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: session.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the seal's history row")],
        dropped: 0,
        compaction_count: 0,
    });
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: a_said_frame("the live row"),
    });
}

/// The next answer to a subscribe, passing over the updates queued ahead of it
/// the way [`snapshot_answering`] does.
async fn an_answer(socket: &mut Client) -> ServerMessage {
    loop {
        match next_server(socket).await {
            ServerMessage::Update { .. } => {}
            other => return other,
        }
    }
}

/// **A fill that cannot reach the seal's history is refused, not synthesised**
/// (#2446).
///
/// A seal is not in the ring, so catch_up's fallback built a copy from the
/// ring's frames alone and the answer paired the new epoch's number with the
/// live row and no history. The occupant check cannot catch it: the fill
/// stamps the synthetic copy with the header's occupant, so it reads as the
/// seat's own.
///
/// **A fold that never applies the seal has no answer to give**, which is what
/// this pins: the read waits out the seal's own bound and refuses by name. The
/// transient window is `a_read_during_a_lagging_seal_answers_once_the_fold_catches_up`.
#[tokio::test]
async fn a_fill_through_a_pending_seal_is_refused_rather_than_synthesised() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    a_seat_sealed_under_a_held_fold(&fleet, &state, &seat);

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let answer = an_answer(&mut socket).await;
    state.conversations.hold_the_fold(false);

    let ServerMessage::Error { what, why, .. } = answer else {
        panic!(
            "an answer whose copy never had the seal's history was served rather than refused: \
             {answer:?}"
        );
    };
    assert_eq!(what, "subscribe", "the refusal names what was asked for");
    assert!(
        why.contains(&seat.display())
            && why.contains("has not settled")
            && why.contains("epoch's seal"),
        "and names the seat and this reason rather than the copy-behind one, so the silent row \
         loss has an artefact: {why}",
    );

    // **The same read after the fold has taken the seal answers its history**,
    // which is what the refusal was protecting.
    let settled = fleet.workspace().seat_next_seq(&seat).saturating_sub(1);
    assert!(
        state.conversations.await_consumed(&seat, settled, SEED_WAIT).await,
        "the fold never took the seal and the live row: the watermark is {}",
        state.conversations.consumed_for(&seat),
    );
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;

    assert_eq!(
        says_times(&data, "the seal's history row"),
        1,
        "the seal's history must be in the answer once the fold has applied it: {data}",
    );
    assert_eq!(says_times(&data, "the live row"), 1, "and the live row exactly once: {data}");
}

/// **An empty-history seal still fills from the ring** (#2446).
///
/// A fresh session's first connect carries no rows, so the ring holds
/// everything its epoch has said and the fill below is a complete copy. Only a
/// seal that carried rows is worth refusing for; recording every seal would
/// turn an ordinary first connect into a wait and, on a wedged fold, refuse an
/// answer the ring could give.
#[tokio::test]
async fn an_empty_history_seal_still_fills_from_the_ring() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    let session = SessionId::from("4c5d6e7f-8091-4ba2-c1d2-e3f4a5b6c7d8");
    fleet.seed_view_facts(
        &seat,
        ViewFacts { session_id: Some(session.clone()), ..Default::default() },
    );

    state.conversations.hold_the_fold(true);
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: session,
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: Vec::new(),
        dropped: 0,
        compaction_count: 0,
    });
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: a_said_frame("the live row"),
    });

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let answer = an_answer(&mut socket).await;
    state.conversations.hold_the_fold(false);

    let ServerMessage::Snapshot { data, .. } = answer else {
        panic!(
            "an empty-history seal leaves nothing for the ring to miss, so the read must answer: \
             {answer:?}"
        );
    };
    let said = says_times(&data, "the live row");
    assert_eq!(said, 1, "the ring's frame is the whole epoch: {data}");
}

/// **A later empty-history epoch does not inherit the previous seal's mark**
/// (#2446).
///
/// The mark describes the latest epoch-opening seal. Once a seat has opened
/// one with rows, a `/new` or a reconnect that starts blank must not leave the
/// mark standing: its seal cleared the ring at the mint, so a fill holds the
/// whole new epoch and there is nothing to wait for or refuse.
#[tokio::test]
async fn an_empty_history_epoch_after_a_sealed_one_still_fills_from_the_ring() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    let first = SessionId::from("5d6e7f80-91a2-4bc3-d2e3-f4a5b6c7d8e9");

    // The first epoch genuinely opens on rows, and the fold applies its seal.
    // Its mark is never taken - a seal is not a frame - so the epoch's first
    // number is still owed when the read below runs.
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: first.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the first run's history row")],
        dropped: 0,
        compaction_count: 0,
    });
    let mut seeded = false;
    for _ in 0..200 {
        if state.conversations.get(&seat).is_some_and(|held| held.holds_occupant(Some(&first))) {
            seeded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(seeded, "the first epoch's seal never reached the copy");

    // The second epoch opens without history, with the fold held at its seal.
    let second = SessionId::from("6e7f8091-a2b3-4cd4-e3f4-a5b6c7d8e9f0");
    state.conversations.hold_the_fold(true);
    fleet.seed_view_facts(
        &seat,
        ViewFacts { session_id: Some(second.clone()), ..Default::default() },
    );
    fleet.emit(SessionUpdate::SessionReplaced {
        key: seat.clone(),
        session_id: second,
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: Vec::new(),
        dropped: 0,
        compaction_count: 0,
    });
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: a_said_frame("the new run's row"),
    });

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let answer = an_answer(&mut socket).await;
    state.conversations.hold_the_fold(false);

    let ServerMessage::Snapshot { data, .. } = answer else {
        panic!(
            "a blank epoch leaves nothing for the ring to miss, so the read must answer: {answer:?}"
        );
    };
    let said = says_times(&data, "the new run's row");
    assert_eq!(said, 1, "the new epoch's frame is the whole epoch: {data}");
    let previous = says_times(&data, "the first run's history row");
    assert_eq!(previous, 0, "and none of the previous run's rows: {data}");
}

/// **A read inside a LAGGING seal's window answers once the fold catches up,
/// without the client leaving the seat** (#2446).
///
/// The same window as the refusal above, with the fold released before the
/// seal's bound runs out. A refused subscribe cannot be re-asked by either
/// client, so the transient must resolve inside the read: the wait is on the
/// fold's own mark, and this pins that the answer carries the history the
/// refused one could not have had.
///
/// The release lands between the encode's 250 ms catch-up wait and the end of
/// its 1 s seal wait, so a read that refused instead of waiting has already
/// answered with an error by then.
#[tokio::test]
async fn a_read_during_a_lagging_seal_answers_once_the_fold_catches_up() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    a_seat_sealed_under_a_held_fold(&fleet, &state, &seat);

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    state.conversations.hold_the_fold(false);

    let answer = an_answer(&mut socket).await;
    let ServerMessage::Snapshot { data, .. } = answer else {
        panic!(
            "a read through a lagging seal must answer once the fold catches up, not fail the \
             seat's only read: {answer:?}"
        );
    };
    assert_eq!(
        says_times(&data, "the seal's history row"),
        1,
        "the answer carries the seal's history: {data}",
    );
    assert_eq!(says_times(&data, "the live row"), 1, "and the live row exactly once: {data}");
    let seq = data["header"]["seq"].as_u64();
    assert_eq!(seq, Some(2), "under the new epoch's own number: {data}");
}

/// **An answer inside the seal's window is the new epoch's, not the previous
/// run's.**
///
/// A seal is minted by the fanout and applied by the fold, and the counter
/// restarts at the mint while the copy reseeds at the apply. A read landing
/// between the two pairs the restarted count with the previous run's rows
/// unless the copy carries its occupant - and one that is not the seat's is
/// no copy at all. The read waits its bounded window for the reseed and,
/// never given one, answers with no conversation rather than the previous
/// run's.
#[tokio::test]
async fn an_answer_inside_the_seal_window_is_not_the_previous_runs_conversation() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    let old = SessionId::from("1f2e3d4c-5b6a-4978-8801-1234567890ab");
    let new = SessionId::from("2a3b4c5d-6e7f-4809-9a1b-234567890abc");

    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(old.clone()), ..Default::default() });
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: old,
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the old run's row")],
        dropped: 0,
        compaction_count: 0,
    });
    for _ in 0..200 {
        if state.conversations.consumed_for(&seat) >= 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // The seal is minted but held short of the fold, and the header already
    // names the new occupant - the window the read must not be fooled in.
    state.conversations.hold_the_fold(true);
    fleet.emit(SessionUpdate::SessionReplaced {
        key: seat.clone(),
        session_id: new.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: Vec::new(),
        dropped: 0,
        compaction_count: 0,
    });
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(new.clone()), ..Default::default() });

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    state.conversations.hold_the_fold(false);

    assert_eq!(
        data["header"]["session_id"], "2a3b4c5d-6e7f-4809-9a1b-234567890abc",
        "the answer names the new occupant",
    );
    assert_eq!(
        says_times(&data, "the old run's row"),
        0,
        "an answer must never carry the previous run's rows under the new epoch's count",
    );
}

/// **A copy of another occupant is waited for, and the reseed that lands in
/// time is the answer.** The transcript the seat holds is the live one: it
/// carries frames a transcript never does, so a header that has moved on is
/// the fold's to reconcile with a reseed, never the file's to replace.
#[tokio::test]
async fn a_mismatched_copy_is_answered_by_the_reseed_that_lands_in_time() {
    let (_url, fleet, state) = a_server_with_state().await;
    wait_for_the_fold(&fleet).await;
    let seat = lead_seat();
    let old = SessionId::from("1f2e3d4c-5b6a-4978-8801-1234567890ab");
    let new = SessionId::from("2a3b4c5d-6e7f-4809-9a1b-234567890abc");

    // The previous run's copy, folded under its own header.
    let notice = state.conversations.seeded_notice();
    tokio::pin!(notice);
    notice.as_mut().enable();
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(old.clone()), ..Default::default() });
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: old.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the old run's row")],
        dropped: 0,
        compaction_count: 0,
    });
    tokio::time::timeout(SEED_WAIT, notice.as_mut())
        .await
        .expect("the previous run's copy is held");

    // The header moves on with the seal still on its way to the fold.
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(new.clone()), ..Default::default() });

    let mut timings = OpenTimings::default();
    let mut read = Box::pin(wire::conversation_for(&state, &seat, &mut timings));
    assert!(
        futures_util::poll!(read.as_mut()).is_pending(),
        "a copy of another occupant is waited for, not answered with",
    );
    // The poll above parked the read inside its wait, with nothing left to
    // wake it but the fold's reseed.
    assert!(
        futures_util::poll!(read.as_mut()).is_pending(),
        "and still waiting rather than answering on a re-poll",
    );

    // The fold applies the seal it was holding; the reseed lands on the copy
    // the reader is waiting on, in place.
    state.conversations.apply(
        &SessionUpdate::SessionReplaced {
            key: seat.clone(),
            session_id: new.clone(),
            cwd: "/tmp".to_owned(),
            current_model: a_current_model(),
            available_models: Vec::new(),
            mode: None,
            history: vec![a_said_frame("the new run's row")],
            dropped: 0,
            compaction_count: 0,
        },
        None,
    );

    let held = match futures_util::poll!(read.as_mut()) {
        std::task::Poll::Ready(answered) => answered,
        std::task::Poll::Pending => panic!("the reseed wakes the waiting read, and it answers"),
    }
    .expect("the reseeded copy is answered");
    assert!(held.holds_occupant(Some(&new)), "the answer is the copy the header names");
    let rows = held
        .read(|conversation| serde_json::to_string(conversation.messages()).unwrap_or_default());
    assert!(rows.contains("the new run's row"), "the reseed's rows are answered: {rows}");
    assert!(
        !rows.contains("the old run's row"),
        "and the previous run's rows went with the seal: {rows}",
    );
}

/// **A mismatched copy that never reseeds is answered over, not served.** No
/// transcript replaces it either: the read waits its bounded window, and a
/// copy that never turns current leaves the request answered without a
/// conversation - the refusal a client already retries - rather than with
/// another run's rows under this epoch's count.
#[tokio::test]
async fn a_mismatched_copy_that_never_reseeds_is_not_served() {
    let (_url, fleet, state) = a_server_with_state().await;
    wait_for_the_fold(&fleet).await;
    let seat = lead_seat();
    let old = SessionId::from("1f2e3d4c-5b6a-4978-8801-1234567890ab");
    let new = SessionId::from("2a3b4c5d-6e7f-4809-9a1b-234567890abc");

    let notice = state.conversations.seeded_notice();
    tokio::pin!(notice);
    notice.as_mut().enable();
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(old.clone()), ..Default::default() });
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: old.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the old run's row")],
        dropped: 0,
        compaction_count: 0,
    });
    tokio::time::timeout(SEED_WAIT, notice.as_mut())
        .await
        .expect("the previous run's copy is held");
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(new), ..Default::default() });

    let mut timings = OpenTimings::default();
    let answered =
        tokio::time::timeout(SEED_WAIT, wire::conversation_for(&state, &seat, &mut timings))
            .await
            .expect("the wait is bounded");
    assert!(answered.is_none(), "a copy that never turns current is not answered");
    assert!(
        state.conversations.get(&seat).is_some_and(|held| held.holds_occupant(Some(&old))),
        "and the copy was left as it stood rather than replaced",
    );
}

/// **A copy the fold inserts while the transcript is being read is the one
/// answered.** A transcript is a row subset - it carries no `Result` frames -
/// so a seed that landed over the stream's copy would drop frames the fold
/// already applied. The seed is parked before its read, the seat's `Connected`
/// lands while it is held there, and the answer is the stream's copy rather
/// than the file's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connect_during_the_transcript_read_is_the_copy_answered() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    fleet
        .seed_asleep_transcript(
            "TestOrg",
            "proj",
            "lead",
            &[r#"{"type":"user","uuid":"u-file","message":{"role":"user","content":"the file's row"}}"#],
        )
        .expect("the transcript seeds");
    let hold = Arc::new(forge_server::transport::conversation::SeedHold::default());
    state.conversations.hold_the_seed(Arc::clone(&hold));

    // The subscribe starts the transcript seed, and the seed parks before its
    // read: the seat is known to hold no copy, and the file has not been read.
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(10), hold.entered.notified())
        .await
        .expect("the seed reaches its hold");

    // The seat's own stream lands inside that window. The notice is enabled
    // before the emit, so the copy's arrival is the signal rather than a
    // polled clock.
    let new = SessionId::from("2a3b4c5d-6e7f-4809-9a1b-234567890abc");
    let landed = state.conversations.seeded_notice();
    tokio::pin!(landed);
    landed.as_mut().enable();
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(new.clone()), ..Default::default() });
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: new.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the live row")],
        dropped: 0,
        compaction_count: 0,
    });
    tokio::time::timeout(SEED_WAIT, landed.as_mut())
        .await
        .expect("the fold's copy lands while the seed is held");
    assert!(
        state.conversations.get(&seat).is_some_and(|held| held.holds_occupant(Some(&new))),
        "the copy the seat holds belongs to the run that just connected",
    );

    // The seed reads the file and inserts now: it must keep the copy the
    // stream gave it rather than overwrite it with the file's rows.
    hold.release.add_permits(1);

    let (_, data, _) = snapshot_answering(&mut socket).await;
    assert!(
        says_times(&data, "the live row") >= 1,
        "the stream's own frame is in the answer: {data}",
    );
    assert_eq!(
        says_times(&data, "the file's row"),
        0,
        "and the transcript's row subset never replaced the copy: {data}",
    );
    assert!(
        state.conversations.get(&seat).is_some_and(|held| held.holds_occupant(Some(&new))),
        "the held copy is still the stream's",
    );
}

/// **An answer whose copy is reseeded under it carries the copy's own number,
/// not the pre-seal one** (#2443).
///
/// catch_up can pass on the watermark the previous epoch left, and the fold
/// can reseed the copy before the answer reads it. The answer then pairs a
/// pre-seal number with the new epoch's rows, and a client adopting that
/// number skips the new epoch's first frames as already drawn - rows lost
/// rather than re-asked.
///
/// The reseed below is placed with the fold's own `apply`, carrying the
/// pre-seal watermark still standing: that is the window between the reseed
/// and the mark the fold leaves after it, held still rather than raced.
#[tokio::test]
async fn an_answer_through_a_reseed_carries_the_copys_own_number() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    let old = SessionId::from("1f2e3d4c-5b6a-4978-8801-1234567890ab");
    let new = SessionId::from("2a3b4c5d-6e7f-4809-9a1b-234567890abc");

    // The old epoch, applied: a watermark above zero is what catch_up can pass
    // on after the seal has moved the copy to the next one.
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(old.clone()), ..Default::default() });
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: old,
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the old run's history row")],
        dropped: 0,
        compaction_count: 0,
    });
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: a_said_frame("the old run's frame"),
    });
    let settled = fleet.workspace().seat_next_seq(&seat).saturating_sub(1);
    assert!(
        state.conversations.await_consumed(&seat, settled, SEED_WAIT).await,
        "the old epoch's frame never reached the copy: the watermark is {}",
        state.conversations.consumed_for(&seat),
    );

    // **The header moves before the fanout mints the seal.** Production has
    // the same gap - the domain records the occupant, the emit follows - and
    // it is what makes the answer read the number of the epoch before.
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(new.clone()), ..Default::default() });
    // **And the fold's reseed lands before the answer's copy read**, with the
    // watermark reset the fold leaves after it still pending: catch_up's check
    // sees the old epoch's number against a copy that is already the new one.
    // The number is the one the fanout would mint the seal under.
    let minted = fleet.workspace().seat_next_seq(&seat);
    assert!(
        minted > 1,
        "the window needs a pre-seal number above the new epoch's first, or the test cannot \
         tell the clamped answer from an unclamped one: {minted}",
    );
    state.conversations.apply(
        &SessionUpdate::Connected {
            key: seat.clone(),
            session_id: new,
            cwd: "/tmp".to_owned(),
            current_model: a_current_model(),
            available_models: Vec::new(),
            mode: None,
            history: vec![a_said_frame("the new run's history row")],
            dropped: 0,
            compaction_count: 0,
        },
        Some(minted),
    );

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;

    assert_eq!(
        data["header"]["seq"].as_u64(),
        Some(1),
        "the answer's number is the one its copy is current through - the new epoch's first \
         frame - and not the pre-seal number a client adopts to skip that frame: {data}",
    );
    assert_eq!(
        says_times(&data, "the new run's history row"),
        1,
        "and the reseeded copy is what it carries: {data}",
    );
}

/// **A seal the fold has not applied must not be answered with the previous
/// run's transcript copy** (#2256).
///
/// A page opening on a sleeping seat seeds its copy from the transcript on
/// disk, and that copy carries no occupant - so when the seat is replaced
/// before the fold applies the seal, the copy still answers: the new epoch's
/// count beside the replaced run's rows, until the apply reseeds it.
#[tokio::test]
async fn a_seal_over_a_transcripts_copy_is_not_answered_with_the_previous_runs_rows() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    let rows: Vec<String> = (0..20).map(a_turns_rows).collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet
        .seed_asleep_transcript("TestOrg", "proj", "lead", &borrowed)
        .expect("the transcript seeds");
    // First read: the sleeping seat's snapshot seeds the copy from its own
    // transcript, occupant and all.
    let mut socket = connect(&url).await;
    let seeded = snapshot_until(&mut socket, Subject::Session(lead_seat()), |data| {
        data["conversation"]["turns"].as_array().is_some_and(|turns| {
            turns.len() == forge_server::transport::wire::SUBSCRIBE_TURNS as usize
        })
    })
    .await;
    assert!(
        seeded,
        "the sleeping seat's transcript must seed the copy before the seal, or the pin \
         cannot tell a refused copy from one that never existed",
    );

    // The seat is sealed to a new run with the fold held at the seal.
    let new = SessionId::from("7d3e4f50-6172-4c83-9940-be2f3a4b5c67");
    state.conversations.hold_the_fold(true);
    fleet.emit(SessionUpdate::SessionReplaced {
        key: seat.clone(),
        session_id: new.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: Vec::new(),
        dropped: 0,
        compaction_count: 0,
    });
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(new), ..Default::default() });

    let mut socket2 = connect(&url).await;
    send(
        &mut socket2,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket2).await;
    // **The seat's stream can move inside this window.** The first read above
    // started the seat's watch, and `seed_view_facts` above gives the seat the
    // domain that pass seeds from: a pass that runs after it announces
    // `WorkChanged` and `FileIndexChanged` under the NEW epoch's numbers,
    // because the seal restarted the counter. So the answer's number is the
    // copy's own and not a constant 1 - what it must be is this epoch's, and
    // the rows below must not be the replaced run's. (The copy's-own-number
    // rule itself is pinned, with a watermark that can be told apart,
    // in `an_answer_through_a_reseed_carries_the_copys_own_number`.)
    let next = fleet.workspace().seat_next_seq(&seat);
    state.conversations.hold_the_fold(false);

    let seq = data["header"]["seq"].as_u64().expect("the answer numbers the seat's stream");
    assert!(
        (1..=next).contains(&seq),
        "the answer numbers the new epoch's stream, at most the number it will mint next \
         ({seq} against 1..={next}): {data}",
    );
    assert_eq!(
        says_times(&data, "reply "),
        0,
        "the answer carries the replaced run's transcript rows under the new epoch's count: {data}",
    );
    assert_eq!(
        data["conversation"]["turns"].as_array().map_or(0, Vec::len),
        0,
        "and the new run has written nothing yet: {data}",
    );
}

/// **The socket answers a call's output read from the transport's own copy.**
///
/// The client asks by the call's id alone; the frame that named the file is a
/// `ChatAppended` row the fold has applied, so the held conversation is where
/// the read resolves it. A call the copy never named and a file that has gone
/// since are each answered with their own reason rather than an empty tail.
#[tokio::test]
async fn a_call_output_read_is_answered_from_the_transports_held_copy() {
    let (url, fleet, state) = a_server_with_state().await;
    wait_for_the_fold(&fleet).await;
    let seat = lead_seat();
    // The copy a live seat holds: the fold's own, which `ChatAppended` appends
    // to only where one exists.
    state
        .conversations
        .insert(&seat, forge_server::transport::conversation::Conversation::new(Vec::new(), 0, 0));
    let path =
        std::env::temp_dir().join(format!("forge-call-output-socket-{}.log", std::process::id()));
    std::fs::write(&path, "alpha\nbeta\n").expect("write the output file");
    let msg: forge_primitives::Message = serde_json::from_value(serde_json::json!({
        "type": "system",
        "subtype": "task_notification",
        "task_id": "t-1",
        "tool_use_id": "tu-1",
        "status": "completed",
        "output_file": path.display().to_string(),
        "summary": "done",
        "session_id": "sess-1",
        "uuid": "u-1",
    }))
    .expect("a task notification frame");
    state.conversations.apply(
        &SessionUpdate::ChatAppended { key: seat.clone(), msg, origin: None, source: None },
        Some(1),
    );

    let mut socket = connect(&url).await;
    let ask = |call_id: &str| ClientMessage::Command {
        command: Box::new(Command::ReadCallOutput {
            key: lead_seat(),
            call_id: call_id.to_owned(),
        }),
        reply_to: None,
    };

    send(&mut socket, ask("tu-1")).await;
    let read = update_until(&mut socket, "the call's output", |update| {
        matches!(update, SessionUpdate::CallOutput { .. })
    })
    .await;
    let SessionUpdate::CallOutput { key, call_id, output } = read else {
        unreachable!("the update was matched on calling it")
    };
    assert_eq!(key, seat, "the answer names the seat that was asked about");
    assert_eq!(call_id, "tu-1", "and the call");
    assert_eq!(
        output,
        forge_primitives::CallOutput::Lines(vec!["alpha".to_owned(), "beta".to_owned()]),
        "the file the copy's own frame named is the one read",
    );

    send(&mut socket, ask("tu-unknown")).await;
    let read = update_until(&mut socket, "the nameless call", |update| {
        matches!(update, SessionUpdate::CallOutput { .. })
    })
    .await;
    assert!(
        matches!(
            read,
            SessionUpdate::CallOutput { output: forge_primitives::CallOutput::NoPath, .. }
        ),
        "a call the copy does not name is the no-path reason: {read:?}",
    );

    std::fs::remove_file(&path).expect("remove the output file");
    send(&mut socket, ask("tu-1")).await;
    let read = update_until(&mut socket, "the gone file", |update| {
        matches!(update, SessionUpdate::CallOutput { .. })
    })
    .await;
    assert!(
        matches!(
            read,
            SessionUpdate::CallOutput { output: forge_primitives::CallOutput::FileGone, .. }
        ),
        "a named file that has gone is its own reason: {read:?}",
    );
}

/// **A read inside the seal's window answers the new epoch's own conversation,
/// and never the rows the previous run left.**
///
/// The seat has NO copy yet - its fold has never applied a frame for it - so
/// the read reaches the transcript the previous run left, which an id source
/// still names. A seat whose header names an occupant is running, and another
/// run's transcript is not its history.
#[tokio::test]
async fn a_read_in_the_seal_window_does_not_answer_the_previous_runs_transcript() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    let new = SessionId::from("2a3b4c5d-6e7f-4809-9a1b-234567890abc");

    // The previous run's rows, where a read with no copy to serve reaches
    // them: its transcript, named by the seat's own session id.
    fleet
        .seed_transcript(
            "TestOrg",
            "proj",
            "lead",
            &[
                r#"{"type":"user","uuid":"u1","message":{"role":"user","content":"the previous run's row"}}"#,
                r#"{"type":"assistant","uuid":"a1","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"the previous run's reply"}]}}"#,
                r#"{"type":"result","uuid":"r1","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}"#,
            ],
        )
        .expect("the transcript seeds");
    // Two frames of the old run, so the number the seal restarts is a real
    // restart and the answer's 1 is the new epoch's own.
    for _ in 0..2 {
        fleet.emit(SessionUpdate::TurnComplete { key: seat.clone(), terminal_reason: None });
    }

    // The seal is minted while the fold is held, and the header already names
    // the new occupant - the mint-to-apply window a read lands in.
    state.conversations.hold_the_fold(true);
    fleet.emit(SessionUpdate::SessionReplaced {
        key: seat.clone(),
        session_id: new.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: Vec::new(),
        dropped: 0,
        compaction_count: 0,
    });
    fleet.seed_view_facts(&seat, ViewFacts { session_id: Some(new.clone()), ..Default::default() });

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    state.conversations.hold_the_fold(false);

    assert_eq!(
        data["header"]["session_id"], "2a3b4c5d-6e7f-4809-9a1b-234567890abc",
        "the answer names the new occupant",
    );
    assert_eq!(
        data["header"]["seq"], 1,
        "under the epoch's own first number, restarted at the seal"
    );
    assert_eq!(
        says_times(&data, "the previous run's row"),
        0,
        "the answer must be the new epoch's own conversation, and it holds NOTHING yet: the \
         previous run's transcript is not this run's rows, so a read in the seal's window must \
         not pair the restarted count with them",
    );
}

/// **A range ask after an occupant change reaches the new epoch's frames.**
///
/// The frame that OPENS an epoch is numbered on the run it ends and is not
/// ringed, so the new run's window starts at 1 - and the first thing a client
/// that missed the change asks for is frame 1. A ring that kept the opening
/// frame reports a front of 2, and the ask falls through to a transcript that
/// holds nothing of the new run: the repair is dead for the whole epoch.
#[tokio::test]
async fn a_range_ask_after_an_occupant_change_reaches_the_new_epoch() {
    let (url, fleet, _state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    fleet.emit(SessionUpdate::TurnComplete { key: seat.clone(), terminal_reason: None });
    fleet.emit(SessionUpdate::SessionReplaced {
        key: seat.clone(),
        session_id: SessionId::from("6c2d3e4f-5061-4b72-8839-ad1e2f3a4b56"),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: Vec::new(),
        dropped: 0,
        compaction_count: 0,
    });
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: a_said_frame("the new epoch's first"),
    });

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::SeatRange { slot: seat.clone(), epoch: None, from: 1, to: 1 })
        .await;

    let ServerMessage::SeatFrames { frames, .. } = next_server(&mut socket).await else {
        panic!("a range ask is answered with the frames it asked for")
    };
    assert_eq!(frames.len(), 1, "the new epoch's first frame is the one frame the range holds");
    assert_eq!(frames[0].seq, 1, "numbered where the new run starts");
    let body = serde_json::to_string(&frames[0].update).unwrap_or_default();
    assert!(body.contains("the new epoch's first"), "and it is that frame: {body}");
}

/// **A repair never hands a seal's window back.** A `Connected` is numbered
/// but not ringed: its window is the seed for the transport's copy and a
/// client reads a fresh one on re-open, so the ring keeps nothing of it and a
/// range ask over its number falls to the transcript. What crosses is the
/// transcript's own rows, never the window the epoch handed over (#2157).
#[tokio::test]
async fn a_connect_is_never_handed_back_by_a_repair() {
    let (url, fleet, _state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: SessionId::from("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45"),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: vec![a_said_frame("the seal's words")],
        dropped: 0,
        compaction_count: 0,
    });

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::SeatRange { slot: seat.clone(), epoch: None, from: 1, to: 1 })
        .await;

    let ServerMessage::SeatFrames { frames, .. } = next_server(&mut socket).await else {
        panic!("a range ask is answered with the frames it asked for")
    };
    assert!(
        frames.iter().all(|frame| !matches!(&frame.update, SessionUpdate::Connected { .. })),
        "a repair handed the seal's window back: {frames:?}"
    );
    let body = serde_json::to_string(&frames).unwrap_or_default();
    assert!(!body.contains("the seal's words"), "the history crossed on a repair: {body}");
}

/// **A live frame carries the same bound a page does** (#2374): an over-cap
/// record is capped on its way to a subscribed client, so the row drawn live
/// and the row drawn after a switch are the same marker.
#[tokio::test]
async fn a_live_frame_caps_an_oversized_tool_record() {
    let (url, fleet) = a_server().await;
    wait_for_the_fold(&fleet).await;
    let (mut socket, _) = a_page_on(&url, Subject::Session(lead_seat())).await;

    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        origin: None,
        source: None,
        msg: a_recorded_frame(&serde_json::json!({"payload": "x".repeat(100_000)})),
    });

    let update = update_until(&mut socket, "the capped live frame", |update| {
        matches!(
            update,
            SessionUpdate::ChatAppended {
                msg: forge_primitives::Message::User { tool_use_result: Some(_), .. },
                ..
            }
        )
    })
    .await;
    let SessionUpdate::ChatAppended { msg, .. } = update else {
        panic!("the update is the frame that was emitted")
    };
    let forge_primitives::Message::User { tool_use_result, .. } = msg else {
        panic!("the frame is the user turn that carries the record")
    };
    let record = tool_use_result.expect("the frame carries its record");
    assert_eq!(record["omitted"], true, "the live record is marked: {record}");
    assert!(
        record["bytes"].as_u64().is_some_and(|bytes| bytes > 100_000),
        "and says how much it weighed: {record}",
    );
}

/// A range ask is answered from the window the seat already keeps: the frames
/// asked for, each carrying the number it was minted under, so a client that
/// missed a stretch can fold exactly those and carry on.
#[tokio::test]
async fn a_range_ask_inside_the_window_answers_exactly_the_asked_frames() {
    let (url, fleet, _state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    for at in 1..=3 {
        fleet.emit(SessionUpdate::ChatAppended {
            key: seat.clone(),
            origin: None,
            source: None,
            msg: a_said_frame(&format!("frame {at}")),
        });
    }

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::SeatRange { slot: seat.clone(), epoch: None, from: 2, to: 3 })
        .await;

    let ServerMessage::SeatFrames { slot, epoch, frames } = next_server(&mut socket).await else {
        panic!("a range ask is answered with the frames it asked for")
    };
    assert_eq!(slot, seat, "the answer names the seat it was asked about");
    assert_eq!(epoch, None, "and the epoch the numbers belong to");
    assert_eq!(
        frames.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
        vec![2, 3],
        "exactly the asked range, in order",
    );
    let asked = serde_json::to_string(&frames[0].update).unwrap_or_default();
    assert!(asked.contains("frame 2"), "with each frame's own content: {asked}");
}

/// **A ring repair caps the record it replays** (#2374): a `SeatRange` inside
/// the window goes through the same client-facing bound as a live frame, so a
/// repaired row and the one that streamed draw the same marker.
#[tokio::test]
async fn a_ring_repair_caps_an_oversized_tool_record() {
    let (url, fleet, _state) = a_server_with_state().await;
    let seat = lead_seat();
    wait_for_the_fold(&fleet).await;
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: a_recorded_frame(&serde_json::json!({"payload": "x".repeat(100_000)})),
    });

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::SeatRange { slot: seat, epoch: None, from: 1, to: 1 }).await;

    let ServerMessage::SeatFrames { frames, .. } = next_server(&mut socket).await else {
        panic!("a range ask is answered with the frames it asked for")
    };
    let SessionUpdate::ChatAppended { msg, .. } = &frames[0].update else {
        panic!("the repaired frame is a chat frame")
    };
    let forge_primitives::Message::User { tool_use_result: Some(record), .. } = msg else {
        panic!("the frame carries the record that was emitted")
    };
    assert_eq!(record["omitted"], true, "the ring repair caps the record: {record}");
}

/// **A range older than the window falls back to the transcript, and the
/// frames still carry the range's own numbers.** Re-numbering them from one
/// would leave the client's sequence looking gapped forever, and dropping them
/// would leave rows nobody can ask for again.
#[tokio::test]
async fn a_range_older_than_the_window_falls_back_to_the_transcript() {
    let (url, fleet, _state) = a_server_with_state().await;
    let seat = lead_seat();
    let rows: Vec<String> = (1..=4)
        .map(|at| {
            // The transcript repair serves a record like every other path,
            // so one row carries an oversized one (#2374).
            let record = if at == 1 {
                format!(",\"toolUseResult\":{{\"payload\":\"{}\"}}", "x".repeat(100_000))
            } else {
                String::new()
            };
            format!(
                "{{\"type\":\"user\",\"uuid\":\"u{at}\",\"session_id\":\"s\",\
                 \"message\":{{\"role\":\"user\",\"content\":\"row {at}\"}}{record}}}"
            )
        })
        .collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");
    // The window has already shed all but its newest frame, which is what
    // makes the ask below reach past it.
    forge_workspace::set_seat_ring_cap(&fleet.workspace(), 1);
    wait_for_the_fold(&fleet).await;
    for at in 1..=4 {
        fleet.emit(SessionUpdate::ChatAppended {
            key: seat.clone(),
            origin: None,
            source: None,
            msg: a_said_frame(&format!("frame {at}")),
        });
    }

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::SeatRange { slot: seat.clone(), epoch: None, from: 1, to: 4 })
        .await;

    let ServerMessage::SeatFrames { frames, .. } = next_server(&mut socket).await else {
        panic!("a range older than the window is answered from the transcript")
    };
    assert_eq!(
        frames.iter().map(|frame| frame.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "the repaired frames carry the range's own numbers rather than restarting at one",
    );
    let texts: Vec<String> = frames
        .iter()
        .map(|frame| serde_json::to_string(&frame.update).unwrap_or_default())
        .collect();
    assert!(
        texts[0].contains("row 1") && texts[3].contains("row 4"),
        "and their content is the transcript's own rows, not the frames the window shed: {texts:?}",
    );
    let SessionUpdate::ChatAppended { msg, .. } = &frames[0].update else {
        panic!("a repaired frame is a chat frame")
    };
    let forge_primitives::Message::User { tool_use_result: Some(record), .. } = msg else {
        panic!("the first row carries the record the cap was given")
    };
    assert_eq!(record["omitted"], true, "the transcript repair caps the record: {record}");
}

/// **A range ask naming an epoch the seat has left is refused.** Those numbers
/// belong to an occupant that is gone, and repairing across the two would
/// splice one conversation onto another - so the client is told to read the
/// seat again rather than handed frames that look like an answer.
#[tokio::test]
async fn a_range_ask_from_another_epoch_is_refused() {
    let (url, fleet, _state) = a_server_with_state().await;
    let seat = lead_seat();
    fleet.seed_view_facts(
        &seat,
        ViewFacts {
            session_id: Some(SessionId::from("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45")),
            ..Default::default()
        },
    );

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::SeatRange {
            slot: seat.clone(),
            epoch: Some(SessionId::from("6c2d3e4f-5061-4b72-8839-ad1e2f3a4b56")),
            from: 1,
            to: 2,
        },
    )
    .await;

    let ServerMessage::Error { what, why, seat: named, .. } = next_server(&mut socket).await else {
        panic!("a range ask from another epoch is refused rather than answered")
    };
    assert_eq!(what, "seat_range", "the refusal names what was asked for");
    assert_eq!(named, Some(seat), "and the seat it belongs to");
    assert!(
        why.contains("occupant"),
        "and why, so a client knows to read the seat again rather than retry: {why}",
    );
}

/// A page that cannot be answered is REFUSED, because an empty one lies.
///
/// **An empty page carries `cursor: null`, and a client reads that as "nothing
/// above" and stops asking** - so answering a question the server could not
/// answer with a value that looks like the answer makes a seat's history
/// unreachable rather than merely late.
#[tokio::test]
async fn a_more_that_cannot_be_answered_is_refused() {
    let (url, fleet) = a_server().await;
    // A seat with a session, so the refusal below is about the CONVERSATION
    // and not about the seat being absent.
    fleet.seed_transcript("TestOrg", "proj", "lead", &[]).expect("the transcript seeds");

    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 5 })
        .await;

    let ServerMessage::Error { what, why, seat, .. } = next_server(&mut socket).await else {
        panic!("a page that cannot be answered is refused rather than answered empty")
    };
    assert_eq!(what, "more", "the refusal names what was asked for");
    assert!(
        why.contains("not held yet"),
        "and says why, so a client can tell a delay from an end: {why}",
    );
    assert_eq!(
        seat,
        Some(lead_seat()),
        "**and names the seat it belongs to**: the connection is shared, so a client holding \
         several seats' asks can only drain its own against a refusal that says which seat it is",
    );
}

/// A seat a client is showing has its working tree read, and the row it
/// picks up reaches that client on the seat's own subscription, and what it
/// carries is what a read answers.
///
/// This is the whole wiring in one test: the subscribe that holds the seat,
/// the watch that reports the edit, the scan the loop takes, the update that
/// routes by the seat's slot, and the differential - the three fields a
/// client would apply are the three a fresh read of the seat hands it.
#[tokio::test]
async fn a_held_seats_moved_tree_reaches_the_client() {
    let (url, _fleet, state, _root) = a_repo_server().await;
    let repo =
        state.surface.roster().cwd_for(&lead_seat()).expect("the fixture seat has a directory");
    // A scan the seat already holds, with the PR and the closing issues the
    // tree itself cannot produce in a test: the same branch and the same
    // (empty) pushed sha the real scan will report, and a fresh fetch stamp,
    // so the scan that follows reuses this PR rather than asking `gh`.
    let pr_number = 1249;
    let closing = 1215;
    state.surface.store_work_snapshot(
        &lead_seat(),
        a_scan(&repo, std::time::Instant::now(), "work", 0, Some((pr_number, closing))),
    );

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    snapshot_answering(&mut socket).await;

    // Edited in a loop, each attempt read for less than the staleness window:
    // the watch arms on its own thread, so an early edit can land before
    // notify is listening - and a write read after 10s would pass on the
    // staleness rule instead, proving nothing about the watch.
    let mut moved = None;
    for edit in 1..12 {
        std::fs::write(repo.join("kept.txt"), "x".repeat(edit)).expect("write");
        if let Some(ServerMessage::Update { update, .. }) =
            next_server_within(&mut socket, 700).await
        {
            let SessionUpdate::WorkChanged { key, work, git, pr, closes } = *update else {
                continue;
            };
            assert_eq!(key, lead_seat(), "the row goes to the seat that was held");
            // The watch's first pass announces the stored row before any edit;
            // the row under test is the one the edit made.
            if work.changed != Some(1) {
                continue;
            }
            moved = Some((work, git, pr, closes));
            break;
        }
    }
    let (work, git, pr, closes) = moved.expect("a held seat's moved tree reaches the client");

    // The differential: what the update carried is what a read answers, for
    // every field - the tree behind the row's depth included, which is the
    // one the pushed path derives rather than forwards. A fresh page learns
    // the tree from the read alone, so an update that disagreed with it
    // would draw one row and then correct itself into another.
    let mut reader = connect(&url).await;
    send(
        &mut reader,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut reader).await;
    assert_eq!(
        serde_json::to_value(&work).expect("encode"),
        data["work"],
        "the pushed row is the row the record answers",
    );
    assert_eq!(
        serde_json::to_value(&git).expect("encode"),
        data["git"],
        "and the tree behind the row's depth the seat pushes is the one its read answers",
    );
    assert_eq!(
        serde_json::to_value(&pr).expect("encode"),
        data["pr"],
        "and so is the PR beside it",
    );
    assert_eq!(
        serde_json::to_value(&closes).expect("encode"),
        data["closes"],
        "and the issues it closes",
    );
    // The populated half of the same claim: empty fields would satisfy the
    // equalities above while carrying nothing.
    assert_eq!(
        data["pr"]["number"],
        serde_json::json!(pr_number),
        "with the PR the seat's scan found: {data}",
    );
    assert_eq!(
        data["closes"][0]["number"],
        serde_json::json!(closing),
        "and the issue it closes: {data}",
    );
}

/// A refused subscribe disturbs nothing another connection is holding.
///
/// Connection A is showing a seat; the seat's session ends; connection B
/// subscribes to the same seat - its hold is refused, and the record is
/// answered - and B leaves. The session comes back, the tree moves, and A is
/// told.
///
/// **This is the end-to-end shape of the steal the hold answer closed.** The
/// release is counted per seat, so a connection that gives back a hold it
/// never took spends one another connection is still using. A seat whose
/// session is gone has its loop idle rather than gone, so the steal shows
/// here: A's hold spent is A's loop stopped, and nothing announces after the
/// session returns.
#[tokio::test]
async fn a_refused_subscribe_leaves_another_connections_hold_alone() {
    let (url, fleet, state, _root) = a_repo_server().await;
    let repo =
        state.surface.roster().cwd_for(&lead_seat()).expect("the fixture seat has a directory");

    let mut a = connect(&url).await;
    send(
        &mut a,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    snapshot_answering(&mut a).await;

    // The session ends under A's hold. Its id leaving the header is what says
    // the core has let it go, rather than a sleep hoping it has.
    state
        .surface
        .dispatch(Command::CloseSession { session_key: lead_seat() })
        .expect("the seat's session closes");
    for _ in 0..200 {
        if state.surface.header(&lead_seat()).session_id.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        state.surface.header(&lead_seat()).session_id.is_none(),
        "the seat's session is gone, so a second subscribe is refused a hold",
    );

    // B subscribes to the same seat: refused a hold, and answered a record.
    let mut b = connect(&url).await;
    send(
        &mut b,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut b).await;
    assert_eq!(
        data["slot"]["label"], "lead",
        "a seat with no session is ANSWERED rather than refused outright: {data}",
    );
    // B leaves the way a page leaving does: unsubscribing, which is the path
    // that gives holds back one at a time.
    send(&mut b, ClientMessage::Unsubscribe { what: Subject::Session(lead_seat()) }).await;
    drop(b);

    // The session comes back, and the tree moves under it.
    fleet.start("TestOrg", "proj").expect("the project starts again");
    for edit in 1..12 {
        std::fs::write(repo.join("kept.txt"), "x".repeat(edit)).expect("write");
        if let Some(ServerMessage::Update { update, .. }) = next_server_within(&mut a, 700).await
            && matches!(*update, SessionUpdate::WorkChanged { .. })
        {
            return;
        }
    }
    panic!("A's hold did not survive B's visit: its seat stopped being scanned");
}

/// **A seat re-opened after its release is filled from the ring, not
/// refused.** The frames a seat minted before its release stay reachable, so a
/// page switching back to a seat whose session has ended reads the
/// conversation it missed rather than an error about a copy that cannot be
/// filled.
#[tokio::test]
async fn a_seat_reopened_after_a_release_is_filled_from_its_ring() {
    let (url, fleet, state, _root) = a_repo_server().await;
    let seat = lead_seat();
    fleet.seed_view_facts(
        &seat,
        ViewFacts {
            session_id: Some(SessionId::from("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45")),
            ..Default::default()
        },
    );

    // A page opens the seat and hears one prompt row.
    let mut first = connect(&url).await;
    send(
        &mut first,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    snapshot_answering(&mut first).await;
    let prompt: forge_primitives::Message = serde_json::from_value(serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": "before the release" },
        "session_id": "s",
    }))
    .expect("parse a user message");
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        msg: prompt,
        origin: None,
        source: None,
    });
    loop {
        let Some(ServerMessage::Update { update, .. }) =
            next_server_within(&mut first, 5_000).await
        else {
            panic!("the prompt row never reached the page");
        };
        if matches!(*update, SessionUpdate::ChatAppended { .. }) {
            break;
        }
    }
    drop(first);

    // The session ends under the seat.
    state
        .surface
        .dispatch(Command::CloseSession { session_key: seat.clone() })
        .expect("the seat's session closes");
    for _ in 0..200 {
        if state.surface.header(&seat).session_id.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(state.surface.header(&seat).session_id.is_none(), "the seat's session is gone");

    // A page switching back reads the seat: the copy is filled from the frames
    // the release kept, so the snapshot carries the prompt rather than the
    // refusal a shed ring would leave.
    let mut second = connect(&url).await;
    send(
        &mut second,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut second).await;
    let conversation = serde_json::to_string(&data["conversation"]).expect("json");
    assert!(
        conversation.contains("before the release"),
        "the re-opened seat's copy carries the frame the ring kept: {conversation}",
    );
}

/// A seat whose tree is a real repository, with its session started - the
/// store rides the seat's own record, so a seat nothing runs behind is not
/// held at all.
async fn a_repo_server() -> (String, Fleet, Arc<TransportState>, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("tempdir");
    // The fleet first: a project directory that exists before the fleet is
    // built resolves under a different key.
    let fleet = Fleet::in_dir(root.path(), &[("TestOrg", &["proj"])]).expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let repo = root.path().join("proj");
    std::fs::create_dir_all(&repo).expect("the project directory");
    a_repo(&repo);

    let state = Arc::new(TransportState::for_workspace(&fleet.workspace()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener, updates).await;
    });
    // The directory comes back with the server: it holds the repository the
    // seat's tree is, and dropping it here would unlink the tree under the
    // test the moment this function returned.
    (format!("ws://{addr}/socket"), fleet, state, root)
}

/// A scan to seed a seat's store with, as the scanner would have answered it.
fn a_scan(
    cwd: &std::path::Path,
    read_at: std::time::Instant,
    branch: &str,
    changed: usize,
    with_pr: Option<(u64, u64)>,
) -> forge_server::surface::inspector::WorkSnapshot {
    use forge_primitives::git::{GitBranch, GitIssueRef, GitPrInfo};
    use forge_primitives::git_diff::{GitDiffSnapshot, GitDiffStats, LayerState, RepoGate};
    let (pr, closes) = match with_pr {
        Some((number, closing)) => (
            Some(GitPrInfo {
                number,
                url: format!("https://example.test/pull/{number}"),
                draft: false,
            }),
            vec![GitIssueRef { number: closing, url: format!("https://example.test/{closing}") }],
        ),
        None => (None, Vec::new()),
    };
    forge_server::surface::inspector::WorkSnapshot {
        diff: GitDiffSnapshot {
            branch: GitBranch::Named(branch.to_owned()),
            pushed_sha: None,
            pr_fetched_at: Some(std::time::SystemTime::now()),
            // None, as the scanner would answer for this fixture: the repo
            // has no `origin/HEAD` and no local `main`, so a scan-faithful
            // seed says the default is unresolved.
            default_branch: None,
            repo_gate: RepoGate::InRepo,
            worktree: LayerState::Populated(GitDiffStats {
                files: Vec::new(),
                total_files: changed,
                total_added: 0,
                total_removed: 0,
            }),
            branch_ahead: LayerState::Clean,
            pr,
            closes,
        },
        cwd: cwd.to_path_buf(),
        read_at,
        base: None,
    }
}

/// **A stored scan past its window is re-read by the record, not carried.**
/// The record's hunks ride the same scan as its row, so serving an
/// arbitrarily old store would put its hunks beside a row the first pass has
/// since moved; the record takes the scan itself and the snapshot carries
/// what git answers now.
#[tokio::test]
async fn a_stale_store_is_re_read_by_the_record() {
    let (url, _fleet, state, _root) = a_repo_server().await;
    let repo =
        state.surface.roster().cwd_for(&lead_seat()).expect("the fixture seat has a directory");
    // A row the tree cannot answer, old enough that the record must not
    // serve it.
    let long_ago = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(60))
        .expect("an instant a minute ago");
    state.surface.store_work_snapshot(
        &lead_seat(),
        a_scan(std::path::Path::new(&repo), long_ago, "not-this-tree", 99, None),
    );
    // The index is pre-walked so the record's only read here is the tree's.
    state.surface.file_index_for_record(&lead_seat()).await;

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;

    assert_eq!(
        data["work"]["branch"], "work",
        "the record re-read the stale store rather than carrying it: {data}",
    );
    assert_eq!(data["work"]["changed"], 0, "with the count the read answered: {data}");
}

/// **The record never waits on the PR lookup, and the watch is where it runs.**
/// The store's PR is past its refetch window, so a read that fetched would
/// spawn `gh`; the snapshot must carry the stored PR with the shim untouched,
/// and the correcting `work_changed` must arrive from the seat's own watch.
#[tokio::test]
async fn a_subscribe_serves_the_stored_pr_and_the_watch_pushes_the_lookup() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("tempdir");
    let shim = tempfile::tempdir().expect("the gh shim's own directory");
    let marker = shim.path().join("gh-spawned");
    let gh = shim.path().join("gh");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\nprintf x > {marker}\ncase \"$*\" in\n  *commits/*pulls) printf '%s' \
             '[{{\"number\":108,\"state\":\"open\",\"html_url\":\"https://example.test/pull/108\",\
             \"updated_at\":\"2026-10-24T00:00:00Z\",\"draft\":false}}]' ;;\n  *\"pr view\"*) \
             printf '%s' '{{\"closingIssuesReferences\":[]}}' ;;\n  *) printf '[]' ;;\nesac\n",
            marker = marker.display(),
        ),
    )
    .expect("write the shim");
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = shim.path().to_string_lossy().into_owned();

    let fleet = Fleet::in_dir_with_server_and_env(
        root.path(),
        &[("TestOrg", &["proj"])],
        "",
        &[("PATH", path.as_str()), ("GH_TOKEN", "t")],
    )
    .expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let repo = root.path().join("proj");
    std::fs::create_dir_all(&repo).expect("the project directory");
    let tip = a_repo_with_a_pushed_branch(&repo);

    let state = Arc::new(TransportState::for_workspace(&fleet.workspace()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener, updates).await;
    });
    let url = format!("ws://{addr}/socket");

    // The store holds a PR, its attempt is past the refetch window, and its
    // tree read is past the record's - so the record re-reads the tree and the
    // lookup is due.
    let long_ago = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(60))
        .expect("an instant a minute ago");
    let mut stored = a_scan(&repo, long_ago, "work", 0, Some((7, 11)));
    stored.diff.pushed_sha = Some(tip);
    stored.diff.pr_fetched_at = Some(
        std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(600))
            .expect("a wall clock at least ten minutes past boot"),
    );
    state.surface.store_work_snapshot(&lead_seat(), stored);
    state.surface.file_index_for_record(&lead_seat()).await;

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;

    // The store's PR is what proves the read served rather than fetched: a
    // lookup would have answered 108. **The marker is not checked here** - the
    // watch's first poke is immediate and its fetch can reach the shim while
    // the snapshot is still being assembled, so "no spawn yet" is a race
    // rather than a property of the read. The check after the push is the one
    // that belongs to the lookup.
    assert_eq!(data["pr"]["number"], 7, "the record serves the PR its store holds: {data}");

    // The watch's next pass is where the lookup runs; its push is what
    // corrects the row a reader is looking at.
    update_until_within(&mut socket, WATCH_WAIT, "the watch's PR correction", |update| {
        matches!(update, SessionUpdate::WorkChanged { pr: Some(pr), .. } if pr.number == 108)
    })
    .await;
    assert!(marker.exists(), "the lookup ran, off the read path");
}

/// **The review page's own read refreshes the base, and the count moves with
/// it.** An ordinary record read serves the stored state and says its base was
/// not checked; the review-scoped read fetches first and re-reads the stats
/// pinned to the same sha, so the file list and the "N commits" describe one
/// base. Deleting the pinned scan leaves the count on the stale base and reds
/// this.
#[tokio::test]
async fn a_review_read_refreshes_the_base_and_the_count_with_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = tempfile::tempdir().expect("upstream");
    let fleet = Fleet::in_dir_with_server_and_env(root.path(), &[("TestOrg", &["proj"])], "", &[])
        .expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let repo = root.path().join("proj");
    std::fs::create_dir_all(&repo).expect("the project directory");
    a_repo_with_a_stale_base(&repo, upstream.path());

    let state = Arc::new(TransportState::for_workspace(&fleet.workspace()));
    // The store holds a scan of the stale tree, taken now and with its PR
    // attempt stamped now, so the seat's watch neither re-reads it nor starts
    // a lookup: the ref can only move when the review read refreshes it.
    let mut stored = forge_workspace::env::git_diff::scan_serving_cached_pr(
        &repo,
        &std::collections::HashMap::new(),
        None,
    )
    .await;
    stored.pr_fetched_at = Some(std::time::SystemTime::now());
    state.surface.store_work_snapshot(
        &lead_seat(),
        forge_server::surface::inspector::WorkSnapshot {
            diff: stored,
            cwd: repo,
            read_at: std::time::Instant::now(),
            base: None,
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener, updates).await;
    });
    let url = format!("ws://{addr}/socket");

    // The ordinary read: the stored state, its base never checked.
    let mut plain = connect(&url).await;
    send(
        &mut plain,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, cached, _) = snapshot_answering(&mut plain).await;
    assert_eq!(
        cached["diff"]["base"], "unverified",
        "an ordinary read does not claim a base: {cached}",
    );
    assert_eq!(
        cached["git"]["ahead"]["commit_count"], 2,
        "the stale base folds upstream's commit into the count: {cached}",
    );
    // **The ordinary read reached no network**: the ref is where the stale
    // fixture left it, so the count above was that ref's and nothing fetched.
    let refs = |at: &std::path::Path, what: &str| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(at)
            .args(["rev-parse", what])
            .output()
            .expect("git rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    assert_eq!(
        refs(
            &state.surface.roster().cwd_for(&lead_seat()).expect("the seat's cwd"),
            "refs/remotes/origin/main"
        ),
        refs(upstream.path(), "main~1"),
        "an ordinary read leaves the base ref where it stood",
    );

    // The review page's own read: the base refreshed, the count with it.
    let mut review = connect(&url).await;
    send(
        &mut review,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: true,
        },
    )
    .await;
    let (_, fresh, _) = snapshot_answering(&mut review).await;
    assert_eq!(fresh["diff"]["base"], "current", "the review read refreshed the base: {fresh}");
    assert_eq!(
        fresh["git"]["ahead"]["commit_count"], 1,
        "the count and the files share one base: {fresh}",
    );
    let files = fresh["diff"]["branch_ahead"]["populated"]
        .as_array()
        .expect("the branch layer is populated");
    let paths: Vec<&str> = files.iter().filter_map(|file| file["path"].as_str()).collect();
    assert_eq!(paths, vec!["feat.txt"], "only the branch's own file crosses: {fresh}");
}

/// **A fetch that cannot run says what git said.** The page draws the reason
/// beside the base line, so a reader learns the remote is gone rather than
/// only that the base could not be checked.
#[tokio::test]
async fn a_review_read_names_why_the_base_fetch_failed() {
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = tempfile::tempdir().expect("upstream");
    let fleet = Fleet::in_dir_with_server_and_env(root.path(), &[("TestOrg", &["proj"])], "", &[])
        .expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let repo = root.path().join("proj");
    std::fs::create_dir_all(&repo).expect("the project directory");
    a_repo_with_a_stale_base(&repo, upstream.path());
    // A remote that is not there any more: git's own words are the failure.
    let gone = root.path().join("gone");
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["remote", "set-url", "origin", &gone.to_string_lossy()])
        .output()
        .expect("run git");
    assert!(out.status.success(), "git set-url: {}", String::from_utf8_lossy(&out.stderr));

    let state = Arc::new(TransportState::for_workspace(&fleet.workspace()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    // The fold's subscription is taken before the serve task is spawned, so
    // a `Connected` emitted right after this cannot find nobody listening.
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener, updates).await;
    });
    let url = format!("ws://{addr}/socket");

    let mut review = connect(&url).await;
    send(
        &mut review,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: true,
        },
    )
    .await;
    let (_, record, _) = snapshot_answering(&mut review).await;

    assert_eq!(
        record["diff"]["base"], "unverified",
        "a fetch that cannot run leaves the base unverified: {record}",
    );
    let said = record["diff"]["base_error"].as_str().unwrap_or_default();
    assert!(
        said.contains("does not appear to be a git repository"),
        "and the record carries git's own words for it: {said:?}",
    );

    // An ordinary read has not checked the base, so it names no failure.
    let mut plain = connect(&url).await;
    send(
        &mut plain,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, cached, _) = snapshot_answering(&mut plain).await;
    assert!(
        cached["diff"]["base_error"].is_null(),
        "an ordinary read claims no failure it never saw: {cached}",
    );
}

/// **A review read never holds its connection.** That read waits for a base
/// fetch with no cap, and it is served off the loop: a second ask on the same
/// socket is answered while the read is still in flight, and a client going
/// away is visible rather than queued behind it.
#[tokio::test]
async fn a_subscribe_behind_a_review_read_is_answered_while_it_waits() {
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = tempfile::tempdir().expect("upstream");
    let fleet = Fleet::in_dir_with_server_and_env(root.path(), &[("TestOrg", &["proj"])], "", &[])
        .expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let repo = root.path().join("proj");
    std::fs::create_dir_all(&repo).expect("the project directory");
    a_repo_with_a_stale_base(&repo, upstream.path());

    let state = Arc::new(TransportState::for_workspace(&fleet.workspace()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    // The fold's subscription is taken before the serve task is spawned, so
    // a `Connected` emitted right after this cannot find nobody listening.
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener, updates).await;
    });
    let url = format!("ws://{addr}/socket");

    // The seat is held by a plain open first, which is what the gate below
    // needs; the review read's own wait is then held open by that gate.
    let mut conn = connect(&url).await;
    send(
        &mut conn,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut conn).await;
    forge_workspace::work::set_pinned_gate_wait(10_000);
    let gate = fleet.workspace().hold_work_gate(&lead_seat()).expect("the seat is held");
    let in_flight = gate.lock().await;

    send(
        &mut conn,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: true,
        },
    )
    .await;
    // While that read waits on the seat's gate, the loop keeps answering: a
    // reply-carrying command is dispatched and its refusal answered. The bound
    // is the discriminator beside the ten-second gate: a loop that waited on
    // the read would answer only after the gate is released, far past this.
    send(
        &mut conn,
        ClientMessage::Command {
            command: Box::new(Command::DeleteReviewThread {
                project: "no-such-project".to_owned(),
                branch: "no-such-branch".to_owned(),
                thread_id: "t1".to_owned(),
                respond: None,
            }),
            reply_to: Some(7),
        },
    )
    .await;
    match next_server_within(&mut conn, 5_000).await {
        Some(ServerMessage::Reply { reply_to, .. }) => {
            assert_eq!(reply_to, 7, "the command was answered while the read waited");
        }
        other => panic!("no reply arrived while the review read waited: {other:?}"),
    }

    drop(in_flight);
    // And the review read's own record follows once its wait ends; the seat's
    // watch may push a row first, which is news rather than the record.
    loop {
        match next_server_within(&mut conn, 5_000).await {
            Some(ServerMessage::Snapshot { subject, .. }) => {
                assert_eq!(subject, Subject::Session(lead_seat()), "the read itself answered");
                break;
            }
            Some(ServerMessage::Update { .. }) => {}
            other => panic!("the review read's snapshot did not arrive: {other:?}"),
        }
    }
    forge_workspace::work::set_pinned_gate_wait(0);
}

/// **A release during an off-loop read ends the read, answers its ask and
/// leaves nothing behind.** The seat's hold goes back, the asks queued behind
/// the read are served, and the client's oldest-first bookkeeping is answered
/// rather than stranded - the three ways a merely-marked abandoned read leaks.
#[tokio::test]
async fn a_release_during_a_review_read_ends_it_and_answers_the_ask() {
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = tempfile::tempdir().expect("upstream");
    let fleet = Fleet::in_dir_with_server_and_env(root.path(), &[("TestOrg", &["proj"])], "", &[])
        .expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let repo = root.path().join("proj");
    std::fs::create_dir_all(&repo).expect("the project directory");
    a_repo_with_a_stale_base(&repo, upstream.path());

    let state = Arc::new(TransportState::for_workspace(&fleet.workspace()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    // The fold's subscription is taken before the serve task is spawned, so
    // a `Connected` emitted right after this cannot find nobody listening.
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener, updates).await;
    });
    let url = format!("ws://{addr}/socket");

    let mut conn = connect(&url).await;
    send(
        &mut conn,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut conn).await;
    forge_workspace::work::set_pinned_gate_wait(10_000);
    let gate = fleet.workspace().hold_work_gate(&lead_seat()).expect("the seat is held");
    let in_flight = gate.lock().await;

    send(
        &mut conn,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: true,
        },
    )
    .await;
    // An ask that arrives during the read waits its turn behind it.
    send(
        &mut conn,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    send(&mut conn, ClientMessage::Unsubscribe { what: Subject::Session(lead_seat()) }).await;

    // The abandoned subscribe is answered, so the client owes nothing for it.
    // The bound sits well under the gate below: an answer only after the gate
    // would mean the loop waited on the read.
    match next_server_within(&mut conn, 5_000).await {
        Some(ServerMessage::Error { what, seat, .. }) => {
            assert_eq!(what, "subscribe", "the abandoned ask is answered as a subscribe");
            assert_eq!(seat, Some(lead_seat()), "and names the seat it was for");
        }
        other => panic!("no answer arrived for the abandoned subscribe: {other:?}"),
    }
    // And the ask queued behind it is served, though the fetch is still held.
    match next_server_within(&mut conn, 5_000).await {
        Some(ServerMessage::Snapshot { subject, .. }) => {
            assert_eq!(subject, Subject::Home, "the queued ask is served");
        }
        other => panic!("the queued ask was not served: {other:?}"),
    }

    // The read itself is gone: releasing its wait answers nothing.
    drop(in_flight);
    while let Some(message) = next_server_within(&mut conn, 500).await {
        assert!(
            !matches!(&message, ServerMessage::Snapshot { subject, .. }
                if subject == &Subject::Session(lead_seat())),
            "an abandoned read still answered its ask",
        );
    }

    // Both holds are back once the plain subscription is released too. This
    // test's own `gate` clone goes first: the seat is released when nothing
    // else holds its gate.
    send(&mut conn, ClientMessage::Unsubscribe { what: Subject::Session(lead_seat()) }).await;
    drop(gate);
    assert!(
        fleet.workspace().hold_work_gate(&lead_seat()).is_none(),
        "the abandoned read's hold was given back",
    );
    forge_workspace::work::set_pinned_gate_wait(0);
}

/// **A read off the loop holds its own seat's frames, and nothing else's.**
/// The seat's rows wait for its record; a fleet frame still reaches the home
/// this connection watches, a fatal still goes out at once, and the held row
/// lands behind the snapshot.
#[tokio::test]
async fn a_read_off_the_loop_holds_its_own_seats_frames_only() {
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = tempfile::tempdir().expect("upstream");
    let fleet = Fleet::in_dir_with_server_and_env(root.path(), &[("TestOrg", &["proj"])], "", &[])
        .expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let repo = root.path().join("proj");
    std::fs::create_dir_all(&repo).expect("the project directory");
    a_repo_with_a_stale_base(&repo, upstream.path());
    let state = Arc::new(TransportState::for_workspace(&fleet.workspace()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    // The fold's subscription is taken before the serve task is spawned, so
    // a `Connected` emitted right after this cannot find nobody listening.
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener, updates).await;
    });
    let url = format!("ws://{addr}/socket");

    // A plain open holds the seat so its gate can be held; it is released
    // before the review read, so the seat is NOT watched while that read runs.
    let mut conn = connect(&url).await;
    send(
        &mut conn,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut conn).await;
    forge_workspace::work::set_pinned_gate_wait(10_000);
    let gate = fleet.workspace().hold_work_gate(&lead_seat()).expect("the seat is held");
    let in_flight = gate.lock().await;
    send(&mut conn, ClientMessage::Unsubscribe { what: Subject::Session(lead_seat()) }).await;
    // The home is watched while the read runs, and the seat is not.
    send(
        &mut conn,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    let _ = snapshot_answering(&mut conn).await;
    send(
        &mut conn,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: true,
        },
    )
    .await;

    // The subscribe has to have reached the loop before these land, or the row
    // is emitted with no read in flight and no watcher. The gate below holds
    // the read until this test releases it, so this settle cannot race a fast
    // answer.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    // A name no earlier frame carries: the start's own spawning row can still
    // be replayed to the home this connection opened, and its presence would
    // otherwise read as this frame's.
    fleet.emit(SessionUpdate::Spawning {
        key: lead_seat(),
        project_name: "proj".to_owned(),
        cwd: repo.to_string_lossy().into_owned(),
        display_name: "held-read-probe".to_owned(),
    });
    fleet.emit(SessionUpdate::FatalError {
        error: forge_primitives::error::AppError::ConnectionFailed,
        message: "the bridge broke".to_owned(),
    });
    // A mid-turn chunk: the home does not cover it, so the only copy this
    // connection could hear is the held one.
    fleet.emit(SessionUpdate::ChatAppended {
        key: lead_seat(),
        msg: an_assistant_chunk(),
        origin: None,
        source: None,
    });

    let mut saw_spawning = false;
    let mut saw_fatal = false;
    while let Some(message) = next_server_within(&mut conn, 1_500).await {
        match message {
            ServerMessage::Update { update, .. } => match *update {
                SessionUpdate::Spawning { display_name, .. } => {
                    saw_spawning = saw_spawning || display_name == "held-read-probe";
                }
                SessionUpdate::FatalError { .. } => saw_fatal = true,
                SessionUpdate::ChatAppended { .. } => {
                    panic!("the seat's held row overtook the record that has not arrived")
                }
                _ => {}
            },
            ServerMessage::Snapshot { subject, .. } => {
                panic!("a record arrived while its read was still held: {subject:?}")
            }
            _ => {}
        }
    }
    assert!(saw_spawning, "the fleet frame reached the home while the seat's read waited");
    assert!(saw_fatal, "the fatal went out at once, not on the batch's clock");

    // And the held row lands behind the record once the read answers.
    drop(in_flight);
    match next_server_within(&mut conn, 5_000).await {
        Some(ServerMessage::Snapshot { subject, .. }) => {
            assert_eq!(subject, Subject::Session(lead_seat()), "the read answered its own ask");
        }
        other => panic!("the read's record did not arrive: {other:?}"),
    }
    let mut saw_row = false;
    while let Some(message) = next_server_within(&mut conn, 3_000).await {
        if matches!(&message, ServerMessage::Update { update, .. }
            if matches!(**update, SessionUpdate::ChatAppended { .. }))
        {
            saw_row = true;
            break;
        }
    }
    assert!(saw_row, "the held row landed behind the record");
    forge_workspace::work::set_pinned_gate_wait(0);
}

/// **An ask queued behind a read is withdrawn when its subject is released**,
/// answered so the client owes nothing for it, and answered in its turn: the
/// client pairs a refusal with the oldest ask it holds, so an answer sent out
/// of band would land on the read still ahead of it. It never starts a read of
/// its own.
#[tokio::test]
async fn a_queued_ask_is_withdrawn_and_answered_when_its_subject_is_released() {
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = tempfile::tempdir().expect("upstream");
    let fleet = Fleet::in_dir_with_server_and_env(root.path(), &[("TestOrg", &["proj"])], "", &[])
        .expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let repo = root.path().join("proj");
    std::fs::create_dir_all(&repo).expect("the project directory");
    a_repo_with_a_stale_base(&repo, upstream.path());
    let state = Arc::new(TransportState::for_workspace(&fleet.workspace()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    // The fold's subscription is taken before the serve task is spawned, so
    // a `Connected` emitted right after this cannot find nobody listening.
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener, updates).await;
    });
    let url = format!("ws://{addr}/socket");

    let mut conn = connect(&url).await;
    send(
        &mut conn,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut conn).await;
    forge_workspace::work::set_pinned_gate_wait(10_000);
    let gate = fleet.workspace().hold_work_gate(&lead_seat()).expect("the seat is held");
    let in_flight = gate.lock().await;
    send(&mut conn, ClientMessage::Unsubscribe { what: Subject::Session(lead_seat()) }).await;
    send(
        &mut conn,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: true,
        },
    )
    .await;
    // This one queues behind the read, and the client lets it go before it is
    // its turn.
    send(
        &mut conn,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    send(&mut conn, ClientMessage::Unsubscribe { what: Subject::Home }).await;

    // **The withdrawn ask keeps its place.** Its answer would carry a refusal,
    // and the client pairs a refusal with the oldest ask it still holds - the
    // seat's read, still in flight - so nothing may arrive for it yet.
    while let Some(message) = next_server_within(&mut conn, 700).await {
        assert!(
            !matches!(&message, ServerMessage::Error { .. }),
            "the withdrawn ask answered before the read ahead of it: {message:?}",
        );
    }

    // Ending the read answers both, in the order they were asked: the seat's
    // own read, then the withdrawn home ask that queued behind it.
    send(&mut conn, ClientMessage::Unsubscribe { what: Subject::Session(lead_seat()) }).await;
    let mut refusals = Vec::new();
    while refusals.len() < 2 {
        match next_server_within(&mut conn, 5_000).await {
            Some(ServerMessage::Error { what, seat, .. }) => refusals.push((what, seat)),
            Some(_) => {}
            None => panic!("only {} of the two answers arrived", refusals.len()),
        }
    }
    assert_eq!(
        refusals[0],
        ("subscribe".to_owned(), Some(lead_seat())),
        "the seat's abandoned read is answered first",
    );
    assert_eq!(refusals[1], ("subscribe".to_owned(), None), "and the withdrawn home ask second");
    while let Some(message) = next_server_within(&mut conn, 700).await {
        assert!(
            !matches!(&message, ServerMessage::Snapshot { subject, .. } if subject == &Subject::Home),
            "a withdrawn ask was served anyway",
        );
    }
    drop(in_flight);
    drop(gate);
    forge_workspace::work::set_pinned_gate_wait(0);
}

/// **A refused pin is re-read locally, so the count and the files still share
/// the base the refresh left.** The store's count predates the fetch; serving
/// it beside content read at the post-fetch ref is the Granite symptom in the
/// path meant to be the safe degradation.
#[tokio::test]
async fn a_refused_pin_re_reads_the_tree_against_the_refreshed_base() {
    let root = tempfile::tempdir().expect("tempdir");
    let upstream = tempfile::tempdir().expect("upstream");
    let fleet = Fleet::in_dir_with_server_and_env(root.path(), &[("TestOrg", &["proj"])], "", &[])
        .expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    let repo = root.path().join("proj");
    std::fs::create_dir_all(&repo).expect("the project directory");
    a_repo_with_a_stale_base(&repo, upstream.path());

    let state = Arc::new(TransportState::for_workspace(&fleet.workspace()));
    let mut stored = forge_workspace::env::git_diff::scan_serving_cached_pr(
        &repo,
        &std::collections::HashMap::new(),
        None,
    )
    .await;
    stored.pr_fetched_at = Some(std::time::SystemTime::now());
    state.surface.store_work_snapshot(
        &lead_seat(),
        forge_server::surface::inspector::WorkSnapshot {
            diff: stored,
            cwd: repo,
            read_at: std::time::Instant::now(),
            base: None,
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let updates = state.surface.subscribe_mirror();
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener, updates).await;
    });
    let url = format!("ws://{addr}/socket");

    // The seat is held by a plain open first, and then its gate is held the
    // way a watch pass holds it across its own scan.
    let mut plain = connect(&url).await;
    send(
        &mut plain,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let _ = snapshot_answering(&mut plain).await;
    // The bound is real time; the test's is short, and restored after.
    forge_workspace::work::set_pinned_gate_wait(50);
    let gate = fleet.workspace().hold_work_gate(&lead_seat()).expect("the seat is held");
    let in_flight = gate.lock().await;

    let mut review = connect(&url).await;
    send(
        &mut review,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: true,
        },
    )
    .await;
    let (_, fresh, _) = snapshot_answering(&mut review).await;
    drop(in_flight);
    forge_workspace::work::set_pinned_gate_wait(0);

    assert_eq!(
        fresh["diff"]["base"], "current",
        "the refresh landed, so the base is the one it left: {fresh}",
    );
    assert_eq!(
        fresh["git"]["ahead"]["commit_count"], 1,
        "the local re-read is the post-fetch base, not the store's stale count: {fresh}",
    );
    // **The row's own base is the one this read measured.** The local re-read
    // carries the refresh's state, so `git.base` and `diff.base` cannot
    // contradict each other in one record.
    assert_eq!(
        fresh["git"]["base"], "current",
        "the row's base is the state the refresh found: {fresh}",
    );
    let files = fresh["diff"]["branch_ahead"]["populated"]
        .as_array()
        .expect("the branch layer is populated");
    let paths: Vec<&str> = files.iter().filter_map(|file| file["path"].as_str()).collect();
    assert_eq!(
        paths,
        vec!["feat.txt"],
        "and the files describe the same base the count does: {fresh}",
    );
}

/// **An open announces nothing its snapshot already carried** (#2506 review).
/// The record's read is the first pass's baseline, so a still tree sends no
/// second copy of the work row or the file index - the two the open's own
/// frame carries - while a value that moves still announces.
#[tokio::test]
async fn an_open_announces_no_value_its_snapshot_carried() {
    let (url, _fleet, state, _root) = a_repo_server().await;
    let repo =
        state.surface.roster().cwd_for(&lead_seat()).expect("the fixture seat has a directory");
    // Nothing is seeded: the record's own first read takes both stores under
    // the seat's gate, and the first pass starts from what it left.

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    assert_eq!(data["work"]["branch"], "work", "the snapshot carries the read row: {data}");

    // The first pass starts from the stores that read left, so nothing it
    // reads is news: the window is the pass plus a beat.
    let quiet = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
    while tokio::time::Instant::now() < quiet {
        let Some(ServerMessage::Update { update, .. }) = next_server_within(&mut socket, 500).await
        else {
            continue;
        };
        match *update {
            SessionUpdate::WorkChanged { .. } => panic!("an open re-sent its work row"),
            SessionUpdate::FileIndexChanged { .. } => panic!("an open re-sent its file index"),
            _ => {}
        }
    }

    // A moved value still announces: an untracked file enters the index, and
    // an edit moves the row.
    std::fs::write(repo.join("new.rs"), "").expect("write");
    loop {
        let Some(ServerMessage::Update { update, .. }) =
            next_server_within(&mut socket, 10_000).await
        else {
            panic!("a written file never reached the seat's viewers");
        };
        if let SessionUpdate::FileIndexChanged { index, .. } = *update {
            assert!(index.entries.contains_key("new.rs"), "the index carries the file");
            break;
        }
    }
    std::fs::write(repo.join("kept.txt"), "moved").expect("write");
    loop {
        let Some(ServerMessage::Update { update, .. }) =
            next_server_within(&mut socket, 10_000).await
        else {
            panic!("a moved row never reached the seat's viewers");
        };
        if let SessionUpdate::WorkChanged { work, .. } = *update
            && work.changed == Some(1)
        {
            break;
        }
    }
}

/// A repository with one commit, for a test that needs a tree git can read.
///
/// The branch it sits on is `work`, not the repository's default: a PR lookup
/// runs only for a branch that is not the default one, so a fixture on `main`
/// could never show the PR a scan cache reuses.
fn a_repo(dir: &std::path::Path) {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "work"]);
    git(&["config", "user.email", "test@example.test"]);
    git(&["config", "user.name", "test"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("kept.txt"), "one").expect("write");
    git(&["add", "."]);
    git(&["commit", "-qm", "first"]);
}

/// A checkout whose `origin/main` sits one commit behind its remote, with a
/// feature branch cut from the remote's tip and one commit of its own: the
/// stale-base shape a review read has to refresh before it draws.
fn a_repo_with_a_stale_base(dir: &std::path::Path, upstream: &std::path::Path) {
    let git = |at: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .arg("-C")
            .arg(at)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} at {}: {}",
            at.display(),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(upstream, &["init", "-q", "--bare"]);
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.test"]);
    git(dir, &["config", "user.name", "test"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("base.txt"), "base\n").expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "X base"]);
    let base = git(dir, &["rev-parse", "HEAD"]);
    git(dir, &["remote", "add", "origin", &upstream.to_string_lossy()]);
    git(dir, &["push", "-q", "-u", "origin", "main"]);
    std::fs::write(dir.join("upstream.txt"), "upstream\n").expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "Y upstream"]);
    git(dir, &["push", "-q", "origin", "main"]);
    git(dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("feat.txt"), "feat\n").expect("write");
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "Z feat"]);
    git(dir, &["update-ref", "refs/remotes/origin/main", &base]);
    git(dir, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
}

/// [`a_repo`] with its tip already on a remote-tracking ref, so a scan
/// resolves a pushed sha and its PR lookup is one `gh` run away. Answers the
/// tip.
fn a_repo_with_a_pushed_branch(dir: &std::path::Path) -> String {
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    a_repo(dir);
    let tip = git(&["rev-parse", "HEAD"]);
    git(&["update-ref", "refs/remotes/origin/main", &tip]);
    git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
    tip
}

/// A subscription hears the updates its subject receives and no others.
#[tokio::test]
async fn a_subscriber_hears_the_update_it_asked_for_and_not_another_seats() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    snapshot_answering(&mut socket).await;

    // An App-level update names no slot, which is what the home subscription is for.
    fleet.emit(SessionUpdate::CatalogLoaded);
    update_until(&mut socket, "a home subscriber hears an App-level update", |update| {
        matches!(update, SessionUpdate::CatalogLoaded)
    })
    .await;

    // A seat's own news reaches the home, because a row states it: whether the
    // seat is running, waiting on an answer, or finished with a turn nobody
    // looked at. A home that heard only the App-level updates would be a still
    // photograph of a fleet changing underneath it.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });
    update_until(&mut socket, "a home subscriber hears a seat's row change", |update| {
        matches!(update, SessionUpdate::TurnCancelled { .. })
    })
    .await;

    // And so does an App-level one the fleet's own classification does not
    // name, because the home is not only the fleet region: the service status
    // is a field of its snapshot, so a client that heard it once at subscribe
    // and never again would draw a stale page.
    // `CatalogLoaded` is no use here - the classification DOES cover it, so it
    // passes over this hole. The wait matches the notice written below rather
    // than the variant, so nobody else's service status can stand in for it.
    let its_own = "a statuspage notice";
    fleet.emit(SessionUpdate::ServiceStatus {
        severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
        message: its_own.to_owned(),
    });
    update_until(
        &mut socket,
        "a home subscriber hears the App-level updates its snapshot carries",
        |update| matches!(update, SessionUpdate::ServiceStatus { message, .. } if message == its_own),
    )
    .await;

    // And the conversation does not. A token is the bulk of the stream and no
    // row draws one, so carrying it here would re-send the whole fleet for
    // every word of every seat in it. The evidence is ORDER, never a timeout:
    // waiting for the update NOT to arrive would hang on the correct
    // behaviour, so the test asks for something whose answer must come next
    // and reads what reached the subscriber before it.
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
        origin: None,
        source: None,
    });
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    let (_, _, passed) = snapshot_answering(&mut socket).await;
    assert!(
        !passed.iter().any(|update| matches!(update, SessionUpdate::ChatAppended { .. })),
        "a chat token must not reach a home subscriber; the snapshot should be next: {passed:?}",
    );
}

/// The interval a connection's stream is written on: a burst leaves as one
/// batch, whole and in the order the core emitted it.
///
/// **The lower bound is the change itself.** Before it every update was
/// written where it landed, so a burst's first frame was on the wire before
/// this read came back; a timer cannot fire early, so half an interval is a
/// floor nothing but writing through can get under.
#[tokio::test]
async fn a_burst_leaves_a_connection_in_one_batch_whole_and_in_order() {
    const EMITTED: [&str; 3] = ["one", "two", "three"];
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: false,
            review: false,
        },
    )
    .await;
    let ServerMessage::Snapshot { .. } = next_server(&mut socket).await else {
        panic!("a seat that exists is answered with its snapshot")
    };

    let emitted = std::time::Instant::now();
    for name in EMITTED {
        fleet.emit(SessionUpdate::ConnectionFailed {
            key: lead_seat(),
            message: name.to_owned(),
            fatal: false,
        });
    }

    let mut heard = Vec::new();
    let mut first = None;
    for _ in 0..EMITTED.len() {
        // Matched on the message rather than the variant: the core raises
        // updates of its own, and a frame this test did not emit is not one
        // of its three.
        let update = update_until(&mut socket, "an update this test emitted", |update| {
            matches!(update, SessionUpdate::ConnectionFailed { message, .. }
                if EMITTED.contains(&message.as_str()))
        })
        .await;
        first.get_or_insert_with(std::time::Instant::now);
        let SessionUpdate::ConnectionFailed { message, .. } = update else {
            panic!("the wait answered with what it was asked for");
        };
        heard.push(message);
    }
    let first = first.expect("three updates were read");

    assert_eq!(
        heard, EMITTED,
        "the batch carries every update, whole, in the order the core emitted them",
    );
    assert!(
        first.duration_since(emitted) >= forge_server::transport::batch::FLUSH_INTERVAL / 2,
        "the first frame waited for the batch's window rather than being written where it \
         landed: {:?} after the update was emitted",
        first.duration_since(emitted),
    );
}

/// A batch still waiting when a client asks goes out ahead of the answer.
///
/// An answer is composed after everything the core has already said, so a
/// snapshot overtaking an update the core emitted before it would land older
/// news on newer.
///
/// **The stream below is what makes the batch's window wide enough to aim
/// at.** Each arrival holds the deadline open another interval and the
/// ceiling caps the wait, so an ask sent mid-stream lands while updates are
/// certainly held - and what is held goes out first. A connection starved for
/// longer than the stream would leave the batch unopened, which is the one
/// way this reads short rather than the code reading wrong.
#[tokio::test]
async fn a_batch_waiting_when_a_client_asks_goes_out_ahead_of_the_answer() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    snapshot_answering(&mut socket).await;

    let held = "held for the answer";
    for step in 0..12 {
        fleet.emit(SessionUpdate::ConnectionFailed {
            key: lead_seat(),
            message: if step == 0 { held.to_owned() } else { format!("filler {step}") },
            fatal: false,
        });
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;

    let (subject, _, passed) = snapshot_answering(&mut socket).await;
    assert!(
        passed
            .iter()
            .any(|update| matches!(update, SessionUpdate::ConnectionFailed { message, .. }
            if message == held)),
        "the answer to a subscribe is composed after what the core has already said, so the held \
         update arrives ahead of the {subject:?} snapshot rather than behind it: {passed:?}",
    );
}

/// One token append for the fixture seat, as the CLI sends them: the running
/// value the block has reached and the growth since the previous event.
fn a_token_append(running: u64, delta: i64) -> SessionUpdate {
    SessionUpdate::ChatAppended {
        key: lead_seat(),
        origin: None,
        source: None,
        msg: forge_primitives::Message::ThinkingTokens {
            estimated_tokens: running,
            estimated_tokens_delta: delta,
            uuid: format!("tokens-{running}"),
            session_id: "s".to_owned(),
            extras: serde_json::Map::new(),
        },
    }
}

/// One update, said shortly enough to compare a run of them.
fn saying(update: &SessionUpdate) -> String {
    match update {
        SessionUpdate::ChatAppended {
            msg:
                forge_primitives::Message::ThinkingTokens {
                    estimated_tokens,
                    estimated_tokens_delta,
                    ..
                },
            ..
        } => format!("{estimated_tokens_delta} grown to {estimated_tokens}"),
        SessionUpdate::TurnCancelled { .. } => "cancelled".to_owned(),
        SessionUpdate::ConnectionFailed { message, .. } => message.clone(),
        other => format!("{other:?}"),
    }
}

/// What a storm is made of: consecutive token appends inside one batch reach
/// a client as ONE update carrying what they grew by, so the frame count a
/// page sees drops with the arrival rate. A run stops at the first frame that
/// is not a counter and starts again after it.
///
/// The evidence that the others were folded is ORDER, never a timeout: what
/// follows a merged update is the next thing the core emitted, so a leftover
/// append would have to arrive where that is.
#[tokio::test]
async fn a_run_of_token_appends_reaches_a_client_as_one_update() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: false,
            review: false,
        },
    )
    .await;
    let ServerMessage::Snapshot { .. } = next_server(&mut socket).await else {
        panic!("a seat that exists is answered with its snapshot")
    };

    // Two runs of two, with a frame that is not a counter between them, and
    // two thinking blocks' worth inside the first: the running value restarts
    // at the second block, so the deltas are the turn's estimate and the last
    // running value is not the total.
    for (running, delta) in [(200u64, 200i64), (250, 50)] {
        fleet.emit(a_token_append(running, delta));
    }
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });
    for (running, delta) in [(30u64, 30i64), (45, 15)] {
        fleet.emit(a_token_append(running, delta));
    }
    let sentinel = "after the runs";
    fleet.emit(SessionUpdate::ConnectionFailed {
        key: lead_seat(),
        message: sentinel.to_owned(),
        fatal: false,
    });

    let mut heard: Vec<String> = Vec::new();
    loop {
        let update = update_until(&mut socket, "a frame this test emitted", |update| {
            matches!(
                update,
                SessionUpdate::ChatAppended {
                    msg: forge_primitives::Message::ThinkingTokens { .. },
                    ..
                } | SessionUpdate::TurnCancelled { .. }
                    | SessionUpdate::ConnectionFailed { .. }
            )
        })
        .await;
        let barrier = matches!(&update, SessionUpdate::ConnectionFailed { message, .. } if message == sentinel);
        heard.push(saying(&update));
        if barrier {
            break;
        }
    }

    let owed = vec![
        "250 grown to 250".to_owned(),
        "cancelled".to_owned(),
        "45 grown to 45".to_owned(),
        sentinel.to_owned(),
    ];
    assert_eq!(
        heard, owed,
        "each run crosses as one frame carrying its own sum, with the frame that split them \
         where it was and nothing left over",
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
            ClientMessage::Subscribe {
                what: Subject::Session(lead_seat()),
                answering: true,
                review: false,
            },
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

/// One unsubscribe on a seat that was subscribed twice: the other
/// subscription stands. A `retain` - the natural simplification - drops every
/// copy, so the client that asked twice and unsubscribed once stops hearing a
/// seat it is still showing.
#[tokio::test]
async fn one_unsubscribe_leaves_the_seats_other_subscription() {
    let (url, fleet, state) = a_server_with_state().await;
    // The page below is the barrier, so the seat has to be able to answer
    // one: a `more` on a seat with no held conversation is refused rather
    // than answered with an empty page, because an empty page carries
    // `cursor: null` and a client reads that as the end of the history.
    let rows: Vec<String> = (0..2).map(a_turns_rows).collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");
    fleet
        .hold_conversation(&state, "TestOrg", "proj", "lead")
        .expect("the seat's conversation is held");
    let mut socket = connect(&url).await;

    for _ in 0..2 {
        send(
            &mut socket,
            ClientMessage::Subscribe {
                what: Subject::Session(lead_seat()),
                answering: false,
                review: false,
            },
        )
        .await;
        let ServerMessage::Snapshot { .. } = next_server(&mut socket).await else {
            panic!("a seat that exists is answered with its snapshot")
        };
    }
    send(&mut socket, ClientMessage::Unsubscribe { what: Subject::Session(lead_seat()) }).await;
    // A barrier, and the test is a race without it: the unsubscribe and the
    // emit below are both in flight at once, so whichever the connection's
    // select happens to take first decides the answer. Messages are handled in
    // order, so the page's answer proves the unsubscribe was handled before
    // the emit.
    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 1 })
        .await;
    let ServerMessage::Page { .. } = next_server(&mut socket).await else {
        panic!("the page is the barrier that proves the unsubscribe was handled")
    };

    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });

    let msg = next_server(&mut socket).await;
    assert!(
        matches!(&msg, ServerMessage::Update { update, .. } if matches!(**update, SessionUpdate::TurnCancelled { .. })),
        "the subscription the client still holds hears the seat it is showing: {msg:?}",
    );
}

/// A subscription does not outlive its socket, which on a long-lived server
/// is a subscription-shaped leak.
#[tokio::test]
async fn a_dropped_socket_leaves_no_subscription_behind() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    snapshot_answering(&mut socket).await;
    // Counted WITH this socket attached, so the transport's own fold - which
    // attaches on a task of its own, a scheduling hop after the server
    // starts - is in the number either way. What the assertion below waits
    // for is that number FALLING, which is this socket letting go.
    let attached = fleet.subscriber_count();
    assert!(attached >= 1, "precondition: something is attached");

    // Closed politely rather than dropped: the server is told the client is
    // going, which is the path a page navigating away takes.
    socket.close(None).await.expect("the client says goodbye");
    drop(socket);

    assert!(
        wait_for_the_server_to_notice(&fleet, attached).await,
        "a subscription must not outlive its socket: {attached} attached, {} still",
        fleet.subscriber_count(),
    );
}

/// A delivery reaches a socket client as the turn it draws, and then as the
/// typed update it came from.
///
/// The CLI does not echo a prompt it was handed on stdin, so the wire carries
/// nothing a view could draw and a page drawing only frames shows the
/// assistant answering something nobody saw. The server forges that turn
/// before it reaches any client, so a client no longer has to do it in its
/// own process.
///
/// The role is `answering: false` on purpose: a client with no dock to reply
/// from is still shown the turn, and the forge must not be what makes an
/// observer look like an answerer.
#[tokio::test]
async fn a_delivery_is_sent_as_a_frame_and_then_as_its_typed_update() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: false,
            review: false,
        },
    )
    .await;
    snapshot_answering(&mut socket).await;

    fleet.emit(SessionUpdate::CronPromptAppended {
        key: lead_seat(),
        text: "run the morning summary".to_owned(),
        uuid: "cap-cron".to_owned(),
        cron_id: "c1".to_owned(),
        description: Some("Morning summary".to_owned()),
    });

    let ServerMessage::Update { update, .. } = next_server(&mut socket).await else {
        panic!("a delivery a view draws as a turn has to reach the client drawing it")
    };
    let SessionUpdate::ChatAppended { key, msg, .. } = *update else {
        panic!("the frame is what a view draws, and it goes ahead of the typed update")
    };
    assert_eq!(key, lead_seat(), "the frame is addressed to the seat the delivery went to");
    let forge_primitives::Message::User { message, .. } = msg else {
        panic!("a delivery draws as the user turn the model's prompt was")
    };
    let Some(forge_primitives::ContentBlock::Text { text, .. }) = message.content.first() else {
        panic!("the turn carries the prose the model received")
    };
    assert!(
        text.starts_with("[Cron]"),
        "the prose is what the fold's envelope detection reads back, so it is the \
         forged turn rather than some other user frame: {text}",
    );

    let ServerMessage::Update { update, .. } = next_server(&mut socket).await else {
        panic!("the typed update still goes out beside the frame")
    };
    assert!(
        matches!(*update, SessionUpdate::CronPromptAppended { .. }),
        "a view keeps the typed update too: the frame is what it draws, the update is what \
         it knows",
    );
}

/// The same stream carries the whole fleet, so a delivery to another seat
/// draws nothing here.
///
/// The forged frame carries the delivery's own slot, so the routing that keeps
/// another seat's news out of this connection has to keep its frame out too -
/// a frame reaching the wrong seat is a client showing one seat's cron fire
/// inside another seat's conversation.
#[tokio::test]
async fn a_delivery_to_another_seat_draws_nothing_on_this_one() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: false,
            review: false,
        },
    )
    .await;
    snapshot_answering(&mut socket).await;

    fleet.emit(SessionUpdate::CronPromptAppended {
        key: SessionSlot::for_label("TestOrg", "proj", Some("w1")),
        text: "run the morning summary".to_owned(),
        uuid: "cap-cron-2".to_owned(),
        cron_id: "c2".to_owned(),
        description: None,
    });
    // The evidence is ORDER, never a timeout: a frame forged for the other
    // seat would have to arrive ahead of an update this connection does hear,
    // and waiting for it NOT to arrive would hang on the correct behaviour.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });

    let msg = next_server(&mut socket).await;
    assert!(
        matches!(&msg, ServerMessage::Update { update, .. } if matches!(**update, SessionUpdate::TurnCancelled { .. })),
        "a delivery for another seat must not draw on this one: {msg:?}",
    );
}

/// The text of a user turn, when the frame is one: the first text block it
/// carries, wherever it sits among the attachments.
fn user_text(msg: &forge_primitives::Message) -> Option<String> {
    let forge_primitives::Message::User { message, .. } = msg else {
        return None;
    };
    message.content.iter().find_map(|block| match block {
        forge_primitives::ContentBlock::Text { text, .. } => Some(text.clone()),
        _ => None,
    })
}

/// The media type of the image a user turn carries, when the frame is one.
fn user_image_mime(msg: &forge_primitives::Message) -> Option<String> {
    let forge_primitives::Message::User { message, .. } = msg else {
        return None;
    };
    message.content.iter().find_map(|block| match block {
        forge_primitives::ContentBlock::Image { source, .. } => {
            source.get("media_type")?.as_str().map(str::to_owned)
        }
        _ => None,
    })
}

/// A prompt one client sends draws for every other client on that seat, and
/// for the one that sent it.
///
/// The CLI queues a prompt a client hands it and never echoes it on
/// stream-json, so nothing on the wire carries the words the reader said: a
/// second viewer watches the assistant answer something nobody saw, and the
/// sender itself draws nothing of its own. The send is the one place the words
/// are known, so the frame is forged from it.
///
/// The sender is included on purpose. A view draws the user's own bubble
/// from that same frame for exactly this reason, so withholding the
/// frame from the sender would be the client inventing a rule the wire
/// does not have.
#[tokio::test]
async fn a_prompt_a_client_sends_draws_for_every_client_on_that_seat() {
    let (url, fleet) = a_server().await;
    fleet.install_agent("TestOrg", "proj", "lead");
    fleet.intercept_dispatch();

    let mut sender = connect(&url).await;
    let mut watcher = connect(&url).await;
    // A third view on the same seat, watching one of the other two send.
    let mut onlooker = connect(&url).await;
    for socket in [&mut sender, &mut watcher, &mut onlooker] {
        send(
            socket,
            ClientMessage::Subscribe {
                what: Subject::Session(lead_seat()),
                answering: true,
                review: false,
            },
        )
        .await;
        let ServerMessage::Snapshot { .. } = next_server(socket).await else {
            panic!("a seat that exists is answered with its snapshot")
        };
    }

    // **The attachment the prompt carries rides the frame too.** The read
    // path draws it from the transcript's queued copy, so a live row without
    // it shows words alone until the page re-reads - and this frame is the
    // live path's only carrier.
    let image =
        ImageAttachment { data: "iVBORw0KGgo=".to_owned(), mime_type: "image/png".to_owned() };
    // Malformed attachments are sent too, one per arm `validate_image`
    // refuses: the sender drops them before the model sees them, so the live
    // row must not draw one that disappears on reload.
    let wrong_mime =
        ImageAttachment { data: "iVBORw0KGgo=".to_owned(), mime_type: "text/plain".to_owned() };
    let bad_base64 =
        ImageAttachment { data: "not base64!".to_owned(), mime_type: "image/png".to_owned() };
    send(
        &mut sender,
        ClientMessage::Command {
            command: Box::new(Command::Prompt {
                key: lead_seat(),
                text: "hello there".to_owned(),
                attachments: vec![image.clone(), wrong_mime, bad_base64],
            }),
            reply_to: None,
        },
    )
    .await;

    let drawn = |update: &SessionUpdate| {
        matches!(update, SessionUpdate::ChatAppended {
            msg,
            origin: Some(forge_workspace::PromptOrigin::View),
            // **And the source survives the socket**: a client arms its follow
            // off this field, so a frame that lost it in transit would draw
            // the reader's own words without bringing the foot under them
            // (#2198).
            source: Some(forge_workspace::PromptSource::You),
            ..
        } if user_text(msg).as_deref() == Some("hello there")
            && user_image_mime(msg).as_deref() == Some(image.mime_type.as_str()))
    };
    let first = update_until(
        &mut watcher,
        "the prompt a client sent, drawn for another client with the attachment it carried",
        drawn,
    )
    .await;
    let SessionUpdate::ChatAppended { msg, .. } = &first else {
        panic!("a prompt draws as a chat frame: {first:?}")
    };
    let forge_primitives::Message::User { message, .. } = msg else {
        panic!("a prompt draws as the user turn it was: {msg:?}")
    };
    let Some(forge_primitives::ContentBlock::Image { source, extras }) = message
        .content
        .iter()
        .find(|block| matches!(block, forge_primitives::ContentBlock::Image { .. }))
    else {
        panic!("the live frame carries the attachment the prompt was sent with: {message:?}")
    };
    // **The payload never rides the live frame.** The frame is kept and
    // re-served by every read of the row, so an image block here carries what
    // a row prints - the mime and the measured size - exactly as the read
    // path's stripped copy does.
    assert_eq!(source.get("data"), None, "the live frame carries no base64 payload: {source}");
    // The literal, not the helper the implementation calls: "iVBORw0KGgo=" is
    // twelve base64 characters, eight bytes once the padding is out.
    assert_eq!(
        extras.get("forge_bytes").and_then(serde_json::Value::as_u64),
        Some(8),
        "the row prints the eight bytes the payload weighed",
    );
    assert_eq!(
        message.content.len(),
        2,
        "only the attachment the sender kept draws, beside the words: {message:?}",
    );
    // **The order the echo documents**: the images first and the words last,
    // the array the prompt was sent as - a text-first echo draws the same row
    // but stops matching the transcript's own copy of the message.
    assert!(
        matches!(message.content.first(), Some(forge_primitives::ContentBlock::Image { .. })),
        "the attachment leads the echo: {message:?}",
    );
    assert!(
        matches!(message.content.last(), Some(forge_primitives::ContentBlock::Text { .. })),
        "the words close it: {message:?}",
    );
    update_until(
        &mut sender,
        "the prompt a client sent, drawn for the client that sent it with the attachment it carried",
        drawn,
    )
    .await;
    update_until(
        &mut onlooker,
        "a third view's send, drawn for this one with the attachment it carried",
        drawn,
    )
    .await;

    // And exactly once: the evidence is ORDER, never a timeout,a nd a second
    // forged frame would have to arrive ahead of an update every subscriber
    // hears. Two connections on one seat both forging is the shape this
    // catches.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });
    let msg = next_server(&mut onlooker).await;
    assert!(
        matches!(&msg, ServerMessage::Update { update, .. } if matches!(**update, SessionUpdate::TurnCancelled { .. })),
        "one send draws one frame per view: {msg:?}",
    );
}

/// The frame name a view reads a seat's stream by: its type, and for a
/// `system` frame the subtype that says which one it is.
fn frame_name(msg: &forge_primitives::Message) -> String {
    match msg {
        forge_primitives::Message::System { subtype, .. } => format!("system/{subtype}"),
        forge_primitives::Message::ThinkingTokens { .. } => "system/thinking_tokens".to_owned(),
        forge_primitives::Message::Assistant { .. } => "assistant".to_owned(),
        forge_primitives::Message::User { .. } => "user".to_owned(),
        forge_primitives::Message::Result { .. } => "result".to_owned(),
        other => format!("{other:?}"),
    }
}

/// One turn that thinks, frame by frame, as the CLI sends it.
///
/// The shapes, the order and the deltas are the 2.1.280 baseline's
/// (`permission_deny`): a thinking block's four counters land before the
/// message that carries the thought.
fn a_thinking_turn() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        (
            "system/init",
            serde_json::json!({
                "type": "system",
                "subtype": "init",
                "session_id": "s",
                "model": "claude-opus-5",
            }),
        ),
        (
            "system/thinking_tokens",
            serde_json::json!({
                "type": "system",
                "subtype": "thinking_tokens",
                "estimated_tokens": 50,
                "estimated_tokens_delta": 50,
                "uuid": "t1",
                "session_id": "s",
            }),
        ),
        (
            "system/thinking_tokens",
            serde_json::json!({
                "type": "system",
                "subtype": "thinking_tokens",
                "estimated_tokens": 100,
                "estimated_tokens_delta": 50,
                "uuid": "t2",
                "session_id": "s",
            }),
        ),
        (
            "system/thinking_tokens",
            serde_json::json!({
                "type": "system",
                "subtype": "thinking_tokens",
                "estimated_tokens": 150,
                "estimated_tokens_delta": 50,
                "uuid": "t3",
                "session_id": "s",
            }),
        ),
        (
            "system/thinking_tokens",
            serde_json::json!({
                "type": "system",
                "subtype": "thinking_tokens",
                "estimated_tokens": 243,
                "estimated_tokens_delta": 93,
                "uuid": "t4",
                "session_id": "s",
            }),
        ),
        (
            "assistant",
            serde_json::json!({
                "type": "assistant",
                "uuid": "a1",
                "session_id": "s",
                "message": {
                    "id": "m1",
                    "role": "assistant",
                    "model": "claude-opus-5",
                    "content": [{ "type": "thinking", "thinking": "weighing the two paths" }],
                },
            }),
        ),
        (
            "assistant",
            serde_json::json!({
                "type": "assistant",
                "uuid": "a2",
                "session_id": "s",
                "message": {
                    "id": "m2",
                    "role": "assistant",
                    "model": "claude-opus-5",
                    "content": [{ "type": "text", "text": "here is the answer" }],
                },
            }),
        ),
        (
            "result",
            serde_json::json!({
                "type": "result",
                "subtype": "success",
                "is_error": false,
                "uuid": "r1",
                "session_id": "s",
                "duration_ms": 1,
                "duration_api_ms": 1,
                "num_turns": 1,
            }),
        ),
    ]
}

/// A turn that thinks reaches a client watching its seat, frame for frame
/// with each run of token appends folded into the one frame it draws as.
///
/// **The instrument is the socket, and the answer is the pair of counts.**
/// A view draws its thinking bar - a running row carrying the spinner,
/// the elapsed clock and `thinking N` - from the frames the CLI sends, and a
/// client watching the same seat is owed the same turn: the view reads
/// every frame, and the connection folds a run of consecutive token appends
/// into one frame carrying what they grew by. Both views draw a turn's
/// estimate as the SUM of the deltas rather than the last running value,
/// which is what makes the fold a fold and not a drop - so a client draws
/// the same number off fewer frames, and what it is owed is `owed`: the sent
/// sequence with each run standing as one entry, and the sum of every delta
/// behind it. `seen` against `owed` is the denominator, and the second socket
/// is the control that says the count can come back short: it hears the
/// barrier every subscriber hears and none of the turn.
#[tokio::test]
async fn a_thinking_turn_reaches_a_client_watching_its_seat_frame_for_frame() {
    let (url, fleet) = a_server().await;

    let mut watched = connect(&url).await;
    // The subjects a session page's own connection watches: the shell holds
    // the home for its whole life, and the page holds the seat it draws.
    send(
        &mut watched,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    snapshot_answering(&mut watched).await;
    send(
        &mut watched,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    // The attach's own read, and nothing else: the seat carries no mark for
    // showing it to spend, so no home follows. The turn's frames below are
    // read against a clean stream.
    snapshot_answering(&mut watched).await;

    // The control: a connection that watches the home and no seat. The
    // fleet's classification is what keeps the conversation off this
    // subscription, so the same reads that count `watched`'s frames must
    // count none here - a check that could not come back empty would prove
    // nothing about the count it makes.
    let mut home_only = connect(&url).await;
    send(
        &mut home_only,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    snapshot_answering(&mut home_only).await;

    let turn = a_thinking_turn();
    let sent: Vec<&str> = turn.iter().map(|(name, _)| *name).collect();
    // Every token the CLI reported, which the client's one frame for the run
    // has to account for: a fold that lost a frame would show up as a short
    // total rather than as a missing name, the run's name being one either
    // way.
    let grown: i64 = turn
        .iter()
        .filter_map(|(_, value)| {
            value.get("estimated_tokens_delta").and_then(serde_json::Value::as_i64)
        })
        .sum();
    for (_, value) in turn {
        fleet.emit(SessionUpdate::ChatAppended {
            key: lead_seat(),
            msg: serde_json::from_value(value).expect("parse a frame of the turn"),
            origin: None,
            source: None,
        });
    }
    // A barrier: every subscriber hears this one, so a read that stops on it
    // has read everything the turn put ahead of it. The evidence is ORDER,
    // never a timeout - waiting for a frame NOT to arrive would hang on the
    // correct behaviour, and would read a dropped frame as a pass.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });

    let mut seen: Vec<String> = Vec::new();
    let mut heard: i64 = 0;
    loop {
        match next_server(&mut watched).await {
            ServerMessage::Update { update, .. } => match *update {
                SessionUpdate::ChatAppended { key, msg, .. } => {
                    assert_eq!(key, lead_seat(), "another seat's frame reached this one");
                    if let forge_primitives::Message::ThinkingTokens {
                        estimated_tokens_delta,
                        ..
                    } = &msg
                    {
                        heard += estimated_tokens_delta;
                    }
                    seen.push(frame_name(&msg));
                }
                SessionUpdate::TurnCancelled { .. } => break,
                _ => {}
            },
            other => panic!(
                "a seat's subscription carries updates, got {other:?} - a snapshot here is an \
                 attach re-sending the home it should only re-send for a mark it spent",
            ),
        }
    }
    // What the sent sequence is owed as: every frame as itself, and each run
    // of consecutive token appends as the one entry the connection folds it
    // into.
    let mut owed: Vec<&str> = Vec::new();
    for name in &sent {
        if *name == "system/thinking_tokens" && owed.last() == Some(&"system/thinking_tokens") {
            continue;
        }
        owed.push(name);
    }
    assert_eq!(
        seen,
        owed,
        "frames seen ({}) against the frames owed ({}): the socket is the instrument, and \
         a frame missing, moved or added here is the server drawing the turn differently \
         from the terminal",
        seen.len(),
        owed.len(),
    );
    assert_eq!(
        heard, grown,
        "the deltas a client hears ({heard}) against every token the CLI reported ({grown}): \
         the fold is a fold only while its sum is whole, and a short one is a token the \
         client never drew",
    );

    let mut leaked: Vec<String> = Vec::new();
    loop {
        match next_server(&mut home_only).await {
            ServerMessage::Update { update, .. } => match *update {
                SessionUpdate::ChatAppended { msg, .. } => leaked.push(frame_name(&msg)),
                SessionUpdate::TurnCancelled { .. } => break,
                _ => {}
            },
            other => panic!("a home subscription carries updates, got {other:?}"),
        }
    }
    // The turn's own words never reach a home: its rows state a seat's
    // lifecycle and no row shows a word of its conversation. The result is
    // the one frame that does, and it is a row's news - the completion a
    // page not showing the seat draws a diamond for - so the control is that
    // it arrives ALONE, with every frame that carries the bar not behind it.
    assert_eq!(
        leaked,
        vec!["result"],
        "a home-only subscription hears the completion and none of the turn: {leaked:?}",
    );
}

/// A send the core refuses draws nothing, in any view.
///
/// It never reached a model, so there is no turn to draw. Drawing it anyway
/// would open a live turn in every view but the sender - the sender at least
/// hears the refusal on its own socket, and those other views hear only the
/// frame, so their turn bar would spin with nothing left to close it.
#[tokio::test]
async fn a_prompt_the_core_refuses_draws_nothing() {
    let (url, fleet) = a_server().await;
    // A seat with a session behind it, so the subscribe is answered - but no
    // dispatch intercept, so the prompt itself is refused.
    fleet.install_agent("TestOrg", "proj", "lead");
    let mut sender = connect(&url).await;
    send(
        &mut sender,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let ServerMessage::Snapshot { .. } = next_server(&mut sender).await else {
        panic!("a seat that exists is answered with its snapshot")
    };

    send(
        &mut sender,
        ClientMessage::Command {
            command: Box::new(Command::Prompt {
                key: lead_seat(),
                text: "hello there".to_owned(),
                attachments: Vec::new(),
            }),
            reply_to: None,
        },
    )
    .await;

    // The refusal first, so the barrier below cannot overtake it: the frame a
    // dispatch emits goes through the fan-out, and a barrier emitted before the
    // refusal could be read ahead of the Error and end the read too early.
    let mut drawn = false;
    loop {
        match next_server_within(&mut sender, 5_000).await {
            Some(ServerMessage::Update { update, .. }) => {
                drawn |= matches!(&*update, SessionUpdate::ChatAppended { msg, .. }
                    if user_text(msg).as_deref() == Some("hello there"));
            }
            Some(ServerMessage::Error { .. }) => break,
            Some(other) => panic!("a send answers with the refusal or the words: {other:?}"),
            None => panic!("waited for the refusal of a send and never heard it"),
        }
    }

    // And now the order proof: a frame from that dispatch would be in the
    // fan-out ahead of this barrier, so reading to the barrier and seeing none
    // is the negative. The evidence is ORDER, never a timeout.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });
    loop {
        match next_server(&mut sender).await {
            ServerMessage::Update { update, .. } => {
                drawn |= matches!(&*update, SessionUpdate::ChatAppended { msg, .. }
                    if user_text(msg).as_deref() == Some("hello there"));
                if matches!(*update, SessionUpdate::TurnCancelled { .. }) {
                    break;
                }
            }
            ServerMessage::Error { .. } => {}
            other => panic!("a send draws, refuses, or says nothing: {other:?}"),
        }
    }
    assert!(!drawn, "a refused prompt reached no model, so no view draws it as a turn");
}

/// The answer to a `more`, with whatever the connection had queued ahead of
/// it read past: the socket is shared with every update the seat emits, and
/// the answer is not necessarily the next message after the ask.
async fn page_answer(socket: &mut Client) -> ServerMessage {
    loop {
        let message = next_server(socket).await;
        if matches!(message, ServerMessage::Page { .. } | ServerMessage::Error { .. }) {
            return message;
        }
    }
}

/// **A `more` below the window's floor, end to end over the socket.** The
/// wiring around it has no other test: a cursor a client sends crosses this
/// arm, the page comes back cut and numbered by it, and the walk continues
/// from the cursor the page hands back - all with the anchors the held window
/// itself named, never a list a test built.
#[tokio::test]
async fn a_more_below_the_floor_is_answered_from_the_transcript() {
    let (url, fleet, state) = a_server_with_state().await;
    fleet.start("TestOrg", "proj").expect("a live lead session");
    // A transcript longer than the window holds, one row a turn, so the held
    // copy drops a prefix of it and the floor lands on a turn's own frame.
    let rows: Vec<String> = (0..5_200)
        .map(|at| {
            // One row in the below-floor range carries an oversized record, so
            // the transcript producer's cap is pinned where a held page cannot
            // answer (#2374).
            let record = if at == 1_197 {
                format!(",\"toolUseResult\":{{\"payload\":\"{}\"}}", "x".repeat(100_000))
            } else {
                String::new()
            };
            format!(
                "{{\"type\":\"user\",\"uuid\":\"u{at}\",\"session_id\":\"s1\",\
                 \"message\":{{\"role\":\"user\",\"content\":\"turn {at}\"}}{record}}}"
            )
        })
        .collect();
    fleet
        .seed_transcript(
            "TestOrg",
            "proj",
            "lead",
            &rows.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .expect("the session's transcript");
    fleet.hold_conversation(&state, "TestOrg", "proj", "lead").expect("the seat's window");
    let held = state.conversations.get(&lead_seat()).expect("the seat is held");
    let dropped = held.lock().dropped();
    assert_eq!(dropped, 5_200 - 4_000, "precondition: the window dropped its oldest rows");
    assert!(
        held.lock().messages().iter().all(forge_workspace::has_a_transcript_row),
        "precondition: the held copy came from the transcript, so every frame has a row",
    );

    // A cursor below the floor - a client holding pages from before a drop
    // asks from one. **It is answered from the window's own oldest row rather
    // than from the exact frame it names**: the frames the CLI streams and
    // never writes sit between the window's rows, so the two numberings cannot
    // be lined up by arithmetic, and a page that starts at the window's front
    // errs toward serving a row twice rather than toward skipping one. The
    // socket is on the server whose state holds the conversation above, which
    // is the one thing this test cannot get wrong.
    let mut socket = connect(&url).await;
    let asked: usize = 5_200 - 4_000 - 4;
    send(
        &mut socket,
        ClientMessage::More {
            conversation: lead_seat(),
            before: Some(asked.to_string()),
            turns: 4,
        },
    )
    .await;
    let answer = page_answer(&mut socket).await;
    if let ServerMessage::Error { what, why, .. } = &answer {
        panic!("the server refused the more: {what}: {why}");
    }
    let ServerMessage::Page { turns: page, cursor, .. } = answer else {
        panic!("a more below the floor is answered with a page");
    };
    let texts: Vec<String> = page
        .iter()
        .map(|turn| {
            turn.messages
                .first()
                .and_then(|frame| frame["message"]["content"][0]["text"].as_str())
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        texts,
        vec!["turn 1196", "turn 1197", "turn 1198", "turn 1199"],
        "the turns directly below the window's own front, which is where a page below the floor \
         starts",
    );
    let capped = page
        .iter()
        .flat_map(|turn| turn.messages.iter())
        .find(|frame| frame["uuid"] == "u1197")
        .expect("the oversized record's frame is in the page below the floor");
    assert_eq!(
        capped["tool_use_result"]["omitted"], true,
        "the transcript producer caps the record it serves: {capped}",
    );
    let cursor = cursor.expect("a cursor below that one, so the walk carries on");
    assert!(
        cursor.starts_with("r:"),
        "and it is a row cursor - the byte the page stopped at - rather than a frame number: \
         {cursor}",
    );

    // The walk, one page down: the client asks with the cursor it was handed.
    send(
        &mut socket,
        ClientMessage::More { conversation: lead_seat(), before: Some(cursor.clone()), turns: 4 },
    )
    .await;
    let ServerMessage::Page { turns: below, cursor: next, .. } = page_answer(&mut socket).await
    else {
        panic!("the page below is answered too");
    };
    assert_eq!(below.len(), 4, "the walk serves the page below the one before it");
    let below_texts: Vec<String> = below
        .iter()
        .map(|turn| {
            turn.messages
                .first()
                .and_then(|frame| frame["message"]["content"][0]["text"].as_str())
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        below_texts,
        vec!["turn 1192", "turn 1193", "turn 1194", "turn 1195"],
        "the turns directly above the row the page stopped at",
    );
    let next = next.expect("and a cursor below it");
    assert!(next.starts_with("r:"), "a row cursor too: {next}");
    assert_ne!(next, cursor, "descending rather than repeating");

    // And the floor of the transcript: the cursor naming the file's own first
    // row has nothing above it, which is the one page that ends the walk.
    send(
        &mut socket,
        ClientMessage::More {
            conversation: lead_seat(),
            before: Some("r:0:u0".to_owned()),
            turns: 4,
        },
    )
    .await;
    let ServerMessage::Page { turns: none, cursor: end, .. } = page_answer(&mut socket).await
    else {
        panic!("a cursor at the transcript's first row is still answered with a page");
    };
    assert!(none.is_empty(), "with nothing above it");
    assert!(end.is_none(), "and no cursor, which is what stops a client asking");

    // **The walk crosses the floor without asking twice.** A client that has
    // been walking from the newest page reaches the window's start, and the
    // page there hands the floor back as its cursor - so the next ask lands
    // in the transcript and the walk carries on to the session's own first
    // turn rather than stopping at the window.
    let mut asked: Option<String> = None;
    let mut crossed = false;
    let mut pages = 0_usize;
    loop {
        send(
            &mut socket,
            ClientMessage::More { conversation: lead_seat(), before: asked.clone(), turns: 20 },
        )
        .await;
        let ServerMessage::Page { turns: page, cursor, .. } = page_answer(&mut socket).await else {
            panic!("the walk's page {pages} is answered");
        };
        let texts: Vec<String> = page
            .iter()
            .filter_map(|turn| {
                turn.messages
                    .first()
                    .and_then(|frame| frame["message"]["content"][0]["text"].as_str())
                    .map(str::to_owned)
            })
            .collect();
        crossed |= texts.iter().any(|text| text == "turn 1999");
        pages += 1;
        assert!(pages < 400, "the walk reached the transcript's beginning");
        let Some(next) = cursor else {
            assert_eq!(
                texts.first().map(String::as_str),
                Some("turn 0"),
                "and the last page it served runs down to the transcript's own first turn",
            );
            break;
        };
        assert!(asked.as_deref() != Some(next.as_str()), "the cursor descends");
        asked = Some(next);
    }
    assert!(
        crossed,
        "the walk passed the floor's own turn on the way: the window's last page and the \
         transcript's first are one sequence, not two that stop at the seam",
    );

    // **The count is the client's, so it is clamped.** A hundred thousand
    // turns would have the read hand over its whole cap and the page encode
    // every row of it; zero is a page that opens nowhere, whose cursor names
    // the message it was asked with.
    for asked in [100_000_u32, 0] {
        send(
            &mut socket,
            ClientMessage::More { conversation: lead_seat(), before: None, turns: asked },
        )
        .await;
        let ServerMessage::Page { turns: page, .. } = page_answer(&mut socket).await else {
            panic!("a more with {asked} turns is still answered with a page");
        };
        assert!(
            (1..=20).contains(&page.len()),
            "a page of {asked} turns is clamped to the twenty a subscribe carries: {}",
            page.len(),
        );
    }
}

/// **A floor whose front frame has no file row still walks** (#2280).
///
/// The boundary a below-floor page starts from is the copy's oldest frame a
/// row rule says the file carries - but the frames the CLI streams and never
/// writes pass that rule too, and when the window's front was one, the anchor
/// named a row the file does not hold: the search came up empty and the whole
/// page answered `cursor: null`, which a client reads as the end of the
/// history. Live, that made a fresh load of a long seat show only its newest
/// slice and scrolling back reach nothing.
#[tokio::test]
async fn a_more_below_a_floor_whose_front_frame_has_no_row_walks_on() {
    let (url, fleet, state) = a_server_with_state().await;
    fleet.start("TestOrg", "proj").expect("a live lead session");
    // The file: turns 0..199. The copy: one frame the file never carried,
    // ahead of the newest 100 turns, behind a floor that stands above both.
    let rows: Vec<String> = (0..200)
        .map(|at| {
            format!(
                "{{\"type\":\"user\",\"uuid\":\"u{at}\",\"session_id\":\"s1\",\
                 \"message\":{{\"role\":\"user\",\"content\":\"turn {at}\"}}}}"
            )
        })
        .collect();
    fleet
        .seed_transcript(
            "TestOrg",
            "proj",
            "lead",
            &rows.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .expect("the session's transcript");

    let mut messages =
        vec![a_frame(&a_turn_row("streamed-only-1", "a frame the file never carried"))];
    messages.extend(
        (100..200).map(|at| a_frame(&a_turn_row(&format!("u{at}"), &format!("turn {at}")))),
    );
    state.conversations.replace(
        &lead_seat(),
        forge_server::transport::conversation::Conversation::new(messages, 0, 1_200),
    );
    let dropped = state.conversations.get(&lead_seat()).expect("held").lock().dropped();
    assert_eq!(dropped, 1_200, "precondition: a floor above the copy's own front");

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::More {
            conversation: lead_seat(),
            before: Some(dropped.to_string()),
            turns: 4,
        },
    )
    .await;
    let answer = page_answer(&mut socket).await;
    if let ServerMessage::Error { what, why, .. } = &answer {
        panic!("the server refused the more: {what}: {why}");
    }
    let ServerMessage::Page { turns: page, cursor, .. } = answer else {
        panic!("a more below the floor is answered with a page");
    };
    let texts: Vec<String> = page
        .iter()
        .map(|turn| {
            turn.messages
                .first()
                .and_then(|frame| frame["message"]["content"][0]["text"].as_str())
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(
        texts,
        vec!["turn 96", "turn 97", "turn 98", "turn 99"],
        "the page below a front frame the file cannot name starts at the oldest row it can",
    );
    let cursor = cursor.expect("a cursor below that one, so the walk carries on");
    assert!(cursor.starts_with("r:"), "a row cursor: {cursor}");
}

/// One frame, decoded from the shape the wire carries it in.
fn a_frame(row: &serde_json::Value) -> forge_primitives::Message {
    serde_json::from_value(row.clone()).expect("a frame")
}

/// The user row that opens a turn.
fn a_turn_row(uuid: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "user",
        "uuid": uuid,
        "session_id": "s1",
        "message": {"role": "user", "content": text},
    })
}

/// An assistant row that opens no turn of its own.
fn a_text_row(uuid: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant",
        "uuid": uuid,
        "session_id": "s1",
        "message": {
            "id": format!("m-{uuid}"),
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": text}],
        },
    })
}

/// An assistant row calling a tool, which opens no turn.
fn a_calling_row(uuid: &str, block: usize) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant",
        "uuid": uuid,
        "session_id": "s1",
        "message": {
            "id": format!("m{block}"),
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [{
                "type": "tool_use",
                "id": format!("tu{block}"),
                "name": "Bash",
                "input": {"command": "ls"},
            }],
        },
    })
}

/// The user row carrying a call's result, which opens no turn either.
fn an_answering_row(uuid: &str, block: usize) -> serde_json::Value {
    serde_json::json!({
        "type": "user",
        "uuid": uuid,
        "session_id": "s1",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": format!("tu{block}"),
                "content": "ok",
            }],
        },
    })
}

/// One thinking-token frame: a frame the CLI streams and never writes a row
/// for, so a copy holding it counts a frame no transcript row carries.
fn a_tick_frame(uuid: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "system",
        "subtype": "thinking_tokens",
        "estimated_tokens": 50,
        "estimated_tokens_delta": 50,
        "uuid": uuid,
        "session_id": "s1",
    })
}

/// **A walk pages back through a running turn's own head.**
///
/// A view attaching mid-turn is handed the window and nothing older, and on a
/// busy seat that window is a COUNT cut inside the running turn: the frames
/// between its front and the turn's first message are the turn's older
/// history, held by the transcript as rows and held by nothing else. This
/// walks that history: the newest page's cursor, then down the dropped rows
/// page by page, across the turn's own opening row into the turn before it,
/// and out at the file's own first row.
///
/// **The fixture carries the shape a live window has**: thinking-token frames
/// interleaved with the turn's rows (the CLI streams them and writes no row),
/// enough of them that the count cut lands on a tick and the window's first
/// row sits a frame or two past its front - the numbering the frame-rank span
/// read could not line up, whose divergence answered the walk `history-end`.
#[tokio::test]
async fn a_walk_pages_back_through_a_running_turn_to_its_first_message() {
    // One finished turn, then a running one: its opening row, then work pairs
    // with two thinking-token frames before each - [tick, tick, call, result].
    const BLOCKS: usize = 1_300;

    let (url, fleet, state) = a_server_with_state().await;
    fleet.start("TestOrg", "proj").expect("a live lead session");
    let mut rows: Vec<serde_json::Value> = vec![
        a_turn_row("turn0-user", "turn 0"),
        a_text_row("turn0-answer", "answer 0"),
        a_turn_row("running-open", "the running turn"),
    ];
    let mut history: Vec<forge_primitives::Message> = rows.iter().map(a_frame).collect();
    for block in 0..BLOCKS {
        history.push(a_frame(&a_tick_frame(&format!("t{block}-0"))));
        history.push(a_frame(&a_tick_frame(&format!("t{block}-1"))));
        let call = a_calling_row(&format!("call{block}"), block);
        let result = an_answering_row(&format!("result{block}"), block);
        history.push(a_frame(&call));
        history.push(a_frame(&result));
        rows.push(call);
        rows.push(result);
    }

    // The transcript holds the conversation's ROWS and none of its ticks, and
    // the seat holds the conversation the wire carried - the state a replay of
    // a busy seat's own frames leaves.
    let rows_json: Vec<String> = rows.iter().map(serde_json::Value::to_string).collect();
    let lines: Vec<&str> = rows_json.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &lines).expect("the session's transcript");
    state.conversations.insert(
        &lead_seat(),
        forge_server::transport::conversation::Conversation::new(history, 0, 0),
    );

    // **Preconditions, and they are the live shape**: the window is the newest
    // 4,000 frames, its front is a thinking-token frame (the count cut landed
    // on one), and the first row it carries sits two frames above that front.
    let held = state.conversations.get(&lead_seat()).expect("the seat is held");
    {
        let held = held.lock();
        assert_eq!(held.dropped(), 1_203, "precondition: the window dropped the head of the turn");
        assert!(
            matches!(held.messages()[0], forge_primitives::Message::ThinkingTokens { .. }),
            "precondition: the count cut landed on a thinking-token frame",
        );
        assert!(
            !forge_workspace::has_a_transcript_row(&held.messages()[0]),
            "precondition: which is a frame no transcript row carries",
        );
        assert_eq!(held.messages().len(), 4_000, "precondition: the window is the cap's frames");
    }

    let mut socket = connect(&url).await;

    // The newest page, as a client's refresh asks for it: the window as one
    // turn, whose cursor is the window's own front.
    send(&mut socket, ClientMessage::More { conversation: lead_seat(), before: None, turns: 20 })
        .await;
    let ServerMessage::Page { turns: newest, cursor, .. } = page_answer(&mut socket).await else {
        panic!("the newest page is answered");
    };
    assert_eq!(newest.len(), 1, "the window is one running turn");
    assert_eq!(newest[0].messages.len(), 4_000, "carrying every frame the seat holds");
    assert_eq!(cursor.as_deref(), Some("1203"), "and the cursor above it names the window's front");

    // The walk: every page below the floor, taken with the cursor the page
    // before it handed back, until a page ends the history.
    let mut asked = cursor;
    let mut served: Vec<Vec<String>> = Vec::new();
    while let Some(cursor) = asked.clone() {
        send(
            &mut socket,
            ClientMessage::More {
                conversation: lead_seat(),
                before: Some(cursor.clone()),
                turns: 4,
            },
        )
        .await;
        let answer = page_answer(&mut socket).await;
        if let ServerMessage::Error { what, why, .. } = &answer {
            panic!("the walk's page below {cursor} was refused: {what}: {why}");
        }
        let ServerMessage::Page { turns: page, cursor: next, .. } = answer else {
            panic!("the walk's page below {cursor} is answered with a page");
        };
        assert!(
            !page.is_empty(),
            "a page below the floor carries the turn's older rows rather than nothing \
             (asked with {cursor})",
        );
        served.push(
            page.iter()
                .flat_map(|turn| turn.messages.iter())
                .map(|frame| frame["uuid"].as_str().unwrap_or_default().to_owned())
                .collect(),
        );
        assert!(served.len() < 100, "the walk reaches the file's own first row");
        asked = next;
    }

    // **Every row below the window's front, once, in conversation order.**
    // Each page is a run of the conversation's own order, and the pages arrive
    // newest first - so reading the pages backward and concatenating is the
    // whole span the window dropped, with nothing skipped and nothing served
    // twice. That span is the running turn's head, then the turn before it.
    let mut expected: Vec<String> =
        vec!["turn0-user".to_owned(), "turn0-answer".to_owned(), "running-open".to_owned()];
    for block in 0..300 {
        expected.push(format!("call{block}"));
        expected.push(format!("result{block}"));
    }
    let mut ordered: Vec<String> = Vec::new();
    for page in served.iter().rev() {
        ordered.extend(page.iter().cloned());
    }
    assert!(served.len() > 1, "the walk chained below the floor rather than ending on one page");
    assert_eq!(
        ordered, expected,
        "the walk serves every row the window dropped, in order, across the running turn's own \
         opening row into the turn before it",
    );
    assert_eq!(asked, None, "and ends at the file's own first row");
}

/// **A client built before v6 still reaches this server, and this is what a
/// live machine depends on.** The server gates nothing on the protocol -
/// nothing a client sends carries the version it speaks - so a subscribe
/// written before the `browser` field existed decodes and is answered like
/// any other. What such a client does with a NEWER greeting is the client's
/// business, and the v5 client's answer is to draw the skew sentence naming
/// the release to install rather than to connect against a shape it cannot
/// read.
#[tokio::test]
async fn a_subscribe_from_a_client_that_predates_v6_is_answered() {
    let mut socket = connected().await;
    socket
        .send(Message::Text(r#"{"kind":"subscribe","what":"home","answering":false}"#.into()))
        .await
        .expect("the older subscribe sends");

    let (subject, _, _) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Home, "the field v6 added is absent, and the ask is answered");
}

/// How long one frame of the giant handover may take to cross: **the batch's
/// own ceiling for an update it is holding**
/// (`forge_server::transport::batch::FLUSH_CEILING`, the cap on how long a
/// frame can sit in a batch before it goes out), plus the 5 s settle margin
/// the `spawn_wait()` helpers in forge-agent's `forge_sdk_worker.rs` and
/// forge-workspace's `spawn.rs` add the same way (`INIT_HANDSHAKE_TIMEOUT + 5
/// s` - a hand-copied margin, not an importable one). A wait over real work
/// must derive from the code's own budget for it (the load family, #2273),
/// and this is that budget: the crossing measures tens of milliseconds, and
/// what a loaded machine is free to eat is the settle margin.
const FRAME_WAIT: std::time::Duration =
    forge_server::transport::batch::FLUSH_CEILING.saturating_add(std::time::Duration::from_secs(5));

/// The ceiling on a transport test's wait for the fold's own mark. The wait
/// wakes on the mark, so it returns as soon as the fold is done however
/// loaded the machine is; the 60 s is a wedge ceiling, set above the giant
/// handover's measured weigh (seconds unloaded, tens under a starved process)
/// so a fold that is merely slow does not false-red on it. Sized well under
/// nextest's outer supervision (30 s slow-timeout, terminate after 4), so a
/// wedged fold reds by the assertion's own name rather than the harness's
/// kill.
const SEED_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The frame carrying the next update satisfying `wants`, with the frames that
/// are not it passed over for the reason [`snapshot_answering`] gives.
///
/// Handed back as TEXT rather than as the typed update: the size of the frame
/// is half of what this is for, and a decode on the way to the assertion
/// would hide it.
async fn frame_until(
    socket: &mut Client,
    what: &str,
    mut wants: impl FnMut(&SessionUpdate) -> bool,
) -> String {
    let wait = u64::try_from(FRAME_WAIT.as_millis()).expect("the frame wait fits in millis");
    for _ in 0..16 {
        let text = next_text_within(socket, wait)
            .await
            .unwrap_or_else(|| panic!("waited for {what}, and the server said nothing"));
        let Ok(ServerMessage::Update { update, .. }) = serde_json::from_str::<ServerMessage>(&text)
        else {
            continue;
        };
        if wants(&update) {
            return text;
        }
    }
    panic!("waited for {what}, and heard 16 frames without it");
}

/// The messages a giant seat's resume hands over: `messages` rows, every one
/// carrying `body` bytes, the shape a transcript read's whole history has.
fn a_giant_history(messages: usize, body: usize) -> Vec<forge_primitives::Message> {
    let text = "x".repeat(body);
    (0..messages)
        .map(|at| {
            serde_json::from_value::<forge_primitives::Message>(serde_json::json!({
                "type": "user",
                "uuid": format!("u{at}"),
                "message": {"role": "user", "content": text},
                "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
            }))
            .expect("a user frame")
        })
        .collect()
}

/// What `messages` weigh as the JSON a page carries them as - the test's own
/// scales, so the ceiling is checked against the frames rather than itself.
fn weigh(messages: &[forge_primitives::Message]) -> usize {
    messages.iter().map(|message| serde_json::to_vec(message).expect("a frame encodes").len()).sum()
}

/// The seat's session facts, as every history-carrying update carries them.
fn a_current_model() -> forge_primitives::runtime::CurrentModel {
    forge_primitives::runtime::CurrentModel {
        requested_id: None,
        resolved_id: "claude-opus-5".to_owned(),
        display_name_short: "Opus".to_owned(),
        display_name_long: "Opus 5".to_owned(),
        catalog_id: None,
        supports_effort: false,
        supported_effort_levels: Vec::new(),
        supports_auto_mode: None,
        supports_adaptive_thinking: None,
        is_authoritative: false,
    }
}

/// **A giant seat's resume crosses the socket bounded, and the connection
/// survives it** (#2150).
///
/// The live incident: a live seat whose transcript was 2.5GiB resumed, and
/// its whole history rode ONE `Connected` update through the socket - 2.46GiB
/// in a single frame, refused at the client's 256MiB cap, which cost the
/// connection and left the server having built and serialized all of it. The
/// conversation is kept in a window and read in pages, so the bypass was
/// this path: the update carried the raw read. A client answers these three
/// variants with a READ (the subscribe snapshot's page and `more` above it),
/// so what crosses is the seat's session facts - a frame with no transcript
/// in it cannot grow with the transcript, whatever the seat weighs.
///
/// The history here is ~90 MB rather than two gigabytes: the property is
/// that the frame does NOT scale with it, and a bound asserted at a size a
/// cap-sized frame would blow is the same property the live seat broke. It is
/// weighed, not counted, that binds here - 500 frames cross the byte ceiling
/// while the count cap is nowhere near - so the cut the session task applies
/// (`Window::tail_of`, the shipped wiring) and the window the transport seeds
/// from it are both under test.
///
/// **The runtime is production's rather than the default's.** The transport's
/// fold is a task of its own, and the single-threaded default would run it
/// beside the connection: weighing the window (~80 MiB of JSON, measured
/// ~1.4 s a reseed) then sits in the frame's own path, so a per-frame wait
/// would cover the fold's work rather than the frame's crossing (~30 ms
/// measured).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_giant_seats_history_crosses_bounded_and_the_connection_survives() {
    let (url, fleet, state) = a_server_with_state().await;
    let mut socket = connect_unbounded(&url).await;
    let seat = lead_seat();

    // The read a client makes at launch: the seat's own subscribe.
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Session(seat.clone()), "precondition: the client shows the seat");

    // The resume's handover as the session task builds it: the read is the
    // whole transcript, the cut is the task's own (`Window::tail_of`, both
    // ceilings), and the update carries the newest window of it plus the cut
    // the window left behind.
    let read = a_giant_history(500, 180 * 1024);
    let (window, dropped) = forge_workspace::conversation_window::Window::tail_of(&read);
    assert!(
        window.len() < read.len(),
        "precondition: the byte ceiling bound the window ({} of {} frames)",
        window.len(),
        read.len(),
    );
    let window_bytes = weigh(window.messages());
    assert!(
        window_bytes
            <= forge_workspace::conversation_window::CONVERSATION_CAP_BYTES
                + forge_workspace::conversation_window::CONVERSATION_SLACK_BYTES,
        "precondition: and the window is inside the ceiling's slack: {window_bytes} bytes",
    );
    let window = window.messages().to_vec();
    let session_id = SessionId::new("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45");

    fleet.emit(SessionUpdate::Connected {
        key: seat.clone(),
        session_id: session_id.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: window.clone(),
        dropped,
        compaction_count: 0,
    });
    let frame = frame_until(
        &mut socket,
        "the seat's connect",
        |update| matches!(update, SessionUpdate::Connected { key, .. } if key == &seat),
    )
    .await;
    assert_transcript_free(&frame, "connect");

    fleet.emit(SessionUpdate::SessionReplaced {
        key: seat.clone(),
        session_id: session_id.clone(),
        cwd: "/tmp".to_owned(),
        current_model: a_current_model(),
        available_models: Vec::new(),
        mode: None,
        history: window.clone(),
        dropped,
        compaction_count: 0,
    });
    let frame = frame_until(
        &mut socket,
        "the seat's replacement",
        |update| matches!(update, SessionUpdate::SessionReplaced { key, .. } if key == &seat),
    )
    .await;
    assert_transcript_free(&frame, "replacement");

    // **One live frame after the handover, and it is the wait's mark.** An
    // epoch frame resets the fold's watermark rather than advancing it, and
    // each seal re-mints from 1, so there is no number on the handover itself
    // to wake on. This row is the run's first counted frame, and reaching
    // `1` proves both seals were applied.
    fleet.emit(SessionUpdate::ChatAppended {
        key: seat.clone(),
        origin: None,
        source: None,
        msg: a_said_frame("after the handover"),
    });
    assert!(
        state.conversations.await_consumed(&seat, 1, SEED_WAIT).await,
        "waited for the fold to take the handover, and it never marked it",
    );
    {
        let held = state.conversations.get(&seat).expect("the handover holds the seat");
        let seat_held = held.lock();
        assert_eq!(seat_held.dropped(), dropped, "the seat's floor is the cut it rode in with");
        assert!(
            weigh(seat_held.messages())
                <= forge_workspace::conversation_window::CONVERSATION_CAP_BYTES
                    + forge_workspace::conversation_window::CONVERSATION_SLACK_BYTES,
            "the held copy is inside the byte ceiling's slack: {} bytes over {} frames",
            weigh(seat_held.messages()),
            seat_held.messages().len(),
        );
    }

    // And the connection is still there: another read is answered on it,
    // which the client's own refusal of the frame took with it live.
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Home, answering: true, review: false },
    )
    .await;
    let (subject, ..) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Home, "the connection survived the giant seat's history");
}

/// The frame is the seat's session facts and no transcript, and small enough
/// that no conversation's size can carry it past the client's socket cap.
fn assert_transcript_free(frame: &str, what: &str) {
    assert!(
        frame.len() < 256 * 1024,
        "the {what} crossed as {} bytes: a frame carrying the seat's transcript grows with the \
         seat (2.46GiB on the one #2150 was filed from) and the client's socket refuses past \
         its 256MiB cap",
        frame.len(),
    );
    let Ok(ServerMessage::Update { update, .. }) = serde_json::from_str::<ServerMessage>(frame)
    else {
        panic!("the {what} crosses as an update");
    };
    let carried = match &*update {
        SessionUpdate::Connected { history, .. }
        | SessionUpdate::SessionReplaced { history, .. } => history.len(),
        other => panic!("the {what} is a history-carrying update, and this is {other:?}"),
    };
    assert_eq!(
        carried, 0,
        "the {what} carries the seat's session facts and not its transcript: a client reads \
         the conversation through its page and `more`, which is what it answers these variants \
         with",
    );
}

/// **A connect's window still hands the walk its floor.**
///
/// The update carries a window now rather than the whole read, and a consumer
/// holding the window alone reads it as the conversation's front: the
/// transport's floor - `dropped`, what a page cursor below the window is
/// measured against - used to come out of cutting a whole read down, so a
/// windowed handover carrying no cut would leave the floor at zero. `floor >
/// 0` is the only thing that mints the cursor crossing into the transcript
/// (connection.rs's `more` arm gates on it), so the walk would stop at the
/// window's front and draw "the history ends here" over every turn below it -
/// on exactly the giant seats the window exists for, with nothing failing.
/// The cut rides the update for this, and the walk below proves it end to
/// end.
#[tokio::test]
async fn a_windowed_connect_still_gives_the_walk_its_floor() {
    let (url, fleet, state) = a_server_with_state().await;
    let seat = lead_seat();

    // The transcript the file carries: 100 turns, one row each. The update's
    // window holds only the newest 40, so 60 turns live below its front.
    let rows: Vec<String> = (0..100)
        .map(|at| {
            serde_json::json!({
                "type": "user",
                "uuid": format!("u{at}"),
                "message": {"role": "user", "content": format!("turn {at}")},
                "session_id": "5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45",
            })
            .to_string()
        })
        .collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    fleet.seed_transcript("TestOrg", "proj", "lead", &borrowed).expect("the transcript seeds");

    let dropped = 60;
    let window: Vec<forge_primitives::Message> = rows[dropped..]
        .iter()
        .map(|row| serde_json::from_str(row).expect("a user frame"))
        .collect();

    // The handover, before anyone is watching: the seat is materialised by
    // the UPDATE, so the floor under test is the one the update carried
    // rather than the file read's own. The transport's fold has to be
    // attached first, or the update is held for the next subscriber - the
    // client - and the seat is never seeded by it.
    for _ in 0..200 {
        if fleet.subscriber_count() > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        fleet.emit_and_report(SessionUpdate::Connected {
            key: seat.clone(),
            session_id: SessionId::new("5b1c2d3e-4f50-4a61-b728-9c0d1e2f3a45"),
            cwd: "/tmp".to_owned(),
            current_model: a_current_model(),
            available_models: Vec::new(),
            mode: None,
            history: window,
            dropped,
            compaction_count: 0,
        }),
        "precondition: the transport's fold takes the handover",
    );
    for _ in 0..200 {
        if state.conversations.get(&seat).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        state.conversations.get(&seat).is_some(),
        "precondition: the connect's handover holds the seat",
    );

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(seat.clone()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    assert!(
        data["conversation"]["turns"].as_array().is_some_and(|turns| !turns.is_empty()),
        "the connect seeds the transport's copy, so the seat's read answers its window: {data}",
    );

    // Walk back the way a client does, and the walk has to cross: a page
    // below the floor answers from the transcript and hands back a row
    // cursor (`r:<byte>[:<id>]`), which is the one signal a client keeps
    // walking on.
    let mut cursor: Option<String> = None;
    let mut crossed = false;
    let mut answered_the_window = None;
    for _ in 0..8 {
        send(
            &mut socket,
            ClientMessage::More { conversation: seat.clone(), before: cursor.take(), turns: 20 },
        )
        .await;
        let (turns, next) = loop {
            match next_server(&mut socket).await {
                ServerMessage::Page { turns, cursor, .. } => break (turns, cursor),
                // The core raises updates of its own: skipped, for the reason
                // `snapshot_answering` gives.
                ServerMessage::Update { .. } => {}
                other => panic!("`more` is answered with a page, and this is {other:?}"),
            }
        };
        answered_the_window.get_or_insert(turns.len());
        match next {
            Some(next) if next.starts_with("r:") => {
                crossed = true;
                break;
            }
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert!(
        answered_the_window.is_some_and(|turns| turns > 0),
        "`more` answers the window the connect seeded, not an empty conversation",
    );
    assert!(
        crossed,
        "the walk crosses into the transcript below the window rather than stopping at its \
         front: with no floor carried, the window reads as the whole history",
    );
}

/// Every record this process's subscriber writes, installed on first use.
///
/// **One process per test under nextest**, so a global subscriber is this
/// test's own; under a shared-process runner a second install is a no-op
/// and the one buffer still receives this test's records, which is all the
/// assertion below reads.
static CAPTURED: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();

struct CaptureSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CaptureSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("the capture is not poisoned").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install the capture and answer its buffer, shaped the way the shipped log
/// is: one JSON line per record, the fields flattened.
fn install_capture() -> Arc<Mutex<Vec<u8>>> {
    let buffer = CAPTURED.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&buffer);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            // The relay's own lines are `forge_workspace::browser`: the
            // reaped holder's release reason is asserted from here.
            // The production filter, not a narrower one: a leak on any
            // target the shipped log enables must reach this capture, and a
            // test filter is blind to every target it does not name.
            .with_env_filter(forge_server::logging::DEFAULT_LOG_DIRECTIVES)
            .with_writer(move || CaptureSink(Arc::clone(&sink)))
            .with_ansi(false)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
        buffer
    });
    Arc::clone(buffer)
}

/// Everything the capture has written so far.
fn captured() -> String {
    let buffer = CAPTURED.get().expect("the capture was installed").lock().expect("not poisoned");
    String::from_utf8(buffer.clone()).expect("the capture holds utf-8")
}

/// **The open path's line actually lands (#2374).** The wire tests pin the
/// numbers the line carries; what this pins is the emission - the release
/// reads a slow switch out of this one record, so a level or a field name
/// that never reached the log would leave the artefacts empty with nothing
/// failing anywhere.
#[tokio::test]
async fn a_session_subscribe_logs_its_own_open_timings() {
    install_capture();
    let (url, _fleet, _state) = a_server_with_state().await;
    let mut socket = connect(&url).await;
    // The address this test's own socket is attached from, which is what
    // both lines have to name.
    let peer = match socket.get_ref() {
        tokio_tungstenite::MaybeTlsStream::Plain(stream) => {
            stream.local_addr().expect("the test client's address")
        }
        _ => panic!("the test's client is a plain socket"),
    };
    send(
        &mut socket,
        ClientMessage::Subscribe {
            what: Subject::Session(lead_seat()),
            answering: true,
            review: false,
        },
    )
    .await;
    let (subject, _data, _passed) = snapshot_answering(&mut socket).await;
    assert!(matches!(subject, Subject::Session(_)), "the answer is the seat's own snapshot");

    let caught = captured();
    let line = caught
        .lines()
        .find(|line| line.contains("\"session_open\""))
        .unwrap_or_else(|| panic!("a session subscribe must log its open; caught: {caught}"));
    let parsed: serde_json::Value = serde_json::from_str(line).expect("the line is JSON");
    assert_eq!(parsed["event_name"], "session_open");
    assert_eq!(
        parsed["level"], "DEBUG",
        "a session's own work is information, not a warning: {line}",
    );
    assert_eq!(parsed["slot"], "TestOrg/proj/lead", "the line names its slot: {line}");
    // **Every slice the open can spend time in is named.** The line
    // exists to place a slow switch, and a read it does not carry is a second
    // nobody can attribute - which is how 1.1 s sat in an unnamed remainder.
    let slices = [
        "prelude_ms",
        "queued_ms",
        "hold_ms",
        "prologue_ms",
        "header_ms",
        "catch_up_ms",
        "seed_ms",
        "conversation_ms",
        "page_ms",
        "cards_ms",
        "work_ms",
        "base_ms",
        "rescan_ms",
        "content_ms",
        "reviews_ms",
        "tree_reviews_ms",
        "state_ms",
        "index_ms",
        "tail_ms",
        "encode_ms",
    ];
    for field in slices {
        assert!(parsed.get(field).is_some(), "the line carries {field}: {line}");
        assert!(
            parsed[field].as_f64().is_some_and(|ms| ms >= 0.0),
            "{field} is a non-negative number: {line}",
        );
    }
    assert!(parsed.get("open_ms").is_some(), "the line carries open_ms: {line}");
    assert!(parsed.get("peer").is_some(), "the line carries its peer: {line}");
    // **The peer is this client's own address (#2506).** The watch has
    // several attachments and no address to tell them apart by, so the open
    // itself has to name the client it served.
    assert_eq!(
        parsed["peer"],
        serde_json::json!(peer.to_string()),
        "the open names this client's own address: {line}",
    );
    // **One record per open.** The write's numbers live on `frame_write`, so
    // the open must not repeat them: a field in both places is a field to
    // keep in step, and an open that doubled its debug volume for it.
    for field in ["bytes", "prep_ms", "write_ms", "waited"] {
        assert!(
            parsed.get(field).is_none(),
            "the open does not repeat the write's {field}: {line}",
        );
    }

    // The write's own record, carrying the fields the open no longer does.
    let line = caught
        .lines()
        .find(|line| line.contains("\"frame_write\""))
        .unwrap_or_else(|| panic!("the open's frame logs its own write; caught: {caught}"));
    let write: serde_json::Value = serde_json::from_str(line).expect("the line is JSON");
    assert_eq!(write["event_name"], "frame_write");
    assert_eq!(write["level"], "DEBUG", "a frame's own cost is information: {line}");
    assert_eq!(write["slot"], "TestOrg/proj/lead", "the line names its slot: {line}");
    for field in ["bytes", "prep_ms", "write_ms", "waited", "peer"] {
        assert!(write.get(field).is_some(), "the line carries {field}: {line}");
    }
    assert!(
        write["bytes"].as_u64().is_some_and(|bytes| bytes > 0),
        "the write names the frame's size: {line}",
    );
    assert_eq!(
        write["peer"], parsed["peer"],
        "the write and the open name the same client: {write} beside {parsed}",
    );
    // **The write's number is the smaller one.** A whole-open value would
    // equal `open_ms`; this reds that substitution, and says nothing about
    // where the clock was read inside the open.
    assert!(
        write["write_ms"].as_f64().expect("write_ms is a number")
            < parsed["open_ms"].as_f64().expect("open_ms is a number"),
        "the write's number is smaller than the open's, which a whole-open value would not be: \
         {write} beside {parsed}",
    );
    // **The slices are disjoint, so they cannot exceed the open they split.**
    // Every one is taken around one read in the order the path runs them, on
    // the open's own clock; a slice measured from the wrong start (the whole
    // open, or a span another slice already counted) pushes this sum past
    // `open_ms`, which is the one thing the line's arithmetic has to hold to.
    let accounted: f64 = slices
        .iter()
        .map(|field| parsed[field].as_f64().expect("a slice is a number"))
        .sum::<f64>()
        + write["prep_ms"].as_f64().expect("prep_ms is a number")
        + write["write_ms"].as_f64().expect("write_ms is a number");
    assert!(
        accounted <= parsed["open_ms"].as_f64().expect("open_ms is a number") + 1e-6,
        "the slices account for no more than the open they split: {accounted} against {parsed}",
    );
    // The sum is one-sided on its own: all-zero slices would satisfy it. The
    // two reads the fixture actually takes must be work, or a slice wired to a
    // constant zero would pass every assertion above.
    assert!(
        parsed["work_ms"].as_f64().is_some_and(|ms| ms > 0.0),
        "the tree scan is work on the path: {line}",
    );
    assert!(
        write["prep_ms"].as_f64().is_some_and(|ms| ms > 0.0),
        "the frame's serialisation is work on the path: {write}",
    );

    // The attach's own record, beside them, for the clients that never open
    // (a declare) and for the address before an open fails.
    let line = caught
        .lines()
        .find(|line| line.contains("\"client_subscribed\""))
        .unwrap_or_else(|| panic!("a session subscribe names its client; caught: {caught}"));
    let subscribed: serde_json::Value = serde_json::from_str(line).expect("the line is JSON");
    assert_eq!(subscribed["event_name"], "client_subscribed");
    assert_eq!(subscribed["level"], "DEBUG", "an attach's own address is information: {line}");
    assert_eq!(subscribed["slot"], "TestOrg/proj/lead", "the line names its slot: {line}");
    assert_eq!(
        subscribed["peer"], parsed["peer"],
        "the attach and the open name the same client: {subscribed} beside {parsed}",
    );
}

/// The server's browser fleet, stubbed at the port: frames and viewer state
/// are watches the test drives, and input is recorded under the controller
/// rule the real service applies.
struct StubBrowser {
    frames: tokio::sync::watch::Sender<Option<forge_primitives::browser::BrowserFrame>>,
    view: tokio::sync::watch::Sender<forge_primitives::browser::BrowserViewState>,
    failures: tokio::sync::watch::Sender<Option<String>>,
    controller: Mutex<Option<u64>>,
    viewers: Mutex<std::collections::HashSet<u64>>,
    inputs: Mutex<Vec<(u64, u64)>>,
    resizes: Mutex<Vec<Resize>>,
    nav: tokio::sync::watch::Sender<Option<forge_primitives::browser::BrowserNavState>>,
    files: tokio::sync::watch::Sender<Option<forge_primitives::browser::BrowserFileAsk>>,
    downloads: tokio::sync::broadcast::Sender<forge_primitives::browser::BrowserDownload>,
    audio: tokio::sync::broadcast::Sender<forge_primitives::browser::BrowserAudio>,
    /// The seat's tab rows, which a strip follows.
    tabs: tokio::sync::watch::Sender<forge_primitives::browser::BrowserTabList>,
    /// Every file answer the server handed the fleet: id and file count.
    answers: Mutex<Vec<(u64, usize)>>,
    detached: Mutex<Vec<u64>>,
    /// Whether the next take is refused, the way a host whose session ended
    /// under it refuses.
    refuse_take: Mutex<bool>,
    /// The tab each viewer attached to, so the stub obeys the role rule
    /// `BrowserHost::attach` states: a same-tab attach keeps the controller,
    /// a move releases it.
    viewing: Mutex<std::collections::HashMap<u64, u16>>,
}

impl StubBrowser {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            frames: tokio::sync::watch::channel(None).0,
            view: tokio::sync::watch::channel(
                forge_primitives::browser::BrowserViewState::default(),
            )
            .0,
            failures: tokio::sync::watch::channel(None).0,
            controller: Mutex::new(None),
            viewers: Mutex::new(std::collections::HashSet::new()),
            inputs: Mutex::new(Vec::new()),
            resizes: Mutex::new(Vec::new()),
            nav: tokio::sync::watch::channel(None).0,
            files: tokio::sync::watch::channel(None).0,
            downloads: tokio::sync::broadcast::channel(4).0,
            audio: tokio::sync::broadcast::channel(4).0,
            tabs: tokio::sync::watch::channel(forge_primitives::browser::BrowserTabList {
                profile: "TestOrg/default".to_owned(),
                id: 5,
                up: true,
                connected: true,
                closing_at_ms: None,
                rows: Vec::new(),
            })
            .0,
            answers: Mutex::new(Vec::new()),
            detached: Mutex::new(Vec::new()),
            refuse_take: Mutex::new(false),
            viewing: Mutex::new(std::collections::HashMap::new()),
        })
    }

    fn publish(&self) {
        let controller = *self.controller.lock().expect("the stub's controller");
        let viewers =
            u32::try_from(self.viewers.lock().expect("the stub's viewers").len()).unwrap_or(0);
        self.view.send_replace(forge_primitives::browser::BrowserViewState { controller, viewers });
    }
}

/// One resize the stub was asked for: profile, tab, viewer, device pixels,
/// ratio.
type Resize = (String, u16, u64, u32, u32, f64);

#[async_trait::async_trait]
impl forge_workspace::browser::BrowserHost for StubBrowser {
    async fn call(
        &self,
        _seat: &SessionSlot,
        _tool: &str,
        _args: serde_json::Value,
    ) -> Result<Vec<forge_primitives::browser::BrowserPart>, String> {
        Err("the stub drives nothing".to_owned())
    }

    async fn attach(
        &self,
        viewer: u64,
        _seat: &SessionSlot,
        tab: Option<u16>,
    ) -> Result<forge_workspace::browser::BrowserAttachment, String> {
        // One number is the refusal's own: a tab the server cannot attach.
        if tab == Some(9) {
            return Err("tab 9 is not open".to_owned());
        }
        // **The stub obeys the role rule `BrowserHost::attach` states**: a
        // same-tab attach keeps the viewer's controller role, and a move
        // releases it.
        let resolved = tab.unwrap_or(2);
        let moved = self
            .viewing
            .lock()
            .expect("the stub's tabs")
            .insert(viewer, resolved)
            .is_some_and(|was| was != resolved);
        // The real service counts its viewers; the stub does the same so a
        // detach can be seen in the state push.
        self.viewers.lock().expect("the stub's viewers").insert(viewer);
        if moved {
            let mut controller = self.controller.lock().expect("the stub's controller");
            if *controller == Some(viewer) {
                *controller = None;
            }
        }
        self.publish();
        Ok(forge_workspace::browser::BrowserAttachment {
            // **The stub resolves an omitted tab the way the service does**:
            // to a tab the profile has open.
            key: "TestOrg/default".to_owned(),
            id: 5,
            tab: resolved,
            frames: self.frames.subscribe(),
            view: self.view.subscribe(),
            tabs: self.tabs.subscribe(),
            nav: self.nav.subscribe(),
            failures: self.failures.subscribe(),
            files: self.files.subscribe(),
            downloads: self.downloads.subscribe(),
            audio: self.audio.subscribe(),
        })
    }

    async fn answer_files(
        &self,
        _profile: &str,
        _tab: u16,
        id: u64,
        files: Vec<forge_primitives::browser::BrowserUpload>,
    ) -> Result<(), String> {
        self.answers.lock().expect("the stub's answers").push((id, files.len()));
        Ok(())
    }

    async fn detach(&self, viewer: u64) {
        self.detached.lock().expect("the stub's detaches").push(viewer);
        self.viewers.lock().expect("the stub's viewers").remove(&viewer);
        let mut controller = self.controller.lock().expect("the stub's controller");
        if *controller == Some(viewer) {
            *controller = None;
        }
        drop(controller);
        self.publish();
    }

    async fn input(
        &self,
        _profile: &str,
        _tab: u16,
        viewer: u64,
        seq: u64,
        _event: &forge_primitives::browser::BrowserInputEvent,
    ) -> Result<(), String> {
        if *self.controller.lock().expect("the stub's controller") != Some(viewer) {
            return Err("this viewer is not the browser's controller".to_owned());
        }
        self.inputs.lock().expect("the stub's inputs").push((viewer, seq));
        Ok(())
    }

    async fn take_control(&self, _profile: &str, _tab: u16, viewer: u64) -> Result<(), String> {
        if std::mem::replace(&mut *self.refuse_take.lock().expect("the stub's refusal"), false) {
            return Err("the session ended under this take".to_owned());
        }
        *self.controller.lock().expect("the stub's controller") = Some(viewer);
        self.publish();
        Ok(())
    }

    async fn release_control(&self, _profile: &str, _tab: u16, viewer: u64) {
        let mut controller = self.controller.lock().expect("the stub's controller");
        if *controller == Some(viewer) {
            *controller = None;
            drop(controller);
            self.publish();
        }
    }

    async fn nav(
        &self,
        _profile: &str,
        _tab: u16,
        viewer: u64,
        _back: bool,
    ) -> Result<forge_primitives::browser::BrowserNavState, String> {
        // The real service's rule: the controller moves the page, a watcher
        // is refused before the history is read.
        if *self.controller.lock().expect("the stub's controller") != Some(viewer) {
            return Err("this viewer is not the browser's controller".to_owned());
        }
        Ok(forge_primitives::browser::BrowserNavState::default())
    }

    async fn nav_state(
        &self,
        _profile: &str,
        _tab: u16,
    ) -> Option<forge_primitives::browser::BrowserNavState> {
        Some(forge_primitives::browser::BrowserNavState::default())
    }

    async fn resize(
        &self,
        profile: &str,
        tab: u16,
        viewer: u64,
        width: u32,
        height: u32,
        scale: f64,
    ) {
        self.resizes.lock().expect("the stub's resizes").push((
            profile.to_owned(),
            tab,
            viewer,
            width,
            height,
            scale,
        ));
    }

    async fn tabs(
        &self,
        _seat: &SessionSlot,
    ) -> Result<forge_primitives::browser::BrowserTabList, String> {
        Ok(self.tabs.borrow().clone())
    }

    async fn tab_stream(
        &self,
        _seat: &SessionSlot,
    ) -> Result<tokio::sync::watch::Receiver<forge_primitives::browser::BrowserTabList>, String>
    {
        Ok(self.tabs.subscribe())
    }

    async fn sweep_sessions(&self, _seats: &[SessionSlot], _generation: u64) {}

    async fn replace_seat(&self, _seat: &SessionSlot) {}

    fn session_ended(&self, _seat: &SessionSlot) {}

    async fn hand_off_started(&self, _seat: &SessionSlot, _tab: u16) -> Result<(), String> {
        Ok(())
    }

    async fn hand_off_finished(&self, _seat: &SessionSlot, _tab: u16) {}

    async fn warm(&self, _seat: &SessionSlot) {}

    async fn stop(&self) -> Vec<String> {
        Vec::new()
    }
}

/// **A refused attach answers the client that asked.** Without an answer the
/// surface went live over nothing and the page sat under its bar with no
/// picture and no reason; the reply is what carries the server's sentence
/// into the surface's failed state.
#[tokio::test]
async fn a_refused_attach_answers_its_reply() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::BrowserView { seat: lead_seat(), tab: Some(9), reply_to: Some(41) },
    )
    .await;

    let ServerMessage::Reply { reply_to, body } = next_server(&mut socket).await else {
        panic!("a refused attach answers the asker")
    };
    assert_eq!(reply_to, 41, "under the id the asker minted");
    assert_eq!(body["ok"], false, "and it says the attach did not happen");
    let why = body["why"].as_str().expect("the server's own words");
    assert!(why.contains("tab 9") && why.contains("not open"), "{why}");
}

/// **Lowering the takeover detaches the viewer, and its role with it.** An
/// attached viewer counts as a use in the idle clock, so a lowered surface
/// that stayed attached kept its profile warm for ever and left the
/// controller role held by nobody's surface.
#[tokio::test]
async fn lowering_detaches_the_viewer_and_releases_its_role() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(&mut socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let ServerMessage::BrowserView { id, viewer, .. } = next_server(&mut socket).await else {
        panic!("an attach is answered with the viewer state")
    };
    let _ = next_server(&mut socket).await; // the nav state behind it
    send(&mut socket, ClientMessage::BrowserControl { take: true }).await;
    let ServerMessage::BrowserView { controller, .. } = next_server(&mut socket).await else {
        panic!("the take answers with the state")
    };
    assert_eq!(controller, Some(viewer), "this viewer holds the role");

    // **No state frame is owed to the client that lowered its own surface**;
    // the re-attach's count is the proof the detach took the viewer off.
    send(&mut socket, ClientMessage::BrowserDetach).await;

    // A fresh attach starts from zero, not from a phantom count.
    send(&mut socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let ServerMessage::BrowserView { id: again, viewers, controller, .. } =
        next_server(&mut socket).await
    else {
        panic!("the re-attach answers with the state")
    };
    assert_eq!((again, viewers, controller), (id, 1, None), "one viewer, and the role free");
}

/// **The controller's surface size reaches the browser it is watching.** The
/// client writes these fields by hand, and a resize nothing forwards leaves
/// the page laid out for a window that is not on screen.
#[tokio::test]
async fn a_resize_reaches_the_browser_it_is_watching() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(&mut socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let ServerMessage::BrowserView { viewer, .. } = next_server(&mut socket).await else {
        panic!("an attach is answered with the viewer state")
    };
    let _ = next_server(&mut socket).await; // the nav state behind it
    send(&mut socket, ClientMessage::BrowserResize { width: 1600, height: 1200, scale: 2.0 }).await;

    // The connection handles one message at a time, so an answer to the next
    // one proves the resize was taken before it arrives here.
    send(&mut socket, ClientMessage::BrowserControl { take: true }).await;
    let _ = next_server(&mut socket).await;
    let resizes = stub.resizes.lock().expect("the stub's resizes").clone();
    assert_eq!(
        resizes,
        vec![("TestOrg/default".to_owned(), 2, viewer, 1600, 1200, 2.0)],
        "the surface size reached the viewed tab, under this viewer"
    );
}

/// **Only the controller moves the page.** The history arrows are drawn on
/// every viewer's surface, so without the role check a watcher's click moved
/// the page under the person driving it.
#[tokio::test]
async fn a_watcher_cannot_move_the_page() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;

    let mut controller = connect(&url).await;
    send(
        &mut controller,
        ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None },
    )
    .await;
    let ServerMessage::BrowserView { viewer: holder, .. } = next_server(&mut controller).await
    else {
        panic!("an attach is answered with the viewer state")
    };
    let _ = next_server(&mut controller).await; // the nav state behind it
    send(&mut controller, ClientMessage::BrowserControl { take: true }).await;
    let ServerMessage::BrowserView { controller: who, .. } = next_server(&mut controller).await
    else {
        panic!("the take answers with the state")
    };
    assert_eq!(who, Some(holder), "the first viewer holds the role");

    let mut watcher = connect(&url).await;
    send(&mut watcher, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let ServerMessage::BrowserView { viewer: second, controller: who, .. } =
        next_server(&mut watcher).await
    else {
        panic!("the watcher's attach answers with the state")
    };
    assert_eq!(who, Some(holder), "the watcher joins under the same controller");
    let _ = next_server(&mut watcher).await; // the nav state behind it

    send(&mut watcher, ClientMessage::BrowserNav { back: true }).await;
    let ServerMessage::Error { what, why, .. } = next_server(&mut watcher).await else {
        panic!("a watcher's move is refused, not answered")
    };
    assert_eq!(what, "browser_nav", "the refusal names the move");
    assert!(why.contains("not the browser's controller"), "in the server's own words: {why}");

    // The role is what was missing: with it taken, the same move lands.
    send(&mut watcher, ClientMessage::BrowserControl { take: true }).await;
    let ServerMessage::BrowserView { controller: who, .. } = next_server(&mut watcher).await else {
        panic!("the take answers with the state")
    };
    assert_eq!(who, Some(second), "the role moved to this viewer");
    send(&mut watcher, ClientMessage::BrowserNav { back: true }).await;
    assert!(
        matches!(next_server(&mut watcher).await, ServerMessage::BrowserNavState { .. }),
        "the controller's move is answered with the arrows",
    );
}

/// **A browser that stops is said once, and the viewer goes with it.** The
/// connection outlives the failed surface; a viewer left on the profile kept
/// it warm and its role held by nobody's surface.
#[tokio::test]
async fn a_host_death_fails_the_surface_and_drops_the_viewer() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(&mut socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let ServerMessage::BrowserView { id, viewer, .. } = next_server(&mut socket).await else {
        panic!("an attach is answered with the viewer state")
    };
    let _ = next_server(&mut socket).await; // the nav state behind it

    stub.failures.send_replace(Some("the host died".to_owned()));
    let ServerMessage::BrowserSurfaceFailed { id: failed, tab: failed_tab, reason } =
        next_server(&mut socket).await
    else {
        panic!("the failure is said, not left as a frozen frame")
    };
    assert_eq!(
        (failed, failed_tab, reason.as_str()),
        (id, 2, "the host died"),
        "the failed profile, the tab and the reason"
    );
    assert!(
        !stub.viewers.lock().expect("the stub's viewers").contains(&viewer),
        "the viewer came off the profile with the failure",
    );
    assert!(
        stub.detached.lock().expect("the stub's detaches").contains(&viewer),
        "through the host's own detach",
    );

    // A second push for the profile this connection no longer views is not a
    // second failure: there is nothing left to draw it on.
    stub.failures.send_replace(Some("again".to_owned()));
    assert!(
        next_server_within(&mut socket, 300).await.is_none(),
        "one failure per surface, not one per push",
    );
}

/// **A viewer that attaches is sent the frame the profile already holds.**
/// The newest frame is a value rather than an event, so without this a
/// surface opening on a still page drew nothing until the next paint.
#[tokio::test]
async fn an_attaching_viewer_is_sent_the_kept_frame() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;
    let encoded = vec![5_u8, 4, 3, 2];
    stub.frames.send_replace(Some(forge_primitives::browser::BrowserFrame {
        profile: 5,
        tab: 2,
        seq: 3,
        encoded: encoded.clone(),
    }));

    send(&mut socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let ServerMessage::BrowserView { id, .. } = next_server(&mut socket).await else {
        panic!("an attach is answered with the viewer state")
    };
    let bytes = loop {
        let msg = socket.next().await.expect("a message").expect("no error");
        if let Message::Binary(bytes) = msg {
            break bytes;
        }
    };
    assert_eq!(bytes[0], forge_server::transport::frame::Kind::BrowserFrame.tag());
    assert_eq!(&bytes[1..], &encoded[..], "the kept frame crosses untouched");
    let ServerMessage::BrowserNavState { id: nav_id, .. } = next_server(&mut socket).await else {
        panic!("the attach is followed by the page's history")
    };
    assert_eq!((nav_id, id), (5, 5), "all three name the viewed profile");
}

/// **One profile's frame watch carries every tab, and a viewer draws only its
/// own.** Another tab's paint reaching this surface would swap the picture
/// under the page the person is on.
#[tokio::test]
async fn a_frame_for_another_tab_is_not_drawn() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::BrowserView { seat: lead_seat(), tab: Some(2), reply_to: None },
    )
    .await;
    let _ = next_server(&mut socket).await; // the viewer state
    let _ = next_server(&mut socket).await; // the nav state

    stub.frames.send_replace(Some(forge_primitives::browser::BrowserFrame {
        profile: 5,
        tab: 3,
        seq: 1,
        encoded: vec![1, 2, 3],
    }));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), socket.next()).await.is_err(),
        "another tab's frame is not drawn on this surface",
    );

    stub.frames.send_replace(Some(forge_primitives::browser::BrowserFrame {
        profile: 5,
        tab: 2,
        seq: 1,
        encoded: vec![4, 5, 6],
    }));
    let bytes = loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("this tab's frame reaches the socket")
            .expect("a message")
            .expect("no error");
        if let Message::Binary(bytes) = msg {
            break bytes;
        }
    };
    assert_eq!(&bytes[1..], &[4, 5, 6], "the viewer's own tab is the one it draws");
}

/// **A viewer that stops reading does not stall another's paint.** Each
/// viewer has its own connection task; the slow one's socket fills and its
/// write waits, while the viewer that keeps reading draws every newer frame.
#[tokio::test]
async fn a_slow_viewer_does_not_stop_a_second_viewers_frames() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut slow = connect(&url).await;
    let mut fast = connect(&url).await;
    for socket in [&mut slow, &mut fast] {
        send(
            socket,
            ClientMessage::BrowserView { seat: lead_seat(), tab: Some(2), reply_to: None },
        )
        .await;
    }
    let _ = next_server(&mut slow).await; // its view state; nothing more is read
    let _ = next_server(&mut fast).await;
    let _ = next_server(&mut fast).await;

    let sender = {
        let frames = stub.frames.clone();
        tokio::spawn(async move {
            for seq in 1..=40_u32 {
                // The host's encoded bytes begin with the client's 18-byte
                // header; the stub carries that shape so the reader below can
                // name the sequence it drew.
                let mut encoded = Vec::with_capacity(18 + 200 * 1024);
                encoded.extend_from_slice(&5_u16.to_le_bytes());
                encoded.extend_from_slice(&2_u16.to_le_bytes());
                encoded.extend_from_slice(&seq.to_le_bytes());
                encoded.extend_from_slice(&1280_u16.to_le_bytes());
                encoded.extend_from_slice(&720_u16.to_le_bytes());
                encoded.push(1);
                encoded.push(1);
                encoded.extend_from_slice(&640_u16.to_le_bytes());
                encoded.extend_from_slice(&360_u16.to_le_bytes());
                encoded.extend(std::iter::repeat_n(7_u8, 200 * 1024));
                frames.send_replace(Some(forge_primitives::browser::BrowserFrame {
                    profile: 5,
                    tab: 2,
                    seq,
                    encoded,
                }));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };

    let mut newest = 0_u32;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while newest < 40 && tokio::time::Instant::now() < deadline {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(Some(Ok(msg))) = tokio::time::timeout(left, fast.next()).await else { break };
        if let Message::Binary(bytes) = msg
            && bytes.len() >= 9
        {
            newest = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]);
        }
    }
    sender.abort();
    assert_eq!(newest, 40, "the reading viewer drew the newest frame past the stuck one");
    drop(slow);
}

/// **The strip's read answers through its reply, and then follows the same
/// rows.** The reply carries the profile's health and its tabs as they stand;
/// a tab that opens afterwards is pushed as the whole list rather than
/// waiting for the client to ask again.
#[tokio::test]
async fn a_strip_read_answers_the_tabs_and_then_follows_them() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(&mut socket, ClientMessage::BrowserTabs { seat: lead_seat(), reply_to: Some(9) }).await;

    let ServerMessage::Reply { reply_to, body } = next_server(&mut socket).await else {
        panic!("the strip's read answers its reply")
    };
    assert_eq!(reply_to, 9);
    assert_eq!(body["ok"], true);
    let tabs = &body["tabs"];
    assert_eq!(tabs["profile"], "TestOrg/default", "the profile the seat resolves to");
    assert_eq!((tabs["id"].clone(), tabs["up"].clone()), (serde_json::json!(5), true.into()));
    assert!(tabs["rows"].as_array().is_some_and(Vec::is_empty), "no tabs yet");

    // A tab opens: the same connection is pushed the new list without asking.
    stub.tabs.send_replace(forge_primitives::browser::BrowserTabList {
        profile: "TestOrg/default".to_owned(),
        id: 5,
        up: true,
        connected: true,
        closing_at_ms: None,
        rows: vec![forge_primitives::browser::BrowserTabRow {
            number: 2,
            title: "portal".to_owned(),
            url: "https://x/".to_owned(),
            owner: lead_seat(),
            state: forge_primitives::browser::BrowserTabState::Idle,
        }],
    });
    let ServerMessage::BrowserTabs { profile, rows, .. } = next_server(&mut socket).await else {
        panic!("a tab change is pushed to the strip that asked")
    };
    assert_eq!(profile, "TestOrg/default");
    assert_eq!(rows.len(), 1, "the strip draws the tab that opened");
    assert_eq!(rows[0].owner, lead_seat());
}

/// **Two viewers see one controller, and only that viewer's input lands.**
/// The role is a fact every viewer is told, not a private guess.
#[tokio::test]
async fn two_viewers_see_one_controller_and_only_its_input_lands() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut first = connect(&url).await;
    let mut second = connect(&url).await;

    send(&mut first, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let ServerMessage::BrowserView { viewer: one, viewers: 1, .. } = next_server(&mut first).await
    else {
        panic!("the first viewer attaches")
    };
    let _ = next_server(&mut first).await; // its nav state
    send(&mut second, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let ServerMessage::BrowserView { viewer: two, viewers: 2, .. } = next_server(&mut second).await
    else {
        panic!("the second viewer attaches")
    };
    assert_ne!(one, two, "each connection is its own viewer");
    let _ = next_server(&mut second).await; // the second attach's nav state
    let ServerMessage::BrowserView { viewers: 2, .. } = next_server(&mut first).await else {
        panic!("the attach publishes the new count to the viewer already there")
    };

    send(&mut first, ClientMessage::BrowserControl { take: true }).await;
    let ServerMessage::BrowserView { controller, .. } = next_server(&mut first).await else {
        panic!("the take answers the taker")
    };
    assert_eq!(controller, Some(one), "the first viewer holds the role");
    let ServerMessage::BrowserView { controller: seen, .. } = next_server(&mut second).await else {
        panic!("the take reaches the other viewer")
    };
    assert_eq!(seen, Some(one), "and it is told who drives");

    send(
        &mut second,
        ClientMessage::BrowserInput {
            seq: 1,
            event: forge_primitives::browser::BrowserInputEvent::Text { text: "x".to_owned() },
        },
    )
    .await;
    let ServerMessage::Error { what, .. } = next_server(&mut second).await else {
        panic!("input from a watcher is refused")
    };
    assert_eq!(what, "browser_input");
    send(
        &mut first,
        ClientMessage::BrowserInput {
            seq: 1,
            event: forge_primitives::browser::BrowserInputEvent::Text { text: "y".to_owned() },
        },
    )
    .await;
    // The input has no reply; a message that does proves it was handled.
    send(&mut first, ClientMessage::BrowserControl { take: true }).await;
    let _ = next_server(&mut first).await;
    let landed = stub.inputs.lock().expect("the stub's inputs").clone();
    assert_eq!(landed, vec![(one, 1)], "only the controller's input landed");
}

/// **A watcher never cancels the controller's chooser.** The ask is the
/// profile's, so every viewer's connection sees it; a watcher that answered
/// it with no files took the dialog away from the person who was showing it
/// - the picker opened for the controller and died because another client
/// was merely watching. Only the controller is asked, and a watcher stays
/// silent.
#[tokio::test]
async fn a_watcher_does_not_cancel_the_controllers_file_chooser() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut controller = connect(&url).await;
    let mut watcher = connect(&url).await;
    for socket in [&mut controller, &mut watcher] {
        send(socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
            .await;
        let _ = next_server(socket).await; // its view state
        let _ = next_server(socket).await; // its nav state
    }
    // The second attach's count reaches the viewer already there.
    let _ = next_server(&mut controller).await;
    send(&mut controller, ClientMessage::BrowserControl { take: true }).await;
    let _ = next_server(&mut controller).await; // the take's answer
    let _ = next_server(&mut watcher).await; // the take, told to the watcher

    stub.files.send_replace(Some(forge_primitives::browser::BrowserFileAsk {
        id: 7,
        tab: 2,
        multiple: false,
    }));
    let asked = next_server(&mut controller).await;
    let ServerMessage::BrowserFileDialog { id, tab, multiple } = asked else {
        panic!("the controller is the one asked for a file, got {asked:?}")
    };
    assert_eq!(tab, 2, "the ask is for the tab the viewer is on");
    assert_eq!((id, multiple), (7, false));
    assert!(
        next_server_within(&mut watcher, 400).await.is_none(),
        "a watcher is sent nothing for the chooser",
    );
    assert!(
        stub.answers.lock().expect("the stub's answers").is_empty(),
        "and nothing cancelled the controller's dialog: {:?}",
        stub.answers.lock().expect("the stub's answers"),
    );
}

/// **A chooser and a save belong to their tab.** A viewer attached to tab 1
/// is neither asked to pick nor offered a file for tab 2, and its own tab's
/// ask and save arrive.
#[tokio::test]
async fn a_chooser_and_a_download_for_another_tab_are_not_delivered() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;
    // **Tab 1 explicitly**: the stub's default is tab 2, and this test is
    // about the tab a message names.
    send(
        &mut socket,
        ClientMessage::BrowserView { seat: lead_seat(), tab: Some(1), reply_to: None },
    )
    .await;
    let _ = next_server(&mut socket).await; // its view state
    let _ = next_server(&mut socket).await; // its nav state
    send(&mut socket, ClientMessage::BrowserControl { take: true }).await;
    let _ = next_server(&mut socket).await; // the take's answer

    stub.files.send_replace(Some(forge_primitives::browser::BrowserFileAsk {
        id: 8,
        tab: 2,
        multiple: false,
    }));
    assert!(
        next_server_within(&mut socket, 400).await.is_none(),
        "another tab's chooser is not sent to this viewer",
    );
    stub.files.send_replace(Some(forge_primitives::browser::BrowserFileAsk {
        id: 9,
        tab: 1,
        multiple: false,
    }));
    let asked = next_server(&mut socket).await;
    let ServerMessage::BrowserFileDialog { id, tab, .. } = asked else {
        panic!("this tab's chooser is sent, got {asked:?}")
    };
    assert_eq!((id, tab), (9, 1));

    let download = |tab, name: &str| forge_primitives::browser::BrowserDownload {
        tab,
        name: name.to_owned(),
        bytes: b"x".to_vec(),
        error: None,
    };
    let _ = stub.downloads.send(download(2, "other.bin"));
    assert!(
        next_server_within(&mut socket, 400).await.is_none(),
        "another tab's save is not offered to this viewer",
    );
    let _ = stub.downloads.send(download(1, "mine.txt"));
    let saved = next_server(&mut socket).await;
    let ServerMessage::BrowserDownload { tab, name, dropped, .. } = saved else {
        panic!("this tab's save is sent, got {saved:?}")
    };
    assert_eq!((tab, name.as_str()), (1, "mine.txt"));
    assert!(!dropped, "a delivered save is not a dropped offer");
}

/// **Only the controller can answer, and only the outstanding ask.** An
/// answer from a watcher, or one naming an id that is not the profile's ask,
/// is refused before the fleet writes a byte or clears the dialog - a stale
/// answer otherwise wiped the uploads a live dialog was handing over.
#[tokio::test]
async fn a_file_answer_is_bound_to_the_controller_and_the_ask() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut controller = connect(&url).await;
    let mut watcher = connect(&url).await;
    for socket in [&mut controller, &mut watcher] {
        send(socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
            .await;
        let _ = next_server(socket).await;
        let _ = next_server(socket).await;
    }
    let _ = next_server(&mut controller).await;
    send(&mut controller, ClientMessage::BrowserControl { take: true }).await;
    let _ = next_server(&mut controller).await;
    let _ = next_server(&mut watcher).await;
    stub.files.send_replace(Some(forge_primitives::browser::BrowserFileAsk {
        id: 7,
        tab: 2,
        multiple: false,
    }));
    let _ = next_server(&mut controller).await; // the dialog

    let answer = |id: u64| ClientMessage::BrowserFileAnswer {
        id,
        files: vec![forge_server::transport::envelope::BrowserFileWire {
            name: "cv.pdf".to_owned(),
            data: "aGVsbG8=".to_owned(),
        }],
    };
    send(&mut watcher, answer(7)).await;
    let ServerMessage::Error { what, why, .. } = next_server(&mut watcher).await else {
        panic!("a watcher's answer is refused")
    };
    assert_eq!(what, "browser_file_answer");
    assert!(why.contains("not the file chooser's"), "{why}");
    send(&mut controller, answer(8)).await;
    let ServerMessage::Error { what, .. } = next_server(&mut controller).await else {
        panic!("a stale id is refused")
    };
    assert_eq!(what, "browser_file_answer");
    assert!(
        stub.answers.lock().expect("the stub's answers").is_empty(),
        "neither answer reached the fleet: {:?}",
        stub.answers.lock().expect("the stub's answers"),
    );
}

/// **A chooser nobody is driving is cancelled once.** With no controller the
/// dialog can never be answered, so the connection that sees it cancels it
/// rather than leaving CEF holding a callback behind a panel nobody shows.
#[tokio::test]
async fn a_chooser_with_no_controller_is_cancelled() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let _ = next_server(&mut socket).await;
    let _ = next_server(&mut socket).await;

    stub.files.send_replace(Some(forge_primitives::browser::BrowserFileAsk {
        id: 9,
        tab: 2,
        multiple: true,
    }));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if !stub.answers.lock().expect("the stub's answers").is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "the chooser was never cancelled");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        stub.answers.lock().expect("the stub's answers").as_slice(),
        [(9, 0)],
        "cancelled once, with no files",
    );
}

/// **A re-attach leaves the move to the attach.** The host's `attach`
/// resolves the profile and tab and is where a surface that moved lets the
/// old tab go; un-attaching here first read as a move even for the tab the
/// connection was already on, and it took the controller role with it - the
/// person's second Open drove a tab whose every gesture was refused. A
/// refused attach still gives the viewer up, so a failed surface leaves no
/// phantom viewer behind.
#[tokio::test]
async fn a_reattach_leaves_the_move_to_the_attach() {
    let (written, _guard) = catch_records();
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::BrowserView { seat: lead_seat(), tab: Some(2), reply_to: None },
    )
    .await;
    let ServerMessage::BrowserView { viewer, tab, .. } = next_server(&mut socket).await else {
        panic!("the first attach answers")
    };
    assert_eq!(tab, 2);
    let _ = next_server(&mut socket).await; // its nav state
    send(&mut socket, ClientMessage::BrowserControl { take: true }).await;
    let ServerMessage::BrowserView { controller, .. } = next_server(&mut socket).await else {
        panic!("the take republishes the viewer state")
    };
    assert_eq!(controller, Some(viewer), "the take lands");

    // **The connection writes no release record**: the service owns that
    // line, so a release is recorded once and never claimed twice.
    let takes = caught(&written).matches("event_name=\"browser_control\"").count();
    send(&mut socket, ClientMessage::BrowserControl { take: false }).await;
    let _ = next_server(&mut socket).await; // the release's own push
    assert_eq!(
        caught(&written).matches("event_name=\"browser_control\"").count(),
        takes,
        "the connection wrote a release record: {}",
        caught(&written),
    );
    // Take it back, so the same-tab re-attach below has a role to keep.
    send(&mut socket, ClientMessage::BrowserControl { take: true }).await;
    let ServerMessage::BrowserView { controller, .. } = next_server(&mut socket).await else {
        panic!("the take answers")
    };
    assert_eq!(controller, Some(viewer), "the role is back");

    send(
        &mut socket,
        ClientMessage::BrowserView { seat: lead_seat(), tab: Some(2), reply_to: None },
    )
    .await;
    let ServerMessage::BrowserView { tab, controller, .. } = next_server(&mut socket).await else {
        panic!("the re-attach answers")
    };
    assert_eq!(tab, 2, "the same tab");
    assert_eq!(controller, Some(viewer), "and the role it took there survives");
    let _ = next_server(&mut socket).await; // its nav state
    assert!(
        stub.detached.lock().expect("the stub's detaches").is_empty(),
        "the transport detached the viewer it was about to re-attach: {:?}",
        stub.detached.lock().expect("the stub's detaches"),
    );

    // **A different tab is still the connection's to view**: leaving the move
    // to the attach must not leave the old tab as the one it reports.
    send(
        &mut socket,
        ClientMessage::BrowserView { seat: lead_seat(), tab: Some(3), reply_to: None },
    )
    .await;
    let ServerMessage::BrowserView { tab, .. } = next_server(&mut socket).await else {
        panic!("the move answers")
    };
    assert_eq!(tab, 3, "the connection views the new tab");
    let _ = next_server(&mut socket).await; // its nav state

    // A tab the host cannot attach still gives the viewer up.
    send(
        &mut socket,
        ClientMessage::BrowserView { seat: lead_seat(), tab: Some(9), reply_to: None },
    )
    .await;
    let ServerMessage::Error { what, .. } = next_server(&mut socket).await else {
        panic!("tab 9 is refused")
    };
    assert_eq!(what, "browser_view");
    assert_eq!(
        stub.detached.lock().expect("the stub's detaches").as_slice(),
        [viewer],
        "the refused attach gives the viewer up"
    );
    // **The refusal is recorded**, with the tab asked for, the surface it
    // left and why, so "my surface fell to failed when I picked another tab"
    // is answered by one line.
    let records = caught(&written);
    let refusal = records
        .lines()
        .find(|line| line.contains("browser_attach_refused"))
        .unwrap_or_else(|| panic!("the refused attach is recorded: {records}"));
    assert!(refusal.contains("requested_tab=9"), "the tab asked for: {refusal}");
    assert!(refusal.contains("was_tab=3"), "what it was on: {refusal}");
    assert!(refusal.contains("dropped=true"), "and that it dropped it: {refusal}");
    assert!(refusal.contains("tab 9 is not open"), "and why: {refusal}");
}

/// **A gesture and a take from a connection with no attach are answered**,
/// and a pointer move is not: the surface can read as live over a page no
/// gesture can reach, so the refusal has to reach it - except the move
/// stream, whose refusal is the click's that follows.
#[tokio::test]
async fn an_unattached_gesture_and_take_are_answered() {
    let (written, _guard) = catch_records();
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::BrowserInput {
            seq: 1,
            event: forge_primitives::browser::BrowserInputEvent::Text {
                text: "hunter2".to_owned(),
            },
        },
    )
    .await;
    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("a text with no attach is refused")
    };
    assert_eq!(what, "browser_input");
    assert!(why.contains("no browser attached"), "{why}");

    send(&mut socket, ClientMessage::BrowserControl { take: true }).await;
    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("a take with no attach is refused")
    };
    assert_eq!(what, "browser_control");
    assert!(why.contains("no browser attached"), "{why}");

    // **Both refusals are recorded, and the text is counted not quoted.**
    let records = caught(&written);
    let refusal = records
        .lines()
        .find(|line| line.contains("browser_input_unattached"))
        .unwrap_or_else(|| panic!("the unattached gesture is recorded: {records}"));
    assert!(refusal.contains("gesture=\"text\""), "the kind is named: {refusal}");
    assert!(refusal.contains("text_chars=7"), "and the text counted: {refusal}");
    assert!(!records.contains("hunter2"), "the typed text is never in the record: {records}");
    assert!(
        records.lines().any(|line| line.contains("browser_control_unattached")),
        "the unattached take is recorded too: {records}",
    );

    let unattached = caught(&written).matches("browser_input_unattached").count();
    send(
        &mut socket,
        ClientMessage::BrowserInput {
            seq: 2,
            event: forge_primitives::browser::BrowserInputEvent::Mouse {
                action: forge_primitives::browser::MouseAction::Move,
                x: 10.0,
                y: 20.0,
                button: 0,
                clicks: 0,
                modifiers: forge_primitives::browser::InputModifiers::default(),
            },
        },
    )
    .await;
    assert!(
        next_server_within(&mut socket, 300).await.is_none(),
        "a pointer move is a stream and its refusal is the click's",
    );
    assert_eq!(
        caught(&written).matches("browser_input_unattached").count(),
        unattached,
        "and it writes no record on either path: {}",
        caught(&written),
    );
}

/// **A refused take is answered and recorded.** The host can refuse one - a
/// session that ended under it - and the person's press would otherwise be a
/// bar that never changes with no trace of why.
#[tokio::test]
async fn a_refused_take_is_answered_and_recorded() {
    let (written, _guard) = catch_records();
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let _ = next_server(&mut socket).await; // its view state
    let _ = next_server(&mut socket).await; // its nav state

    *stub.refuse_take.lock().expect("the stub's refusal") = true;
    send(&mut socket, ClientMessage::BrowserControl { take: true }).await;
    let ServerMessage::Error { what, why, .. } = next_server(&mut socket).await else {
        panic!("a refused take answers")
    };
    assert_eq!(what, "browser_control");
    assert!(why.contains("the session ended under this take"), "{why}");

    let records = caught(&written);
    let refusal = records
        .lines()
        .find(|line| {
            line.contains("event_name=\"browser_control\"") && line.contains("landed=false")
        })
        .unwrap_or_else(|| panic!("the refused take is recorded: {records}"));
    for needle in ["profile=TestOrg/default", "tab=2", "reason=the session ended under this take"] {
        assert!(refusal.contains(needle), "the record carries `{needle}`: {refusal}");
    }
}

/// **A refused gesture's record names its kind and never its text.** The
/// record is the artifact a dead page is diagnosed from, and this browser
/// signs people in: a typed text is a password as often as not. The capture
/// is the shipped artifact's own shape, so a site that starts carrying the
/// text fails here rather than in the field; the pointer-move gate is the
/// record's other half - a stream writes none.
#[tokio::test]
async fn a_refused_gestures_record_names_its_kind_and_never_its_text() {
    let (written, _guard) = catch_records();
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let _ = next_server(&mut socket).await; // its view state
    let _ = next_server(&mut socket).await; // its nav state

    // A surface that attached and never took the role: its text is refused.
    send(
        &mut socket,
        ClientMessage::BrowserInput {
            seq: 1,
            event: forge_primitives::browser::BrowserInputEvent::Text {
                text: "hunter2".to_owned(),
            },
        },
    )
    .await;
    let ServerMessage::Error { what, .. } = next_server(&mut socket).await else {
        panic!("a watcher's text is refused")
    };
    assert_eq!(what, "browser_input");
    let records = caught(&written);
    let refusal = records
        .lines()
        .find(|line| line.contains("browser_input_refused"))
        .unwrap_or_else(|| panic!("the refusal is recorded: {records}"));
    assert!(refusal.contains("gesture=\"text\""), "the kind is named: {refusal}");
    assert!(refusal.contains("text_chars=7"), "and the text counted: {refusal}");
    assert!(!records.contains("hunter2"), "the typed text is never in the record: {records}");

    // A pointer move is a stream: still answered, but with no record of its
    // own - the click that follows carries its refusal.
    let refused = caught(&written).matches("browser_input_refused").count();
    send(
        &mut socket,
        ClientMessage::BrowserInput {
            seq: 2,
            event: forge_primitives::browser::BrowserInputEvent::Mouse {
                action: forge_primitives::browser::MouseAction::Move,
                x: 10.0,
                y: 20.0,
                button: 0,
                clicks: 0,
                modifiers: forge_primitives::browser::InputModifiers::default(),
            },
        },
    )
    .await;
    let _ = next_server(&mut socket).await; // its refusal, still answered
    assert_eq!(
        caught(&written).matches("browser_input_refused").count(),
        refused,
        "a pointer move writes no record: {}",
        caught(&written),
    );
}

/// **A page that navigates on its own moves the bar's arrows.** The client
/// asks nothing after a link click; the profile's history watch is pushed as
/// `browser_nav_state` so the arrows do not stay grey until the next open.
#[tokio::test]
async fn a_page_driven_navigation_pushes_the_history_to_the_viewer() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(&mut socket, ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: None })
        .await;
    let ServerMessage::BrowserView { id, .. } = next_server(&mut socket).await else {
        panic!("an attach is answered with the viewer state")
    };
    let _ = next_server(&mut socket).await; // the attach's own history read

    stub.nav.send_replace(Some(forge_primitives::browser::BrowserNavState {
        can_back: true,
        can_forward: false,
    }));

    let ServerMessage::BrowserNavState { id: moved, tab, can_back, can_forward } =
        next_server(&mut socket).await
    else {
        panic!("a page-driven navigation is pushed to the viewer")
    };
    assert_eq!((moved, tab, can_back, can_forward), (id, 2, true, false), "under the viewed tab");
}

/// **A viewer attaches, is sent frames as the server's own binary kind, and
/// its input obeys the controller role.** The frames stay encoded end to
/// end, and input from a viewer without the role is refused rather than
/// injected.
#[tokio::test]
async fn the_viewer_wire_attaches_relays_frames_and_guards_input() {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let stub = StubBrowser::new();
    fleet
        .workspace()
        .install_browser_host(Arc::clone(&stub) as Arc<dyn forge_workspace::browser::BrowserHost>);
    let (url, _state) = serve_fleet(&fleet, Heartbeat::default()).await;
    let mut socket = connect(&url).await;

    send(
        &mut socket,
        ClientMessage::BrowserView { seat: lead_seat(), tab: None, reply_to: Some(42) },
    )
    .await;
    let ServerMessage::BrowserView { id, profile, tab, controller, viewers, viewer } =
        next_server(&mut socket).await
    else {
        panic!("an attach is answered with the viewer state")
    };
    assert_eq!(id, 5, "the profile's wire id");
    assert_eq!(profile, "TestOrg/default");
    assert_eq!(tab, 2, "the tab the attach resolved");
    assert_eq!(controller, None, "a surface starts controller-less");
    assert_eq!(viewers, 1);
    assert!(viewer > 0, "and this connection's own number, which a take makes the controller");
    let ServerMessage::BrowserNavState { id: nav_id, .. } = next_server(&mut socket).await else {
        panic!("the attach is followed by the page's history")
    };
    assert_eq!(nav_id, 5);
    // **The reply names the profile the attach took**, which is what the
    // page's Done lowers: an omitted profile is resolved by the service, and
    // the client never has to guess what came up.
    let ServerMessage::Reply { reply_to, body } = next_server(&mut socket).await else {
        panic!("the attach answers its reply")
    };
    assert_eq!(reply_to, 42);
    assert_eq!(body["profile"], "TestOrg/default");
    assert_eq!(body["tab"], 2, "and the tab it attached to");

    // A frame reaches the socket as the browser kind, its bytes untouched.
    let encoded = vec![9_u8, 8, 7, 6];
    stub.frames.send_replace(Some(forge_primitives::browser::BrowserFrame {
        profile: 5,
        tab: 2,
        seq: 1,
        encoded: encoded.clone(),
    }));
    let deadline = Instant::now() + Duration::from_secs(5);
    let bytes = loop {
        assert!(Instant::now() < deadline, "the frame reaches the socket");
        let msg = socket.next().await.expect("a message").expect("no error");
        if let Message::Binary(bytes) = msg {
            break bytes;
        }
    };
    assert_eq!(bytes[0], forge_server::transport::frame::Kind::BrowserFrame.tag());
    assert_eq!(&bytes[1..], &encoded[..], "the host's frame crosses untouched");

    // Input before the role is refused, and said.
    send(
        &mut socket,
        ClientMessage::BrowserInput {
            seq: 1,
            event: forge_primitives::browser::BrowserInputEvent::Text { text: "x".to_owned() },
        },
    )
    .await;
    let ServerMessage::Error { what, .. } = next_server(&mut socket).await else {
        panic!("input without the role is refused")
    };
    assert_eq!(what, "browser_input");

    // Taking the role publishes the controller, and input lands.
    send(&mut socket, ClientMessage::BrowserControl { take: true }).await;
    let ServerMessage::BrowserView { controller, .. } = next_server(&mut socket).await else {
        panic!("a take republishes the viewer state")
    };
    assert!(controller.is_some(), "the taker holds the role");
    send(
        &mut socket,
        ClientMessage::BrowserInput {
            seq: 2,
            event: forge_primitives::browser::BrowserInputEvent::Text { text: "y".to_owned() },
        },
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(2);
    while stub.inputs.lock().expect("the stub's inputs").is_empty() {
        assert!(Instant::now() < deadline, "the controller's input reaches the fleet");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        stub.inputs.lock().expect("the stub's inputs")[0].1,
        2,
        "the event's own sequence reached the fleet",
    );

    // Releasing gives the role back, and the state says so.
    send(&mut socket, ClientMessage::BrowserControl { take: false }).await;
    let ServerMessage::BrowserView { controller, .. } = next_server(&mut socket).await else {
        panic!("a release republishes the viewer state")
    };
    assert_eq!(controller, None, "the role is free again");
}

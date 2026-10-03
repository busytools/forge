//! The socket, from the other side of it: a client opens one against a
//! server this test starts itself.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use forge_primitives::{AgentCommand, SessionId, SessionSlot};
use forge_server::Command;
use forge_server::live::Live;
use forge_server::surface::SessionUpdate;
use forge_server::surface::inspector::ContextUsage;
use forge_server::testing::{Fleet, ViewFacts};
use forge_server::transport::TransportState;
use forge_server::transport::envelope::{ClientMessage, ServerMessage, Subject};
use forge_server::transport::serve;
use forge_server::work::WorkCache;
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
/// **The transport does not read a transcript**, so a fixture that seeds one
/// has to hand the conversation over itself - see
/// [`Fleet::hold_conversation`]. A test that skipped that would ask for a page
/// on a seat nothing has seeded and get the empty one.
async fn a_server_with_state() -> (String, Fleet, Arc<TransportState>) {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    let state = Arc::new(TransportState {
        surface: fleet.surface(),
        work: Arc::new(WorkCache::new()),
        conversations: Arc::new(forge_server::transport::conversation::Conversations::new()),
        live: Mutex::new(Live::new()),
        config: forge_primitives::WebConfig::default(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = serve(served, listener).await;
    });
    (format!("ws://{addr}/socket"), fleet, state)
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
        send(socket, ClientMessage::Subscribe { what: subject.clone(), answering: true }).await;
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
async fn next_server_within(socket: &mut Client, ms: u64) -> Option<ServerMessage> {
    let msg = tokio::time::timeout(std::time::Duration::from_millis(ms), socket.next()).await;
    let msg = msg.ok()?.expect("a message").expect("no error");
    Some(serde_json::from_str(msg.to_text().expect("text")).expect("decode"))
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
            ServerMessage::Update { update } => passed.push(*update),
            ServerMessage::Snapshot { subject, data } => return (subject, data, passed),
            other => panic!("a subscribe is answered with a snapshot, not {other:?}"),
        }
    }
}

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
    mut satisfies: impl FnMut(&SessionUpdate) -> bool,
) -> SessionUpdate {
    for _ in 0..64 {
        // Split rather than a `let-else`, so a message that arrived and was not
        // an update is not reported as silence.
        let update = match next_server_within(socket, 5_000).await {
            Some(ServerMessage::Update { update }) => *update,
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
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;

    let (subject, data, _) = snapshot_answering(&mut socket).await;
    assert_eq!(subject, Subject::Home);
    // `Roster`'s field is `projects`, not `orgs` - `orgs` belongs to `AccountsView`,
    // which is the gateway's subject and a different record entirely. Asserting the
    // wrong key here would fail for a reason that looks like the encoder's fault.
    assert!(data.get("projects").is_some(), "the home snapshot carries its projects: {data}");
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

    // Polled, for the same reason as the marks test above: the fold is the
    // transport's own task, so a single snapshot after the emit can lag it by
    // a scheduling hop.
    let mut fresh = connect(&url).await;
    let held = snapshot_until(&mut fresh, Subject::Session(lead_seat()), |data| {
        data["composer"]["take"]["phase"] == "recording"
            && data["composer"]["take"]["floor_db"] == -50.0
    })
    .await;

    assert!(
        held,
        "the take a client never saw announced is on the record, with the silence floor its own meter measures against",
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
    send(&mut socket, ClientMessage::Subscribe { what, answering: true }).await;
    let (_, data, _) = snapshot_answering(&mut socket).await;
    (socket, data)
}

/// A seat a client opens with no usage to report asks the core for one, so the
/// header draws a bar rather than the dash an unasked seat carries for its
/// whole life.
///
/// The terminal asks for the seat it is addressing; a client subscribing to a
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
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
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
            ..ViewFacts::default()
        },
    );
    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
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

/// A turn finishing on a seat a page is holding is the moment its reading
/// stops being true: the transcript grew by the turn, and the number the seat
/// holds was taken before it.
///
/// The other half of the pair above: that one keeps an open page from costing a
/// probe per read, and this one keeps a reading from standing for the life of
/// its occupant.
#[tokio::test]
async fn a_turn_finishing_on_a_seat_a_page_holds_asks_the_core_for_a_reading() {
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
/// transcript under a high reading, and the terminal's own post-compaction
/// refresh is forced for exactly this: its gate reads the same stale number.
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
/// run of quick turns both produce several at once - and the terminal asks a
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

    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
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
            ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
        )
        .await;
        snapshot_answering(&mut leaving).await;
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
    });

    // The fold is the transport's own task, so the mark lands a scheduling
    // hop after the emit. Polling is honest because the property IS
    // "eventually", and it is bounded so a fold that never runs fails this
    // test rather than hanging it.
    let mut fresh = connect(&url).await;
    let mut marked = false;
    let mut last = 0;
    for _ in 0..200 {
        send(&mut fresh, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
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
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
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

    let ServerMessage::Error { what, why } = next_server(&mut socket).await else {
        panic!("a client that asked for an answer has to hear one");
    };
    assert_eq!(what, "reply_to", "the refusal names the field: {why}");
    assert!(why.contains("reply_to"), "and says which field in the sentence: {why}");
    assert!(fleet.dispatched().is_empty(), "a refused command must not run behind the refusal");
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

    let ServerMessage::Error { what, why } = next_server(&mut socket).await else {
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

    let ServerMessage::Error { what, why } = next_server(&mut socket).await else {
        panic!("a page that cannot be answered is refused rather than answered empty")
    };
    assert_eq!(what, "more", "the refusal names what was asked for");
    assert!(
        why.contains("not held yet"),
        "and says why, so a client can tell a delay from an end: {why}",
    );
}

/// The inputs forge can record from, asked for on demand.
///
/// The walk opens the microphone stack, so the request is answered with the
/// list - or, on a machine with no stack to enumerate, with an error naming
/// `devices`, which is what the terminal renders in place of a list. Either
/// way the request is ANSWERED: a client that heard nothing would draw a
/// picker that never fills.
#[tokio::test]
async fn a_devices_request_is_answered_with_the_list_or_its_refusal() {
    let (url, _fleet) = a_server().await;
    let mut socket = connect(&url).await;

    send(&mut socket, ClientMessage::Devices).await;

    match next_server(&mut socket).await {
        ServerMessage::Devices { configured, .. } => {
            assert!(
                configured.is_none(),
                "the fixture's config pins no device, so nothing is configured",
            );
        }
        ServerMessage::Error { what, why } => {
            assert_eq!(what, "devices", "a refusal names what was asked for");
            assert!(!why.is_empty(), "and carries the walk's own reason");
        }
        other => {
            panic!("a devices request is answered with the list or its refusal, got {other:?}")
        }
    }
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
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
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
        if let Some(ServerMessage::Update { update }) = next_server_within(&mut socket, 700).await {
            let SessionUpdate::WorkChanged { key, work, pr, closes } = *update else {
                continue;
            };
            assert_eq!(key, lead_seat(), "the row goes to the seat that was held");
            assert_eq!(work.changed, Some(1), "and carries the count the edit made");
            moved = Some((work, pr, closes));
            break;
        }
    }
    let (work, pr, closes) = moved.expect("a held seat's moved tree reaches the client");

    // The differential: what the update carried is what a read answers, for
    // all three fields. A fresh page learns the tree from the read alone, so
    // an update that disagreed with it would draw one row and then correct
    // itself into another.
    let mut reader = connect(&url).await;
    send(
        &mut reader,
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut reader).await;
    assert_eq!(
        serde_json::to_value(&work).expect("encode"),
        data["work"],
        "the pushed row is the row the record answers",
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
    send(&mut a, ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true })
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
    send(&mut b, ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true })
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
        if let Some(ServerMessage::Update { update }) = next_server_within(&mut a, 700).await
            && matches!(*update, SessionUpdate::WorkChanged { .. })
        {
            return;
        }
    }
    panic!("A's hold did not survive B's visit: its seat stopped being scanned");
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

    let state = Arc::new(TransportState {
        surface: fleet.surface(),
        work: Arc::new(WorkCache::new()),
        conversations: Arc::new(forge_server::transport::conversation::Conversations::new()),
        live: Mutex::new(Live::new()),
        config: forge_primitives::WebConfig::default(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let served = Arc::clone(&state);
    tokio::spawn(async move {
        let _ = forge_server::transport::serve(served, listener).await;
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
            Some(GitPrInfo { number, url: format!("https://example.test/pull/{number}") }),
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
    }
}

/// A seat's snapshot is the row of the tree as the hold read it, however
/// stale what the store held was.
///
/// The hold is taken BEFORE the snapshot is encoded, and it is what reads the
/// tree: a read taken first would hand a page a row from before this
/// subscription, and nothing would correct it, because the loop announces
/// only what moves after the hold.
#[tokio::test]
async fn a_snapshot_carries_the_row_the_hold_read() {
    let (url, _fleet, state, _root) = a_repo_server().await;
    let repo =
        state.surface.roster().cwd_for(&lead_seat()).expect("the fixture seat has a directory");
    // Stale, and saying what the tree cannot: a row the encode can only
    // answer from the store, which the hold's own read then replaces.
    let long_ago = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_secs(60))
        .expect("an instant a minute ago");
    state.surface.store_work_snapshot(
        &lead_seat(),
        a_scan(std::path::Path::new(&repo), long_ago, "not-this-tree", 99, None),
    );

    let mut socket = connect(&url).await;
    send(
        &mut socket,
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
    )
    .await;
    let (_, data, _) = snapshot_answering(&mut socket).await;

    assert_eq!(
        data["work"]["branch"], "work",
        "the snapshot carries the branch the hold read, not the one the store held: {data}",
    );
    assert_eq!(data["work"]["changed"], 0, "and the count the hold read: {data}");
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
    std::fs::write(dir.join("kept.txt"), "one").expect("write");
    git(&["add", "."]);
    git(&["commit", "-qm", "first"]);
}

/// A subscription hears the updates its subject receives and no others.
#[tokio::test]
async fn a_subscriber_hears_the_update_it_asked_for_and_not_another_seats() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
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
    // and the plugin records are fields of its snapshot, so a client that
    // heard them once at subscribe and never again would draw a stale page.
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
    });
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
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
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: false },
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
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
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
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;

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
        msg: forge_primitives::Message::ThinkingTokens {
            estimated_tokens: running,
            estimated_tokens_delta: delta,
            uuid: format!("tokens-{running}"),
            session_id: "s".to_owned(),
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
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: false },
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
            ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: false },
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
        matches!(&msg, ServerMessage::Update { update } if matches!(**update, SessionUpdate::TurnCancelled { .. })),
        "the subscription the client still holds hears the seat it is showing: {msg:?}",
    );
}

/// A subscription does not outlive its socket, which on a long-lived server
/// is a subscription-shaped leak.
#[tokio::test]
async fn a_dropped_socket_leaves_no_subscription_behind() {
    let (url, fleet) = a_server().await;
    let mut socket = connect(&url).await;
    send(&mut socket, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
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
/// assistant answering something nobody saw. The terminal forges that turn in
/// its own process; this is the server's way out doing the same, so a client
/// no longer has to.
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
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: false },
    )
    .await;
    snapshot_answering(&mut socket).await;

    fleet.emit(SessionUpdate::CronPromptAppended {
        key: lead_seat(),
        text: "run the morning summary".to_owned(),
    });

    let ServerMessage::Update { update } = next_server(&mut socket).await else {
        panic!("a delivery a view draws as a turn has to reach the client drawing it")
    };
    let SessionUpdate::ChatAppended { key, msg, .. } = *update else {
        panic!("the frame is what a view draws, and it goes ahead of the typed update")
    };
    assert_eq!(key, lead_seat(), "the frame is addressed to the seat the delivery went to");
    let forge_primitives::Message::User { message, .. } = msg else {
        panic!("a delivery draws as the user turn the model's prompt was")
    };
    let Some(forge_primitives::ContentBlock::Text { text }) = message.content.first() else {
        panic!("the turn carries the prose the model received")
    };
    assert!(
        text.starts_with("[Cron]"),
        "the prose is what the fold's envelope detection reads back, so it is the \
         forged turn rather than some other user frame: {text}",
    );

    let ServerMessage::Update { update } = next_server(&mut socket).await else {
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
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: false },
    )
    .await;
    snapshot_answering(&mut socket).await;

    fleet.emit(SessionUpdate::CronPromptAppended {
        key: SessionSlot::for_label("TestOrg", "proj", Some("w1")),
        text: "run the morning summary".to_owned(),
    });
    // The evidence is ORDER, never a timeout: a frame forged for the other
    // seat would have to arrive ahead of an update this connection does hear,
    // and waiting for it NOT to arrive would hang on the correct behaviour.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });

    let msg = next_server(&mut socket).await;
    assert!(
        matches!(&msg, ServerMessage::Update { update } if matches!(**update, SessionUpdate::TurnCancelled { .. })),
        "a delivery for another seat must not draw on this one: {msg:?}",
    );
}

/// The text of a user turn, when the frame is one.
fn user_text(msg: &forge_primitives::Message) -> Option<String> {
    let forge_primitives::Message::User { message, .. } = msg else {
        return None;
    };
    let Some(forge_primitives::ContentBlock::Text { text }) = message.content.first() else {
        return None;
    };
    Some(text.clone())
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
/// The sender is included on purpose. The terminal pushes the user's own
/// bubble locally on every send for exactly this reason, so withholding the
/// frame from the sender would be the client inventing a rule the terminal
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
            ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
        )
        .await;
        let ServerMessage::Snapshot { .. } = next_server(socket).await else {
            panic!("a seat that exists is answered with its snapshot")
        };
    }

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

    let drawn = |update: &SessionUpdate| {
        matches!(update, SessionUpdate::ChatAppended {
            msg,
            origin: Some(forge_workspace::PromptOrigin::View),
            ..
        } if user_text(msg).as_deref() == Some("hello there"))
    };
    update_until(&mut watcher, "the prompt a client sent, drawn for another client", drawn).await;
    update_until(&mut sender, "the prompt a client sent, drawn for the client that sent it", drawn)
        .await;
    update_until(&mut onlooker, "a third view's send, drawn for this one", drawn).await;

    // And exactly once: the evidence is ORDER, never a timeout,a nd a second
    // forged frame would have to arrive ahead of an update every subscriber
    // hears. Two connections on one seat both forging is the shape this
    // catches.
    fleet.emit(SessionUpdate::TurnCancelled { key: lead_seat() });
    let msg = next_server(&mut onlooker).await;
    assert!(
        matches!(&msg, ServerMessage::Update { update } if matches!(**update, SessionUpdate::TurnCancelled { .. })),
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
/// The terminal draws its thinking bar - a running row carrying the spinner,
/// the elapsed clock and `thinking N` - from the frames the CLI sends, and a
/// client watching the same seat is owed the same turn: the terminal reads
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
    send(&mut watched, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
    snapshot_answering(&mut watched).await;
    send(
        &mut watched,
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
    )
    .await;
    snapshot_answering(&mut watched).await;

    // The control: a connection that watches the home and no seat. The
    // fleet's classification is what keeps the conversation off this
    // subscription, so the same reads that count `watched`'s frames must
    // count none here - a check that could not come back empty would prove
    // nothing about the count it makes.
    let mut home_only = connect(&url).await;
    send(&mut home_only, ClientMessage::Subscribe { what: Subject::Home, answering: true }).await;
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
            ServerMessage::Update { update } => match *update {
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
            other => panic!("a seat's subscription carries updates, got {other:?}"),
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
            ServerMessage::Update { update } => match *update {
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
        ClientMessage::Subscribe { what: Subject::Session(lead_seat()), answering: true },
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
            Some(ServerMessage::Update { update }) => {
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
            ServerMessage::Update { update } => {
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

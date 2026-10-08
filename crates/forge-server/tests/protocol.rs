//! The protocol client, driven against a server this test starts.
//!
//! The binary is run as a subprocess rather than called as a library, because
//! the binary IS the instrument: a test that reached its logic another way
//! would leave the thing a person actually runs unexercised.

// An integration test is a crate of its own, so clippy's test exemption does
// not reach it: the denied lints fire on a file that is entirely test code.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use forge_primitives::tasks::{Task, TaskId, TaskStatus};
use forge_server::live::Live;
use forge_server::testing::Fleet;
use forge_server::transport::TransportState;
use forge_server::transport::serve;
use forge_server::work::WorkCache;
use serde_json::Value;

/// A server over a fixture fleet with something in it, and the URL to reach
/// it.
///
/// Seeded rather than empty, because the walk exists to prove the socket
/// carries REAL VALUES: a walk against a quiet fleet reports null everywhere
/// and reads exactly as green.
async fn a_server() -> (String, Fleet) {
    let fleet = Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
    fleet.start("TestOrg", "proj").expect("the project starts");
    fleet.seed_task(Task {
        id: TaskId::from("t-1"),
        project_name: "proj".to_owned(),
        subject: "a task the home's row counts".to_owned(),
        active_form: Some("counting a task".to_owned()),
        detail: None,
        status: TaskStatus::InProgress,
        owner: None,
        parent: None,
        waiting_on: None,
        estimate: Some(forge_primitives::tasks::Estimate { words: "1d".to_owned(), secs: 86_400 }),
        rank: None,
        verify: None,
        links: vec![forge_primitives::tasks::TaskLink {
            kind: forge_primitives::tasks::LinkKind::Path,
            label: None,
            target: "crates/forge-server".to_owned(),
            state: None,
            added_at: std::time::SystemTime::UNIX_EPOCH,
        }],
        attempt: 0,
        archived_at: None,
        created_at: std::time::SystemTime::UNIX_EPOCH,
        updated_at: std::time::SystemTime::UNIX_EPOCH,
    });

    let state = Arc::new(TransportState {
        surface: fleet.surface(),
        work: Arc::new(WorkCache::new()),
        conversations: Arc::new(forge_server::transport::conversation::Conversations::new()),
        live: Mutex::new(Live::new()),
        client: forge_primitives::ClientConfig::default(),
        browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = serve(state, listener).await;
    });
    (format!("ws://{addr}/socket"), fleet)
}

/// The client's transcript: every message the server said, in order.
async fn walk(script: &str, url: &str) -> Vec<Value> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("walk.txt");
    std::fs::write(&path, script).expect("write the script");

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_forge-protocol-client"))
        .arg("--url")
        .arg(url)
        .arg("--script")
        .arg(&path)
        .arg("--seconds")
        .arg("3")
        .output()
        .await
        .expect("the client runs");
    assert!(
        output.status.success(),
        "the client exited cleanly: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("one message per line"))
        .collect()
}

fn of_kind<'a>(transcript: &'a [Value], kind: &str) -> Vec<&'a Value> {
    transcript
        .iter()
        .filter(|message| message.get("kind").and_then(Value::as_str) == Some(kind))
        .collect()
}

/// The walk the plan asks for: subscribe to the home and to a seat, and hear
/// a snapshot for each - the seat's attach re-sending the home, because
/// showing a seat spends the marks its rows carry.
#[tokio::test]
async fn the_protocol_client_walks_the_whole_protocol() {
    let (url, _fleet) = a_server().await;
    let transcript = walk("subscribe home\nsubscribe session TestOrg/proj/lead\n", &url).await;

    let snapshots = of_kind(&transcript, "snapshot");
    assert_eq!(
        snapshots.len(),
        3,
        "one snapshot per subscribe, and the home the attach re-sends: {transcript:?}",
    );
    // The order is the contract: what was asked for, then the refreshed home.
    assert_eq!(
        snapshots[0]["subject"],
        Value::String("home".to_owned()),
        "the home answers first: {transcript:?}",
    );
    assert!(
        snapshots[1]["subject"].get("session").is_some(),
        "then the seat's own read: {transcript:?}",
    );
    assert_eq!(
        snapshots[2]["subject"],
        Value::String("home".to_owned()),
        "and the attach re-sends the home: {transcript:?}",
    );
}

/// The models page's read: a subscription answered with the page's whole
/// view - the pinned models, the catalogue check and the feed's rows.
#[tokio::test]
async fn the_models_subject_answers_the_pages_read() {
    let (url, _fleet) = a_server().await;
    let transcript = walk("subscribe dictate_models\n", &url).await;

    let snapshot = of_kind(&transcript, "snapshot")
        .into_iter()
        .find(|message| message["subject"] == Value::String("dictate_models".to_owned()))
        .expect("the models subscription is answered with its snapshot");
    let data = &snapshot["data"];

    assert_eq!(data["enabled"], Value::Bool(true), "the fixture fleet runs dictation");
    assert_eq!(
        data["in_use"].as_array().map(Vec::len),
        Some(2),
        "both pinned models cross with the read: {data}",
    );
    assert_eq!(
        data["check"]["state"], "never",
        "no check has run on this machine, and the client needs that as a value: {data}",
    );
    assert!(
        data["rows"].as_array().is_some_and(Vec::is_empty),
        "no fetched feed is no rows rather than a broken field: {data}",
    );
}

/// The five fields Task 8b added, asserted as VALUES rather than as keys.
///
/// **The wire fixtures pin shape and normalise anything that moves** - paths,
/// and the build stamp, which carries the commit - so this walk is the only
/// place a real value can appear. A key that exists and is null would satisfy
/// a presence check and leave a client drawing a blank cell.
#[tokio::test]
async fn the_homes_five_reads_arrive_with_real_values() {
    let (url, _fleet) = a_server().await;
    let transcript = walk("subscribe home\n", &url).await;
    let home = of_kind(&transcript, "snapshot")
        .into_iter()
        .find(|message| message["subject"] == Value::String("home".to_owned()))
        .expect("the home snapshot");
    let row = &home["data"]["projects"][0];

    assert!(
        row["work"]["gate"].is_string(),
        "the row's working tree says something about itself: {}",
        row["work"],
    );
    assert!(
        row["rows"].as_array().is_some_and(|rows| !rows.is_empty()),
        "the row's board rows are a populated list, not an empty one: {}",
        row["rows"],
    );
    assert!(
        row["rows"][0]["worked_secs"].is_u64(),
        "and each row carries its worked time, so the client never derives it: {}",
        row["rows"][0],
    );
    assert!(
        row["rows"][0]["marks"]["ready"].is_boolean(),
        "and its marks, one boolean per fact: {}",
        row["rows"][0]["marks"],
    );
    assert!(
        home["data"]["fleet"].as_array().is_some_and(|fleet| !fleet.is_empty()),
        "the fleet glance crosses beside the projects: {}",
        home["data"]["fleet"],
    );
    assert!(row["would_bind"].is_boolean(), "the row says whether a spawn would bind: {row}");
    assert!(
        home["data"]["forge_version"].as_str().is_some_and(|version| !version.is_empty()),
        "the header carries the forge build that answered: {}",
        home["data"]["forge_version"],
    );
    assert!(
        home["data"]["dictate"]["enabled"].is_boolean(),
        "and dictate says whether it is on, rather than leaving it to be inferred: {}",
        home["data"]["dictate"],
    );
}

/// A command that asked for an answer hears one, on the channel it is
/// watching - the path a client's worker-spawn and review flows depend on.
#[tokio::test]
async fn an_awaited_command_is_answered_with_a_reply() {
    let (url, _fleet) = a_server().await;
    let seat = r#"{"org":"TestOrg","project":"proj","label":"lead"}"#;
    let script = format!(
        "ask {{\"submit_review\":{{\"project\":\"proj\",\"branch\":\"main\",\"summary\":null,\"thread_ids\":[],\"origin\":{seat}}}}}\n"
    );

    let transcript = walk(&script, &url).await;

    let replies = of_kind(&transcript, "reply");
    assert_eq!(replies.len(), 1, "the awaited command is answered once: {transcript:?}");
    assert_eq!(replies[0]["reply_to"], Value::from(1), "on the handle the client set");
    assert!(
        replies[0].get("body").is_some(),
        "and the body carries the answer, a refusal included: {}",
        replies[0],
    );
}

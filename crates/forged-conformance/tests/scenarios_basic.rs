//! Live-capture scenarios for the forged↔client wire.
//!
//! These tests are `#[ignore]` by default — they spin up a real forged on
//! an ephemeral loopback port, drive a WS client through the scenario,
//! capture the bidirectional trace as JSONL under
//! `target/forged-wire-traces/`, and exit.
//!
//! Opt in with `FORGED_WIRE_CAPTURE=1` and `--run-ignored only`:
//!
//! ```bash
//! FORGED_WIRE_CAPTURE=1 cargo nextest run -p forged-conformance \
//!   --no-capture --run-ignored only capture_m1_status
//! ```
//!
//! Promote a captured trace into the committed baseline:
//!
//! ```bash
//! cp target/forged-wire-traces/m1_status-*.jsonl \
//!    crates/forged-conformance/baselines/0.1.64/m1_status.jsonl
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use forged_conformance::{PINNED_FORGED_VERSION, TraceEntry};

#[tokio::test]
#[ignore = "live capture; opt-in via FORGED_WIRE_CAPTURE=1"]
async fn capture_m1_status() {
    if std::env::var("FORGED_WIRE_CAPTURE").is_err() {
        eprintln!("FORGED_WIRE_CAPTURE not set; skipping");
        return;
    }

    // 1. Bind forged on ephemeral port.
    let state = forged::registry::DaemonState::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _h = tokio::spawn(forged::server::run(listener, state));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 2. Trace sink.
    let trace = Arc::new(Mutex::new(Vec::<TraceEntry>::new()));

    // 3. Open WS, capture client.identify (out from daemon's POV → "out"),
    //    send daemon.status (in from daemon's POV → "in"), capture response
    //    (out → "out").
    let url = format!("ws://{addr}/");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected first frame to be Text (client.identify)");
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "daemon.status",
        "params": {}
    });
    let body = serde_json::to_string(&req).unwrap();
    trace.lock().push(TraceEntry {
        dir: "in".into(),
        line: body.clone(),
    });
    ws.send(WsMsg::Text(body)).await.unwrap();

    let WsMsg::Text(t) = ws.next().await.unwrap().unwrap() else {
        panic!("expected response frame to be Text");
    };
    trace.lock().push(TraceEntry {
        dir: "out".into(),
        line: t,
    });

    // 4. Dump to disk.
    let target = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/forged-wire-traces");
    std::fs::create_dir_all(&target).unwrap();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = target.join(format!("m1_status-{ts}.jsonl"));
    let mut body = String::new();
    for e in trace.lock().iter() {
        use std::fmt::Write;
        let line = serde_json::to_string(e).expect("serialize trace entry");
        writeln!(body, "{line}").expect("write trace line");
    }
    std::fs::write(&path, body).unwrap();
    eprintln!("captured trace: {}", path.display());
    eprintln!(
        "promote with: cp {} crates/forged-conformance/baselines/{}/m1_status.jsonl",
        path.display(),
        PINNED_FORGED_VERSION
    );
}

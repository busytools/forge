//! The browser host against the vendored stack: launch, drive, answer.
//!
//! **Deliberately ignored.** It needs `just vendor-browser-stack` to have
//! run - half a gigabyte of browser, node and driver - so it cannot be part
//! of a gate every checkout runs. Run it where the stack is:
//!
//! ```text
//! cargo nextest run --manifest-path client/src-tauri/Cargo.toml \
//!     --run-ignored ignored-only -E 'test(a_session_drives_a_page)'
//! ```
//!
//! What it proves is the whole client half of the pipe in one go: the
//! vendored Chromium launches against a profile nobody has used, the vendored
//! driver attaches to it over CDP, and upstream's own `browser_navigate` and
//! `browser_snapshot` answer through the host - which is exactly what an ask
//! from a session rides.

use std::path::PathBuf;
use std::process::Command;

use forge_client::browser::{BrowserHost, StackPaths};
use serde_json::json;

/// Kill the browser this test launched, by the PORT it holds - never by a
/// pattern: this machine runs other browsers, and one of them belongs to the
/// person sitting at it.
fn kill_the_browser_on(port: u16) {
    let Ok(listed) = Command::new("lsof")
        .args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
    else {
        return;
    };
    let pids = String::from_utf8_lossy(&listed.stdout);
    for pid in pids.split_whitespace() {
        let _ = Command::new("kill").arg(pid).status();
    }
}

#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn a_session_drives_a_page() {
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = StackPaths {
        stack,
        profile: dir.path().join("profile"),
        output: dir.path().join("output"),
    };
    let host = BrowserHost::new(paths.clone());

    let page = "data:text/html,<h1>forge browser host</h1>";
    let navigated = host.call("browser_navigate", json!({ "url": page })).await;
    let port = forge_client::browser::chromium::read_active_port(&paths.profile)
        .expect("the launch wrote its port file")
        .port;
    let parts = match navigated {
        Ok(parts) => parts,
        Err(why) => {
            kill_the_browser_on(port);
            panic!("the host could not navigate: {why}");
        }
    };
    assert!(!parts.is_empty(), "a navigate answers with something");

    let snapshot = match host.call("browser_snapshot", json!({})).await {
        Ok(parts) => parts,
        Err(why) => {
            kill_the_browser_on(port);
            panic!("the host could not snapshot: {why}");
        }
    };
    let text = snapshot
        .iter()
        .filter_map(|part| match part {
            forge_client::browser::driver::ReplyPart::Text { text } => Some(text.clone()),
            forge_client::browser::driver::ReplyPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    kill_the_browser_on(port);

    assert!(
        text.contains("forge browser host"),
        "the snapshot is of the page that was navigated to: {text}",
    );
}

//! A page's file chooser, live: the host announces the dialog instead of
//! raising one, and the answer's server-local paths become the input's files.
//!
//! **This is the human's half of the upload path.** The bytes are the
//! client's business; what this pins is that the host hands CEF the paths the
//! server wrote, and the page sees them as chosen files.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::time::{Duration, Instant};

use forge_browser::control::{HostEvent, parse};
use forge_browser::input::{HostInput, host_line};
use forge_browser::window::Cdp;
use serde_json::json;

/// How long CEF is given to boot, paint and answer.
const BOUND: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(50);

/// Wait for the host's answer that `wanted` accepts, failing with what it
/// wrote otherwise.
fn wait_event(host: &support::Host, wanted: impl Fn(&HostEvent) -> bool) -> HostEvent {
    let deadline = Instant::now() + BOUND;
    loop {
        if let Some(event) =
            host.events().iter().filter_map(|line| parse(line)).find(|event| wanted(event))
        {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "the host never answered; it wrote {:?} (log: {})",
            host.events(),
            host.log(),
        );
        std::thread::sleep(POLL);
    }
}

/// **A click on a file input raises the dialog here, and the answer's path
/// becomes the input's file.** The name the page sees is the file's own,
/// not the server's flattened spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_page_file_chooser_is_answered_with_a_server_path() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let port = support::free_port();
    let ring_name = format!("/forge-fc-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let mut host =
        support::Host::start_with_events(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let id = support::wait_for_page(port, &log, BOUND);
    let (mut cdp, _events) =
        Cdp::connect_watch(port, &format!("/devtools/page/{id}")).await.expect("devtools");
    cdp.call(
        "Runtime.evaluate",
        json!({ "expression": "document.documentElement.style.margin='0'; \
            document.body.style.margin='0'; document.body.innerHTML = '<input id=\"f\" \
            type=\"file\" style=\"position:absolute;left:100px;top:100px;width:300px;\
            height:40px\">'; 0" }),
        None,
    )
    .await
    .expect("the input is on the page");

    // **The first pointer event after a host starts is swallowed by
    // activation**, so the click that means something comes second.
    for (x, y) in [(600, 500), (200, 120)] {
        for action in [
            forge_primitives::browser::MouseAction::Press,
            forge_primitives::browser::MouseAction::Release,
        ] {
            host.input(&host_line(&HostInput::Mouse {
                tab: 1,
                action,
                x,
                y,
                button: 0,
                clicks: 1,
                modifiers: forge_primitives::browser::InputModifiers::default(),
            }));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let HostEvent::FileDialog { id: dialog, multiple } =
        wait_event(&host, |event| matches!(event, HostEvent::FileDialog { .. }))
    else {
        panic!("the answer is a file dialog event");
    };
    assert!(!multiple, "a plain file input takes one file");

    let chosen = scratch.path().join("resume.txt");
    std::fs::write(&chosen, b"my resume").expect("the chosen file");
    host.input(&host_line(&HostInput::File {
        id: dialog,
        paths: vec![chosen.to_string_lossy().into_owned()],
    }));

    let deadline = Instant::now() + BOUND;
    let files = loop {
        let reply = cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": "(() => { const f = document.getElementById('f').files[0]; \
                    return f ? [f.name, f.size] : null; })()", "returnByValue": true }),
                None,
            )
            .await
            .expect("the page answers");
        if let Some(value) = reply.pointer("/result/value").filter(|value| !value.is_null()) {
            break value.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the input never took the chosen file (log: {})",
            host.log()
        );
        std::thread::sleep(POLL);
    };
    assert_eq!(files[0], "resume.txt", "the page sees the file's own name");
    assert_eq!(files[1], 9, "and its bytes");

    let status = host.terminate(Duration::from_secs(15));
    assert!(status.success(), "the host exited cleanly: {status} (log: {})", host.log());
}

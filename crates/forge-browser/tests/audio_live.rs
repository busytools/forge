//! A page's sound, live: the host's audio handler turns CEF's planar floats
//! into the PCM packets the server relays, beside the frames.
//!
//! **The whole path is pinned here**: an oscillator in the page reaches the
//! host's `CefAudioHandler`, crosses the profile's socket as an [`Audio`]
//! message under its own kind byte, and carries the profile id, a rate and
//! samples rather than an empty stream.
//!
//! **These need the real bundle**: they stage the workspace's own host the
//! way the server ships it, then drive it as the server does.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::time::{Duration, Instant};

use forge_browser::channel::{self, ChannelMessage};
use forge_browser::osr::Ring;
use forge_browser::window::Cdp;
use serde_json::json;

/// How long CEF is given to boot, play and answer.
const BOUND: Duration = Duration::from_secs(60);

/// **A page that plays a tone is heard on the profile's socket.** The tone
/// needs a gesture, which the evaluation carries; the packets that follow
/// carry the profile's id and real samples.
#[tokio::test(flavor = "multi_thread")]
async fn a_pages_sound_crosses_the_socket() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let socket = channel::socket_path(&profile);
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = channel::serve(&socket, move |message| {
        if let ChannelMessage::Audio(audio) = message {
            let _ = tx.send(audio);
        }
    })
    .expect("the socket is served");

    let port = support::free_port();
    let ring_name = format!("/forge-snd-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let args = ["--profile-id".to_owned(), "7".to_owned()];
    let mut host = support::Host::start(&exe, &profile, port, &ring_name, "800x600", &args, &log);
    let id = support::wait_for_page(port, &log, BOUND);
    let (mut cdp, _events) =
        Cdp::connect_watch(port, &format!("/devtools/page/{id}")).await.expect("devtools");

    // **A viewer is watching**, which is what lets the host's sink pass
    // audio rather than drop it with the frames.
    let ring = {
        let deadline = Instant::now() + BOUND;
        loop {
            match Ring::open(&ring_name) {
                Ok(ring) => break ring,
                Err(why) => {
                    assert!(
                        Instant::now() < deadline,
                        "the host's ring never appeared: {why} (log: {})",
                        host.log()
                    );
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    };
    ring.pause(false);
    ring.key();

    // **The tone runs for five seconds**, so a slow first packet is still
    // inside the stream rather than a playback that already ended.
    cdp.call(
        "Runtime.evaluate",
        json!({
            "expression": "window.__context = new AudioContext(); \
                const oscillator = window.__context.createOscillator(); \
                const gain = window.__context.createGain(); \
                gain.gain.value = 0.2; \
                oscillator.frequency.value = 440; \
                oscillator.connect(gain); gain.connect(window.__context.destination); \
                oscillator.start(); oscillator.stop(window.__context.currentTime + 5); \
                window.__context.state + ' ' + String(window.__context.sampleRate)",
            "userGesture": true,
            "returnByValue": true,
        }),
        None,
    )
    .await
    .expect("the page starts its tone");

    // **The first packets are the stream's priming silence**, so the tone is
    // waited for rather than read off the first packet.
    let deadline = Instant::now() + BOUND;
    let audio = loop {
        let packet = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("a packet crosses the socket");
        if packet.samples.iter().any(|sample| *sample != 0) {
            break packet;
        }
    };
    assert_eq!(audio.profile, 7, "the packet names the server's profile id");
    assert!(audio.sample_rate >= 8_000, "a real rate: {}", audio.sample_rate);
    assert!(audio.channels >= 1, "at least one channel");
    assert!(!audio.samples.is_empty(), "the packet carries samples");

    let status = host.terminate(Duration::from_secs(15));
    assert!(status.success(), "the host exited cleanly: {status} (log: {})", host.log());
    reader.stop();
    Ring::unlink(&ring_name);
}

//! Frames as the host writes them and the server reads them: one length
//! prefix per frame over a Unix socket, and the newest frame winning.
//!
//! The wire test uses bytes standing in for encoded pixels, because what it
//! pins is the framing and the header, not libwebp's output. The macOS test
//! beside it is the encoder's own proof: a real painted page becomes a WebP
//! frame on a real socket.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

#[cfg(target_os = "macos")]
mod support;

use std::time::Duration;

use forge_browser::channel;
use forge_browser::frame::{Format, Frame};

fn frame(seq: u32, profile: u16, width: u16, height: u16) -> Frame {
    Frame {
        profile,
        tab: 1,
        seq,
        width,
        height,
        format: Format::Webp,
        key: seq == 0,
        page_width: width / 2,
        page_height: height / 2,
        bytes: vec![42; 64],
    }
}

/// **Frames cross a socket whole and in order.** The reader's sink is what a
/// service relays, so a frame arriving cut short or out of order is a
/// protocol defect, not a drawing one.
#[test]
fn frames_cross_the_socket_whole_and_in_order() {
    let dir = tempfile::tempdir().expect("a scratch dir");
    let path = channel::socket_path(dir.path());
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = channel::serve(&path, move |message| {
        if let channel::ChannelMessage::Frame(frame) = message {
            let _ = tx.send(frame);
        }
    })
    .expect("the socket is served");

    let mut stream = channel::connect(&path).expect("the host side connects");
    channel::write(&mut stream, &frame(0, 3, 640, 480)).expect("the first frame");
    channel::write(&mut stream, &frame(1, 3, 1024, 768)).expect("the second frame");

    let first = rx.recv_timeout(Duration::from_secs(5)).expect("the first frame arrives");
    let second = rx.recv_timeout(Duration::from_secs(5)).expect("the second frame arrives");
    assert_eq!(first.seq, 0);
    assert_eq!(first.profile, 3);
    assert!(first.key, "a surface's first frame is a key frame");
    assert_eq!((second.seq, second.key), (1, false));
    assert_eq!((second.width, second.height), (1024, 768), "a resize carries its size");
    assert_eq!(second.bytes, vec![42; 64], "the payload crosses untouched");

    drop(stream);
    reader.stop();
    assert!(!path.exists(), "the socket is unlinked when the reader ends");
}

/// **Audio crosses the same socket as frames, and the kinds do not mix.**
/// The page's sound is a stream beside the picture: a frame that arrives
/// after a packet is still a frame, and the samples cross whole.
#[test]
fn audio_crosses_the_socket_beside_frames() {
    let dir = tempfile::tempdir().expect("a scratch dir");
    let path = channel::socket_path(dir.path());
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = channel::serve(&path, move |message| {
        let _ = tx.send(message);
    })
    .expect("the socket is served");

    let mut stream = channel::connect(&path).expect("the host side connects");
    let packet = channel::Audio {
        profile: 3,
        tab: 1,
        seq: 9,
        sample_rate: 48_000,
        channels: 2,
        samples: vec![1, -1, 300, -300],
    };
    channel::write_audio(&mut stream, &packet).expect("an audio packet");
    channel::write(&mut stream, &frame(4, 3, 640, 480)).expect("a frame after it");

    let channel::ChannelMessage::Audio(got) =
        rx.recv_timeout(Duration::from_secs(5)).expect("the packet arrives")
    else {
        panic!("the first message is the audio packet");
    };
    assert_eq!(got, packet, "the samples cross whole and in order");
    let channel::ChannelMessage::Frame(got) =
        rx.recv_timeout(Duration::from_secs(5)).expect("the frame arrives")
    else {
        panic!("the second message is the frame");
    };
    assert_eq!(got.seq, 4, "and the frame beside it is still a frame");

    drop(stream);
    reader.stop();
}

/// **A large frame crosses whole, and the stream survives the quiet spells
/// between frames.** A body arrives in many chunks and the reader polls with
/// a short timeout; a timeout mid-body resumes rather than failing the frame.
/// Measured on the dev stack: a ~100 KB frame was cut short on EAGAIN, the
/// reader closed its end, and every later frame died on a broken pipe.
#[test]
fn a_large_frame_crosses_whole_and_the_stream_survives_quiet() {
    let dir = tempfile::tempdir().expect("a scratch dir");
    let path = channel::socket_path(dir.path());
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = channel::serve(&path, move |message| {
        if let channel::ChannelMessage::Frame(frame) = message {
            let _ = tx.send(frame);
        }
    })
    .expect("the socket is served");

    let mut stream = channel::connect(&path).expect("the host side connects");
    let body = vec![7_u8; 1024 * 1024];
    for seq in 0..4u32 {
        let mut frame = frame(seq, 3, 1440, 900);
        frame.bytes.clone_from(&body);
        channel::write(&mut stream, &frame).expect("a large frame");
    }

    for seq in 0..4u32 {
        let got = rx.recv_timeout(Duration::from_secs(10)).expect("every frame arrives");
        assert_eq!(got.seq, seq, "and in order");
        assert_eq!(got.bytes.len(), 1024 * 1024, "with its body whole");
    }

    drop(stream);
    reader.stop();
}

/// **A body that trickles across the reader's polls is read whole.** The
/// reader polls so it can look at its stop flag; a write that pauses in the
/// middle resumes where it left off. Measured on the dev stack: EAGAIN cut a
/// ~100 KB frame short, the reader closed its end, and every later frame
/// died on a broken pipe.
#[test]
fn a_frame_that_trickles_across_polls_is_read_whole() {
    use std::io::Write as _;

    let dir = tempfile::tempdir().expect("a scratch dir");
    let path = channel::socket_path(dir.path());
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = channel::serve(&path, move |message| {
        if let channel::ChannelMessage::Frame(frame) = message {
            let _ = tx.send(frame);
        }
    })
    .expect("the socket is served");

    let mut stream = channel::connect(&path).expect("the host side connects");
    let mut whole = frame(4, 2, 640, 480);
    whole.bytes = vec![9_u8; 200 * 1024];
    let encoded = whole.encode();
    stream
        .write_all(&u32::try_from(encoded.len() + 1).expect("a frame's length").to_le_bytes())
        .expect("the length");
    stream.write_all(&[channel::KIND_FRAME]).expect("the kind byte");
    for chunk in encoded.chunks(40 * 1024) {
        stream.write_all(chunk).expect("a chunk of the body");
        // Longer than the reader's 200 ms poll: each chunk lands as a
        // separate resume rather than one read.
        std::thread::sleep(Duration::from_millis(300));
    }

    let got = rx.recv_timeout(Duration::from_secs(5)).expect("the frame arrives whole");
    assert_eq!(got.bytes, whole.bytes, "every byte of the body arrived");
    drop(stream);
    reader.stop();
}

/// **The host encodes real frames onto its socket.** A painted page becomes
/// a WebP frame at the view's size, sent to the profile's socket with the
/// id the server assigned, and the first one is full quality.
#[cfg(target_os = "macos")]
#[test]
fn the_host_encodes_frames_to_its_socket() {
    use forge_browser::osr::Ring;

    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let path = channel::socket_path(&profile);
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = channel::serve(&path, move |message| {
        if let channel::ChannelMessage::Frame(frame) = message {
            let _ = tx.send(frame);
        }
    })
    .expect("the socket is served");

    let ring_name = format!("/forge-fr-{}", std::process::id());
    let log = scratch.path().join("host.log");
    let args = ["--profile-id".to_owned(), "9".to_owned()];
    let mut host = support::Host::start(
        &exe,
        &profile,
        support::free_port(),
        &ring_name,
        "800x600",
        &args,
        &log,
    );

    // **A host with no viewer is paused**: the service unpauses it when the
    // first viewer attaches and asks for a key frame. Attach the same way.
    let ring = {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match Ring::open(&ring_name) {
                Ok(ring) => break ring,
                Err(why) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the host's ring never appeared: {why} (log: {})",
                        host.log()
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    };
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "a host nobody watches encodes no frames"
    );
    ring.pause(false);
    ring.key();

    let first = rx.recv_timeout(Duration::from_secs(45)).expect("a painted frame arrives");
    assert_eq!(first.profile, 9, "the frame names the server's profile id");
    assert_eq!((first.width, first.height), (800, 600), "the frame is the requested view");
    assert_eq!(
        (first.page_width, first.page_height),
        (800, 600),
        "and it names the page it was painted from"
    );
    assert!(first.key, "a surface's first frame is full quality");
    assert_eq!(&first.bytes[..4], b"RIFF", "the payload is an image container");
    assert_eq!(&first.bytes[8..12], b"WEBP", "and it is WebP");

    // **A show cue is acknowledged by the host**, not merely written: the
    // server waits for the applied sequence before it drives a call.
    let show = ring.show();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ring.shown(show) {
        assert!(
            std::time::Instant::now() < deadline,
            "the host never acknowledged the show cue (log: {})",
            host.log()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // **The view-size request reaches CEF**: the next frame is the size the
    // controller's surface asked for, and a resize is full quality.
    ring.request(400, 300, 1.0);
    let resized = {
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        loop {
            let frame = match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(frame) => frame,
                Err(why) => {
                    assert!(
                        std::time::Instant::now() < deadline && host.try_wait().is_none(),
                        "no resized frame arrived ({why}; log: {})",
                        host.log()
                    );
                    continue;
                }
            };
            if (frame.width, frame.height) == (400, 300) {
                break frame;
            }
        }
    };
    assert!(resized.key, "a resize draws a key frame");
    assert_eq!(
        (resized.page_width, resized.page_height),
        (400, 300),
        "and the resized frame names the view it was laid out at"
    );

    let status = host.terminate(Duration::from_secs(15));
    assert!(status.success(), "the host exited cleanly: {status} (log: {})", host.log());
    reader.stop();
    Ring::unlink(&ring_name);
}

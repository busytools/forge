//! The server's own host bundle: a real `Forge Browser.app` starts, creates
//! a windowless browser, paints one frame and exits on SIGTERM.
//!
//! **The bundle is built by the staging script from this workspace's own
//! binary**, never from the client's - nothing here touches the client
//! bundle, and the test passes on a machine with no client installed.
//!
//! The bundle is staged into a scratch directory unless an installed one is
//! named (`FORGE_BROWSER_TEST_BUNDLE`): the installed copy at
//! `host_bundle::path()` is the same layout, and this test is the layout's
//! proof, not a second install.

#![cfg(target_os = "macos")]
#![allow(unsafe_code, reason = "the test reads the ring mapping and signals its own child")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use forge_browser::osr::{MAX_HEIGHT, MAX_WIDTH, Ring, STATE_LIVE};

/// How long a fresh host is given to initialize CEF and paint.
const FRAME_BOUND: Duration = Duration::from_secs(45);
/// How long a signalled host is given to close its browser and exit.
const EXIT_BOUND: Duration = Duration::from_secs(15);
const POLL_EVERY: Duration = Duration::from_millis(50);

#[tokio::test(flavor = "current_thread")]
async fn the_server_bundle_starts_paints_one_frame_and_exits_on_sigterm() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    assert!(exe.is_file(), "the bundle carries its executable at {}", exe.display());

    let profile = scratch.path().join("profile");
    let log = scratch.path().join("host.log");
    let port = support::free_port();
    // POSIX shared-memory names are short; the pid keeps two runs apart.
    let ring_name = format!("/forge-hb-{}", std::process::id());
    let mut host = support::Host::start(&exe, &profile, port, &ring_name, "800x600", &[], &log);
    let mut ring = RingGuard::new(ring_name);

    // A frame is live: the host said so, and a whole frame is readable.
    let deadline = Instant::now() + FRAME_BOUND;
    loop {
        if let Ok(mapped) = Ring::open(ring.name()) {
            ring.hold(mapped);
            if ring.live() {
                break;
            }
        }
        let exited = host.try_wait();
        let tail = host.log();
        assert!(exited.is_none(), "the host exited before painting a frame (log: {tail})");
        assert!(
            Instant::now() < deadline,
            "no frame within {} s (log: {tail})",
            FRAME_BOUND.as_secs()
        );
        tokio::time::sleep(POLL_EVERY).await;
    }

    // The pixels are a whole frame: an even counter across the read, at the
    // requested size, with real content in it.
    let mut settled = false;
    for _ in 0..40 {
        if let Some(frame) = ring.whole_frame()
            && frame.whole
        {
            assert_eq!((frame.width, frame.height), (800, 600), "the frame is the requested view");
            assert!(
                frame.bytes.iter().any(|channel| *channel != 0),
                "the frame carries pixels, not a blank mapping"
            );
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(settled, "no whole frame was readable (log: {})", host.log());

    // The browser really exists: the host's devtools endpoint names its page.
    let page = forge_browser::spawn::page_url(port).await;
    assert_eq!(page.as_deref(), Some("about:blank"), "the windowless browser opened blank");

    // A signal is a shutdown: the host closes its browser and exits cleanly.
    let status = host.terminate(EXIT_BOUND);
    assert!(status.success(), "the host exited cleanly: {status} (log: {})", host.log());
}

/// The ring, kept mapped for the test and unlinked when it ends.
struct RingGuard {
    name: String,
    mapped: Option<Ring>,
}

impl RingGuard {
    fn new(name: String) -> Self {
        Self { name, mapped: None }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn hold(&mut self, ring: Ring) {
        self.mapped = Some(ring);
    }

    /// Whether the host has said it is live and has painted at least once.
    fn live(&self) -> bool {
        let Some(ring) = self.mapped.as_ref() else { return false };
        // SAFETY: the mapping is held for the test's whole body.
        let header = unsafe { &*ring.header() };
        header.state.load(Ordering::Acquire) == STATE_LIVE
            && header.paints.load(Ordering::Relaxed) >= 1
    }

    /// One frame's pixels, with whether the seqlock was even across the copy.
    fn whole_frame(&self) -> Option<Frame> {
        let ring = self.mapped.as_ref()?;
        // SAFETY: the mapping is held for the test's whole body.
        let header = unsafe { &*ring.header() };
        let before = header.frame.load(Ordering::Acquire);
        let width = header.width.load(Ordering::Relaxed);
        let height = header.height.load(Ordering::Relaxed);
        if width == 0 || height == 0 || width > MAX_WIDTH || height > MAX_HEIGHT {
            return None;
        }
        let len = usize::try_from(width).ok()? * usize::try_from(height).ok()? * 4;
        // SAFETY: the ring holds header plus MAX_WIDTH*MAX_HEIGHT pixels, and
        // the size above is bounded by those caps.
        let bytes = unsafe { std::slice::from_raw_parts(ring.pixels(), len) }.to_vec();
        let after = header.frame.load(Ordering::Acquire);
        Some(Frame { width, height, bytes, whole: before == after && before % 2 == 0 })
    }
}

/// One read of the ring's current frame.
struct Frame {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
    whole: bool,
}

impl Drop for RingGuard {
    fn drop(&mut self) {
        Ring::unlink(&self.name);
    }
}

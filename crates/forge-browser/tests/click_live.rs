//! The pointer's coordinate, live: the page a viewer's surface asks for is
//! the page it gets, and a click the server sends reaches the pixel it names.
//!
//! **The regression guard is the layout assertion**: with no viewport
//! override applied, a surface's size is measured in device pixels - a
//! 2048x1112 desktop takeover at 2x asks for 4096x2224 - and the frame budget
//! that holds those pixels must never move the CSS size the page lays out at.
//! When the budget moved the layout instead, the page came back 1920x1080
//! under a 2048x1112 surface, the desktop drew it letterboxed, and every
//! click the surface sent landed right of the pixel under the pointer. **The
//! click checks pin the other half**: the host forwards a gesture's DIP one to
//! one, which holds on unmodified main, so they catch a scale or an offset
//! introduced between the host and the page rather than the budget itself.
//!
//! **These need the real bundle**: they stage the workspace's own host the
//! way the server ships it, then drive it as the server does.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
#![allow(unsafe_code, reason = "a test ends its own child with a signal")]

mod support;

use std::time::{Duration, Instant};

use forge_browser::channel;
use forge_browser::frame::Frame;
use forge_browser::input::{HostInput, host_line};
use forge_browser::osr::{MAX_HEIGHT, MAX_WIDTH, Ring};
use forge_browser::window::Cdp;
use forge_primitives::browser::{InputModifiers, MouseAction};
use serde_json::{Value, json};

/// How long CEF is given to boot, paint and answer a gesture.
const BOUND: Duration = Duration::from_secs(45);
const POLL_EVERY: Duration = Duration::from_millis(50);

/// A grid page that records where each mousedown landed, and the viewport it
/// landed in.
fn grid_page() -> String {
    let page = "<!doctype html><meta charset=utf-8><style>\
        html,body{margin:0;padding:0;height:100%;width:100%;overflow:hidden}\
        #g{position:absolute;inset:0;background:\
        repeating-linear-gradient(to right,#345 0 1px,transparent 1px 100px),\
        repeating-linear-gradient(to bottom,#345 0 1px,transparent 1px 100px)}</style>\
        <div id=g></div><script>\
        window.__clicks=[];\
        addEventListener('mousedown',function(e){\
        window.__clicks.push({clientX:e.clientX,clientY:e.clientY,\
        offsetX:e.offsetX,offsetY:e.offsetY,target:e.target&&e.target.id});},true);\
        window.__env=function(){var r=document.getElementById('g').getBoundingClientRect();\
        var v=window.visualViewport;return {innerWidth:innerWidth,innerHeight:innerHeight,\
        dpr:devicePixelRatio,vw:v?v.width:null,vh:v?v.height:null,vscale:v?v.scale:null,\
        grid:{x:r.x,y:r.y,width:r.width,height:r.height}};};\
        </script>";
    let encoded: String = page
        .chars()
        .map(|character| match character {
            'a'..='z'
            | 'A'..='Z'
            | '0'..='9'
            | '-'
            | '_'
            | '.'
            | '!'
            | '~'
            | '*'
            | '\''
            | '('
            | ')'
            | ';'
            | ':'
            | '@'
            | '&'
            | '='
            | '+'
            | '$'
            | ','
            | '/'
            | '?' => character.to_string(),
            other => format!("%{:02X}", u32::from(other)),
        })
        .collect();
    format!("data:text/html;charset=utf-8,{encoded}")
}

/// Whether one reported number is `want`, to the pixel the fractional
/// raster's own arithmetic can move it.
fn near(value: &Value, name: &str, want: u32) -> bool {
    value
        .get(name)
        .and_then(Value::as_f64)
        .is_some_and(|each| (each - f64::from(want)).abs() <= 1.0)
}

/// One click's received coordinate, to the pixel the fractional raster can
/// move it. **A scale or an offset would miss by tens**, so this catches the
/// defect while allowing the arithmetic its own rounding.
fn assert_click(seen: &Value, x: i32, y: i32) {
    for (name, want) in [("clientX", x), ("clientY", y), ("offsetX", x), ("offsetY", y)] {
        let got = seen.get(name).and_then(Value::as_f64).expect("a coordinate");
        assert!(
            (got - f64::from(want)).abs() <= 1.0,
            "{name} should be {want} but is {got}: {seen}"
        );
    }
}

struct Fixture {
    /// **Before `_scratch`**: fields drop in declaration order, and the host's
    /// own drop reads its log out of the scratch dir.
    host: support::Host,
    _scratch: tempfile::TempDir,
    cdp: Cdp,
    _events: tokio::sync::mpsc::Receiver<Value>,
    ring: String,
    ring_handle: Option<Ring>,
    /// The frames the host encoded, off its own socket.
    frames: std::sync::mpsc::Receiver<Frame>,
    frame_reader: Option<channel::Reader>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.ring_handle.take();
        Ring::unlink(&self.ring);
        if let Some(reader) = self.frame_reader.take() {
            reader.stop();
        }
    }
}

async fn fixture(name: &str) -> Fixture {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = support::host_executable(scratch.path());
    let profile = scratch.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    let frames_path = channel::socket_path(&profile);
    let (frames_tx, frames) = std::sync::mpsc::channel();
    let frame_reader = channel::serve(&frames_path, move |message| {
        if let channel::ChannelMessage::Frame(frame) = message {
            let _ = frames_tx.send(frame);
        }
    })
    .expect("the frame socket is served");
    let port = support::free_port();
    let ring = format!("/forge-click-{}-{name}", std::process::id());
    let log = scratch.path().join("host.log");
    let args = ["--frames".to_owned(), frames_path.display().to_string()];
    let host = support::Host::start(&exe, &profile, port, &ring, "1440x900", &args, &log);
    let id = support::wait_for_page(port, &log, BOUND);
    let (cdp, events) = Cdp::connect_watch(port, &format!("/devtools/page/{id}"))
        .await
        .expect("the page's devtools endpoint accepts");
    let mut fixture = Fixture {
        _scratch: scratch,
        host,
        cdp,
        _events: events,
        ring,
        ring_handle: None,
        frames,
        frame_reader: Some(frame_reader),
    };
    fixture.navigate(&grid_page()).await;
    fixture
}

impl Fixture {
    /// Open the ring the host created, once it exists.
    fn ring(&mut self) -> &Ring {
        if self.ring_handle.is_none() {
            let deadline = Instant::now() + BOUND;
            loop {
                match Ring::open(&self.ring) {
                    Ok(ring) => {
                        self.ring_handle = Some(ring);
                        break;
                    }
                    Err(why) => {
                        assert!(
                            Instant::now() < deadline,
                            "the host never created its ring {}: {why}",
                            self.ring
                        );
                        std::thread::sleep(POLL_EVERY);
                    }
                }
            }
        }
        self.ring_handle.as_ref().expect("the ring was just opened")
    }

    /// Ask the host to lay the page out at `width x height` device pixels
    /// drawn at `scale`, exactly as a controller's resize does.
    fn request(&mut self, width: u32, height: u32, scale: f64) {
        self.ring().request(width, height, scale);
    }

    /// The frame size the ring's header names: the last delivered paint, or
    /// the request the host adopted before one arrives.
    fn painted(&mut self) -> (u32, u32) {
        let ring = self.ring();
        // SAFETY: the header is mapped for this process's whole life.
        let header = unsafe { &*ring.header() };
        (
            header.width.load(std::sync::atomic::Ordering::Relaxed),
            header.height.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// One tab's own resize, written the way a take's restore writes it.
    fn resize_tab(&mut self, width: u32, height: u32, scale_milli: u32) {
        self.host.input(&host_line(&HostInput::TabResize { tab: 1, width, height, scale_milli }));
    }

    async fn evaluate(&mut self, expression: &str) -> Value {
        let reply = self
            .cdp
            .call(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true }),
                None,
            )
            .await
            .expect("the page evaluates");
        reply.pointer("/result/value").cloned().unwrap_or(Value::Null)
    }

    async fn navigate(&mut self, url: &str) {
        self.cdp
            .call("Page.navigate", json!({ "url": url }), None)
            .await
            .expect("the page navigates");
    }

    /// Ask the host to lay the surface out at `width x height` drawn at
    /// `scale`, and wait for the page to report it.
    ///
    /// **One ask is the whole request, and the wait is the page's, not the
    /// host's.** The host answers CEF's `GetViewRect` inside CEF's own
    /// `WasResized`, so the CSS size is settled as soon as the host has taken
    /// the request off the ring; what remains is the renderer adopting it,
    /// which is a duration and not a count.
    ///
    /// **CEF's windowless view holds one resize in flight.** `hold_resize_`
    /// stays set from the resize that changed its root layer until a paint
    /// arrives at exactly the requested size and scale. A later `WasResized`
    /// is not dropped - it sets `pending_resize_` and `ReleaseResizeHold`
    /// re-posts it - but it is deferred behind a paint a starved host may
    /// never deliver, so no number of re-asks shortens the hang. Each surface
    /// below therefore gets its own host, so every resize asked for is that
    /// host's first and applies before any hold exists.
    async fn adopt(&mut self, width: u32, height: u32, scale: f64, want: (u32, u32)) -> Value {
        self.request(width, height, scale);
        let deadline = Instant::now() + BOUND;
        loop {
            let env = self.evaluate("window.__env ? window.__env() : null").await;
            if near(&env, "innerWidth", want.0) && near(&env, "innerHeight", want.1) {
                println!("the page reports {env}");
                return env;
            }
            assert!(
                Instant::now() < deadline,
                "the page never laid out at {want:?} (log: {})",
                self.host.log()
            );
            tokio::time::sleep(POLL_EVERY).await;
        }
    }

    /// One full click at the point the SERVER names, written the way the
    /// server writes it.
    fn click(&mut self, x: i32, y: i32) {
        for action in [MouseAction::Press, MouseAction::Release] {
            self.host.input(&host_line(&HostInput::Mouse {
                tab: 1,
                action,
                x,
                y,
                button: 0,
                clicks: 1,
                modifiers: InputModifiers::default(),
            }));
        }
    }

    /// Wait until the page has recorded `count` clicks and answer the last.
    async fn click_received(&mut self, count: usize) -> Value {
        let deadline = Instant::now() + BOUND;
        loop {
            let clicks = self.evaluate("window.__clicks").await;
            if let Some(last) =
                clicks.as_array().filter(|all| all.len() >= count).and_then(|all| all.last())
            {
                return last.clone();
            }
            assert!(Instant::now() < deadline, "the page never recorded click {count}");
            tokio::time::sleep(POLL_EVERY).await;
        }
    }

    /// A click at the server's own point, and the page's record of it.
    async fn click_and_read(&mut self, x: i32, y: i32, count: usize) -> Value {
        self.click(x, y);
        let seen = self.click_received(count).await;
        println!("click {x},{y} -> {seen}");
        seen
    }

    /// Let the host encode: a host nobody watches paints but writes nothing.
    fn publish(&mut self) {
        let ring = self.ring();
        ring.pause(false);
        ring.key();
    }

    /// The host's next encoded frame at `width`x`height`, off its socket.
    fn next_frame(&mut self, width: u16, height: u16) -> Frame {
        let deadline = Instant::now() + BOUND;
        loop {
            match self.frames.try_recv() {
                Ok(frame) if (frame.width, frame.height) == (width, height) => return frame,
                Ok(_) => {}
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "the host never painted {width}x{height} (log: {})",
                        self.host.log()
                    );
                    std::thread::sleep(POLL_EVERY);
                }
            }
        }
    }
}

/// **With no viewport override, the frame budget never moves the page's
/// layout.** A desktop takeover measures 2048x1112 CSS pixels at 2x, asking
/// for 4096x2224 device pixels - past the ring's 3840x2160 budget. Clamping
/// THAT halves the page's layout to 1920x1080, and the surface then draws a
/// narrower page scaled up and centred, so a click sent at the overlay's x
/// lands right of the pixel under the pointer by 55 px at x 700 and 78 px at
/// x 1500 (measured before the fix). The phone's portrait surface, 390x780 at
/// 3x, failed the same way in y. The layout is the surface's to name; only the
/// raster may give.
///
/// **One host per surface.** CEF's windowless view applies a resize and then
/// holds the next one until a paint arrives at the size it asked for - a
/// paint a starved runner may never send (see [`Fixture::adopt`]). Stacking
/// the three sizes on one host made the later ones depend on that paint
/// rather than on the budget.
#[tokio::test(flavor = "multi_thread")]
async fn a_surface_past_the_frame_budget_keeps_its_layout_size() {
    // The control: a surface inside the budget lays out exactly as asked,
    // and the pointer reaches the pixel it names. This proves the rig itself
    // can report a correct mapping, so the case below is the budget's doing.
    let mut inside = fixture("inside").await;
    let env = inside.adopt(1920, 1080, 2.0, (960, 540)).await;
    println!("the in-budget page reports {env}");
    for (count, (x, y)) in [(1_usize, (100_i32, 100_i32)), (2, (900, 500))] {
        let seen = inside.click_and_read(x, y, count).await;
        assert_click(&seen, x, y);
    }
    // **A fixture carries a CEF host and its staged bundle**, and two hosts at
    // once starve the runner (`.config/nextest.toml`'s `cef-live` group exists
    // for exactly that). Each one goes before the next is staged, so the peak
    // is one host and one bundle.
    drop(inside);

    // The takeover's own shape: 2048x1112 CSS at 2x.
    let mut at = fixture("budget").await;
    let env = at.adopt(4096, 2224, 2.0, (2048, 1112)).await;
    println!("the takeover's page reports {env}");
    let (width, height) = at.painted();
    println!("the host paints {width}x{height}");
    assert!(
        u64::from(width) * u64::from(height) <= u64::from(MAX_WIDTH) * u64::from(MAX_HEIGHT),
        "the frame stays inside the ring's budget, {width}x{height}"
    );
    // With no viewport override the frame is the layout at the raster the
    // budget allowed, and the client scales that back to the surface; a frame
    // whose ratio disagrees with the surface is the letterbox that moved every
    // click.
    let dpr = env.get("dpr").and_then(Value::as_f64).expect("a dpr");
    assert!(
        (f64::from(width) - 2048.0 * dpr).abs() <= 2.0
            && (f64::from(height) - 1112.0 * dpr).abs() <= 2.0,
        "with no viewport override the frame is the layout at the painted raster: {width}x{height} \
         against 2048x1112 at {dpr}"
    );

    for (count, (x, y)) in [(1_usize, (700_i32, 400_i32)), (2, (1500, 900))] {
        let seen = at.click_and_read(x, y, count).await;
        assert_click(&seen, x, y);
    }

    // **A take's restore names the tab's size too, on the same raw request.**
    // The view it leaves must stay inside the budget, or every later paint is
    // dropped and the surface freezes on its last picture after a recording.
    // The view size that path produces is pinned in `osr`'s unit test; this
    // only proves the wire path takes the line and the page keeps its size.
    at.resize_tab(4096, 2224, 2_000);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (width, height) = at.painted();
    println!("after the take's restore the host holds {width}x{height}");
    assert!(
        u64::from(width) * u64::from(height) <= u64::from(MAX_WIDTH) * u64::from(MAX_HEIGHT),
        "a restored surface stays inside the ring's budget, {width}x{height}"
    );
    let env = at.evaluate("window.__env()").await;
    assert!(
        near(&env, "innerWidth", 2048) && near(&env, "innerHeight", 1112),
        "and keeps the page at the surface's size, {env}"
    );
    drop(at);

    // **The portrait phone's surface, on a host of its own.** 390x780 CSS at 3x
    // asks for 1170x2340, past the old 2160-high clamp, which laid the page
    // out 390x720 under a 390x780 canvas: a tap at y 700 mapped 8.3 percent
    // low and the bottom 60 CSS pixels were unreachable. The canvas maps
    // through the page area it measured, so it needs the page to be that
    // size.
    let mut phone = fixture("phone").await;
    let env = phone.adopt(1170, 2340, 3.0, (390, 780)).await;
    println!("the phone's page reports {env}");
    let (width, height) = phone.painted();
    println!("the host paints {width}x{height}");
    assert!(
        u64::from(width) * u64::from(height) <= u64::from(MAX_WIDTH) * u64::from(MAX_HEIGHT),
        "the phone's frame stays inside the ring's budget, {width}x{height}"
    );
    let seen = phone.click_and_read(195, 700, 1).await;
    assert_click(&seen, 195, 700);
}

/// **The frame names the page its pixels were painted from, and a point
/// measured in it is received there.** The page is laid out 1280x720 at 2x -
/// a 2560x1440 frame - so a 1200x800 surface draws the picture 1200x675 at y
/// 62.5: a press on the picture at (900, 500) is the page's (960, 466.7), and
/// the page's own event log names that point when a click is sent at it.
///
/// **This is the host's wire and the page's log, not the client's mapping.**
/// Both clicks travel the same server path, so what it pins is the viewport
/// the header carries and that a point measured through it arrives. The
/// client's own rule is pinned in the shell's `surface.rs` and in the
/// canvas test.
#[tokio::test(flavor = "multi_thread")]
async fn a_letterboxed_frame_names_the_page_a_click_is_sent_in() {
    let mut at = fixture("letterbox").await;
    at.publish();
    let env = at.adopt(2560, 1440, 2.0, (1280, 720)).await;
    let frame = at.next_frame(2560, 1440);
    let page = (
        env.get("innerWidth").and_then(Value::as_f64).expect("a width"),
        env.get("innerHeight").and_then(Value::as_f64).expect("a height"),
    );
    assert_eq!(
        (u32::from(frame.page_width), u32::from(frame.page_height)),
        (1280, 720),
        "the frame names the page's own viewport, not its pixels"
    );

    // The surface's box, and the rect the picture is drawn in: an
    // aspect-preserving fit, as the desktop's layer and the phone's canvas
    // both draw it.
    let bounds = (1200.0_f64, 800.0_f64);
    let fit = (bounds.0 / f64::from(frame.width)).min(bounds.1 / f64::from(frame.height));
    let drawn = (f64::from(frame.width) * fit, f64::from(frame.height) * fit);
    let origin = ((bounds.0 - drawn.0) / 2.0, (bounds.1 - drawn.1) / 2.0);
    println!("the picture draws {drawn:?} at {origin:?} in the {bounds:?} box");

    // A press on the picture at (900, 500) of the box is the page's (960, 466.7).
    let pressed = (900.0_f64, 500.0_f64);
    let shown =
        ((pressed.0 - origin.0) / drawn.0 * page.0, (pressed.1 - origin.1) / drawn.1 * page.1);
    assert!(
        (shown.0 - pressed.0).abs() > 1.0 || (shown.1 - pressed.1).abs() > 1.0,
        "the control: the picture's page point and the box's own point must differ, {shown:?}"
    );

    // The box's own point is what a 1:1 mapping sends; the page receives it,
    // and it is the control that the two points really differ.
    let seen = at.click_and_read(900, 500, 1).await;
    assert_click(&seen, 900, 500);

    // And the point the picture shows is received where the picture shows it.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a whole-pixel page coordinate, far inside i32"
    )]
    let mapped = (shown.0.round() as i32, shown.1.round() as i32);
    let seen = at.click_and_read(mapped.0, mapped.1, 2).await;
    assert_click(&seen, mapped.0, mapped.1);
    println!(
        "the box's own point is ({:.1}, {:.1}) page px from the pixel the picture shows",
        pressed.0 - shown.0,
        pressed.1 - shown.1
    );
}

//! The desktop's own microphone.
//!
//! The page does not capture here. This half opens the device through
//! `forge-capture`, frames what it delivers into the 20 ms mono i16 frames
//! the wire carries, and hands them to the page, which streams them down the
//! take's own socket - so the server and the take feed see exactly what the
//! browser's capture sends, and a native open skips the browser's audio
//! pipeline and its permission round trip on the way.
//!
//! **The device is held, not opened per take.** It is opened when the page
//! names an input to hold - at client start, and again whenever the pick
//! changes - and it stays open through every take, so a press records from
//! the press: no device open under it, and none of the device's own start-up
//! silence inside its first second. The cost is the microphone indicator
//! staying lit while the client runs, which is the trade this half makes.
//!
//! While no take is recording, the held stream's frames are dropped - the
//! audio belongs to no take, and only a take's frames go to the page.
//!
//! A hold or a press from a page other than the one recording ends that
//! recording instead of failing against it: a reload discards the page that
//! held the stop, so the page that comes back is what ends the orphaned take.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::bridge::Bridge;

/// One frame's payload: 20 ms of mono 16 kHz samples, the cadence the
/// server's meter polls at.
pub const FRAME_SAMPLES: usize = 320;

/// How long the capture stays open past the release.
///
/// The device's own pipeline holds audio that is already captured and not
/// yet handed over; it only arrives while the stream is still running. The
/// window is the browser leg's measured 42-53 ms with room for this one,
/// which was not measured separately.
pub const DRAIN_MS: u64 = 150;

/// The open capture's stream side, shared with the callback that fills it.
///
/// Held across takes: the callback is fixed when the device is opened, so
/// what a take changes is this flag, not the stream. The lock is what makes
/// a take's first frame and its last one exact - the callback reads the flag
/// and the buffer under the same lock the take flips them under, so no
/// pre-press sample can land in a take and none of a take's tail can be
/// dropped for a callback that ran late.
struct Carry {
    bridge: Arc<Bridge>,
    /// `true` while a take is recording on this capture; `false` while it is
    /// held between takes, when its frames are dropped rather than sent.
    recording: AtomicBool,
    /// Samples not yet a whole frame, as the callback assembles them.
    held: Mutex<Vec<f32>>,
}

impl Carry {
    fn new(bridge: &Arc<Bridge>, recording: bool) -> Self {
        Self {
            bridge: Arc::clone(bridge),
            recording: AtomicBool::new(recording),
            held: Mutex::new(Vec::with_capacity(FRAME_SAMPLES)),
        }
    }

    /// One block from the device: dropped while no take records, framed and
    /// handed on while one does.
    fn push(&self, samples: &[f32]) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.recording.load(Ordering::Relaxed) {
            return;
        }
        held.extend_from_slice(samples);
        while held.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = held.drain(..FRAME_SAMPLES).collect();
            self.bridge.mic_frame(encode_frame(&frame));
        }
    }

    /// Begin a take: the partial frame older than this instant is not the
    /// take's, so it goes rather than leading the take.
    fn begin(&self) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.clear();
        self.recording.store(true, Ordering::Relaxed);
    }

    /// End a take, answering the partial frame it was holding so the caller
    /// can send it. The flag flips under the same lock the callback reads it
    /// under, so nothing lands after this returns.
    fn end(&self) -> Vec<f32> {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        self.recording.store(false, Ordering::Relaxed);
        std::mem::take(&mut *held)
    }

    /// Whether a take is recording on this capture right now.
    fn recording(&self) -> bool {
        self.recording.load(Ordering::Relaxed)
    }
}

/// One open capture, as the hold keeps it: the device it records from, the
/// stream side its frames feed, and how it is let go.
///
/// A trait so the hold's lifecycle - what is opened, what is reused, what is
/// released - can be tested with no audio hardware; the shell has one real
/// implementation, [`Opened`]. `Send` because the hold is Tauri-managed state
/// a command may reach from any thread.
trait Held: Send {
    fn device(&self) -> &forge_capture::Device;
    fn carry(&self) -> &Arc<Carry>;
    fn release(self: Box<Self>);
}

/// A live `forge-capture` stream.
struct Opened {
    capture: forge_capture::Capture,
    carry: Arc<Carry>,
}

impl Held for Opened {
    fn device(&self) -> &forge_capture::Device {
        self.capture.device()
    }

    fn carry(&self) -> &Arc<Carry> {
        &self.carry
    }

    fn release(self: Box<Self>) {
        self.capture.stop();
    }
}

/// Open one capture over `device`, feeding `carry`.
///
/// A boxed closure rather than a plain fn so a test can stand a device in:
/// the lifecycle below is what a hardware-free test needs to reach.
type Opener = Box<dyn Fn(Option<&str>, Arc<Carry>) -> Result<Box<dyn Held>, String> + Send + Sync>;

/// The open capture, and who it is for.
struct Warm {
    /// What the page asked for when it was opened, `None` for the system
    /// default; a take asking for the same one is served by this capture.
    requested: Option<String>,
    held: Box<dyn Held>,
    /// The page that began the recording on this capture, `None` while none
    /// does.
    ///
    /// A page's own id, minted per load: a hold or a press naming a different
    /// one is a page other than the one recording, and since the client is
    /// one page, that page is gone.
    owner: Option<String>,
}

impl Warm {
    fn recording(&self) -> bool {
        self.held.carry().recording()
    }

    /// Whether `page` is the one recording on this capture.
    fn owned_by(&self, page: &str) -> bool {
        self.owner.as_deref() == Some(page)
    }

    /// End a recording `page` does not own, if one is running.
    ///
    /// A hold or a press naming a page other than the one recording means
    /// that page is gone (the client is one page) and its stop went with it,
    /// so the recording can never be closed by the path that closes a live
    /// one - it is ended here, its audio thrown away, and the device stays
    /// held.
    fn claim(&mut self, page: &str) {
        if self.recording() && !self.owned_by(page) {
            self.end();
        }
    }

    /// Begin a take here, for `page`.
    fn begin(&mut self, page: &str) {
        self.held.carry().begin();
        self.owner = Some(page.to_owned());
    }

    /// End the recording here, handing over the partial frame it held, and
    /// forget the page that began it: the next take names its own.
    fn end(&mut self) -> Vec<f32> {
        self.owner = None;
        self.held.carry().end()
    }
}

/// The microphone, as this half holds it: at most one capture, open from the
/// page's first hold to its release, serving every take between.
pub struct Mic {
    warm: Mutex<Option<Warm>>,
    /// How a capture is opened; the shell's own value is `forge-capture`.
    opener: Opener,
}

impl Default for Mic {
    fn default() -> Self {
        Self {
            warm: Mutex::new(None),
            opener: Box::new(|device, carry| {
                let seen = Arc::clone(&carry);
                let capture = forge_capture::open(device, move |block| seen.push(block.samples))
                    .map_err(|error| error.to_string())?;
                Ok(Box::new(Opened { capture, carry }))
            }),
        }
    }
}

impl Mic {
    /// Open the picked device - or the system default when nothing is
    /// picked - and keep it open until [`Mic::release`].
    ///
    /// Idempotent for the pick it is already holding, and a no-op for the
    /// page recording on it: the pick applies from the next take, so a change
    /// mid take never tears the stream out from under the recording.
    ///
    /// **A hold is also a claim on the device**, which is how the page that
    /// comes back from a reload catches the take its predecessor left
    /// recording: no live page owns that recording, so it is ended rather
    /// than kept (see `Warm::claim`), and this page's press is warm and free.
    ///
    /// A pick that will not open is an error rather than a fallback: someone
    /// who chose an interface needs to know it is not there, not to be
    /// quietly recorded on something else. The caller carries on without a
    /// hold - a press then opens the device itself, exactly as it did before
    /// there was one.
    pub fn hold(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<String>,
        page: &str,
    ) -> Result<forge_capture::Device, String> {
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = warm.as_mut() {
            held.claim(page);
            if held.recording() || held.requested == device {
                return Ok(held.held.device().clone());
            }
        }
        // Opened before the old one is let go, so a pick that will not open
        // leaves the hold that was working standing.
        let next = self.open(bridge, device.as_deref(), None)?;
        let resolved = next.held.device().clone();
        if let Some(stale) = warm.replace(next) {
            stale.held.release();
        }
        Ok(resolved)
    }

    /// Begin a take: the held capture when it is the one asked for, a fresh
    /// open otherwise.
    ///
    /// The device is opened before this returns, so a device that will not
    /// open is an error here rather than a take that records silence. The
    /// input it opened comes back with it, which is the only place a plain
    /// take names the system default: the picker's list carries no mark.
    ///
    /// **The take consumes nothing**: the held capture serves it and stays
    /// held, so the next take is warm too. A capture opened for this take
    /// alone becomes the hold, so the pick self-corrects after a press the
    /// hold could not serve.
    ///
    /// A take this same page is already recording is refused, which is the
    /// busy guard. A press claims the device the way a hold does, so a
    /// recording left by any other page is ended first (see `Warm::claim`):
    /// the first press after a reload records, rather than being denied by
    /// the take the reload discarded.
    pub fn start(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<String>,
        page: &str,
    ) -> Result<forge_capture::Device, String> {
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = warm.as_mut() {
            held.claim(page);
            if held.recording() {
                return Err("the microphone is already recording".to_owned());
            }
            if held.requested == device {
                held.begin(page);
                return Ok(held.held.device().clone());
            }
        }
        let next = self.open(bridge, device.as_deref(), Some(page))?;
        let resolved = next.held.device().clone();
        if let Some(stale) = warm.replace(next) {
            stale.held.release();
        }
        Ok(resolved)
    }

    /// End the take at once, throwing its audio away. The device stays
    /// open, so the next press records from the press.
    pub fn stop(&self) {
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = warm.as_mut() {
            held.end();
        }
    }

    /// Hold the capture open for the drain window, hand over the last
    /// partial frame, then end the take and keep the device warm.
    ///
    /// The caller sends its stop only after this returns: the stop is what
    /// finalises the take on the server, so a frame that follows it has no
    /// take to land in.
    pub async fn drain(&self) {
        tokio::time::sleep(Duration::from_millis(DRAIN_MS)).await;
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(held) = warm.as_mut() else {
            return;
        };
        let tail = held.end();
        if !tail.is_empty() {
            held.held.carry().bridge.mic_frame(encode_frame(&tail));
        }
    }

    /// Let the device go: the client is exiting, and the microphone's
    /// indicator goes out with it.
    pub fn release(&self) {
        let stale = self.warm.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(held) = stale {
            held.held.release();
        }
    }

    /// Open one capture; a take's while `owner` names the page recording on
    /// it, a hold's while it is `None`.
    fn open(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<&str>,
        owner: Option<&str>,
    ) -> Result<Warm, String> {
        let carry = Arc::new(Carry::new(bridge, owner.is_some()));
        let held = (self.opener)(device, Arc::clone(&carry))?;
        Ok(Warm {
            requested: device.map(str::to_owned),
            held,
            owner: owner.map(str::to_owned),
        })
    }
}

/// One frame as the wire carries it: the codec tag, then little-endian i16
/// at 2^15, clamped first.
///
/// The same bytes `client/src/composer/capture.svelte.ts` sends from the
/// browser's microphone, and the server divides by the same 2^15; the two
/// encoders are pinned to one fixture on both sides.
pub fn encode_frame(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(1 + samples.len() * 2);
    bytes.push(0); // codec: pcm i16
    for sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let value = (clamped * 32768.0).round().clamp(-32768.0, 32767.0);
        let value = value as i16;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// One device as the page reads it: the panel's own `{ id, label }` shape,
/// for both the take's `resolved` (`ShellMic.open`) and the picker's rows.
pub fn device_json(device: &forge_capture::Device) -> serde_json::Value {
    serde_json::json!({ "id": device.id, "label": device.name })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keys the page reads the default's name back by
    /// (`dictation.ts`'s `rememberDefaultDevice`): `ShellMic.open` hands this
    /// straight to the take, so a renamed key is a panel that names nothing,
    /// and no page test sees it.
    #[test]
    fn the_device_crosses_as_the_panels_id_and_label() {
        let opened = forge_capture::Device {
            id: "coreaudio:builtin".to_owned(),
            name: "MacBook Pro Microphone".to_owned(),
            is_default: true,
        };
        assert_eq!(
            device_json(&opened),
            serde_json::json!({ "id": "coreaudio:builtin", "label": "MacBook Pro Microphone" }),
            "the page reads `id` and `label`; any other key is a default it cannot name"
        );
    }

    /// The vector `capture.test.ts` pins the page's encoder to: the two
    /// halves of one wire contract, so a change on either side fails here.
    #[test]
    fn the_frame_is_the_codec_tag_then_little_endian_i16() {
        assert_eq!(
            encode_frame(&[0.0, 0.5, -1.0, 0.25]),
            vec![0, 0x00, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0x20],
            "the tag, then 0, 16384, -32768 and 8192, low byte first"
        );
    }

    #[test]
    fn what_the_microphone_cannot_have_meant_is_clamped() {
        // Full scale lands on the largest positive sample, not 32768, which
        // does not fit; an overshoot reads as full scale rather than
        // wrapping its sign.
        assert_eq!(&encode_frame(&[1.0])[1..], &[0xff, 0x7f]);
        assert_eq!(&encode_frame(&[2.0])[1..], &[0xff, 0x7f]);
        assert_eq!(&encode_frame(&[-2.0])[1..], &[0x00, 0x80]);
    }

    mod the_hold {
        use super::*;

        /// A stand-in capture: it records that it was opened, what for, and
        /// whether it was released - which is the whole of the hold's
        /// lifecycle, with no audio hardware in the test run.
        struct Fake {
            device: forge_capture::Device,
            carry: Arc<Carry>,
            log: Arc<Mutex<Vec<String>>>,
        }

        impl Held for Fake {
            fn device(&self) -> &forge_capture::Device {
                &self.device
            }

            fn carry(&self) -> &Arc<Carry> {
                &self.carry
            }

            fn release(self: Box<Self>) {
                self.log.lock().unwrap().push(format!("release {}", self.device.id));
            }
        }

        /// The shell half over stand-in devices: the log is the whole of
        /// what the hold did - what it opened and what it let go.
        struct Rig {
            mic: Mic,
            log: Arc<Mutex<Vec<String>>>,
            bridge: Arc<Bridge>,
        }

        impl Rig {
            fn new() -> Self {
                let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let opened = Arc::clone(&log);
                let mic = Mic {
                    warm: Mutex::new(None),
                    opener: Box::new(move |device, carry| {
                        let name = device.unwrap_or("default").to_owned();
                        opened.lock().unwrap().push(format!("open {name}"));
                        Ok(Box::new(Fake {
                            device: forge_capture::Device {
                                id: name,
                                name: "stand-in".to_owned(),
                                is_default: device.is_none(),
                            },
                            carry,
                            log: Arc::clone(&opened),
                        }))
                    }),
                };
                let bridge = Bridge::new(None, tokio::sync::mpsc::unbounded_channel().0);
                Self { mic, log, bridge }
            }

            fn calls(&self) -> Vec<String> {
                self.log.lock().unwrap().clone()
            }
        }

        #[test]
        fn a_hold_opens_the_device_and_keeps_it() {
            let rig = Rig::new();
            let opened = rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens");
            assert_eq!(opened.id, "one");
            assert_eq!(rig.calls(), vec!["open one"], "the hold is the open, and nothing else");

            // A second hold for the same pick is the same capture, not a
            // second open: the page re-states the pick it is holding.
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("idempotent");
            assert_eq!(rig.calls(), vec!["open one"], "the same pick does not reopen");
        }

        #[test]
        fn a_take_consumes_the_held_capture_without_reopening() {
            let rig = Rig::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens");
            let started =
                rig.mic.start(&rig.bridge, Some("one".to_owned()), "page").expect("starts");
            assert_eq!(started.id, "one");
            assert_eq!(rig.calls(), vec!["open one"], "the press opens nothing");

            // And ending the take does not close it either: the same device
            // serves the next take.
            rig.mic.stop();
            assert_eq!(rig.calls(), vec!["open one"], "the take's end keeps the device open");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page")
                .expect("the next take is warm");
            assert_eq!(rig.calls(), vec!["open one"], "the second press opens nothing either");
        }

        #[test]
        fn the_pick_changing_reacquires_the_device() {
            let rig = Rig::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens");
            rig.mic.hold(&rig.bridge, Some("two".to_owned()), "page").expect("reopens");
            assert_eq!(
                rig.calls(),
                vec!["open one", "open two", "release one"],
                "the new pick opens before the old one is let go"
            );
            // And the new pick is the one a take gets.
            let held = rig.mic.start(&rig.bridge, Some("two".to_owned()), "page")
                .expect("the take starts");
            assert_eq!(held.id, "two", "the re-acquired device is the one recorded from");
        }

        #[test]
        fn a_press_for_another_device_takes_over_the_hold() {
            let rig = Rig::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens");
            rig.mic.start(&rig.bridge, None, "page").expect("a plain take opens the default");
            assert_eq!(
                rig.calls(),
                vec!["open one", "open default", "release one"],
                "the press the hold cannot serve opens its own, and keeps it"
            );
            // The hold now follows the pressed device: the pick moves with
            // the take rather than fighting it.
            rig.mic.stop();
            rig.mic.start(&rig.bridge, None, "page").expect("the next plain take is warm");
            assert_eq!(rig.calls().len(), 3, "the second press reuses the take's own capture");
        }

        #[test]
        fn release_lets_the_device_go() {
            let rig = Rig::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens");
            rig.mic.release();
            assert_eq!(rig.calls(), vec!["open one", "release one"]);
            rig.mic.release();
            assert_eq!(rig.calls().len(), 2, "a second release has nothing to let go");
        }

        #[test]
        fn a_hold_while_recording_waits_for_the_next_take() {
            let rig = Rig::new();
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page").expect("starts cold");
            // The page recording states a new pick as it changes: its own
            // hold is not a claim from another page, so the take keeps the
            // stream it is recording on.
            let answered = rig
                .mic
                .hold(&rig.bridge, Some("two".to_owned()), "page")
                .expect("held for the next take");
            assert_eq!(
                rig.calls(),
                vec!["open one"],
                "the recording stream is not torn out from under the take"
            );
            assert_eq!(answered.id, "one", "and the take's own input is what a hold answers");
        }

        #[test]
        fn a_second_take_on_a_live_capture_is_refused() {
            let rig = Rig::new();
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page").expect("starts");
            let refused = rig.mic.start(&rig.bridge, Some("one".to_owned()), "page");
            assert_eq!(refused.err().as_deref(), Some("the microphone is already recording"));
        }

        /// The state a reload leaves: a take still recording, and the page
        /// that held its stop gone.
        fn reload(rig: &Rig, device: Option<String>) {
            rig.mic.hold(&rig.bridge, device.clone(), "page-a").expect("opens");
            rig.mic.start(&rig.bridge, device, "page-a").expect("starts");
            // Nothing else happens on this side: a reload discards the page
            // without ever ending the take it began.
        }

        #[test]
        fn a_press_after_the_recording_page_reloads_recovers() {
            let rig = Rig::new();
            reload(&rig, Some("one".to_owned()));
            // The page that comes back mounts and states its hold, as every
            // page does on its way in.
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page-b").expect("holds");
            match rig.mic.start(&rig.bridge, Some("one".to_owned()), "page-b") {
                Ok(started) => assert_eq!(started.id, "one"),
                Err(why) => panic!("the press after a reload is refused: {why}"),
            }
            assert_eq!(
                rig.calls(),
                vec!["open one"],
                "only the orphaned recording ends: the device stays held across the reload"
            );
        }

        #[test]
        fn a_press_before_the_new_pages_hold_ends_the_orphaned_take_too() {
            let rig = Rig::new();
            reload(&rig, Some("one".to_owned()));
            // The mount's hold is in flight, so the press is the first call
            // the reloaded page makes.
            match rig.mic.start(&rig.bridge, Some("one".to_owned()), "page-b") {
                Ok(started) => assert_eq!(started.id, "one"),
                Err(why) => panic!("the press before the new page's hold is refused: {why}"),
            }
        }

        #[test]
        fn a_pick_that_will_not_open_leaves_the_working_hold_standing() {
            let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let opened = Arc::clone(&log);
            let mic = Mic {
                warm: Mutex::new(None),
                opener: Box::new(move |device, carry| {
                    let name = device.unwrap_or("default").to_owned();
                    if name == "gone" {
                        return Err("no input device named \"gone\"".to_owned());
                    }
                    opened.lock().unwrap().push(format!("open {name}"));
                    Ok(Box::new(Fake {
                        device: forge_capture::Device {
                            id: name,
                            name: "stand-in".to_owned(),
                            is_default: false,
                        },
                        carry,
                        log: Arc::clone(&opened),
                    }))
                }),
            };
            let bridge = Bridge::new(None, tokio::sync::mpsc::unbounded_channel().0);
            mic.hold(&bridge, Some("one".to_owned()), "page").expect("opens");
            let refused = mic.hold(&bridge, Some("gone".to_owned()), "page");
            assert!(refused.is_err(), "the pick that will not open is reported");
            assert_eq!(
                mic.hold(&bridge, Some("one".to_owned()), "page").expect("still held").id,
                "one",
                "the hold that was working is the one still standing"
            );
        }
    }

    /// The real device, through the hold. What the stand-ins cannot prove is
    /// that `forge-capture`'s own callback reaches the carry, so this opens
    /// the machine's input and reads one frame off it. Ignored by default,
    /// like the capture leaf's own hardware test; run with `--run-ignored
    /// all` on a machine with a microphone.
    #[test]
    #[ignore = "opens the real default input; run with --run-ignored all on a machine with audio hardware"]
    fn a_held_capture_hands_a_take_real_frames() {
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let bridge = Bridge::new(None, events);
        let mic = Mic::default();
        let Ok(held) = mic.hold(&bridge, None, "page") else {
            eprintln!("no input on this machine; the wiring is unproven here");
            return;
        };
        let take = mic.start(&bridge, None, "page").expect("the held capture must serve the take");
        assert_eq!(take.id, held.id, "the take records from the device the hold opened");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut frames = 0;
        while frames == 0 && std::time::Instant::now() < deadline {
            match received.try_recv() {
                Ok(crate::bridge::ClientEvent::Mic(bytes)) => {
                    assert_eq!(
                        bytes.len(),
                        1 + FRAME_SAMPLES * 2,
                        "a take's frames are whole 20 ms frames on the wire's own shape"
                    );
                    frames += 1;
                }
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        assert!(frames > 0, "a take on a held capture must be fed the device's frames");

        // And ending the take keeps the device: the second take is served by
        // the same capture, which is the whole point of holding it.
        mic.stop();
        let again =
            mic.start(&bridge, None, "page").expect("the next take is served without reopening");
        assert_eq!(again.id, held.id, "the same device, still open");
        mic.release();
    }

    mod the_carry {

        use super::*;

        /// The bridge's own channel, so the frames a take sends are read
        /// where the page would read them.
        fn bridge()
        -> (Arc<Bridge>, tokio::sync::mpsc::UnboundedReceiver<crate::bridge::ClientEvent>) {
            let (events, received) = tokio::sync::mpsc::unbounded_channel();
            (Bridge::new(None, events), received)
        }

        fn frame_bytes(event: &crate::bridge::ClientEvent) -> Vec<u8> {
            match event {
                crate::bridge::ClientEvent::Mic(bytes) => bytes.clone(),
                other => panic!("a take's frames cross as Mic events, got {other:?}"),
            }
        }

        #[test]
        fn a_held_capture_drops_its_frames() {
            let (bridge, mut received) = bridge();
            let carry = Carry::new(&bridge, false);
            for _ in 0..10 {
                carry.push(&[0.5; 160]);
            }
            assert!(
                received.try_recv().is_err(),
                "audio that belongs to no take must not reach the page"
            );
        }

        #[test]
        fn a_take_frames_exactly_what_it_records() {
            let (bridge, mut received) = bridge();
            let carry = Carry::new(&bridge, false);
            carry.push(&[0.5; 160]);
            carry.begin();
            // The pre-press half of a frame is not the take's.
            carry.push(&[0.5; 160]);
            assert!(received.try_recv().is_err(), "a partial frame waits for its other half");

            carry.push(&[0.25; FRAME_SAMPLES]);
            let event = received.try_recv().expect("one whole frame crosses");
            let bytes = frame_bytes(&event);
            assert_eq!(bytes.len(), 1 + FRAME_SAMPLES * 2, "the tag, then one sample per word");
            assert_eq!(
                bytes[1..5],
                encode_frame(&[0.5, 0.5])[1..5],
                "the take's first sample is the first one pushed INSIDE it"
            );
        }

        #[test]
        fn the_end_hands_over_the_partial_and_stops_the_flow() {
            let (bridge, mut received) = bridge();
            let carry = Carry::new(&bridge, true);
            carry.push(&[0.5; 100]);
            let tail = carry.end();
            assert_eq!(tail.len(), 100, "the last partial frame is the caller's to send");

            carry.push(&[0.5; FRAME_SAMPLES]);
            assert!(received.try_recv().is_err(), "and nothing crosses after the end");
        }
    }
}

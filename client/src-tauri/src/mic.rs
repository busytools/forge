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
use std::time::{Duration, Instant};

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

/// How long a take's open may run before the shell says the microphone did
/// not come up.
///
/// Long enough for a permission dialog to be answered, short enough that a
/// box waiting on nothing becomes a line.
pub const OPEN_LIMIT: Duration = Duration::from_secs(20);

/// What the take's line says when the open has not answered within the
/// caller's bound, whether the bound answered it or the device did, late.
const NOT_COME_UP: &str = "the microphone did not come up";

/// The caller's give-up signal on a bounded open: the flag it sets when its
/// bound is spent, and the deadline that bound was measured from.
///
/// Both are read because the flag lands a scheduling lag AFTER the deadline:
/// an open that answers inside that lag is one the caller has already given
/// up on, and the flag alone would leave its take recording.
struct GivenUp {
    flag: Arc<AtomicBool>,
    deadline: Instant,
}

impl GivenUp {
    /// Whether the caller has given up: its flag is set, or the deadline it
    /// measured its bound from has passed.
    fn spent(&self) -> bool {
        self.flag.load(Ordering::Acquire) || Instant::now() >= self.deadline
    }
}

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

/// The recording on a capture as the page named it: the page load that began
/// the take, and the token that take minted.
struct Owner {
    page: String,
    take: String,
}

/// The open capture, and who it is for.
struct Warm {
    /// What the page asked for when it was opened, `None` for the system
    /// default; a take asking for the same one is served by this capture.
    requested: Option<String>,
    held: Box<dyn Held>,
    /// The take recording on this capture, `None` while none does.
    ///
    /// The page is what a hold or a press claims against - one naming a
    /// different page is a page other than the one recording, and since the
    /// client is one page, that page is gone. The token is what the take's
    /// own stop and drain match: one page runs take after take, so the page
    /// id cannot tell the take that just ended from the one that followed it.
    owner: Option<Owner>,
}

impl Warm {
    fn recording(&self) -> bool {
        self.held.carry().recording()
    }

    /// Whether `page` is the page whose recording runs on this capture.
    fn owned_by(&self, page: &str) -> bool {
        self.owner.as_ref().is_some_and(|owner| owner.page == page)
    }

    /// Whether `take` is the take recording on this capture.
    fn own_take(&self, take: &str) -> bool {
        self.owner.as_ref().is_some_and(|owner| owner.take == take)
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

    /// Begin a take here: `take`, on `page`.
    fn begin(&mut self, page: &str, take: &str) {
        self.held.carry().begin();
        self.owner = Some(Owner { page: page.to_owned(), take: take.to_owned() });
    }

    /// End the recording here, handing over the partial frame it held, and
    /// forget the take that began it: the next take names its own.
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
    ///
    /// `take` is the token the page minted for this take, and the take's own
    /// identity for the stop and drain that end it - which a page id cannot
    /// be, because one page runs take after take.
    pub fn start(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<String>,
        page: &str,
        take: &str,
    ) -> Result<forge_capture::Device, String> {
        self.start_inner(bridge, device, page, take, None)
    }

    /// [`Mic::start`], with `abandoned` read as the LAST act - once the take is
    /// already the caller's - so a give-up landing anywhere before that read,
    /// the release's own window included, ends the take in place: the capture
    /// is kept as the hold - the device is up, and the next press is warm on
    /// it - and no take nobody owns is left recording.
    fn start_inner(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<String>,
        page: &str,
        take: &str,
        abandoned: Option<&GivenUp>,
    ) -> Result<forge_capture::Device, String> {
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = warm.as_mut() {
            held.claim(page);
            if held.recording() {
                return Err("the microphone is already recording".to_owned());
            }
            if held.requested == device {
                held.begin(page, take);
                // **The read comes after the begin, as the last act**: a warm
                // capture answers at once, so a give-up only lands inside this
                // call when the lock waited - a wedged open holding it - and
                // the take begun above is one the caller gave up on. Ended in
                // place, the capture stays the hold.
                if abandoned.is_some_and(GivenUp::spent) {
                    held.end();
                    return Err(NOT_COME_UP.to_owned());
                }
                return Ok(held.held.device().clone());
            }
        }
        let next = self.open(bridge, device.as_deref(), Some((page, take)))?;
        let resolved = next.held.device().clone();
        if let Some(stale) = warm.replace(next) {
            stale.held.release();
        }
        // **And here the read comes last, after the release**: letting the
        // replaced capture go takes the capture thread's own window (up to
        // its 20 ms loop), and a give-up landing inside it must still leave no
        // take recording. Ended in place, the answered capture stays the hold.
        if abandoned.is_some_and(GivenUp::spent) {
            if let Some(held) = warm.as_mut() {
                held.end();
            }
            return Err(NOT_COME_UP.to_owned());
        }
        Ok(resolved)
    }

    /// [`Mic::start`] under a bound: an open that has not answered within
    /// `limit` is reported as the microphone not coming up rather than waited
    /// on, which is the line the page draws for a device layer that is wedged
    /// - a permission whose prompt never surfaced, a device another process
    /// holds. The open is left to answer on its own thread, and a late
    /// success ends the take nobody owns rather than leaving it recording.
    pub async fn start_within(
        self: &Arc<Self>,
        bridge: &Arc<Bridge>,
        device: Option<String>,
        page: &str,
        take: &str,
        limit: Duration,
    ) -> Result<forge_capture::Device, String> {
        // The deadline is fixed here, before the open is spawned, because the
        // opener compares it: the flag below only lands when this task wakes,
        // and a device answering inside that lag must read as given up too.
        let signal =
            GivenUp { flag: Arc::new(AtomicBool::new(false)), deadline: Instant::now() + limit };
        let flag = Arc::clone(&signal.flag);
        let mic = Arc::clone(self);
        let bridge = Arc::clone(bridge);
        let (page, take) = (page.to_owned(), take.to_owned());
        let started = tokio::task::spawn_blocking(move || {
            mic.start_inner(&bridge, device, &page, &take, Some(&signal))
        });
        match tokio::time::timeout(limit, started).await {
            Ok(joined) => joined.map_err(|error| error.to_string())?,
            Err(_) => {
                flag.store(true, Ordering::Release);
                Err(NOT_COME_UP.to_owned())
            }
        }
    }

    /// End the take at once, throwing its audio away. The device stays
    /// open, so the next press records from the press.
    ///
    /// **One naming a take other than the recording does nothing** (#2252,
    /// #2279): a stop in flight lands after the take it names has ended -
    /// the page's own re-press may already have begun the next, and a
    /// reload's discarded page leaves its take for another page to end -
    /// and the take that followed is not this one's to throw away.
    pub fn stop(&self, take: &str) {
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(held) = warm.as_mut() else {
            return;
        };
        if held.own_take(take) {
            held.end();
        }
    }

    /// Hold the capture open for the drain window, hand over the last
    /// partial frame, then end the take and keep the device warm.
    ///
    /// The caller sends its stop only after this returns: the stop is what
    /// finalises the take on the server, so a frame that follows it has no
    /// take to land in.
    ///
    /// **One naming a take other than the recording drops its tail without
    /// ending anything** (#2252, #2279): the window it sleeps is wide enough
    /// for that take to end - by a drop, or the server's own `dictate_ended` -
    /// and for the page to begin its next take, and the take that followed
    /// is not this one's to throw away.
    pub async fn drain(&self, take: &str) {
        tokio::time::sleep(Duration::from_millis(DRAIN_MS)).await;
        let mut warm = self.warm.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(held) = warm.as_mut() else {
            return;
        };
        if !held.own_take(take) {
            return;
        }
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

    /// Open one capture; a take's while `owner` names the page and the take
    /// recording on it, a hold's while it is `None`.
    fn open(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<&str>,
        owner: Option<(&str, &str)>,
    ) -> Result<Warm, String> {
        let carry = Arc::new(Carry::new(bridge, owner.is_some()));
        let held = (self.opener)(device, Arc::clone(&carry))?;
        Ok(Warm {
            requested: device.map(str::to_owned),
            held,
            owner: owner.map(|(page, take)| Owner { page: page.to_owned(), take: take.to_owned() }),
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
            /// What the take sent, where a test that watches the wire reads it.
            frames: tokio::sync::mpsc::UnboundedReceiver<crate::bridge::ClientEvent>,
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
                let (events, frames) = tokio::sync::mpsc::unbounded_channel();
                let bridge = Bridge::new(None, events);
                Self { mic, log, bridge, frames }
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
            let started = rig
                .mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take-1")
                .expect("starts");
            assert_eq!(started.id, "one");
            assert_eq!(rig.calls(), vec!["open one"], "the press opens nothing");

            // And ending the take does not close it either: the same device
            // serves the next take.
            rig.mic.stop("take-1");
            assert_eq!(rig.calls(), vec!["open one"], "the take's end keeps the device open");
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take-2")
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
            let held = rig
                .mic
                .start(&rig.bridge, Some("two".to_owned()), "page", "take")
                .expect("the take starts");
            assert_eq!(held.id, "two", "the re-acquired device is the one recorded from");
        }

        #[test]
        fn a_press_for_another_device_takes_over_the_hold() {
            let rig = Rig::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens");
            rig.mic
                .start(&rig.bridge, None, "page", "take-1")
                .expect("a plain take opens the default");
            assert_eq!(
                rig.calls(),
                vec!["open one", "open default", "release one"],
                "the press the hold cannot serve opens its own, and keeps it"
            );
            // The hold now follows the pressed device: the pick moves with
            // the take rather than fighting it.
            rig.mic.stop("take-1");
            rig.mic
                .start(&rig.bridge, None, "page", "take-2")
                .expect("the next plain take is warm");
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
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take")
                .expect("starts cold");
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
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");
            let refused = rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-2");
            assert_eq!(refused.err().as_deref(), Some("the microphone is already recording"));
        }

        /// The state a reload leaves: a take still recording, and the page
        /// that held its stop gone.
        fn reload(rig: &Rig, device: Option<String>) {
            rig.mic.hold(&rig.bridge, device.clone(), "page-a").expect("opens");
            rig.mic.start(&rig.bridge, device, "page-a", "take-a").expect("starts");
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
            match rig.mic.start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b") {
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
            match rig.mic.start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b") {
                Ok(started) => assert_eq!(started.id, "one"),
                Err(why) => panic!("the press before the new page's hold is refused: {why}"),
            }
        }

        /// Run one drain to its end, the way the shell runs it: the window
        /// it sleeps for is real, so the test waits it out.
        fn drained(rig: &Rig, take: &str) {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("a runtime")
                .block_on(rig.mic.drain(take));
        }

        #[test]
        fn a_drain_in_flight_when_the_next_take_begins_leaves_it_alone() {
            let rig = Rig::new();
            reload(&rig, Some("one".to_owned()));

            // page-a's drain is invoked while it still records, and page-b's
            // press lands inside the window: the live collision, drain first.
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("a runtime")
                .block_on(async {
                    let drain = rig.mic.drain("take-a");
                    let begin = async {
                        rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page-b").expect("holds");
                        rig.mic
                            .start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b")
                            .expect("page-b records");
                    };
                    tokio::join!(drain, begin);
                });

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b-2")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "the stale drain must not end the take the reloaded page began",
            );
        }

        /// The issue's own sequence (#2279): one page, two takes, and a
        /// drain still in flight from the first.
        #[test]
        fn a_drain_waking_after_the_same_pages_re_press_leaves_the_new_take_alone() {
            let rig = Rig::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");

            // The release begins the drain and the socket drops - or the
            // server says `dictate_ended` - while it sleeps, so the take ends
            // and the page's own re-press is free.
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("a runtime")
                .block_on(async {
                    let drain = rig.mic.drain("take-1");
                    let re_press = async {
                        rig.mic.stop("take-1");
                        rig.mic
                            .start(&rig.bridge, Some("one".to_owned()), "page", "take-2")
                            .expect("the re-press records");
                    };
                    tokio::join!(drain, re_press);
                });

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "page", "take-3")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "the stale drain must not end the take the same page's re-press began",
            );
        }

        /// The stop's half of the same window: it is fire-and-forget on the
        /// page's side, so one for the take that ended can land after the
        /// re-press began.
        #[test]
        fn a_stop_for_the_take_that_ended_leaves_the_new_take_alone() {
            let rig = Rig::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");
            rig.mic.stop("take-1");
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take-2")
                .expect("the re-press records");

            rig.mic.stop("take-1");

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "page", "take-3")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "the late stop must not end the take the re-press began",
            );
        }

        /// One page, and a first take whose token happens to equal the page's
        /// own id: the guards must read the take field, not the page, however
        /// the two strings line up.
        #[test]
        fn a_guard_reads_the_take_field_even_when_the_token_equals_the_page() {
            let rig = Rig::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "same").expect("opens");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "same", "same").expect("starts");
            rig.mic.stop("same");
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "same", "next")
                .expect("the re-press records");

            rig.mic.stop("same");

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "same", "third")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "a stop naming the page-id take must not end the take that followed it",
            );
        }

        #[test]
        fn a_stop_from_the_reloaded_page_leaves_the_new_take_alone() {
            let rig = Rig::new();
            reload(&rig, Some("one".to_owned()));
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page-b").expect("holds");
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b")
                .expect("page-b records");

            rig.mic.stop("take-a");

            assert_eq!(
                rig.mic
                    .start(&rig.bridge, Some("one".to_owned()), "page-b", "take-b-2")
                    .err()
                    .as_deref(),
                Some("the microphone is already recording"),
                "the stale stop must not end the take the reloaded page began",
            );
        }

        #[test]
        fn a_drain_from_the_recording_take_ends_that_take() {
            let mut rig = Rig::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("opens");
            rig.mic.start(&rig.bridge, Some("one".to_owned()), "page", "take-1").expect("starts");

            // A partial frame the device left mid-frame: the drain's hand-over.
            let partial = 8usize;
            {
                let warm = rig.mic.warm.lock().unwrap();
                let held = warm.as_ref().expect("warm");
                held.held.carry().push(&vec![0.0; partial]);
            }
            drained(&rig, "take-1");

            assert!(
                matches!(
                    rig.frames.try_recv(),
                    Ok(crate::bridge::ClientEvent::Mic(bytes)) if bytes.len() == 1 + partial * 2
                ),
                "the take's own drain carries the partial frame it was holding",
            );
            rig.mic
                .start(&rig.bridge, Some("one".to_owned()), "page", "take-2")
                .expect("the take's own drain ended its take");
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
        let take = mic
            .start(&bridge, None, "page", "take-1")
            .expect("the held capture must serve the take");
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
        mic.stop("take-1");
        let again = mic
            .start(&bridge, None, "page", "take-2")
            .expect("the next take is served without reopening");
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

    /// The bounded open: a device layer that answers neither way is reported
    /// rather than waited on for ever.
    mod the_bounded_open {
        use std::sync::mpsc::{Receiver, Sender};

        use tokio::sync::oneshot;

        use super::*;

        /// The bound the tests drive an open with. The shipped one is
        /// [`OPEN_LIMIT`], and what matters here is that the bound is the
        /// caller's, so no test waits the real twenty seconds.
        const BOUND: Duration = Duration::from_millis(50);

        /// The bound on everything a test does not mean to measure: the open
        /// reaching the device, the parked open being let go, and the budget
        /// an open that ANSWERS is given. A failure bound rather than a wait -
        /// nothing here is timing the open itself.
        const SETTLE: Duration = Duration::from_secs(10);

        /// A stand-in capture, as the hold's own test stands one in.
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

        /// A stand-in device whose FIRST open answers only when the test lets
        /// it go: the wedged layer, answering neither way until then. A later
        /// open is answered at once, so a test that trips the busy guard hears
        /// about it rather than parking again.
        struct Wedged {
            mic: Arc<Mic>,
            /// Fires as the opener is entered, which is what tells a test the
            /// open is in flight rather than a thread that never ran.
            entered: oneshot::Receiver<()>,
            /// Lets the parked open answer, once the test is done with it.
            release: Sender<()>,
            bridge: Arc<Bridge>,
            log: Arc<Mutex<Vec<String>>>,
        }

        impl Wedged {
            fn new() -> Self {
                let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let opened = Arc::clone(&log);
                let (entered, answer) = oneshot::channel::<()>();
                let (release, holding) = std::sync::mpsc::channel::<()>();
                let parked: Arc<Mutex<Option<Receiver<()>>>> = Arc::new(Mutex::new(Some(holding)));
                // The signal crosses once, and a `Fn` closure may be called
                // again, so the sender is held where it can be taken.
                let arrived: Arc<Mutex<Option<oneshot::Sender<()>>>> =
                    Arc::new(Mutex::new(Some(entered)));
                let mic = Arc::new(Mic {
                    warm: Mutex::new(None),
                    opener: Box::new(move |device, carry| {
                        let name = device.unwrap_or("default").to_owned();
                        let sent = arrived.lock().unwrap_or_else(PoisonError::into_inner).take();
                        if let Some(sent) = sent {
                            let _ = sent.send(());
                        }
                        let waiting = parked.lock().unwrap_or_else(PoisonError::into_inner).take();
                        if let Some(waiting) = waiting {
                            let _ = waiting.recv_timeout(SETTLE);
                        }
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
                });
                let (events, _frames) = tokio::sync::mpsc::unbounded_channel();
                Self { mic, entered: answer, release, bridge: Bridge::new(None, events), log }
            }
        }

        fn runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread().enable_time().build().expect("a runtime")
        }

        /// A stand-in capture whose release parks until the test lets it go:
        /// the window between a take installing its own capture and the one it
        /// replaced being let go.
        struct Letting {
            device: forge_capture::Device,
            carry: Arc<Carry>,
            /// Fires as the release is entered.
            entered: Arc<Mutex<Option<Sender<()>>>>,
            /// Lets the parked release finish.
            parked: Arc<Mutex<Option<Receiver<()>>>>,
        }

        impl Held for Letting {
            fn device(&self) -> &forge_capture::Device {
                &self.device
            }

            fn carry(&self) -> &Arc<Carry> {
                &self.carry
            }

            fn release(self: Box<Self>) {
                let sent = self.entered.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(sent) = sent {
                    let _ = sent.send(());
                }
                let waiting = self.parked.lock().unwrap_or_else(PoisonError::into_inner).take();
                if let Some(waiting) = waiting {
                    let _ = waiting.recv_timeout(SETTLE);
                }
            }
        }

        /// The swap rig: every capture's release parks, so a test can hold that
        /// window open and hand the give-up in inside it.
        struct Swap {
            mic: Arc<Mic>,
            bridge: Arc<Bridge>,
            /// Fires as a release is entered.
            letting: Receiver<()>,
            /// Lets that release finish.
            let_go: Sender<()>,
        }

        impl Swap {
            fn new() -> Self {
                let (entered, letting) = std::sync::mpsc::channel::<()>();
                let (let_go, parked) = std::sync::mpsc::channel::<()>();
                let entered = Arc::new(Mutex::new(Some(entered)));
                let parked = Arc::new(Mutex::new(Some(parked)));
                let mic = Arc::new(Mic {
                    warm: Mutex::new(None),
                    opener: Box::new(move |device, carry| {
                        let name = device.unwrap_or("default").to_owned();
                        Ok(Box::new(Letting {
                            device: forge_capture::Device {
                                id: name,
                                name: "stand-in".to_owned(),
                                is_default: device.is_none(),
                            },
                            carry,
                            entered: Arc::clone(&entered),
                            parked: Arc::clone(&parked),
                        }))
                    }),
                });
                let (events, _frames) = tokio::sync::mpsc::unbounded_channel();
                Self { mic, bridge: Bridge::new(None, events), letting, let_go }
            }
        }

        /// A stand-in device that answers at once, the way the machine's own
        /// inputs do. `refusal` stands one in that will not open at all.
        struct Open {
            mic: Arc<Mic>,
            bridge: Arc<Bridge>,
        }

        impl Open {
            fn new() -> Self {
                Self::with_opener(None)
            }

            fn refusing(why: &str) -> Self {
                Self::with_opener(Some(why.to_owned()))
            }

            fn with_opener(refusal: Option<String>) -> Self {
                let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                let opened = Arc::clone(&log);
                let mic = Arc::new(Mic {
                    warm: Mutex::new(None),
                    opener: Box::new(move |device, carry| {
                        if let Some(why) = &refusal {
                            return Err(why.clone());
                        }
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
                });
                let (events, _frames) = tokio::sync::mpsc::unbounded_channel();
                Self { mic, bridge: Bridge::new(None, events) }
            }
        }

        #[test]
        fn an_open_that_answers_neither_way_is_reported_and_leaves_no_take() {
            let rig = Wedged::new();
            let runtime = runtime();
            // Both must complete: the open reaches the device, and the bound
            // answers it. Joined rather than ordered, so neither thread's
            // scheduling decides what this test reads.
            let (refused, entered) = runtime.block_on(async {
                tokio::join!(
                    rig.mic.start_within(&rig.bridge, None, "page", "take-1", BOUND),
                    tokio::time::timeout(SETTLE, rig.entered),
                )
            });
            // The parked open is let go FIRST, whatever the assertions below
            // make of what it did: a thread left parked would hang the test
            // rather than fail it, and a failing mutation has to fail.
            rig.release.send(()).expect("the open is parked");

            assert!(
                matches!(entered, Ok(Ok(()))),
                "the opener was entered: the bound answered an open in flight, not a thread \
                 that never ran, got {entered:?}"
            );
            assert_eq!(
                refused.err().as_deref(),
                Some("the microphone did not come up"),
                "an open that answers neither way leaves the take's line saying the microphone \
                 did not come up, rather than the page waiting on it for ever",
            );

            // The open answered late, and successfully. The take it began is
            // nobody's - the page gave up on it - so it must not be left
            // holding the microphone.
            let next = rig.mic.start(&rig.bridge, None, "page", "take-2");
            assert!(
                next.is_ok(),
                "the press after a given-up open records rather than being refused: {next:?}"
            );
            assert_eq!(
                rig.log.lock().unwrap().clone(),
                vec!["open default"],
                "the late open is kept as the hold, with the take nobody owns ended"
            );
        }

        #[test]
        fn an_open_that_answers_is_the_take_it_opened() {
            let rig = Open::new();
            let started = runtime().block_on(rig.mic.start_within(
                &rig.bridge,
                Some("one".to_owned()),
                "page",
                "take-1",
                SETTLE,
            ));
            assert_eq!(
                started.expect("an open that answers is the take").id,
                "one",
                "the bounded open hands a take that came up its own device back"
            );
        }

        #[test]
        fn a_device_that_will_not_open_keeps_its_own_words() {
            let rig = Open::refusing("no input device named \"gone\"");
            let refused = runtime().block_on(rig.mic.start_within(
                &rig.bridge,
                Some("gone".to_owned()),
                "page",
                "take-1",
                SETTLE,
            ));
            assert_eq!(
                refused.err().as_deref(),
                Some("no input device named \"gone\""),
                "only an open that answers neither way draws the bound's sentence: a device \
                 that will not open says why itself"
            );
        }

        /// A signal whose bound is spent but whose flag is unset: the
        /// scheduling lag between the deadline and the task waking to set it.
        fn spent_signal() -> GivenUp {
            GivenUp {
                flag: Arc::new(AtomicBool::new(false)),
                deadline: Instant::now() - Duration::from_secs(1),
            }
        }

        #[test]
        fn an_open_that_answers_past_the_deadline_is_given_up_on_without_the_flag() {
            let rig = Open::new();
            let opened =
                rig.mic.start_inner(&rig.bridge, None, "page", "take-1", Some(&spent_signal()));
            assert!(
                opened.is_err(),
                "an open answering past the deadline is the caller's lost attempt: {opened:?}"
            );
            let next = rig.mic.start(&rig.bridge, None, "page", "take-2");
            assert!(
                next.is_ok(),
                "the take nobody owns is ended rather than left recording: {next:?}"
            );
        }

        /// The flag is the caller's own word, and a paused clock is the one
        /// shape where only it can be read: tokio's clock is what paces the
        /// bound and it moves only where this test moves it, so the bound is
        /// spent - the flag set - while `Instant::now()` has barely moved.
        #[test]
        fn the_give_up_the_bound_sets_is_read_when_the_clock_is_paused() {
            let rig = Wedged::new();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .start_paused(true)
                .build()
                .expect("a runtime");
            let refused = runtime.block_on(async {
                tokio::join!(
                    rig.mic.start_within(&rig.bridge, None, "page", "take-1", SETTLE),
                    async {
                        rig.entered.await.expect("the opener is entered");
                        // Advanced by hand: the open holds a blocking task, and
                        // the clock never auto-advances past one.
                        tokio::time::advance(SETTLE).await;
                    },
                )
                .0
            });
            rig.release.send(()).expect("the open is parked");

            assert_eq!(
                refused.err().as_deref(),
                Some("the microphone did not come up"),
                "a bound the clock spends is a give-up like any other"
            );
            let next = rig.mic.start(&rig.bridge, None, "page", "take-2");
            assert!(
                next.is_ok(),
                "the flag the bound set is what ends the take nobody owns: {next:?}"
            );
        }

        #[test]
        fn a_spent_deadline_does_not_begin_a_warm_take() {
            let rig = Open::new();
            rig.mic.hold(&rig.bridge, None, "page").expect("the hold opens the device");
            let opened =
                rig.mic.start_inner(&rig.bridge, None, "page", "take-1", Some(&spent_signal()));
            assert!(
                opened.is_err(),
                "a warm capture answers at once, so a spent deadline is a take that waited out \
                 a wedged open: {opened:?}"
            );
            let next = rig.mic.start(&rig.bridge, None, "page", "take-2");
            assert!(
                next.is_ok(),
                "the take nobody owns is ended rather than left recording: {next:?}"
            );
        }

        /// The tail the read has to come after: a press that changes the pick
        /// installs its own capture and lets the held one go, and that release
        /// takes the capture thread's own window. The give-up lands inside it,
        /// and the take nobody owns must still end.
        #[test]
        fn a_give_up_landing_while_the_old_capture_is_let_go_still_ends_the_take() {
            let rig = Swap::new();
            rig.mic.hold(&rig.bridge, Some("one".to_owned()), "page").expect("the hold opens");
            let signal = GivenUp {
                flag: Arc::new(AtomicBool::new(false)),
                deadline: Instant::now() + SETTLE,
            };
            let flag = Arc::clone(&signal.flag);

            // The press runs on its own thread: it blocks inside the release,
            // which is the window this test holds open.
            let pressing = {
                let mic = Arc::clone(&rig.mic);
                let bridge = Arc::clone(&rig.bridge);
                std::thread::spawn(move || {
                    mic.start_inner(
                        &bridge,
                        Some("two".to_owned()),
                        "page",
                        "take-1",
                        Some(&signal),
                    )
                })
            };
            rig.letting.recv_timeout(SETTLE).expect("the release is entered");
            // The caller gives up while the release is inside its window.
            flag.store(true, Ordering::Release);
            rig.let_go.send(()).expect("the release is parked");

            let outcome = pressing.join().expect("the press finishes");
            assert_eq!(
                outcome.err().as_deref(),
                Some("the microphone did not come up"),
                "a give-up landing in the release's window is still the caller's word"
            );
            let next = rig.mic.start(&rig.bridge, Some("two".to_owned()), "page", "take-2");
            assert!(
                next.is_ok(),
                "the take nobody owns is ended rather than left recording: {next:?}"
            );
        }
    }
}

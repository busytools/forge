//! A painted frame as it leaves the host: a small header and encoded pixels.
//!
//! The wire form is `[profile u16][tab u16][seq u32][width u16][height u16]
//! [format u8][key u8][page_width u16][page_height u16][bytes]` in
//! little-endian, and [`crate::channel`] carries it with a length prefix. The
//! bytes stay encoded end to end: the host is the only encoder, and the
//! server relays them untouched.

/// How a frame's pixels are encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Lossy or lossless, decided by the frame's `key` flag.
    Webp,
}

impl Format {
    fn code(self) -> u8 {
        match self {
            Format::Webp => 1,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Format::Webp),
            _ => None,
        }
    }
}

/// The header's own length; the payload follows it.
pub const HEADER_LEN: usize = 18;

/// One encoded frame for one tab of one profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The served profile's id, as the server assigned it.
    pub profile: u16,
    /// The profile's tab this frame belongs to.
    pub tab: u16,
    /// Monotonic per tab; a newer frame supersedes an older one.
    pub seq: u32,
    pub width: u16,
    pub height: u16,
    pub format: Format,
    /// A full-quality frame: the first of a surface, and every resize.
    pub key: bool,
    /// The page's own viewport, the space a gesture's point is sent in. **Not
    /// the frame's pixels divided by anything**: the raster between them is
    /// the host's, and the encoded scale may change it too.
    pub page_width: u16,
    pub page_height: u16,
    pub bytes: Vec<u8>,
}

impl Frame {
    /// The header and the payload, with no length prefix.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.bytes.len());
        out.extend_from_slice(&self.profile.to_le_bytes());
        out.extend_from_slice(&self.tab.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.push(self.format.code());
        out.push(u8::from(self.key));
        out.extend_from_slice(&self.page_width.to_le_bytes());
        out.extend_from_slice(&self.page_height.to_le_bytes());
        out.extend_from_slice(&self.bytes);
        out
    }

    /// One encoded frame, header checked before any payload is trusted.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < HEADER_LEN {
            return Err(format!("a frame is at least {HEADER_LEN} bytes, got {}", bytes.len()));
        }
        let profile = u16::from_le_bytes([bytes[0], bytes[1]]);
        let tab = u16::from_le_bytes([bytes[2], bytes[3]]);
        let seq = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let width = u16::from_le_bytes([bytes[8], bytes[9]]);
        let height = u16::from_le_bytes([bytes[10], bytes[11]]);
        let format = Format::from_code(bytes[12])
            .ok_or_else(|| format!("unknown frame format {}", bytes[12]))?;
        let key = match bytes[13] {
            0 => false,
            1 => true,
            other => return Err(format!("the key flag is 0 or 1, got {other}")),
        };
        let page_width = u16::from_le_bytes([bytes[14], bytes[15]]);
        let page_height = u16::from_le_bytes([bytes[16], bytes[17]]);
        let payload = bytes[HEADER_LEN..].to_vec();
        if payload.is_empty() {
            return Err("a frame carries no encoded pixels".to_owned());
        }
        if width == 0 || height == 0 {
            return Err(format!("a frame has a size, got {width}x{height}"));
        }
        if page_width == 0 || page_height == 0 {
            return Err(format!(
                "a frame names the page it was laid out at, got {page_width}x{page_height}"
            ));
        }
        Ok(Frame {
            profile,
            tab,
            seq,
            width,
            height,
            format,
            key,
            page_width,
            page_height,
            bytes: payload,
        })
    }
}

/// Encode one BGRA view buffer.
///
/// **BGRA is what CEF paints and what the ring holds**; libwebp's own
/// layouts do not include it, so the pixels are swapped into RGBA once.
/// `key` frames are lossless (a surface's first frame and every resize);
/// the rest use the configured quality. `None` is an encoder refusal, which
/// is a frame not produced rather than a panicked paint callback.
#[cfg(target_os = "macos")]
pub fn encode_webp(
    bgra: &[u8],
    width: u32,
    height: u32,
    quality: u8,
    key: bool,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let mut rgba = bgra.to_vec();
    for pixel in rgba.chunks_mut(4) {
        pixel.swap(0, 2);
    }
    let encoder = webp::Encoder::from_rgba(&rgba, width, height);
    match encoder.encode_simple(key, f32::from(quality)) {
        Ok(memory) => Some(memory.to_vec()),
        Err(why) => {
            eprintln!(
                "cef host: a frame was not encoded (event_name cef_frame_encode_failed, \
                 {width}x{height}): {why:?}"
            );
            None
        }
    }
}

/// The host's paint-to-wire path: encode one paint and write it to the
/// profile's socket.
///
/// **The configured rate is the worker's wait, not this sink's drop.** The
/// worker waits out [`Sink::interval_remaining`], then hands the newest paint
/// over, so a 60 Hz paint cadence costs one encoder run per interval and the
/// last state of a burst is never lost. A frame a slow reader cannot take is
/// dropped whole and the surface is served by the next one.
#[cfg(target_os = "macos")]
pub struct Sink {
    profile: u16,
    tab: u16,
    path: std::path::PathBuf,
    fps: u16,
    quality: u8,
    stream: Option<std::os::unix::net::UnixStream>,
    last_attempt: Option<std::time::Instant>,
    last_sent: Option<std::time::Instant>,
    size: Option<(u32, u32)>,
    seq: u32,
    logged_failure: bool,
    /// Set by [`Sink::force_key`]: the next sent frame is full quality.
    forced_key: bool,
    /// While set, paints are counted but nothing is encoded or written.
    paused: bool,
}

#[cfg(target_os = "macos")]
impl Sink {
    pub fn new(profile: u16, tab: u16, path: std::path::PathBuf, fps: u16, quality: u8) -> Self {
        Self {
            profile,
            tab,
            path,
            fps: fps.clamp(1, 120),
            quality: quality.min(100),
            stream: None,
            last_attempt: None,
            last_sent: None,
            size: None,
            seq: 0,
            logged_failure: false,
            forced_key: false,
            paused: false,
        }
    }

    /// Ask for a full-quality frame on the next paint: a viewer has attached,
    /// and its first draw is the crisp one.
    pub fn force_key(&mut self) {
        self.forced_key = true;
    }

    /// Offer one packet of the page's audio.
    ///
    /// **No rate limit and no encode**: the samples are already the wire's
    /// own, and audio is a clock of its own rather than a repaint. A paused
    /// viewer is silent, the same rule frames follow.
    pub fn offer_audio(&mut self, audio: &crate::channel::Audio) {
        if self.paused {
            return;
        }
        if self.stream.is_none() && !self.reconnect() {
            return;
        }
        if let Some(stream) = self.stream.as_mut()
            && let Err(why) = crate::channel::write_audio(stream, audio)
        {
            eprintln!(
                "cef host: an audio packet was not written (event_name \
                 cef_audio_write_failed, profile {}): {why}",
                self.profile
            );
            // The connection is dropped; the next packet reconnects.
            self.stream = None;
        }
    }

    /// Pause or resume encoding. **Resuming asks for a key frame**: frames
    /// that changed while nobody watched are not a delta's to describe.
    pub fn set_paused(&mut self, paused: bool) {
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        if !paused {
            self.forced_key = true;
        }
    }

    /// How long the configured frame interval still owes, since the last
    /// frame was written. **The worker waits this out before encoding the
    /// newest paint**, which is where the configured rate is spent; the sink
    /// itself never drops a frame for arriving early.
    pub fn interval_remaining(&self) -> std::time::Duration {
        let interval = std::time::Duration::from_millis(1000 / u64::from(self.fps));
        self.last_sent
            .map_or(std::time::Duration::ZERO, |last| interval.saturating_sub(last.elapsed()))
    }

    /// Offer one painted view: encode it and write it. Called by the host's
    /// frame worker, which has already picked the newest paint; `page` is the
    /// viewport that paint was laid out at.
    pub fn offer(&mut self, bgra: &[u8], width: u32, height: u32, page: (u32, u32)) {
        // **Nobody is watching: paint, but do not encode.** The paint itself
        // is CEF's and still happens; this is the WebP cost a viewer pays
        // for and an unwatched host does not.
        if self.paused {
            return;
        }
        // **A key frame is never coalesced away**: a resize or an attach is
        // the one paint the viewer cannot get from a later delta.
        let key = self.forced_key || self.size != Some((width, height));
        if self.stream.is_none() && !self.reconnect() {
            return;
        }
        let Some(bytes) = encode_webp(bgra, width, height, self.quality, key) else {
            return;
        };
        let (Ok(width16), Ok(height16)) = (u16::try_from(width), u16::try_from(height)) else {
            return;
        };
        let (Ok(page_width), Ok(page_height)) =
            (u16::try_from(page.0.max(1)), u16::try_from(page.1.max(1)))
        else {
            return;
        };
        let frame = Frame {
            profile: self.profile,
            tab: self.tab,
            seq: self.seq,
            width: width16,
            height: height16,
            format: Format::Webp,
            key,
            page_width,
            page_height,
            bytes,
        };
        if let Some(stream) = self.stream.as_mut() {
            match crate::channel::write(stream, &frame) {
                Ok(()) => {
                    self.seq = self.seq.wrapping_add(1);
                    self.last_sent = Some(std::time::Instant::now());
                    self.size = Some((width, height));
                    self.logged_failure = false;
                    self.forced_key = false;
                }
                Err(why) => {
                    // **A reader that cannot take a frame is not the paint
                    // path's problem.** The connection is dropped and the
                    // next interval tries again; the frame is superseded by
                    // the one after it, which carries the whole page.
                    if !self.logged_failure {
                        eprintln!(
                            "cef host: a frame was dropped (event_name cef_frame_dropped, \
                             profile {}): {why}",
                            self.profile
                        );
                        self.logged_failure = true;
                    }
                    self.stream = None;
                }
            }
        }
    }

    /// A connection at most once a second, so a service that has not taken
    /// the socket yet costs the paint path nothing but a failed syscall.
    fn reconnect(&mut self) -> bool {
        let now = std::time::Instant::now();
        if self.last_attempt.is_some_and(|last| now.duration_since(last) < ONE_SECOND) {
            return false;
        }
        self.last_attempt = Some(now);
        match crate::channel::connect(&self.path) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.logged_failure = false;
                true
            }
            Err(why) => {
                if !self.logged_failure {
                    eprintln!(
                        "cef host: the frame socket is not taken yet (event_name \
                         cef_frames_unconnected, profile {}): {why}",
                        self.profile
                    );
                    self.logged_failure = true;
                }
                false
            }
        }
    }
}

#[cfg(target_os = "macos")]
const ONE_SECOND: std::time::Duration = std::time::Duration::from_secs(1);

#[cfg(test)]
mod tests {
    use super::{Format, Frame, HEADER_LEN};

    fn frame(seq: u32, profile: u16) -> Frame {
        Frame {
            profile,
            tab: 1,
            seq,
            width: 800,
            height: 600,
            format: Format::Webp,
            key: seq == 0,
            page_width: 400,
            page_height: 300,
            bytes: vec![1, 2, 3, 4],
        }
    }

    /// **The wire form round-trips, field for field.** A header placed at
    /// the wrong offset draws shifted rows or another profile's page.
    #[test]
    fn a_frame_round_trips_with_its_header() {
        let original = frame(7, 3);
        let bytes = original.encode();
        assert_eq!(bytes.len(), HEADER_LEN + original.bytes.len());
        assert_eq!(Frame::decode(&bytes), Ok(original));
    }

    /// **The profile and the tab are both on the wire.** A host serves one
    /// profile's tabs, and a frame that lost its tab drew on the wrong one.
    #[test]
    fn a_frame_names_its_profile_and_its_tab() {
        let frame = Frame {
            profile: 3,
            tab: 7,
            seq: 9,
            width: 800,
            height: 600,
            format: Format::Webp,
            key: true,
            page_width: 400,
            page_height: 300,
            bytes: vec![1, 2, 3],
        };
        let encoded = frame.encode();
        assert_eq!(u16::from_le_bytes([encoded[0], encoded[1]]), 3);
        assert_eq!(u16::from_le_bytes([encoded[2], encoded[3]]), 7);
        assert_eq!(Frame::decode(&encoded).expect("a frame decodes"), frame);

        let mut old = encoded.clone();
        old[2..4].copy_from_slice(&0_u16.to_le_bytes());
        old.truncate(12);
        assert_eq!(
            Frame::decode(&old).expect_err("a short header is refused"),
            "a frame is at least 18 bytes, got 12",
        );
    }

    /// **The page's own viewport crosses beside the frame's pixels**, and
    /// the two are not the same number: a viewer maps a gesture through the
    /// page, so a header that carried only the pixels left every click on a
    /// rescaled or letterboxed picture at the wrong point.
    #[test]
    fn a_frame_names_the_page_its_pixels_were_painted_from() {
        let mut frame = frame(1, 0);
        frame.width = 2560;
        frame.height = 1440;
        frame.page_width = 1280;
        frame.page_height = 720;
        let encoded = frame.encode();
        assert_eq!(u16::from_le_bytes([encoded[14], encoded[15]]), 1280, "the header's page width");
        assert_eq!(u16::from_le_bytes([encoded[16], encoded[17]]), 720, "and its height");
        let read = Frame::decode(&encoded).expect("a frame decodes");
        assert_eq!((read.page_width, read.page_height), (1280, 720));
        assert_eq!((read.width, read.height), (2560, 1440), "and its pixels are their own");

        let mut unnamed = encoded.clone();
        unnamed[14..16].copy_from_slice(&0_u16.to_le_bytes());
        assert_eq!(
            Frame::decode(&unnamed).expect_err("a frame with no page is refused"),
            "a frame names the page it was laid out at, got 0x720",
        );
    }

    /// **A frame the header cannot describe is refused, not read.** The
    /// negative control is the control: a truncated buffer and an unknown
    /// format byte must not decode into something plausible.
    #[test]
    fn a_frame_that_does_not_fit_its_header_is_refused() {
        let whole = frame(1, 0).encode();
        assert!(Frame::decode(&whole[..HEADER_LEN - 1]).is_err(), "a short header");
        assert!(Frame::decode(&whole[..HEADER_LEN]).is_err(), "no pixels");
        let mut unknown = whole.clone();
        unknown[12] = 9;
        assert!(Frame::decode(&unknown).is_err(), "an unknown format");
        let mut flag = whole;
        flag[13] = 2;
        assert!(Frame::decode(&flag).is_err(), "a key flag that is not a flag");
    }

    /// **An unwatched host paints but does not encode.** The paused sink
    /// writes nothing; unpausing sends one key frame, because frames that
    /// changed while nobody watched are not a delta's to describe.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_paused_sink_writes_nothing_and_resumes_with_a_key_frame() {
        let dir = std::env::temp_dir().join(format!("forge-sink-pause-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        let path = dir.join("frames.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("a listener");
        let bgra = [255_u8, 0, 0, 255, 0, 0, 255, 255];

        let mut sink = super::Sink::new(1, 1, path.clone(), 120, 80);
        sink.set_paused(true);
        sink.offer(&bgra, 2, 1, (1, 1));
        listener.set_nonblocking(true).expect("a non-blocking listener");
        assert!(listener.accept().is_err(), "a paused sink writes no frame");

        sink.set_paused(false);
        sink.offer(&bgra, 2, 1, (1, 1));
        let (mut stream, _) = listener.accept().expect("a frame connection");
        stream.set_read_timeout(Some(super::ONE_SECOND)).expect("a read bound");
        let crate::channel::Reception::Frame(frame) =
            crate::channel::read(&mut stream).expect("a frame")
        else {
            panic!("a resumed sink sends a frame");
        };
        assert!(frame.key, "the first frame after a pause is a key frame");
        std::fs::remove_dir_all(&dir).expect("the temp dir goes");
    }

    /// **The sink never drops a frame, and a resize is a key frame.** The
    /// configured rate is the worker's wait; a sink that ate the newest paint
    /// left the viewer on a state no later paint repaired.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_sink_writes_every_frame_and_a_resize_is_a_key() {
        let dir = std::env::temp_dir().join(format!("forge-sink-resize-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        let path = dir.join("frames.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("a listener");
        let two = [255_u8, 0, 0, 255, 0, 0, 255, 255];
        let four = [1_u8; 16];

        // One frame a second, so every frame after the first is well inside
        // the interval and would have been dropped by the old rate limit.
        let mut sink = super::Sink::new(1, 1, path.clone(), 1, 80);
        sink.offer(&two, 2, 1, (1, 1));
        let (mut stream, _) = listener.accept().expect("the first connection");
        stream.set_read_timeout(Some(super::ONE_SECOND)).expect("a read bound");
        let crate::channel::Reception::Frame(first) =
            crate::channel::read(&mut stream).expect("the first frame")
        else {
            panic!("a painted frame arrives");
        };
        assert_eq!((first.width, first.height), (2, 1));

        sink.offer(&four, 4, 1, (2, 1));
        let crate::channel::Reception::Frame(resized) =
            crate::channel::read(&mut stream).expect("the resized frame")
        else {
            panic!("a resize sends inside the interval");
        };
        assert_eq!((resized.width, resized.height), (4, 1));
        assert!(resized.key, "and the resize is a key frame");

        sink.offer(&four, 4, 1, (2, 1));
        assert!(
            matches!(
                crate::channel::read(&mut stream).expect("a same-size frame"),
                crate::channel::Reception::Frame(_)
            ),
            "a same-size frame inside the interval is written, not dropped"
        );
        std::fs::remove_dir_all(&dir).expect("the temp dir goes");
    }

    /// **A painted BGRA buffer becomes a WebP a decoder would recognise.**
    /// The channel between two profiles is only bytes, so this pins the
    /// encoder's own output shape: RIFF/WebP, and libwebp's `VP8`/`VP8L`
    /// chunk for the lossy and lossless cases.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_bgra_view_encodes_to_webp() {
        // Two pixels: blue opaque, red opaque, in BGRA order.
        let bgra = [255_u8, 0, 0, 255, 0, 0, 255, 255];
        let lossy = super::encode_webp(&bgra, 2, 1, 80, false).expect("a lossy frame");
        assert_eq!(&lossy[..4], b"RIFF");
        assert_eq!(&lossy[8..12], b"WEBP");
        assert!(lossy.windows(4).any(|window| window == b"VP8 "), "lossy uses VP8");
        let key = super::encode_webp(&bgra, 2, 1, 80, true).expect("a key frame");
        assert!(key.windows(4).any(|window| window == b"VP8L"), "key frames are lossless");
        assert!(super::encode_webp(&bgra, 0, 1, 80, false).is_none(), "no view, no frame");
    }
}

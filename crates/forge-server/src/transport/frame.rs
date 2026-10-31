//! The binary frames on the socket.
//!
//! One WebSocket binary message per frame: a one-byte kind tag, then the
//! payload. Three kinds, and what tells them apart is the tag alone:
//!
//! - [`Kind::Dictation`] - a client's mono PCM at [`SAMPLE_RATE`], the rate
//!   the dictation models read. The server decodes this one.
//! - [`Kind::BrowserFrame`] - the server's relay of one encoded browser
//!   frame: the profile id, sequence, view size, format, key flag, the page's
//!   own viewport and the WebP payload, exactly as the host encoded it. **The
//!   bytes are never re-encoded or decoded here**; the server only moves them.
//! - [`Kind::BrowserAudio`] - the viewed page's audio, relayed to the
//!   controlling client with its profile, sequence, sample rate and channels.
//!
//! **Tag 3 is not a kind here.** A whole `ServerMessage` body, gzipped for a
//! client that declared `deflate=1`, is built by the connection's encoder and
//! read by that client's decoder, so it crosses as a server-only frame rather
//! than as a variant of [`Kind`].
//!
//! **The kind tag is why the shape can grow**: a new client frame is a variant
//! here plus a line in the recorded contract, not a new message type.
//!
//! **The server decodes what clients send and encodes what it sends.** A
//! browser frame arriving from a client is refused: the host is the only
//! encoder.
//!
//! **A frame leaves the server through one interface.** [`FrameTransport`]
//! is what the connection asks to deliver a viewer's frame; the socket is its
//! only implementation today, and a WebRTC backend later is another
//! implementation rather than a change to input, tools or the draw path.

use axum::extract::ws::Message;
use forge_primitives::browser::{BrowserAudio, BrowserFrame};
use futures_util::SinkExt as _;

use super::{WRITE_BOUND, write_within};

/// The rate every dictation frame's samples are at. Pinned against the
/// dictation crate's own constant by a test rather than imported: the wire's
/// rate is this crate's rule, and the transport does not need the audio crate
/// to state it.
pub const SAMPLE_RATE: u32 = 16_000;

/// One dictation frame's header: the kind tag and nothing else.
pub const HEADER_BYTES: usize = 1;

/// The largest dictation payload a frame may carry. Twenty-five times the
/// 20 ms frame a client sends, so a malformed or hostile length cannot make
/// the server hold a large allocation per message.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

/// What a binary frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Mono 16-bit little-endian PCM: what a browser's own i16 conversion
    /// produces, with no encoder on either side.
    Dictation,
    /// One encoded browser frame, relayed from the host.
    BrowserFrame,
    /// One packet of the viewed page's audio, relayed to the controlling
    /// client: the profile id, sequence, sample rate and channels, then
    /// interleaved 16-bit samples.
    BrowserAudio,
}

impl Kind {
    /// The tag byte this kind crosses as.
    pub const fn tag(self) -> u8 {
        match self {
            Self::Dictation => 0,
            Self::BrowserFrame => 1,
            Self::BrowserAudio => 2,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Dictation),
            1 => Some(Self::BrowserFrame),
            2 => Some(Self::BrowserAudio),
            _ => None,
        }
    }
}

/// How one viewer receives a browser's frames.
///
/// **The connection never formats a frame itself**: it hands one to this
/// and the transport decides how the person sees it. The socket transport
/// below is the only implementation now.
#[async_trait::async_trait]
pub trait FrameTransport: Send {
    /// Deliver one encoded frame to this viewer. `Err` is a connection that
    /// is gone, and the caller ends it.
    async fn deliver(&mut self, frame: &BrowserFrame) -> anyhow::Result<()>;

    /// Deliver one packet of the page's audio to this viewer.
    async fn deliver_audio(&mut self, audio: &BrowserAudio) -> anyhow::Result<()>;
}

/// The socket's own transport: one tagged binary message per frame.
pub struct SocketFrames<'a, S> {
    pub socket: &'a mut S,
}

#[async_trait::async_trait]
impl<S> FrameTransport for SocketFrames<'_, S>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin + Send,
{
    async fn deliver(&mut self, frame: &BrowserFrame) -> anyhow::Result<()> {
        let mut bytes = Vec::with_capacity(HEADER_BYTES + frame.encoded.len());
        bytes.push(Kind::BrowserFrame.tag());
        bytes.extend_from_slice(&frame.encoded);
        write_within(WRITE_BOUND, async {
            self.socket.send(Message::Binary(bytes.into())).await.map_err(anyhow::Error::from)
        })
        .await
    }

    async fn deliver_audio(&mut self, audio: &BrowserAudio) -> anyhow::Result<()> {
        let bytes = audio_bytes(audio);
        write_within(WRITE_BOUND, async {
            self.socket.send(Message::Binary(bytes.into())).await.map_err(anyhow::Error::from)
        })
        .await
    }
}

/// One audio packet's bytes: the kind tag, the profile, tab, sequence, sample
/// rate and channels, then the interleaved little-endian i16 samples.
fn audio_bytes(audio: &BrowserAudio) -> Vec<u8> {
    // One tag byte plus the viewer's 13-byte header.
    let mut bytes = Vec::with_capacity(14 + audio.samples.len() * 2);
    bytes.push(Kind::BrowserAudio.tag());
    bytes.extend_from_slice(&audio.profile.to_le_bytes());
    bytes.extend_from_slice(&audio.tab.to_le_bytes());
    bytes.extend_from_slice(&audio.seq.to_le_bytes());
    bytes.extend_from_slice(&audio.sample_rate.to_le_bytes());
    bytes.push(audio.channels);
    for sample in &audio.samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

/// Why a binary message was not a frame this server takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A message with no tag at all.
    ShortHeader,
    /// A kind tag this server does not know.
    UnknownKind(u8),
    /// A dictation payload past [`MAX_PAYLOAD_BYTES`].
    Oversized(usize),
    /// A dictation payload that is not a whole number of samples.
    UnevenPayload(usize),
    /// A browser frame, which only the server sends.
    ServerOnly(u8),
}

impl Refusal {
    /// One line for the debug record.
    pub fn reason(self) -> String {
        match self {
            Self::ShortHeader => "a binary message with no kind byte".to_owned(),
            Self::UnknownKind(tag) => format!("kind tag {tag} is not one this server knows"),
            Self::Oversized(bytes) => format!("{bytes} bytes of payload is past the frame cap"),
            Self::UnevenPayload(bytes) => {
                format!("{bytes} bytes is not a whole number of samples")
            }
            Self::ServerOnly(tag) => {
                format!("kind tag {tag} is a frame the server sends, not one a client does")
            }
        }
    }
}

/// One decoded frame.
#[derive(Debug, PartialEq)]
pub enum Frame {
    /// A dictation frame: the samples its bytes carry.
    Audio(Vec<f32>),
}

/// Decode one binary message a client sent.
pub fn decode(bytes: &[u8]) -> Result<Frame, Refusal> {
    let Some((&tag, payload)) = bytes.split_first() else {
        return Err(Refusal::ShortHeader);
    };
    let Some(kind) = Kind::from_tag(tag) else {
        return Err(Refusal::UnknownKind(tag));
    };
    match kind {
        Kind::Dictation => {
            if payload.len() > MAX_PAYLOAD_BYTES {
                return Err(Refusal::Oversized(payload.len()));
            }
            if !payload.len().is_multiple_of(2) {
                return Err(Refusal::UnevenPayload(payload.len()));
            }
            let samples = payload
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32768.0)
                .collect();
            Ok(Frame::Audio(samples))
        }
        Kind::BrowserFrame | Kind::BrowserAudio => Err(Refusal::ServerOnly(tag)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dictation payload built from i16 samples, the wire's own vocabulary.
    fn payload(samples: &[i16]) -> Vec<u8> {
        let mut bytes = vec![Kind::Dictation.tag()];
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    /// The dictation wire shape: little-endian i16, scaled by 2^15. The two
    /// ends pin byte order and scaling together - a big-endian read of the
    /// same bytes trades 0.5 for 128.0, and a /32767 scale moves the
    /// full-scale sample.
    #[test]
    fn a_dictation_frame_decodes_to_the_samples_the_bytes_carry() {
        let frame = decode(&payload(&[0, 32767, -32768, 16384])).expect("a well-formed frame");
        assert_eq!(
            frame,
            Frame::Audio(vec![0.0, 32767.0 / 32768.0, -1.0, 0.5]),
            "little-endian i16 over 2^15, in order"
        );
    }

    /// **The audio wire carries the tab.** The viewer keys every packet to
    /// its tab, so a header without it shifts every field after the profile
    /// and every viewer misreads the packet.
    #[test]
    fn an_audio_packet_carries_the_profile_tab_and_samples() {
        let audio = BrowserAudio {
            profile: 3,
            tab: 7,
            seq: 9,
            sample_rate: 48_000,
            channels: 2,
            samples: vec![1, -2],
        };
        let mut expected = vec![Kind::BrowserAudio.tag()];
        expected.extend_from_slice(&3_u16.to_le_bytes());
        expected.extend_from_slice(&7_u16.to_le_bytes());
        expected.extend_from_slice(&9_u32.to_le_bytes());
        expected.extend_from_slice(&48_000_u32.to_le_bytes());
        expected.push(2);
        expected.extend_from_slice(&1_i16.to_le_bytes());
        expected.extend_from_slice(&(-2_i16).to_le_bytes());
        let bytes = audio_bytes(&audio);
        assert_eq!(
            bytes, expected,
            "the tag, profile, tab, sequence, sample rate, channels, then samples"
        );
        assert_eq!(
            bytes.len() - audio.samples.len() * 2,
            1 + 13,
            "one tag byte plus the viewer's 13-byte SOUND_HEADER_LEN"
        );
    }

    /// **A browser frame is the server's to send, never a client's.** The
    /// host is the only encoder, so a client sending one is refused rather
    /// than read as something the server could have produced.
    #[test]
    fn a_browser_frame_from_a_client_is_refused() {
        assert_eq!(decode(&[Kind::BrowserFrame.tag(), 1, 2, 3]), Err(Refusal::ServerOnly(1)));
        assert_eq!(Kind::BrowserFrame.tag(), 1, "the tag the client decodes as a frame");
        assert_eq!(Kind::Dictation.tag(), 0, "and the audio stream keeps its own");
    }

    #[test]
    fn a_message_with_no_header_is_refused() {
        assert_eq!(decode(&[]), Err(Refusal::ShortHeader), "an empty message is not a frame");
    }

    /// A tag this server does not know is refused naming it, so a client
    /// speaking a newer kind is legible in the log rather than silent.
    #[test]
    fn an_unknown_kind_tag_is_refused_naming_it() {
        assert_eq!(decode(&[7, 0, 0]), Err(Refusal::UnknownKind(7)));
    }

    /// The cap is checked before the payload is walked, so a hostile length
    /// cannot make the server allocate or scan on its say-so.
    #[test]
    fn an_oversized_payload_is_refused_with_its_length() {
        let mut bytes = vec![Kind::Dictation.tag()];
        bytes.extend(std::iter::repeat_n(0u8, MAX_PAYLOAD_BYTES + 2));
        assert_eq!(decode(&bytes), Err(Refusal::Oversized(MAX_PAYLOAD_BYTES + 2)));
    }

    /// Half a sample is a malformed frame, not a sample to drop quietly.
    #[test]
    fn an_uneven_payload_is_refused_rather_than_truncated() {
        assert_eq!(decode(&[Kind::Dictation.tag(), 1, 2, 3]), Err(Refusal::UnevenPayload(3)));
    }

    /// The wire's rate is the one the models read. Not imported, because
    /// the transport states its own contract - so a drift between the two
    /// has to fail here rather than ride a shared constant.
    #[test]
    fn the_wire_rate_is_the_rate_the_models_read() {
        assert_eq!(
            SAMPLE_RATE,
            forge_dictate::SAMPLE_RATE,
            "a frame's samples are what the dictation models consume, at their rate"
        );
    }

    /// **A frame crosses the seam whole, and the seam needs no socket.**
    /// The connection's only coupling to delivery is this trait, so another
    /// backend implements one method; the frame it receives is the host's
    /// own bytes, untouched.
    struct Memory {
        frames: Vec<BrowserFrame>,
        audio: Vec<BrowserAudio>,
    }

    #[async_trait::async_trait]
    impl FrameTransport for Memory {
        async fn deliver(&mut self, frame: &BrowserFrame) -> anyhow::Result<()> {
            self.frames.push(frame.clone());
            Ok(())
        }

        async fn deliver_audio(&mut self, audio: &BrowserAudio) -> anyhow::Result<()> {
            self.audio.push(audio.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_frame_crosses_the_transport_seam_whole() {
        let mut transport = Memory { frames: Vec::new(), audio: Vec::new() };
        let frame = BrowserFrame { profile: 5, tab: 2, seq: 3, encoded: vec![9, 8, 7] };
        transport.deliver(&frame).await.expect("a memory transport never fails");
        assert_eq!(
            transport.frames,
            vec![frame],
            "the transport receives the whole frame: the profile id, sequence and the host's own bytes"
        );
    }

    /// Every refusal has its own sentence, so a log line says what the
    /// frame was rather than that something was wrong.
    #[test]
    fn every_refusal_names_what_it_refused() {
        for refusal in [
            Refusal::ShortHeader,
            Refusal::UnknownKind(9),
            Refusal::ServerOnly(1),
            Refusal::Oversized(MAX_PAYLOAD_BYTES + 1),
            Refusal::UnevenPayload(3),
        ] {
            assert!(!refusal.reason().is_empty(), "{refusal:?} has no line");
        }
    }
}

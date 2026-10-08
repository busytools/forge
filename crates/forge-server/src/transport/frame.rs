//! The binary frames a client streams in.
//!
//! One WebSocket binary message per frame: a one-byte kind tag, then the
//! payload. Two kinds, and what tells them apart is the tag alone:
//!
//! - [`Kind::Dictation`] - mono PCM at [`SAMPLE_RATE`], the rate the dictation
//!   models read.
//! - [`Kind::BrowserImage`] - one browser answer's image bytes, under the
//!   answer's own id. The mime type crossed on the answer's own message, so
//!   this frame is the id and the bytes and nothing else.
//!
//! **The kind tag is why the shape can grow**: a third kind is a variant here
//! plus a line in the recorded contract, not a new message type.
//!
//! **The server only decodes.** Encoding lives with the client that captures
//! or answers, and a message this server cannot decode is dropped with a
//! record rather than answered - a take's audio has no reply channel, and an
//! image frame carries its own id.

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

/// One image frame's header: the kind tag, then the answer's id as a
/// big-endian u64.
pub const IMAGE_HEADER_BYTES: usize = 1 + 8;

/// The largest image payload a frame may carry. A screenshot of a full page
/// is a few megabytes; the cap is a bound on what a client can make this
/// server hold, not a budget a real capture comes close to. The socket's own
/// frame limit is raised to this plus a header at the upgrade.
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// What a binary frame is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Mono 16-bit little-endian PCM: what a browser's own i16 conversion
    /// produces, with no encoder on either side.
    Dictation,
    /// One browser answer's image bytes.
    BrowserImage,
}

impl Kind {
    /// The tag byte this kind crosses as.
    pub const fn tag(self) -> u8 {
        match self {
            Self::Dictation => 0,
            Self::BrowserImage => 1,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Dictation),
            1 => Some(Self::BrowserImage),
            _ => None,
        }
    }
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
    /// An image frame shorter than its own header.
    ShortImageHeader(usize),
    /// An image payload past [`MAX_IMAGE_BYTES`].
    OversizedImage(usize),
}

impl Refusal {
    /// Whether this refusal could have been an IMAGE frame's.
    ///
    /// A header too short to say, a kind nobody knows, a truncated image
    /// header and an oversized image are all things an image frame can be.
    /// A dictation refusal - an oversized or uneven payload - belongs to the
    /// audio stream, and failing asks that are waiting for images on it would
    /// hand a session an image-error sentence over a microphone frame, which
    /// is a reason that names the wrong thing.
    pub fn concerns_images(self) -> bool {
        match self {
            Self::ShortHeader
            | Self::UnknownKind(_)
            | Self::ShortImageHeader(_)
            | Self::OversizedImage(_) => true,
            Self::Oversized(_) | Self::UnevenPayload(_) => false,
        }
    }

    /// One line for the debug record.
    pub fn reason(self) -> String {
        match self {
            Self::ShortHeader => "a binary message with no kind byte".to_owned(),
            Self::UnknownKind(tag) => format!("kind tag {tag} is not one this server knows"),
            Self::Oversized(bytes) => format!("{bytes} bytes of payload is past the frame cap"),
            Self::UnevenPayload(bytes) => {
                format!("{bytes} bytes is not a whole number of samples")
            }
            Self::ShortImageHeader(bytes) => {
                format!("{bytes} bytes is shorter than an image frame's own header")
            }
            Self::OversizedImage(bytes) => {
                format!("{bytes} bytes of image is past the image frame cap")
            }
        }
    }
}

/// One decoded frame.
#[derive(Debug, PartialEq)]
pub enum Frame {
    /// A dictation frame: the samples its bytes carry.
    Audio(Vec<f32>),
    /// One browser answer's image bytes, under the answer's id.
    Image { id: u64, bytes: Vec<u8> },
}

/// Decode one binary message.
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
        Kind::BrowserImage => {
            if bytes.len() < IMAGE_HEADER_BYTES {
                return Err(Refusal::ShortImageHeader(bytes.len()));
            }
            // The header's own length is checked first, so the id's eight
            // bytes are there to copy.
            let mut id = [0_u8; 8];
            id.copy_from_slice(&bytes[1..IMAGE_HEADER_BYTES]);
            let id = u64::from_be_bytes(id);
            let payload = &bytes[IMAGE_HEADER_BYTES..];
            if payload.len() > MAX_IMAGE_BYTES {
                return Err(Refusal::OversizedImage(payload.len()));
            }
            Ok(Frame::Image { id, bytes: payload.to_vec() })
        }
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

    /// An image frame built at the wire's shape.
    fn image_frame(id: u64, bytes: &[u8]) -> Vec<u8> {
        let mut frame = vec![Kind::BrowserImage.tag()];
        frame.extend_from_slice(&id.to_be_bytes());
        frame.extend_from_slice(bytes);
        frame
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

    /// An image frame is its id and its bytes: the id is what pairs it with
    /// the answer that declared the image, and the bytes are the bytes.
    #[test]
    fn a_browser_image_frame_decodes_to_its_id_and_bytes() {
        let frame = decode(&image_frame(0x0102_0304_0506_0708, &[0x89, 0x50, 0x4e, 0x47]))
            .expect("a well-formed image frame");
        assert_eq!(
            frame,
            Frame::Image { id: 0x0102_0304_0506_0708, bytes: vec![0x89, 0x50, 0x4e, 0x47] },
            "the id big-endian, then the bytes, and nothing else",
        );

        // The mime crossed on the answer, so an empty image is a shape the
        // frame can carry rather than a malformed one.
        assert_eq!(decode(&image_frame(9, &[])), Ok(Frame::Image { id: 9, bytes: Vec::new() }));
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

    /// An image frame cut inside its own header names the length it had: the
    /// id cannot be read, so there is no answer to give the bytes to.
    ///
    /// A message holding the whole header and no payload is NOT this case: it
    /// names an answer, and an image of nothing is a shape the frame may
    /// carry.
    #[test]
    fn an_image_frame_short_of_its_header_is_refused_with_its_length() {
        for body in 0..IMAGE_HEADER_BYTES - 1 {
            let mut bytes = vec![Kind::BrowserImage.tag()];
            bytes.extend(std::iter::repeat_n(0u8, body));
            assert_eq!(
                decode(&bytes),
                Err(Refusal::ShortImageHeader(bytes.len())),
                "a {}-byte image frame names no answer",
                bytes.len(),
            );
        }
    }

    /// The same bound as dictation, at the image's own size: what a client
    /// can make the server hold is capped.
    #[test]
    fn an_oversized_image_is_refused_with_its_length() {
        let mut bytes = image_frame(1, &[]);
        bytes.extend(std::iter::repeat_n(0u8, MAX_IMAGE_BYTES + 1));
        assert_eq!(decode(&bytes), Err(Refusal::OversizedImage(MAX_IMAGE_BYTES + 1)));
    }

    /// **The cap is a ceiling, not a shape the largest real image is under.**
    /// A payload of exactly the cap is a frame this server takes, which is
    /// what makes the refusal above a boundary rather than an off-by-one.
    #[test]
    fn an_image_payload_of_exactly_the_cap_is_taken() {
        let mut frame = image_frame(3, &[]);
        frame.extend(std::iter::repeat_n(0u8, MAX_IMAGE_BYTES));
        assert_eq!(
            decode(&frame),
            Ok(Frame::Image { id: 3, bytes: vec![0; MAX_IMAGE_BYTES] }),
            "exactly the cap crosses; one byte more is refused",
        );
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

    /// **Who a refusal can fail.** Only the kinds an image frame can be may
    /// fail asks that are waiting for images; a dictation refusal belongs to
    /// the audio stream, and failing image waits on it would hand a session
    /// an image-error sentence over a microphone frame.
    #[test]
    fn only_image_possible_refusals_concern_image_waits() {
        for refusal in [
            Refusal::ShortHeader,
            Refusal::UnknownKind(9),
            Refusal::ShortImageHeader(3),
            Refusal::OversizedImage(MAX_IMAGE_BYTES + 1),
        ] {
            assert!(refusal.concerns_images(), "{refusal:?} could be an image frame's");
        }
        for refusal in [Refusal::Oversized(MAX_PAYLOAD_BYTES + 1), Refusal::UnevenPayload(3)] {
            assert!(!refusal.concerns_images(), "{refusal:?} is the dictation stream's own");
        }
    }
}

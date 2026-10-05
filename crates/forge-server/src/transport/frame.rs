//! The binary frame a client streams dictation audio in.
//!
//! One WebSocket binary message per frame: a one-byte codec tag, then the
//! payload. Mono PCM at [`SAMPLE_RATE`], the rate the dictation models
//! read. The codec tag is why the shape can grow: a second codec is a
//! variant here plus a line in the recorded contract, not a new message
//! type.
//!
//! **The server only decodes.** Encoding lives with the client that
//! captures, and a message this server cannot decode is dropped with a
//! record rather than answered, because a take's audio has no reply
//! channel.

/// The rate every frame's samples are at. Pinned against the dictation
/// crate's own constant by a test rather than imported: the wire's rate is
/// this crate's rule, and the transport does not need the audio crate to
/// state it.
pub const SAMPLE_RATE: u32 = 16_000;

/// One frame's header: the codec tag and nothing else.
pub const HEADER_BYTES: usize = 1;

/// The largest payload a frame may carry. Twenty-five times the 20 ms
/// frame a client sends, so a malformed or hostile length cannot make the
/// server hold a large allocation per message.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

/// The codec a frame's payload is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Mono 16-bit little-endian PCM: what a browser's own i16 conversion
    /// produces, with no encoder on either side.
    PcmI16,
}

impl Codec {
    /// The tag byte this codec crosses as.
    pub const fn tag(self) -> u8 {
        match self {
            Self::PcmI16 => 0,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::PcmI16),
            _ => None,
        }
    }
}

/// Why a binary message was not a frame this server takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A message with no header at all.
    ShortHeader,
    /// A codec tag this server does not know.
    UnknownCodec(u8),
    /// A payload past [`MAX_PAYLOAD_BYTES`].
    Oversized(usize),
    /// A payload that is not a whole number of samples.
    UnevenPayload(usize),
}

impl Refusal {
    /// One line for the debug record.
    pub fn reason(self) -> String {
        match self {
            Self::ShortHeader => "a binary message with no codec byte".to_owned(),
            Self::UnknownCodec(tag) => format!("codec tag {tag} is not one this server knows"),
            Self::Oversized(bytes) => format!("{bytes} bytes of payload is past the frame cap"),
            Self::UnevenPayload(bytes) => {
                format!("{bytes} bytes is not a whole number of samples")
            }
        }
    }
}

/// One decoded frame.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub codec: Codec,
    pub samples: Vec<f32>,
}

/// Decode one binary message.
pub fn decode(bytes: &[u8]) -> Result<Frame, Refusal> {
    let Some((&tag, payload)) = bytes.split_first() else {
        return Err(Refusal::ShortHeader);
    };
    let Some(codec) = Codec::from_tag(tag) else {
        return Err(Refusal::UnknownCodec(tag));
    };
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
    Ok(Frame { codec, samples })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload built from i16 samples, the wire's own vocabulary.
    fn payload(samples: &[i16]) -> Vec<u8> {
        let mut bytes = vec![Codec::PcmI16.tag()];
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    /// The wire shape: little-endian i16, scaled by 2^15. The two ends pin
    /// byte order and scaling together - a big-endian read of the same
    /// bytes trades 0.5 for 128.0, and a /32767 scale moves the full-scale
    /// sample.
    #[test]
    fn a_frame_decodes_to_the_samples_the_bytes_carry() {
        let frame = decode(&payload(&[0, 32767, -32768, 16384])).expect("a well-formed frame");
        assert_eq!(frame.codec, Codec::PcmI16);
        assert_eq!(
            frame.samples,
            vec![0.0, 32767.0 / 32768.0, -1.0, 0.5],
            "little-endian i16 over 2^15, in order"
        );
    }

    #[test]
    fn a_message_with_no_header_is_refused() {
        assert_eq!(decode(&[]), Err(Refusal::ShortHeader), "an empty message is not a frame");
    }

    /// A tag this server does not know is refused naming it, so a client
    /// speaking a newer codec is legible in the log rather than silent.
    #[test]
    fn an_unknown_codec_tag_is_refused_naming_it() {
        assert_eq!(decode(&[7, 0, 0]), Err(Refusal::UnknownCodec(7)));
    }

    /// The cap is checked before the payload is walked, so a hostile
    /// length cannot make the server allocate or scan on its say-so.
    #[test]
    fn an_oversized_payload_is_refused_with_its_length() {
        let mut bytes = vec![Codec::PcmI16.tag()];
        bytes.extend(std::iter::repeat_n(0u8, MAX_PAYLOAD_BYTES + 2));
        assert_eq!(decode(&bytes), Err(Refusal::Oversized(MAX_PAYLOAD_BYTES + 2)));
    }

    /// Half a sample is a malformed frame, not a sample to drop quietly.
    #[test]
    fn an_uneven_payload_is_refused_rather_than_truncated() {
        assert_eq!(decode(&[Codec::PcmI16.tag(), 1, 2, 3]), Err(Refusal::UnevenPayload(3)));
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
}

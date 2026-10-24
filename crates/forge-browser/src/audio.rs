//! The page's audio, on its way to the person driving it.
//!
//! **The sound must cross the wire.** The host is a windowless process on the
//! server, so Chromium would play a page's audio through the server's own
//! device - a person at a client hears nothing. CEF hands the mixed stream
//! here as planar float samples; they become interleaved 16-bit PCM on the
//! host's channel and the controlling client plays them.

#![allow(
    clippy::transmute_ptr_to_ptr,
    reason = "CEF's wrap_* macros transmute the wrapper reference to the base object"
)]
#![allow(unsafe_code, reason = "CEF hands raw sample planes for the length of the callback")]

use std::sync::{Arc, Mutex, PoisonError};

use cef::rc::Rc as _;
use cef::{AudioHandler, AudioParameters, ImplAudioHandler, WrapAudioHandler};

use crate::channel::Audio;

/// Where one audio packet goes: the host's channel writer.
pub type AudioSink = Arc<dyn Fn(Audio) + Send + Sync>;

/// What the stream said when it started.
#[derive(Clone, Copy)]
struct Stream {
    sample_rate: u32,
    channels: u8,
    seq: u32,
}

impl Default for Stream {
    fn default() -> Self {
        Self { sample_rate: 48_000, channels: 2, seq: 0 }
    }
}

cef::wrap_audio_handler! {
    pub struct PageAudioHandler {
        profile: u16,
        sink: AudioSink,
        stream: Arc<Mutex<Stream>>,
    }

    impl AudioHandler {
        /// **`1` proceeds with the stream; the Rust trait's default cancels
        /// it.** CEF reads this as "yes, capture", and the binding's default
        /// is `0`, so a handler that omits this is never called at all.
        fn audio_parameters(
            &self,
            _browser: Option<&mut cef::Browser>,
            _params: Option<&mut AudioParameters>,
        ) -> i32 {
            1
        }

        fn on_audio_stream_started(
            &self,
            _browser: Option<&mut cef::Browser>,
            params: Option<&AudioParameters>,
            channels: i32,
        ) {
            let mut stream = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
            stream.sample_rate =
                params.map_or(48_000, |params| u32::try_from(params.sample_rate).unwrap_or(48_000));
            stream.channels =
                u8::try_from(channels.clamp(1, 8)).unwrap_or(2);
            stream.seq = 0;
        }

        fn on_audio_stream_packet(
            &self,
            _browser: Option<&mut cef::Browser>,
            data: *mut *const f32,
            frames: i32,
            _pts: i64,
        ) {
            let (sample_rate, channels, seq) = {
                let mut stream = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
                stream.seq = stream.seq.wrapping_add(1);
                (stream.sample_rate, stream.channels, stream.seq)
            };
            let Ok(frames) = usize::try_from(frames) else { return };
            let channels = usize::from(channels);
            if data.is_null() || frames == 0 || channels == 0 {
                return;
            }
            // SAFETY: CEF hands `channels` planes, each `frames` long, for the
            // whole of this callback.
            let Some(samples) = (unsafe { interleave(data, frames, channels) }) else { return };
            (self.sink)(Audio { profile: self.profile, seq, sample_rate, channels: u8::try_from(channels).unwrap_or(2), samples });
        }

        fn on_audio_stream_stopped(&self, _browser: Option<&mut cef::Browser>) {
            // **An empty packet is the stream's end**, so the client drains
            // what it has and stops its player rather than waiting forever.
            let (sample_rate, channels, seq) = {
                let mut stream = self.stream.lock().unwrap_or_else(PoisonError::into_inner);
                stream.seq = stream.seq.wrapping_add(1);
                (stream.sample_rate, stream.channels, stream.seq)
            };
            (self.sink)(Audio { profile: self.profile, seq, sample_rate, channels, samples: Vec::new() });
        }
    }
}

/// CEF's planar floats as the interleaved PCM the wire carries.
///
/// # Safety
///
/// `data` names `channels` planes, each `frames` samples long, for the whole
/// of the callback it arrived in.
unsafe fn interleave(data: *mut *const f32, frames: usize, channels: usize) -> Option<Vec<i16>> {
    let mut samples = Vec::with_capacity(frames * channels);
    for frame in 0..frames {
        for channel in 0..channels {
            // SAFETY: the planes are CEF's, and the loop bounds are the ones
            // it gave.
            let plane = unsafe { *data.add(channel) };
            if plane.is_null() {
                return None;
            }
            // SAFETY: the plane holds `frames` samples, and `frame` is inside
            // that.
            let value = unsafe { *plane.add(frame) };
            samples.push(sample(value));
        }
    }
    Some(samples)
}

/// One float sample as 16-bit PCM, clamped to the range an i16 holds.
fn sample(value: f32) -> i16 {
    let scaled = f64::from(value.clamp(-1.0, 1.0)) * 32767.0;
    #[expect(clippy::cast_possible_truncation, reason = "clamped to the i16 range and rounded")]
    let sample = scaled.round() as i16;
    sample
}

/// The handler the host's client hands CEF.
pub fn handler(profile: u16, sink: AudioSink) -> AudioHandler {
    PageAudioHandler::new(profile, sink, Arc::new(Mutex::new(Stream::default())))
}

#[cfg(test)]
mod tests {
    use super::sample;

    /// **A sample never wraps.** A page's float can arrive just outside the
    /// nominal range, and a wrapped sample is a click in the person's ear.
    #[test]
    fn a_sample_is_clamped_to_its_range() {
        assert_eq!(sample(0.0), 0);
        assert_eq!(sample(1.0), i16::MAX);
        assert_eq!(sample(-1.0), -i16::MAX);
        assert_eq!(sample(2.0), i16::MAX, "above the range clamps");
        assert_eq!(sample(-2.0), -i16::MAX, "and below it too");
    }
}

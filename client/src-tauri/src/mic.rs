//! The desktop's own microphone.
//!
//! The page does not capture here. This half opens the device through
//! `forge-capture`, frames what it delivers into the 20 ms mono i16 frames
//! the wire carries, and hands them to the page, which streams them down the
//! take's own socket - so the server and the take feed see exactly what the
//! browser's capture sends, and a native open skips the browser's audio
//! pipeline and its permission round trip on the way.
//!
//! The device is opened when a take starts and released when it ends: the
//! microphone is live only during a take, and nothing is held between them.

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

/// One live capture, and the frame the next callback fills.
struct Take {
    capture: forge_capture::Capture,
    /// Samples not yet a whole frame, shared with the stream callback.
    held: Arc<Mutex<Vec<f32>>>,
}

/// The microphone, as this half holds it: at most one take at a time.
#[derive(Default)]
pub struct Mic {
    live: Mutex<Option<Take>>,
}

impl Mic {
    /// Open the picked device - or the system default when nothing is
    /// picked - and start handing `bridge`'s page the frames.
    ///
    /// The device is opened before this returns, so a device that will not
    /// open is an error here rather than a take that records silence. The
    /// input it opened comes back with it, which is the only place a plain
    /// take names the system default: the picker's list carries no mark.
    pub fn start(
        &self,
        bridge: &Arc<Bridge>,
        device: Option<String>,
    ) -> Result<forge_capture::Device, String> {
        let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        if live.is_some() {
            return Err("the microphone is already recording".to_owned());
        }
        let frames = Arc::clone(bridge);
        let held: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::with_capacity(FRAME_SAMPLES)));
        let counting = Arc::clone(&held);
        // Frames are built on the stream's own callback thread: one 320-sample
        // copy and its bytes per 20 ms, the same small allocation the browser's
        // worklet makes per chunk on its audio thread.
        let capture = forge_capture::open(device.as_deref(), move |block| {
            let mut held = counting.lock().unwrap_or_else(PoisonError::into_inner);
            held.extend_from_slice(block.samples);
            while held.len() >= FRAME_SAMPLES {
                let frame: Vec<f32> = held.drain(..FRAME_SAMPLES).collect();
                frames.mic_frame(encode_frame(&frame));
            }
        })
        .map_err(|error| error.to_string())?;
        let opened = capture.device().clone();
        *live = Some(Take { capture, held });
        Ok(opened)
    }

    /// Let go at once. An abandoned take's audio is thrown away anyway, so
    /// nothing waits for the pipeline to land.
    pub fn stop(&self) {
        if let Some(take) = self.live.lock().unwrap_or_else(PoisonError::into_inner).take() {
            take.capture.stop();
        }
    }

    /// Hold the capture open for the drain window, then hand over the last
    /// partial frame and release the device.
    ///
    /// The caller sends its stop only after this returns: the stop is what
    /// finalises the take on the server, so a frame that follows it has no
    /// take to land in.
    pub async fn drain(&self, bridge: &Arc<Bridge>) {
        tokio::time::sleep(Duration::from_millis(DRAIN_MS)).await;
        let Some(take) = self.live.lock().unwrap_or_else(PoisonError::into_inner).take() else {
            return;
        };
        // The stream goes first: a callback landing between the flush and
        // the stop would otherwise fill the buffer this just took.
        take.capture.stop();
        let held = std::mem::take(&mut *take.held.lock().unwrap_or_else(PoisonError::into_inner));
        if !held.is_empty() {
            bridge.mic_frame(encode_frame(&held));
        }
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
}

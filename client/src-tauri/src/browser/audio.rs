//! The page's sound, played by the client's own audio device.
//!
//! **A server-side browser has no ears.** Chromium would play a page's audio
//! through the server's device; the person is at this client, so the packets
//! arrive here and an `AVAudioEngine` plays them on this machine. The engine
//! is rebuilt when the page's format changes and keeps running between
//! packets, so a page that pauses does not click on every resume.

#![allow(unsafe_code, reason = "AVAudioEngine is an Objective-C object graph")]

use std::sync::Mutex;

use objc2::AnyThread as _;
use objc2::rc::Retained;
use objc2_avf_audio::{AVAudioEngine, AVAudioFormat, AVAudioPCMBuffer, AVAudioPlayerNode};

use crate::browser::Sound;

/// One playing engine, for the format the page's stream is in.
struct Player {
    /// Kept for the engine's life; dropping it stops the device.
    _engine: Retained<AVAudioEngine>,
    node: Retained<AVAudioPlayerNode>,
    format: Retained<AVAudioFormat>,
    rate: u32,
    channels: u8,
}

// SAFETY: every use is on the main thread (the caller hops there), and the
// mutex only carries the player between calls.
unsafe impl Send for Player {}

static PLAYER: Mutex<Option<Player>> = Mutex::new(None);

/// Play one packet. **Called on the main thread**, where AppKit and
/// AVFoundation objects are the process's own.
pub fn play(sound: &Sound) {
    // **An empty packet is the stream stopping.** The engine is left running:
    // draining what is scheduled and stopping it on a timer would buy a click
    // and lose the next page's first packet.
    if sound.samples.is_empty() {
        return;
    }
    let mut slot = PLAYER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let stale = slot
        .as_ref()
        .is_none_or(|player| player.rate != sound.sample_rate || player.channels != sound.channels);
    if stale {
        *slot = None;
        if let Some(player) = start(sound.sample_rate, sound.channels) {
            *slot = Some(player);
        }
    }
    if let Some(player) = slot.as_ref() {
        player.push(sound);
    }
}

/// One engine and player at the stream's format, or `None` when the device
/// refuses - a silent page rather than a panicked one.
fn start(rate: u32, channels: u8) -> Option<Player> {
    // SAFETY: the objects are made, wired and started on the main thread,
    // and the player owns them afterwards.
    unsafe {
        let format = AVAudioFormat::initStandardFormatWithSampleRate_channels(
            AVAudioFormat::alloc(),
            f64::from(rate),
            u32::from(channels),
        )?;
        let engine = AVAudioEngine::new();
        let node = AVAudioPlayerNode::new();
        engine.attachNode(&node);
        engine.connect_to_format(&node, &engine.mainMixerNode(), Some(&format));
        if engine.startAndReturnError().is_err() {
            return None;
        }
        node.play();
        Some(Player { _engine: engine, node, format, rate, channels })
    }
}

impl Player {
    /// Schedule one packet's samples: interleaved i16 becomes the format's
    /// planar float, which is what the mixer takes.
    fn push(&self, sound: &Sound) {
        let channels = usize::from(sound.channels);
        if channels == 0 {
            return;
        }
        let frames = sound.samples.len() / channels;
        let Ok(frames) = u32::try_from(frames) else { return };
        if frames == 0 {
            return;
        }
        // SAFETY: the buffer is made at the player's own format, filled to
        // its frame length, and scheduled on the main thread; AVAudioPlayerNode
        // retains it until it has played.
        unsafe {
            let Some(buffer) = AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                AVAudioPCMBuffer::alloc(),
                &self.format,
                frames,
            ) else {
                return;
            };
            buffer.setFrameLength(frames);
            let planes = buffer.floatChannelData();
            if planes.is_null() {
                return;
            }
            for channel in 0..channels {
                let plane = (*planes.add(channel)).as_ptr();
                for frame in 0..usize::try_from(frames).unwrap_or(0) {
                    let sample = sound.samples[frame * channels + channel];
                    *plane.add(frame) = f32::from(sample) / 32767.0;
                }
            }
            self.node.scheduleBuffer_completionHandler(&buffer, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::browser::{Sound, split_sound};

    /// **The wire is parsed before anything touches the audio device.** A
    /// short packet or one that does not hold whole samples is refused at
    /// the door rather than handed to a frame length the player would
    /// misread.
    #[test]
    fn an_audio_packet_is_split_or_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7_u16.to_le_bytes());
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.push(2);
        bytes.extend_from_slice(&100_i16.to_le_bytes());
        bytes.extend_from_slice(&(-100_i16).to_le_bytes());
        let sound = split_sound(&bytes).expect("a whole packet");
        assert_eq!(
            sound,
            Sound {
                profile: 7,
                seq: 3,
                sample_rate: 48_000,
                channels: 2,
                samples: vec![100, -100]
            }
        );

        assert!(split_sound(&bytes[..10]).is_none(), "a short header is refused");
        let odd = &bytes[..bytes.len() - 1];
        assert!(split_sound(odd).is_none(), "half a sample is refused");
    }
}

//! Microphone capture.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::Error;
use crate::audio::SAMPLE_RATE;

/// What the recording thread and the handle share.
pub(crate) struct Recording {
    samples: Mutex<Vec<f32>>,
    /// Loudest absolute sample so far, as `f32` bits. Atomic so a caller
    /// drawing a level meter never blocks on the audio callback.
    peak_bits: AtomicU32,
    stop: AtomicBool,
    /// Set when the recorder stopped itself at the cap rather than
    /// because it was asked to.
    truncated: AtomicBool,
}

impl Recording {
    /// `capacity` is the cap in samples. Reserved up front because the
    /// audio callback appends: growing a multi-megabyte buffer means a
    /// memcpy inside a realtime callback, which is a dropout.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            samples: Mutex::new(Vec::with_capacity(capacity)),
            peak_bits: AtomicU32::new(0.0f32.to_bits()),
            stop: AtomicBool::new(false),
            truncated: AtomicBool::new(false),
        }
    }

    /// Fold one callback's worth of audio in, downmixed to mono and
    /// keeping the running peak.
    ///
    /// MUST NOT ALLOCATE. This runs on the realtime audio thread, where a
    /// `malloc` can miss the deadline and drop a buffer, so the downmix
    /// happens while extending the preallocated buffer rather than into
    /// an intermediate.
    ///
    /// Channels are averaged rather than one being chosen: discarding a
    /// capsule silently halves the signal on hardware where the speaker
    /// sits nearer one of them.
    fn push(&self, block: &[f32], channels: usize, limit: usize) {
        debug_assert!(
            channels > 0 && block.len().is_multiple_of(channels),
            "interleaved audio must divide evenly into frames"
        );

        // Peak over the RAW block, not the averages: a meter should show
        // a channel clipping even when the mean of the channels does not.
        let mut loudest = 0.0f32;
        for sample in block {
            loudest = loudest.max(sample.abs());
        }
        let mut current = self.peak_bits.load(Ordering::Relaxed);
        loop {
            if f32::from_bits(current) >= loudest {
                break;
            }
            match self.peak_bits.compare_exchange_weak(
                current,
                loudest.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => current = seen,
            }
        }

        let mut samples = self.samples.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if samples.len() >= limit {
            // The cap is reached: stop growing and ask the recorder to
            // release the device. A host that never calls `finish` gets a
            // truncated transcript rather than a microphone nobody can use.
            self.truncated.store(true, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
            return;
        }

        let room = limit - samples.len();
        let frames = block.len() / channels.max(1);
        if frames > room {
            // The block that FIRST overruns is the one that loses its
            // tail, so the flag has to be set here. Setting it only on
            // the next callback leaves a `finish` in between reporting a
            // recording that really was cut as complete.
            self.truncated.store(true, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
        }

        if channels <= 1 {
            samples.extend_from_slice(&block[..block.len().min(room)]);
        } else {
            // Channel counts are single digits; the cast cannot lose anything.
            let scale = f32::from(u16::try_from(channels).unwrap_or(u16::MAX));
            samples.extend(
                block.chunks_exact(channels).take(room).map(|f| f.iter().sum::<f32>() / scale),
            );
        }
    }

    pub(crate) fn peak_dbfs(&self) -> f32 {
        let peak = f32::from_bits(self.peak_bits.load(Ordering::Relaxed));
        if peak <= 0.0 { f32::NEG_INFINITY } else { 20.0 * peak.log10() }
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub(crate) fn take(&self) -> Vec<f32> {
        let mut samples = self.samples.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *samples)
    }

    pub(crate) fn was_truncated(&self) -> bool {
        self.truncated.load(Ordering::Relaxed)
    }
}

/// An input the host can offer a user, and record from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Stable identity, and the value a host persists. cpal documents
    /// this as the thing to store and re-resolve, which is why the
    /// selection is keyed on it rather than on `name`: names collide
    /// between two identical interfaces and change when a user renames
    /// one, and neither should silently move the recording.
    pub id: String,
    /// Human label for a picker. Not an identity.
    pub name: String,
    /// Whether the system would pick this one when asked for no
    /// particular device.
    pub is_default: bool,
}

/// Every input the host could record from.
///
/// The crate never chooses: a caller that names nothing gets the system
/// default, and one that names a device gets that device or an error.
/// Same rule [`crate::AudioSource`] already establishes for audio that
/// does not come from a microphone.
pub fn devices() -> Result<Vec<Device>, Error> {
    let host = cpal::default_host();
    let default = host.default_input_device().and_then(|d| d.id().ok());
    let found =
        host.input_devices().map_err(|error| Error::Capture { message: error.to_string() })?;

    Ok(found
        .filter_map(|device| {
            let id = device.id().ok()?;
            Some(Device {
                is_default: Some(&id) == default.as_ref(),
                id: id.to_string(),
                name: device.to_string(),
            })
        })
        .collect())
}

/// Resolve a caller's choice to a cpal device.
///
/// A named device that has gone is an error rather than a fallback:
/// someone who chose a USB interface and finds it unplugged needs to
/// know, not to be quietly recorded on the built-in microphone.
fn open_device(wanted: Option<&str>) -> Result<cpal::Device, Error> {
    let host = cpal::default_host();
    let Some(wanted) = wanted else {
        return host.default_input_device().ok_or(Error::NoInputDevice);
    };
    let found =
        host.input_devices().map_err(|error| Error::Capture { message: error.to_string() })?;
    for device in found {
        if device.id().is_ok_and(|id| id.to_string() == wanted) {
            return Ok(device);
        }
    }
    Err(Error::DeviceNotFound {
        wanted: wanted.to_owned(),
        available: devices()?
            .into_iter()
            .map(|d| format!("{} ({})", d.id, d.name))
            .collect::<Vec<_>>()
            .join(", "),
    })
}

/// The cap in samples. Integer maths so it cannot be a truncating float
/// cast.
pub(crate) fn sample_cap(max_capture: Duration) -> usize {
    usize::try_from(max_capture.as_millis().saturating_mul(u128::from(SAMPLE_RATE)) / 1000)
        .unwrap_or(usize::MAX)
}

/// Open the default input and record until asked to stop or until
/// `max_capture` elapses.
///
/// The whole stream lives on this thread: `cpal::Stream` is neither
/// `Send` nor `Sync`, so it can only be built and dropped where it was
/// created. Dropping it is what releases the device.
pub(crate) fn record(
    shared: &Arc<Recording>,
    max_capture: Duration,
    wanted: Option<&str>,
    ready: &std::sync::mpsc::Sender<Result<(), Error>>,
) {
    let device = match open_device(wanted) {
        Ok(device) => device,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let config = match input_config(&device) {
        Ok(config) => config,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let limit = sample_cap(max_capture);
    let channels = config.channels as usize;
    tracing::debug!(channels, rate = SAMPLE_RATE, "input open");
    let sink = Arc::clone(shared);
    let stream = device.build_input_stream(
        config,
        move |block: &[f32], _: &cpal::InputCallbackInfo| sink.push(block, channels, limit),
        |error| tracing::warn!(%error, "input stream error"),
        None,
    );
    let stream = match stream.and_then(|s| s.play().map(|()| s)) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready.send(Err(Error::Capture { message: error.to_string() }));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    let started = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) {
        if started.elapsed() >= max_capture {
            shared.truncated.store(true, Ordering::Relaxed);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stream);
}

/// Pick an input the models can read.
///
/// The rate must be exactly [`SAMPLE_RATE`], because changing it means
/// filtering and a naive decimation would alias speech down into the
/// band the model listens to. Channel count is different: averaging
/// channels at one rate is exact, so a stereo device is accepted and
/// downmixed. Measured on the target hardware, which offers 16, 24 and
/// 32 kHz and never fewer than two channels - so requiring mono here
/// would have meant no microphone at all.
///
/// A device offering no 16 kHz config at all is reported rather than
/// resampled; that is the case that needs a real filter, and no device
/// we have has needed it.
fn input_config(device: &cpal::Device) -> Result<cpal::StreamConfig, Error> {
    let wanted: cpal::SampleRate = SAMPLE_RATE;
    let supported = device
        .supported_input_configs()
        .map_err(|error| Error::Capture { message: error.to_string() })?;

    let mut offered = Vec::new();
    let mut best: Option<cpal::StreamConfig> = None;
    for range in supported {
        offered.push(format!(
            "{}ch {}-{}Hz {:?}",
            range.channels(),
            range.min_sample_rate(),
            range.max_sample_rate(),
            range.sample_format()
        ));
        if range.sample_format() != cpal::SampleFormat::F32 {
            continue;
        }
        if let Some(config) = range.try_with_sample_rate(wanted) {
            let config: cpal::StreamConfig = config.into();
            // Fewest channels wins: less to average, less to go wrong.
            if best.as_ref().is_none_or(|b| config.channels < b.channels) {
                best = Some(config);
            }
        }
    }
    best.ok_or(Error::UnsupportedInput { wanted: SAMPLE_RATE, offered: offered.join(", ") })
}

#[cfg(test)]
mod tests_recording {
    use super::*;

    /// One second at 16 kHz, so the cap is reached well inside the test.
    const LIMIT: usize = SAMPLE_RATE as usize;

    #[test]
    fn the_cap_truncates_rather_than_growing_without_bound() {
        let recording = Recording::new(LIMIT);
        // Three seconds of audio pushed into a one second cap.
        for _ in 0..3 {
            recording.push(&vec![0.5; LIMIT], 1, LIMIT);
        }
        assert_eq!(
            recording.take().len(),
            LIMIT,
            "audio past the cap must be dropped, not accumulated"
        );
        assert!(
            recording.was_truncated(),
            "reaching the cap must be recorded, or a short transcript looks like a short utterance"
        );
    }

    #[test]
    fn the_block_that_overruns_flags_it_without_waiting_for_the_next_one() {
        let recording = Recording::new(LIMIT);
        // A single block that does not fit. Nothing follows it, which is
        // the point: a `finish` landing here must not see the recording
        // as complete when its tail was dropped.
        recording.push(&vec![0.5; LIMIT + 1], 1, LIMIT);
        assert!(
            recording.was_truncated(),
            "the block losing its tail must flag it, or a finish between callbacks reports a cut recording as whole"
        );
        assert!(
            recording.stop.load(Ordering::Relaxed),
            "a capture nobody stopped must free the microphone itself, not hold it open"
        );
    }

    #[test]
    fn enumeration_names_at_most_one_default() {
        // No audio hardware in CI, so an empty list is a valid answer;
        // what must never happen is two devices both claiming default,
        // which would make a host's picker ambiguous.
        let Ok(found) = devices() else { return };
        let defaults = found.iter().filter(|d| d.is_default).count();
        assert!(defaults <= 1, "at most one input can be the system default, found {defaults}");
        for device in &found {
            assert!(
                !device.id.is_empty(),
                "a device with no id cannot be persisted or re-resolved"
            );
        }
    }

    #[test]
    fn stereo_is_averaged_rather_than_half_of_it_discarded() {
        let recording = Recording::new(LIMIT);
        // Two frames, left and right deliberately different. Discarding
        // either channel gives 1.0/1.0 or 0.0/0.0; averaging gives 0.5.
        recording.push(&[1.0, 0.0, 1.0, 0.0], 2, LIMIT);
        assert_eq!(
            recording.take(),
            vec![0.5, 0.5],
            "both capsules must contribute, or the signal halves on hardware favouring one"
        );
    }

    #[test]
    fn the_level_meter_sees_a_clipping_channel_the_average_would_hide() {
        let recording = Recording::new(LIMIT);
        // One channel at full scale, the other silent: the mean is -6 dBFS
        // but a meter must report the channel that is actually clipping.
        recording.push(&[1.0, 0.0], 2, LIMIT);
        assert!(
            recording.peak_dbfs().abs() < 0.01,
            "the meter must read the raw peak (0 dBFS), got {}",
            recording.peak_dbfs()
        );
    }

    #[test]
    fn the_level_tracks_the_loudest_sample_so_far() {
        let recording = Recording::new(LIMIT);
        let silent = recording.peak_dbfs();
        assert!(
            silent.is_infinite() && silent.is_sign_negative(),
            "an untouched recording must read as no signal, got {silent}"
        );

        recording.push(&[0.1, -0.5, 0.25], 1, LIMIT);
        let loud = recording.peak_dbfs();
        recording.push(&[0.01], 1, LIMIT);
        assert!(
            (recording.peak_dbfs() - loud).abs() < f32::EPSILON,
            "the peak must hold rather than follow the signal down, or a meter flickers to silence"
        );
        // -0.5 full scale is about -6 dBFS.
        assert!((loud + 6.02).abs() < 0.1, "peak of 0.5 must read near -6 dBFS, got {loud}");
    }
}

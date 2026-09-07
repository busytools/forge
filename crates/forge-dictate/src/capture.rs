//! Microphone capture.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::Resampler as _;

use crate::Error;
use crate::audio::SAMPLE_RATE;

/// What the recording thread and the handle share.
pub(crate) struct Recording {
    samples: Mutex<Vec<f32>>,
    /// Loudest absolute sample since the last read, as `f32` bits.
    /// Atomic so a caller drawing a level meter never blocks on the
    /// audio callback.
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
        let recording = Self {
            samples: Mutex::new(Vec::with_capacity(capacity)),
            peak_bits: AtomicU32::new(0.0f32.to_bits()),
            stop: AtomicBool::new(false),
            truncated: AtomicBool::new(false),
        };
        // `Mutex::lock` allocates on its FIRST acquisition on macOS, and
        // the first one would otherwise happen inside the audio callback,
        // where `push` is documented as allocation-free. Taking it once
        // here moves that 64-byte malloc off the realtime thread.
        drop(recording.samples.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        recording
    }

    /// Fold one callback's worth of audio in, downmixed to mono and
    /// keeping the running peak since the last read.
    ///
    /// MUST NOT ALLOCATE. This runs on the realtime audio thread, where a
    /// `malloc` can miss the deadline and drop a buffer, so the downmix
    /// happens while extending the preallocated buffer rather than into
    /// an intermediate.
    ///
    /// Channels are averaged rather than one being chosen: discarding a
    /// capsule silently halves the signal on hardware where the speaker
    /// sits nearer one of them.
    pub(crate) fn push(&self, block: &[f32], channels: usize, limit: usize) {
        debug_assert!(
            channels > 0 && block.len().is_multiple_of(channels),
            "interleaved audio must divide evenly into frames"
        );

        self.observe_peak(block);

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
            samples.extend(block.chunks_exact(channels).take(room).map(channel_mean));
        }
    }

    /// Fold the loudest absolute sample of `block` into the meter's
    /// accumulator. Peak over the RAW block, not any downmix: a meter
    /// should show a channel clipping even when the mean of the
    /// channels does not.
    fn observe_peak(&self, block: &[f32]) {
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
    }

    /// Loudest input since the last read, in dBFS. Take-and-reset: the
    /// read clears the accumulator, so a poller gets one window per ask.
    pub(crate) fn peak_dbfs(&self) -> f32 {
        let peak = f32::from_bits(self.peak_bits.swap(0.0f32.to_bits(), Ordering::Relaxed));
        if peak <= 0.0 { f32::NEG_INFINITY } else { 20.0 * peak.log10() }
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Whether the recording has been asked to stop. Test seam: the
    /// feeding microphone in `test_support` polls it between chunks.
    #[cfg(any(test, feature = "test-helpers"))]
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Samples currently held. The append-only buffer keeps written
    /// ranges stable, so a segmenter can cut by offset while the
    /// callback keeps appending past them.
    pub(crate) fn sample_len(&self) -> usize {
        self.samples.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }

    /// Copy `[from..to)` out without draining. The recording keeps the
    /// whole take so finish can hand diagnostics the full capture, and
    /// so an offset that raced a stop is a short copy, not a panic.
    /// The lock is held for the memcpy only - long enough to be felt in
    /// a callback budget if it happened every block, which it does not:
    /// this runs once per window boundary, every 30 to 60 seconds.
    pub(crate) fn copy_region(&self, from: usize, to: usize) -> Vec<f32> {
        let samples = self.samples.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let to = to.min(samples.len());
        let from = from.min(to);
        samples[from..to].to_vec()
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
    let wanted =
        usize::try_from(max_capture.as_millis().saturating_mul(u128::from(SAMPLE_RATE)) / 1000)
            .unwrap_or(CAP_CEILING);
    // Clamped because the whole cap is reserved eagerly, per capture: at
    // 4 bytes a sample an hour is 219 MiB and `Duration::MAX` aborts the
    // process inside `Vec::with_capacity`.
    wanted.min(CAP_CEILING)
}

/// Mean of one interleaved frame, so every channel contributes. Both
/// downmix sites share this one formula: discarding a capsule silently
/// halves the signal on hardware where the speaker sits nearer one of
/// them.
fn channel_mean(frame: &[f32]) -> f32 {
    // Channel counts are single digits; the cast cannot lose anything.
    let scale = f32::from(u16::try_from(frame.len()).unwrap_or(u16::MAX));
    frame.iter().sum::<f32>() / scale
}

/// Resampler input per call, in device frames. 10 ms at 48 kHz; any
/// block size the device delivers is staged up to it.
const RESAMPLE_CHUNK: usize = 480;

/// Turns what the device delivers into the mono [`SAMPLE_RATE`] signal
/// everything downstream reads. Pass-through when the device natively
/// offers the model rate; otherwise downmix to mono, then resample
/// with a windowed-sinc filter. Decimating without the filter would
/// fold everything above the new Nyquist into the speech band as
/// confident wrong words.
///
/// Built on the recorder thread and moved into the stream callback,
/// which is `FnMut`, so `push` is the whole realtime path: it extends
/// preallocated buffers and never allocates.
pub(crate) struct InputConverter {
    channels: usize,
    resampling: Option<Resampling>,
}

impl InputConverter {
    /// `channels` is what the stream will deliver, `device_rate` the
    /// rate it will deliver at; a `None` rate is the pass-through.
    pub(crate) fn new(channels: usize, device_rate: Option<u32>) -> Result<Self, Error> {
        let resampling = device_rate.map(Resampling::new).transpose()?;
        Ok(Self { channels, resampling })
    }

    /// Fold one callback's worth of device audio in.
    pub(crate) fn push(&mut self, block: &[f32], sink: &Recording, limit: usize) {
        match &mut self.resampling {
            None => sink.push(block, self.channels, limit),
            Some(resampling) => resampling.push(block, self.channels, sink, limit),
        }
    }
}

struct Resampling {
    resampler: rubato::SincFixedIn<f32>,
    staging: Vec<f32>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
}

impl Resampling {
    fn new(device_rate: u32) -> Result<Self, Error> {
        // The documented starting point; `f_cutoff` is relative to the
        // lower of the two Nyquists, so speech below it passes and
        // everything that would alias is in the stopband.
        let parameters = rubato::SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            oversampling_factor: 256,
            interpolation: rubato::SincInterpolationType::Cubic,
            window: rubato::WindowFunction::BlackmanHarris2,
        };
        let resampler = rubato::SincFixedIn::<f32>::new(
            f64::from(SAMPLE_RATE) / f64::from(device_rate),
            1.0,
            parameters,
            RESAMPLE_CHUNK,
            1,
        )
        .map_err(|error| Error::Capture { message: error.to_string() })?;
        let output = resampler.output_buffer_allocate(true);
        // Sized for the largest block a callback could bring, so the
        // extends below stay inside the allocation. Overflow past it
        // would grow once and then hold - the same first-malloc
        // trade `Recording::new` makes.
        let staging = Vec::with_capacity(RESAMPLE_CHUNK * 4);
        let input = vec![vec![0.0; RESAMPLE_CHUNK]];
        Ok(Self { resampler, staging, input, output })
    }

    fn push(&mut self, block: &[f32], channels: usize, sink: &Recording, limit: usize) {
        sink.observe_peak(block);
        if channels <= 1 {
            self.staging.extend_from_slice(block);
        } else {
            self.staging.extend(block.chunks_exact(channels).map(channel_mean));
        }
        while self.staging.len() >= RESAMPLE_CHUNK {
            self.input[0].copy_from_slice(&self.staging[..RESAMPLE_CHUNK]);
            // The only failure is a buffer-size mismatch, and both
            // buffers are sized to the resampler at construction; a
            // warn beats panicking the audio thread.
            let (_, produced) =
                match self.resampler.process_into_buffer(&self.input, &mut self.output, None) {
                    Ok(used) => used,
                    Err(error) => {
                        // Defensive: a persistent rejection must not
                        // grow staging without bound.
                        self.staging.clear();
                        tracing::warn!(%error, "resampler rejected a block; dropping it");
                        return;
                    }
                };
            self.staging.drain(..RESAMPLE_CHUNK);
            sink.push(&self.output[0][..produced], 1, limit);
        }
    }
}

/// One hour of audio, the point past which reserving the cap costs more
/// than any dictation could use.
const CAP_CEILING: usize = SAMPLE_RATE as usize * 3600;

/// The body a capture thread runs: open the input, record into `shared`
/// until stopped or capped, answer `ready` either way. The engine holds
/// one, so a test can stand in a recorder that never touches hardware.
/// A boxed closure rather than a fn pointer so a stand-in can carry the
/// samples it feeds.
pub(crate) type Recorder = Arc<
    dyn Fn(&Arc<Recording>, Duration, Option<&str>, &std::sync::mpsc::Sender<Result<(), Error>>)
        + Send
        + Sync,
>;

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

    let plan = match input_config(&device) {
        Ok(plan) => plan,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let limit = sample_cap(max_capture);
    let channels = plan.config.channels as usize;
    let mut converter = match InputConverter::new(channels, plan.resample_from) {
        Ok(converter) => converter,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    tracing::debug!(channels, rate = SAMPLE_RATE, resample_from = ?plan.resample_from, "input open");
    let sink = Arc::clone(shared);
    let stream = device.build_input_stream(
        plan.config,
        move |block: &[f32], _: &cpal::InputCallbackInfo| converter.push(block, &sink, limit),
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

/// How a capture opens: the stream config the device will run at, and
/// the rate its samples arrive at when that is not already
/// [`SAMPLE_RATE`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InputPlan {
    pub(crate) config: cpal::StreamConfig,
    /// `Some` when the device cannot offer [`SAMPLE_RATE`] itself and
    /// its audio needs converting; `None` when the samples flow
    /// through untouched.
    pub(crate) resample_from: Option<u32>,
}

/// Pick an input the models can read.
///
/// A device offering [`SAMPLE_RATE`] in F32 opens natively, fewest
/// channels winning: less to average, less to go wrong. One that does
/// not still opens its best F32 config - fewest channels, then the
/// rate nearest the model rate - and its audio is downmixed and
/// resampled with a real filter downstream. Measured on the target
/// hardware, which offers 16, 24 and 32 kHz and never fewer than two
/// channels - so requiring mono here would have meant no microphone at
/// all - and on USB microphones that offer only 48 kHz.
///
/// What is still refused is a device with no F32 config at all: the
/// error names everything it offered, so the mismatch is legible.
fn input_config(device: &cpal::Device) -> Result<InputPlan, Error> {
    let supported = device
        .supported_input_configs()
        .map_err(|error| Error::Capture { message: error.to_string() })?;
    input_plan(supported)
}

/// The negotiation over synthetic ranges, so a device is not needed to
/// test it.
fn input_plan(
    supported: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
) -> Result<InputPlan, Error> {
    let wanted: cpal::SampleRate = SAMPLE_RATE;
    let mut offered = Vec::new();
    let mut native: Option<cpal::StreamConfig> = None;
    // (channels, distance from the model rate, rate, config)
    let mut fallback: Option<(u16, u32, cpal::SampleRate, cpal::StreamConfig)> = None;
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
            if native.as_ref().is_none_or(|b| config.channels < b.channels) {
                native = Some(config);
            }
            continue;
        }
        // The range cannot serve the model rate itself, so its nearest
        // end is the resample candidate.
        let rate = SAMPLE_RATE.clamp(range.min_sample_rate(), range.max_sample_rate());
        let candidate = (
            range.channels(),
            rate.abs_diff(SAMPLE_RATE),
            rate,
            cpal::StreamConfig {
                channels: range.channels(),
                sample_rate: rate,
                buffer_size: cpal::BufferSize::Default,
            },
        );
        let better = match fallback {
            None => true,
            Some((channels, distance, _, _)) => {
                candidate.0 < channels || (candidate.0 == channels && candidate.1 < distance)
            }
        };
        if better {
            fallback = Some(candidate);
        }
    }
    if let Some(config) = native {
        return Ok(InputPlan { config, resample_from: None });
    }
    fallback
        .map(|(_, _, rate, config)| InputPlan { config, resample_from: Some(rate) })
        .ok_or(Error::UnsupportedInput { wanted: SAMPLE_RATE, offered: offered.join(", ") })
}

#[cfg(test)]
mod tests_input_config {
    use super::*;
    use cpal::{SampleFormat, SupportedBufferSize, SupportedStreamConfigRange};

    /// A cpal range with no device behind it. The negotiation reads
    /// only these fields, so this is a complete synthetic device.
    fn range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(channels, min, max, SupportedBufferSize::Unknown, format)
    }

    #[test]
    fn a_device_offering_16k_takes_the_native_path() {
        // The Mac's built-in input measured inventory: 16 kHz exists,
        // but only in stereo beside 24 and 32.
        let plan = input_plan([
            range(2, 16_000, 16_000, SampleFormat::F32),
            range(2, 24_000, 24_000, SampleFormat::F32),
            range(2, 32_000, 32_000, SampleFormat::F32),
        ])
        .expect("a 16 kHz config is on offer");
        assert_eq!(
            plan.config.sample_rate, SAMPLE_RATE,
            "the native path must open at the model rate"
        );
        assert_eq!(
            plan.resample_from, None,
            "a native 16 kHz config must not engage the resampler"
        );
    }

    #[test]
    fn a_device_without_16k_falls_back_to_its_best_f32_offer() {
        // The AirHug 28's whole inventory, the device this case exists for.
        let plan = input_plan([range(2, 48_000, 48_000, SampleFormat::F32)])
            .expect("an F32 offer must be usable, not refused");
        assert_eq!(
            (plan.config.channels, plan.config.sample_rate),
            (2, 48_000),
            "the fallback opens the device's own best F32 config"
        );
        assert_eq!(
            plan.resample_from,
            Some(48_000),
            "the samples arrive at 48 kHz and must be converted to the model rate"
        );
    }

    #[test]
    fn the_fallback_prefers_fewer_channels_then_the_rate_nearest_16k() {
        let plan = input_plan([
            range(2, 24_000, 24_000, SampleFormat::F32),
            range(1, 44_100, 44_100, SampleFormat::F32),
        ])
        .expect("offers exist");
        assert_eq!(
            (plan.config.channels, plan.config.sample_rate),
            (1, 44_100),
            "fewest channels wins even when its rate is the farther one"
        );

        let plan = input_plan([
            range(2, 48_000, 48_000, SampleFormat::F32),
            range(2, 44_100, 44_100, SampleFormat::F32),
        ])
        .expect("offers exist");
        assert_eq!(
            plan.config.sample_rate, 44_100,
            "equal channels: the rate nearest the model rate wins"
        );
    }

    #[test]
    fn a_device_with_no_f32_offer_at_all_is_still_refused_with_what_it_offers() {
        let err =
            input_plan([range(2, 44_100, 44_100, SampleFormat::I16)]).expect_err("no F32, no open");
        match err {
            Error::UnsupportedInput { wanted, offered } => {
                assert_eq!(wanted, SAMPLE_RATE);
                assert!(
                    offered.contains("2ch 44100-44100Hz"),
                    "the refusal must keep naming what the device offered, got: {offered}"
                );
            }
            other => panic!("the no-F32 refusal must stay UnsupportedInput, got: {other:?}"),
        }
    }

    #[test]
    fn an_f32_offer_wins_even_when_a_non_f32_range_serves_16k() {
        // Only the F32 stream can be opened, so a native 16 kHz I16
        // range must not beat an F32 range that needs conversion.
        let plan = input_plan([
            range(1, 16_000, 16_000, SampleFormat::I16),
            range(2, 48_000, 48_000, SampleFormat::F32),
        ])
        .expect("an F32 offer exists");
        assert_eq!(
            plan.resample_from,
            Some(48_000),
            "the F32 offer is the only one this crate can open"
        );
        assert_eq!((plan.config.channels, plan.config.sample_rate), (2, 48_000));
    }

    /// Feed a stereo take through the converter in CoreAudio-sized
    /// blocks, interleaved the way the F32 callback delivers.
    fn feed_stereo(
        converter: &mut InputConverter,
        recording: &Recording,
        left: &[f32],
        right: &[f32],
    ) {
        for (l, r) in left.chunks(512).zip(right.chunks(512)) {
            let block: Vec<f32> = l.iter().zip(r.iter()).flat_map(|(l, r)| [*l, *r]).collect();
            converter.push(&block, recording, usize::MAX);
        }
    }

    /// Magnitude of `freq` over `pcm`, taken as 16 kHz samples. Over a
    /// whole second (16_000 of them) the hertz bins land exactly on the
    /// DFT grid, so a tone's level reads without windowing leakage.
    #[allow(clippy::cast_precision_loss)]
    fn magnitude_at(pcm: &[f32], freq: f32) -> f32 {
        let n = pcm.len();
        let omega = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE as f32;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &sample in pcm {
            let s = sample + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / (n as f32 / 2.0)
    }

    /// The converted take, windowed to its last whole second: whole so
    /// the hertz bins land exactly, past the head so the filter's
    /// warmup is not in the window.
    fn interior_second(mut pcm: Vec<f32>) -> Vec<f32> {
        let second = SAMPLE_RATE as usize;
        pcm.split_off(pcm.len() - second)
    }

    /// `gain * sin(freq)` over two seconds at `rate`.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn sine(freq: f32, gain: f32, rate: f32) -> Vec<f32> {
        (0..2 * rate as usize)
            .map(|i| gain * (2.0 * std::f32::consts::PI * freq * i as f32 / rate).sin())
            .collect()
    }

    #[test]
    fn a_48k_stereo_take_converts_to_mono_16k_keeping_its_tone_and_dropping_what_would_alias() {
        let rate = 48_000.0;

        // In band, at different levels per channel: only the average
        // (0.2) survives the downmix, so its level pins that the
        // channels were averaged rather than one carried through.
        let recording = Recording::new(8 * SAMPLE_RATE as usize);
        let mut converter = InputConverter::new(2, Some(48_000)).expect("the resampler must build");
        feed_stereo(&mut converter, &recording, &sine(440.0, 0.3, rate), &sine(440.0, 0.1, rate));
        let pcm = recording.take();
        assert!(
            (31_600..=32_400).contains(&pcm.len()),
            "two seconds at 48 kHz must land as two seconds at 16 kHz, got {} samples",
            pcm.len()
        );
        let pcm = interior_second(pcm);
        let kept = magnitude_at(&pcm, 440.0);
        assert!(
            (kept - 0.2).abs() < 0.02,
            "the tone must come through at the downmixed level 0.2, got {kept}"
        );
        assert!(
            magnitude_at(&pcm, 540.0) < 0.02,
            "the energy must stay at the tone's own frequency: a wrong rate would move it"
        );

        // Above the new Nyquist. Naive every-third-sample decimation
        // delivers a 12 kHz tone as a full-size 4 kHz tone - speech-band
        // content that was never spoken - which is the failure a real
        // filter exists to prevent.
        let recording = Recording::new(8 * SAMPLE_RATE as usize);
        let mut converter = InputConverter::new(2, Some(48_000)).expect("the resampler must build");
        feed_stereo(
            &mut converter,
            &recording,
            &sine(12_000.0, 0.25, rate),
            &sine(12_000.0, 0.25, rate),
        );
        let pcm = interior_second(recording.take());
        let aliased = magnitude_at(&pcm, 4_000.0);
        assert!(
            aliased < 0.005,
            "a 12 kHz tone must be filtered away, not aliased into the speech band at 4 kHz, got {aliased}"
        );
    }

    #[test]
    fn a_native_16k_path_forwards_its_samples_through_todays_downmix() {
        let recording = Recording::new(SAMPLE_RATE as usize);
        let mut converter =
            InputConverter::new(2, None).expect("pass-through has nothing to build");
        converter.push(&[1.0, 0.0, 0.5, -0.5], &recording, SAMPLE_RATE as usize);
        assert_eq!(
            recording.take(),
            vec![0.5, 0.0],
            "a stereo block on the native path must flow through today's exact downmix"
        );
    }

    #[test]
    fn one_callback_larger_than_a_resampler_chunk_is_consumed_in_full() {
        let recording = Recording::new(SAMPLE_RATE as usize);
        let mut converter = InputConverter::new(2, Some(48_000)).expect("the resampler must build");
        // 960 frames is exactly two chunks: the one push must run the
        // resampler for both, not stop after the first.
        converter.push(&vec![0.0; 2 * 480 * 2], &recording, SAMPLE_RATE as usize);
        let produced = recording.sample_len();
        assert!(
            (200..=320).contains(&produced),
            "both chunks of a 960-frame callback must be resampled (116 then 160 frames), got {produced}"
        );
    }

    #[test]
    fn the_level_meter_reads_the_raw_channels_on_the_resampled_path() {
        let recording = Recording::new(SAMPLE_RATE as usize);
        let mut converter = InputConverter::new(2, Some(48_000)).expect("the resampler must build");
        // One channel at full scale, the other silent: the filtered mean
        // is -6 dBFS but the meter must report the channel that is
        // actually clipping.
        converter.push(&[1.0, 0.0], &recording, SAMPLE_RATE as usize);
        let peak = recording.peak_dbfs();
        assert!(
            peak.abs() < 0.01,
            "the meter must read the raw channel peak (0 dBFS) through the resampler, got {peak}"
        );
    }
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
    #[ignore = "enumerates the real audio devices; run with --run-ignored all on a machine with audio hardware"]
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
    #[ignore = "enumerates the real audio devices; run with --run-ignored all on a machine with audio hardware"]
    fn a_named_device_that_is_absent_never_falls_back() {
        // Skipped where there is no audio stack at all, since then the
        // absence proves nothing about the resolution path.
        // Guarded on a DEFAULT existing, not merely on the list being
        // non-empty: the second assertion needs `default_input_device()`
        // to be Some, which is a different cpal call.
        let Ok(found) = devices() else { return };
        if !found.iter().any(|d| d.is_default) {
            return;
        }
        assert!(
            open_device(Some("forge-dictate-no-such-device")).is_err(),
            "a named device that is not present must error, never quietly record on the default"
        );
        assert!(
            open_device(None).is_ok(),
            "naming nothing must still resolve to the system default"
        );
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
        // Bound once: the read is take-and-reset, so a second call in
        // the message would report the empty window instead of what
        // failed.
        let peak = recording.peak_dbfs();
        assert!(peak.abs() < 0.01, "the meter must read the raw peak (0 dBFS), got {peak}");
    }

    /// The read is take-and-reset: each read answers "loudest since the
    /// last read" and then clears, which is what a windowed level meter
    /// polls. A read that held the all-time peak would freeze a meter on
    /// the first syllable; the deliberate change from the old peak-hold
    /// behaviour is covered by the two quieter-push assertions below.
    #[test]
    fn the_level_read_is_take_and_reset() {
        let recording = Recording::new(LIMIT);
        let silent = recording.peak_dbfs();
        assert!(
            silent.is_infinite() && silent.is_sign_negative(),
            "an untouched recording must read as no signal, got {silent}"
        );

        recording.push(&[0.1, -0.5, 0.25], 1, LIMIT);
        let loud = recording.peak_dbfs();
        // -0.5 full scale is about -6 dBFS.
        assert!((loud + 6.02).abs() < 0.1, "peak of 0.5 must read near -6 dBFS, got {loud}");

        // A quieter window after the read reads its own peak, not the
        // all-time high.
        recording.push(&[0.01], 1, LIMIT);
        let quiet = recording.peak_dbfs();
        // 0.01 full scale is about -40 dBFS.
        assert!((quiet + 40.0).abs() < 0.1, "a quieter window must read its own peak, got {quiet}");

        // And having consumed it, a read with nothing new is no signal.
        let empty = recording.peak_dbfs();
        assert!(
            empty.is_infinite() && empty.is_sign_negative(),
            "a read past the last one must be empty, got {empty}"
        );
    }
}

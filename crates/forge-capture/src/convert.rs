//! What the device delivers, turned into the mono model-rate signal.

use rubato::Resampler as _;

use crate::Error;
use crate::SAMPLE_RATE;

/// Mean of one interleaved frame, so every channel contributes. Discarding
/// a capsule silently halves the signal on hardware where the speaker sits
/// nearer one of them.
fn channel_mean(frame: &[f32]) -> f32 {
    // Channel counts are single digits; the cast cannot lose anything.
    let scale = f32::from(u16::try_from(frame.len()).unwrap_or(u16::MAX));
    frame.iter().sum::<f32>() / scale
}

/// The loudest absolute sample of a raw block, before any downmix: a meter
/// should show a channel clipping even where the mono mix halves it.
pub(crate) fn raw_peak(block: &[f32]) -> f32 {
    block.iter().fold(0.0f32, |loudest, sample| loudest.max(sample.abs()))
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
/// preallocated buffers and hands the samples on as they are ready.
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

    /// Fold one callback's worth of device audio in, handing the mono
    /// model-rate samples and the raw block's own peak to `out`.
    pub(crate) fn push(&mut self, block: &[f32], out: &mut impl FnMut(&[f32], f32)) {
        let peak = raw_peak(block);
        match &mut self.resampling {
            None => {
                if self.channels <= 1 {
                    out(block, peak);
                } else {
                    let mono: Vec<f32> =
                        block.chunks_exact(self.channels).map(channel_mean).collect();
                    out(&mono, peak);
                }
            }
            Some(resampling) => resampling.push(block, self.channels, peak, out),
        }
    }
}

struct Resampling {
    resampler: rubato::SincFixedIn<f32>,
    staging: Vec<f32>,
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
    /// The loudest raw peak of the blocks staged into the next chunk.
    /// Blocks are gathered here before they are resampled, so the peak
    /// is folded the same way: a meter must not see only the block that
    /// happened to complete a chunk.
    pending_peak: f32,
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
        // would grow once and then hold - the same first-malloc trade
        // the recorder makes.
        let staging = Vec::with_capacity(RESAMPLE_CHUNK * 4);
        let input = vec![vec![0.0; RESAMPLE_CHUNK]];
        Ok(Self { resampler, staging, input, output, pending_peak: 0.0 })
    }

    fn push(
        &mut self,
        block: &[f32],
        channels: usize,
        peak: f32,
        out: &mut impl FnMut(&[f32], f32),
    ) {
        self.pending_peak = self.pending_peak.max(peak);
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
                        tracing::warn!(
                            event_name = "resampler_block_rejected",
                            %error,
                            "resampler rejected a block; dropping it"
                        );
                        return;
                    }
                };
            self.staging.drain(..RESAMPLE_CHUNK);
            out(&self.output[0][..produced], std::mem::take(&mut self.pending_peak));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model rate as a float. The cases here work in whole seconds at
    /// this rate, so a sample count and its seconds are the same number.
    const RATE: f32 = 16_000.0;

    /// One second at the model rate.
    const SECOND: usize = 16_000;

    /// Feed a stereo take through the converter in CoreAudio-sized
    /// blocks, interleaved the way the F32 callback delivers.
    fn feed_stereo(converter: &mut InputConverter, left: &[f32], right: &[f32]) -> Vec<f32> {
        let mut mono = Vec::new();
        for (l, r) in left.chunks(512).zip(right.chunks(512)) {
            let block: Vec<f32> = l.iter().zip(r.iter()).flat_map(|(l, r)| [*l, *r]).collect();
            converter.push(&block, &mut |samples, _| mono.extend_from_slice(samples));
        }
        mono
    }

    /// Magnitude of `freq` over `pcm`, taken as one second at the model
    /// rate: whole, so the hertz bins land exactly on the DFT grid and a
    /// tone's level reads without windowing leakage.
    fn magnitude_at(pcm: &[f32], freq: f32) -> f32 {
        let omega = std::f32::consts::TAU * freq / RATE;
        let coeff = 2.0 * omega.cos();
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &sample in pcm {
            let s = sample + coeff * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / (RATE / 2.0)
    }

    /// The tail of a converted take, as its last whole second: past the
    /// head, so the filter's warmup is not in the window.
    fn interior_second(pcm: &[f32]) -> Vec<f32> {
        pcm[pcm.len() - SECOND..].to_vec()
    }

    /// `gain * sin(freq)` at `rate`, over `seconds`.
    fn sine(freq: f32, gain: f32, rate: u32, seconds: u32) -> Vec<f32> {
        let rate_f = f32::from(u16::try_from(rate).unwrap_or(u16::MAX));
        let step = std::f32::consts::TAU * freq / rate_f;
        let mut phase = 0.0f32;
        (0..seconds * rate)
            .map(|_| {
                let sample = gain * phase.sin();
                phase += step;
                sample
            })
            .collect()
    }

    #[test]
    fn a_48k_stereo_take_converts_to_mono_16k_keeping_its_tone_and_dropping_what_would_alias() {
        // In band, at different levels per channel: only the average
        // (0.2) survives the downmix, so its level pins that the
        // channels were averaged rather than one carried through.
        let mut converter = InputConverter::new(2, Some(48_000)).expect("the resampler must build");
        let pcm =
            feed_stereo(&mut converter, &sine(440.0, 0.3, 48_000, 2), &sine(440.0, 0.1, 48_000, 2));
        assert!(
            (31_600..=32_400).contains(&pcm.len()),
            "two seconds at 48 kHz must land as two seconds at 16 kHz, got {} samples",
            pcm.len()
        );
        let pcm = interior_second(&pcm);
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
        let mut converter = InputConverter::new(2, Some(48_000)).expect("the resampler must build");
        let pcm = feed_stereo(
            &mut converter,
            &sine(12_000.0, 0.25, 48_000, 2),
            &sine(12_000.0, 0.25, 48_000, 2),
        );
        let aliased = magnitude_at(&interior_second(&pcm), 4_000.0);
        assert!(
            aliased < 0.005,
            "a 12 kHz tone must be filtered away, not aliased into the speech band at 4 kHz, got {aliased}"
        );
    }

    #[test]
    fn a_native_16k_path_forwards_its_samples_through_todays_downmix() {
        let mut converter =
            InputConverter::new(2, None).expect("pass-through has nothing to build");
        let mut out = Vec::new();
        converter.push(&[1.0, 0.0, 0.5, -0.5], &mut |samples, _| out.extend_from_slice(samples));
        assert_eq!(
            out,
            vec![0.5, 0.0],
            "a stereo block on the native path must flow through today's exact downmix"
        );
    }

    #[test]
    fn one_callback_larger_than_a_resampler_chunk_is_consumed_in_full() {
        let mut converter = InputConverter::new(2, Some(48_000)).expect("the resampler must build");
        // 960 frames is exactly two chunks: the one push must run the
        // resampler for both, not stop after the first.
        let mut produced = 0;
        converter.push(&vec![0.0; 2 * 480 * 2], &mut |samples, _| produced += samples.len());
        assert!(
            (200..=320).contains(&produced),
            "both chunks of a 960-frame callback must be resampled (116 then 160 frames), got {produced}"
        );
    }

    #[test]
    fn the_raw_peak_reaches_the_caller_from_a_clipping_channel() {
        let mut converter = InputConverter::new(2, Some(48_000)).expect("the resampler must build");
        // One channel at full scale, the other silent, across a whole
        // chunk: the downmixed mean is -6 dBFS, and a meter must still
        // report the channel that is actually clipping.
        let mut block = vec![0.0f32; 2 * RESAMPLE_CHUNK];
        for frame in block.as_chunks_mut::<2>().0 {
            frame[0] = 1.0;
        }
        let mut peak = 0.0f32;
        converter.push(&block, &mut |_, block_peak| peak = block_peak);
        assert!(
            (peak - 1.0).abs() < f32::EPSILON,
            "the peak must be the raw channel's, not the mix's, got {peak}"
        );
    }
}

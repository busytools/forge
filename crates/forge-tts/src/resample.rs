//! A speaker reference at the codec's own rate.
//!
//! The public corpus's clips are 16 kHz; the Qwen3-TTS codec reads and
//! writes 24 kHz, so a reference is converted before it is encoded.

use rubato::Resampler as _;

use crate::Error;

/// Resampler input per call, in frames.
const CHUNK: usize = 1024;

/// The windowed-sinc filter: `f_cutoff` is relative to the lower of the two
/// Nyquists, so speech below it passes and everything that would alias is
/// in the stopband.
fn sinc_parameters() -> rubato::SincInterpolationParameters {
    rubato::SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        oversampling_factor: 256,
        interpolation: rubato::SincInterpolationType::Cubic,
        window: rubato::WindowFunction::BlackmanHarris2,
    }
}

/// One mono buffer at `from` converted to `to`. The same rate hands the
/// buffer straight back.
pub(crate) fn resample_mono(pcm: &[f32], from: u32, to: u32) -> Result<Vec<f32>, Error> {
    if from == to {
        return Ok(pcm.to_vec());
    }
    if from == 0 || to == 0 {
        return Err(Error::Render("a sample rate of zero cannot be converted".to_owned()));
    }
    if pcm.is_empty() {
        return Ok(Vec::new());
    }

    let mut resampler = rubato::SincFixedIn::<f32>::new(
        f64::from(to) / f64::from(from),
        1.0,
        sinc_parameters(),
        CHUNK,
        1,
    )
    .map_err(|error| Error::Render(format!("the reference could not be resampled: {error}")))?;
    let from_frames = usize::try_from(from).unwrap_or(usize::MAX);
    let to_frames = usize::try_from(to).unwrap_or(usize::MAX);
    let mut converted = Vec::with_capacity(
        pcm.len().saturating_mul(to_frames).saturating_div(from_frames).saturating_add(CHUNK),
    );
    let mut buffer = resampler.output_buffer_allocate(true);

    let mut position = 0;
    while position < pcm.len() {
        let remaining = pcm.len() - position;
        let produced = if remaining >= CHUNK {
            let input = &pcm[position..position + CHUNK];
            resampler
                .process_into_buffer(&[input], &mut buffer, None)
                .map_err(|error| {
                    Error::Render(format!("the reference could not be resampled: {error}"))
                })?
                .1
        } else {
            let input = &pcm[position..];
            resampler
                .process_partial_into_buffer(Some(std::slice::from_ref(&input)), &mut buffer, None)
                .map_err(|error| {
                    Error::Render(format!("the reference could not be resampled: {error}"))
                })?
                .1
        };
        converted.extend_from_slice(&buffer[0][..produced]);
        position += CHUNK.min(remaining);
    }
    // The filter's own delay: pad once more so the tail comes out rather
    // than being cut off by the delay line, then cut at the buffer's own
    // duration.
    let flushed = resampler
        .process_partial_into_buffer::<&[f32], Vec<f32>>(None, &mut buffer, None)
        .map_err(|error| Error::Render(format!("the reference could not be resampled: {error}")))?
        .1;
    converted.extend_from_slice(&buffer[0][..flushed]);
    converted.truncate(pcm.len().saturating_mul(to_frames) / from_frames);
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::resample_mono;

    #[test]
    fn a_buffer_at_another_rate_is_converted_by_the_ratio() {
        let pcm = vec![0.25_f32; 16_000];
        let converted = resample_mono(&pcm, 16_000, 24_000).expect("the rate converts");
        assert_eq!(converted.len(), 24_000, "one second stays one second");
    }

    #[test]
    fn the_same_rate_hands_the_buffer_back() {
        let pcm = vec![0.5_f32; 8];
        assert_eq!(resample_mono(&pcm, 24_000, 24_000).expect("same rate"), pcm);
    }
}

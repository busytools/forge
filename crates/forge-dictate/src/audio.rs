//! The audio a transcription reads from.

/// Sample rate every model in this crate expects.
pub const SAMPLE_RATE: u32 = 16_000;

/// A pull-based source of mono audio.
///
/// Chunked rather than streamed so a source needs no hardware and no
/// clock: a buffer already in memory drains through it as fast as the
/// caller asks, and feeding a twenty second clip takes nowhere near
/// twenty seconds.
///
/// The format accessors exist to be checked. A source whose rate or
/// channel count does not match is rejected outright, because silently
/// resampling or de-interleaving it would turn a wrong input into
/// plausible output rather than an error, which is the failure nobody
/// notices.
pub trait AudioSource: Send {
    /// Next samples, or None once the source is exhausted. An empty
    /// chunk means "nothing yet", not end of stream.
    fn next_chunk(&mut self) -> Option<Vec<f32>>;

    /// Samples per second, as the source knows it to be.
    fn sample_rate(&self) -> u32;

    /// Interleaved channels. Deliberately without a default: a source
    /// that has not thought about this is exactly the one that would
    /// get it wrong.
    fn channels(&self) -> u16;
}

/// Samples already in memory, the shape a file or a test uses.
pub struct Samples {
    samples: std::vec::IntoIter<f32>,
    sample_rate: u32,
    channels: u16,
    chunk: usize,
}

impl Samples {
    /// Mono samples at [`SAMPLE_RATE`].
    pub fn mono(samples: impl Into<Vec<f32>>) -> Self {
        Self::new(samples, SAMPLE_RATE, 1)
    }

    /// Samples at a rate and channel count the caller declares. What is
    /// declared here is what an engine checks against.
    pub fn new(samples: impl Into<Vec<f32>>, sample_rate: u32, channels: u16) -> Self {
        Self { samples: samples.into().into_iter(), sample_rate, channels, chunk: 4096 }
    }
}

impl AudioSource for Samples {
    fn next_chunk(&mut self) -> Option<Vec<f32>> {
        let chunk: Vec<f32> = self.samples.by_ref().take(self.chunk).collect();
        (!chunk.is_empty()).then_some(chunk)
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }
}

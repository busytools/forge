//! The take's own sample buffer: what a host's pushed frames grow, and
//! what the segmenter cuts windows off.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use crate::audio::SAMPLE_RATE;

/// What the recording thread and the handle share.
pub(crate) struct Recording {
    samples: Mutex<Vec<f32>>,
    /// Loudest absolute sample since the last read, as `f32` bits.
    /// Atomic so a caller drawing a level meter never blocks the
    /// pusher.
    peak_bits: AtomicU32,
    stop: AtomicBool,
    /// Set when the take stopped itself at the cap rather than
    /// because it was asked to.
    truncated: AtomicBool,
}

impl Recording {
    /// `capacity` is the cap in samples. Reserved up front so the buffer
    /// never reallocates as a take grows.
    pub(crate) fn new(capacity: usize) -> Self {
        let recording = Self {
            samples: Mutex::new(Vec::with_capacity(capacity)),
            peak_bits: AtomicU32::new(0.0f32.to_bits()),
            stop: AtomicBool::new(false),
            truncated: AtomicBool::new(false),
        };
        // `Mutex::lock` allocates on its FIRST acquisition on macOS;
        // taking it once here keeps that 64-byte malloc out of the first
        // `push`, which is documented as allocation-free.
        drop(recording.samples.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        recording
    }

    /// Fold one block of audio in, keeping the running peak since the
    /// last read.
    ///
    /// Mono at the model rate: a device's channels are downmixed and
    /// resampled before they reach this buffer. MUST NOT ALLOCATE: the
    /// samples extend the preallocated buffer rather than an
    /// intermediate.
    pub(crate) fn push(&self, block: &[f32], limit: usize) {
        self.observe_peak(block);

        let mut samples = self.samples.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if samples.len() >= limit {
            // The cap is reached: stop growing and flag the take
            // stopped, so its host submits what it has rather than
            // feeding a take that can hold nothing more.
            self.truncated.store(true, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
            return;
        }

        let room = limit - samples.len();
        if block.len() > room {
            // The block that FIRST overruns is the one that loses its
            // tail, so the flag has to be set here. Setting it only on
            // the next callback leaves a `finish` in between reporting a
            // recording that really was cut as complete.
            self.truncated.store(true, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
        }

        samples.extend_from_slice(&block[..block.len().min(room)]);
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

    /// The take's cap was reached by WALL CLOCK rather than by samples.
    ///
    /// The sample cap fires only while audio is arriving, and a take fed by
    /// frames has no callback loop to notice it either: a stream that stops
    /// mid-take would hold the seat until teardown, so its host counts the
    /// clock and flags it here.
    pub(crate) fn cap_reached(&self) {
        self.truncated.store(true, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Whether the recording has been asked to stop - the cap reached
    /// itself, or the host ended the take. What a feeder polls before
    /// appending: a stream take's frame sink, which has no thread to poll
    /// it for it.
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

/// One hour of audio, the point past which reserving the cap costs more
/// than any dictation could use.
const CAP_CEILING: usize = SAMPLE_RATE as usize * 3600;

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
            recording.push(&vec![0.5; LIMIT], LIMIT);
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
        recording.push(&vec![0.5; LIMIT + 1], LIMIT);
        assert!(
            recording.was_truncated(),
            "the block losing its tail must flag it, or a finish between callbacks reports a cut recording as whole"
        );
        assert!(
            recording.stop.load(Ordering::Relaxed),
            "a capture nobody stopped must free the microphone itself, not hold it open"
        );
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

        recording.push(&[0.1, -0.5, 0.25], LIMIT);
        let loud = recording.peak_dbfs();
        // -0.5 full scale is about -6 dBFS.
        assert!((loud + 6.02).abs() < 0.1, "peak of 0.5 must read near -6 dBFS, got {loud}");

        // A quieter window after the read reads its own peak, not the
        // all-time high.
        recording.push(&[0.01], LIMIT);
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

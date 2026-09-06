//! Engines for tests outside this crate: every one stands in the
//! recorder, so `try_capture` succeeds without opening a device.

use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::engine::Engine;
use crate::{Config, Error};

/// The body of a microphone thread that hears nothing: reports the
/// input open and finishes at once.
fn synthetic_microphone(
    _shared: &Arc<crate::capture::Recording>,
    _max_capture: Duration,
    _wanted: Option<&str>,
    ready: &Sender<Result<(), Error>>,
) {
    let _ = ready.send(Ok(()));
}

/// An engine whose microphone is the synthetic stand-in. What
/// [`crate::Engine::new`] is in production.
pub fn engine_with_synthetic_microphone(cfg: Config) -> Result<Arc<Engine>, Error> {
    Engine::with_recorder(cfg, Arc::new(synthetic_microphone))
}

/// An engine whose microphone streams `samples` in one-second chunks at
/// the model rate, sleeping `pace` after each chunk, and then holds the
/// device open until stopped - the seam a pipelining test drives:
/// segments cut and settle while the capture is still live. A paced
/// feed (say, 100 ms per second of audio) is 10x faster than real
/// speech while still slower than the models transcribe; an unpaced
/// one fills the buffer quickly for tests that only care about the
/// cut-and-queue mechanics.
pub fn engine_with_feeding_microphone(
    cfg: Config,
    samples: Arc<Vec<f32>>,
    pace: Duration,
) -> Result<Arc<Engine>, Error> {
    let feed: crate::capture::Recorder = Arc::new(move |shared, max, _wanted, ready| {
        let _ = ready.send(Ok(()));
        let limit = crate::capture::sample_cap(max);
        for chunk in samples.chunks(crate::SAMPLE_RATE as usize) {
            if shared.stopped() {
                return;
            }
            shared.push(chunk, 1, limit);
            std::thread::sleep(pace);
        }
        // Held open, like a real microphone between takes: the host
        // decides when the take is over.
        while !shared.stopped() {
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    Engine::with_recorder(cfg, feed)
}

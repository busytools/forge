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
    Engine::with_recorder(cfg, synthetic_microphone)
}

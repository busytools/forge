//! Opening a device and reading its audio as it arrives.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};

use crate::Error;
use crate::SAMPLE_RATE;
use crate::convert::InputConverter;
use crate::device::{Device, describe, input_config, open_device};

/// One block of the device's audio, as it arrived.
pub struct Block<'a> {
    /// The samples, downmixed to mono at [`SAMPLE_RATE`]. A block is
    /// whatever the device delivered (or what the resampler produced for
    /// it), so a caller that needs a fixed frame size gathers its own.
    pub samples: &'a [f32],
    /// The loudest raw sample of the block, before any downmix, so a
    /// meter can show a channel clipping that the mono mix halves.
    pub peak: f32,
}

/// A live capture: the device is open and `on_block` is being fed.
///
/// The stream itself lives on the reader thread, because `cpal::Stream` is
/// neither `Send` nor `Sync` and dropping it is what releases the device.
pub struct Capture {
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    /// The input this capture is recording from, as [`open`] resolved it:
    /// the system default when the caller named none.
    device: Device,
}

impl Capture {
    /// The input this capture opened.
    ///
    /// The one place a host learns what the system default IS: naming no
    /// device is resolved here, and nothing upstream sees the choice.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Release the device and wait for the reader to finish.
    pub fn stop(mut self) {
        self.signal();
        self.join();
    }

    fn signal(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn join(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for Capture {
    /// A capture dropped without [`Capture::stop`] still releases the
    /// device: the handle owns no other way out.
    fn drop(&mut self) {
        self.signal();
        self.join();
    }
}

/// Open the named input (or the system default) and feed `on_block` its
/// audio, already mono at [`SAMPLE_RATE`], until the capture is stopped.
///
/// The device is opened and the stream started before this returns, so a
/// device that will not open is an error here rather than silence later.
pub fn open(
    device: Option<&str>,
    mut on_block: impl FnMut(Block<'_>) + Send + 'static,
) -> Result<Capture, Error> {
    let device = open_device(device)?;
    let held = describe(&device);
    let plan = input_config(&device)?;
    // Channel counts are single digits; the cast cannot lose anything.
    let channels = plan.config.channels as usize;
    let mut converter = InputConverter::new(channels, plan.resample_from)?;
    tracing::debug!(
        event_name = "input_stream_opened",
        channels,
        rate = SAMPLE_RATE,
        resample_from = ?plan.resample_from,
        "input open"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let (ready, started) = std::sync::mpsc::channel::<Result<(), Error>>();
    let reader = std::thread::Builder::new()
        .name("forge-capture".into())
        .spawn(move || {
            let stream = device.build_input_stream(
                plan.config,
                move |block: &[f32], _: &cpal::InputCallbackInfo| {
                    converter.push(block, &mut |samples, peak| on_block(Block { samples, peak }));
                },
                |error| tracing::warn!(event_name = "input_stream_error", %error, "input stream error"),
                None,
            );
            let stream = match stream.and_then(|stream| stream.play().map(|()| stream)) {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = ready.send(Err(Error::Capture { message: error.to_string() }));
                    return;
                }
            };
            let _ = ready.send(Ok(()));
            while !reader_stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(stream);
        })
        .map_err(|error| Error::Capture { message: error.to_string() })?;

    let capture = Capture { stop, reader: Some(reader), device: held };
    match started.recv() {
        Ok(Ok(())) => Ok(capture),
        // The stream never played: the reader is on its way out, and its
        // reason for giving up is the answer.
        Ok(Err(error)) => {
            capture.stop();
            Err(error)
        }
        Err(_) => {
            capture.stop();
            Err(Error::Capture { message: "the capture thread did not start".into() })
        }
    }
}

#[cfg(test)]
mod tests_open {
    use super::*;
    use crate::device::devices;

    #[test]
    #[ignore = "opens the real default input; run with --run-ignored all on a machine with audio hardware"]
    fn a_capture_reports_the_device_it_opened() {
        // No audio stack in CI, so an unopenable device proves nothing here;
        // what must hold is that the device a capture reports back is the one
        // the host calls the default, which is the fact a panel draws.
        let Ok(all) = devices() else { return };
        let Some(default) = all.iter().find(|device| device.is_default) else { return };
        let Ok(capture) = open(None, |_| {}) else { return };
        assert_eq!(
            capture.device().id,
            default.id,
            "naming no device must report the system default back to the caller"
        );
        assert!(capture.device().is_default, "and report it as the default");
        capture.stop();
    }
}

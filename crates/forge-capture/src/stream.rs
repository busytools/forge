//! Opening a device and reading its audio as it arrives.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cpal::traits::{DeviceTrait, StreamTrait};

use crate::Error;
use crate::SAMPLE_RATE;
use crate::convert::InputConverter;
use crate::device::{Device, InputFormat, describe, input_config, open_device};

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

/// The scale a full-scale `i16` sample maps to, in the f32 the pipeline reads.
const I16_SCALE: f32 = 32_768.0;

/// One device sample as the f32 the pipeline reads.
///
/// Pulled out of the callback so the scale is a thing a test can pin: an
/// operator flipped in place would compile and ship scaled nonsense silently.
fn i16_sample(sample: i16) -> f32 {
    f32::from(sample) / I16_SCALE
}

std::thread_local! {
    /// The backtrace a panic hook took on this thread.
    static PANIC_SITE: std::cell::RefCell<Option<std::backtrace::Backtrace>> =
        const { std::cell::RefCell::new(None) };
}

/// Install the hook that records a panic's OWN backtrace.
///
/// **Taken at the catch, a backtrace names the catch site, not the raise.**
/// The hook runs where the panic is raised, so a guarded block clears its
/// slot first and reads what was recorded; an installed hook is chained
/// rather than replaced.
fn record_panic_sites() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANIC_SITE.with(|slot| {
                if let Ok(mut slot) = slot.try_borrow_mut() {
                    *slot = Some(std::backtrace::Backtrace::force_capture());
                }
            });
            previous(info);
        }));
    });
}

/// The backtrace this thread's panic hook took, if it took one.
fn take_panic_site() -> Option<std::backtrace::Backtrace> {
    PANIC_SITE.with(|slot| slot.try_borrow_mut().ok().and_then(|mut slot| slot.take()))
}

/// Run one audio-callback block with its panic caught.
///
/// **cpal calls this body from its own frame**, so an unwind out of it crosses
/// the FFI boundary and aborts the process as `rust_foreign_exception` - the
/// signature in #2426's report. This is the one guard that can stop that
/// crossing; after it the capture delivers nothing more, and the log names the
/// loss once, with the input and the backtrace taken at the raise.
fn guard_block(panicked: &AtomicBool, input: &str, body: impl FnOnce()) {
    if panicked.load(Ordering::Acquire) {
        return;
    }
    // Cleared first: a cpal thread is reused, and the slot must hold THIS
    // block's panic rather than an earlier one's.
    take_panic_site();
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        panicked.store(true, Ordering::Release);
        let why = crate::panic_message(panic.as_ref());
        let backtrace = take_panic_site().unwrap_or_else(std::backtrace::Backtrace::force_capture);
        tracing::warn!(
            event_name = "input_callback_panicked",
            input = %input,
            why = %why,
            backtrace = %backtrace,
            "a panic in the audio callback was caught before it could cross the FFI boundary; \
             this capture delivers nothing more"
        );
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
    // What a panic line has to name for a reader to act: the input, its
    // format, and the rate the samples were converted from where they were
    // not already the model rate.
    let input = format!(
        "{} ({}) {:?}{}",
        held.name,
        held.id,
        plan.sample_format,
        plan.resample_from.map(|rate| format!(", resampled from {rate} Hz")).unwrap_or_default()
    );
    record_panic_sites();
    // Channel counts are single digits; the cast cannot lose anything.
    let channels = plan.config.channels as usize;
    let mut converter = InputConverter::new(channels, plan.resample_from)?;
    tracing::debug!(
        event_name = "input_stream_opened",
        channels,
        rate = SAMPLE_RATE,
        format = ?plan.sample_format,
        resample_from = ?plan.resample_from,
        "input open"
    );

    // One flag for both formats' callbacks: a panic is logged once, and every
    // later block returns without touching the pipeline.
    let panicked = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = Arc::clone(&stop);
    let (ready, started) = std::sync::mpsc::channel::<Result<(), Error>>();
    let reader = std::thread::Builder::new()
        .name("forge-capture".into())
        .spawn(move || {
            // **A panic on this thread is answered, not lost.** Without the
            // guard a panic before the first send leaves the receiver with a
            // dropped sender, which reads as "the thread did not start" even
            // though it did; the guard sends the real reason and logs it
            // (#2362's review).
            take_panic_site();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || -> Result<(), Error> {
                    // The stream is built in the format the device negotiated:
                    // I16 is converted on the way in, because everything
                    // downstream reads f32.
                    let stream = match plan.sample_format {
                        InputFormat::F32 => {
                            let panicked = Arc::clone(&panicked);
                            let input = input.clone();
                            device.build_input_stream(
                                plan.config,
                                move |block: &[f32], _: &cpal::InputCallbackInfo| {
                                    guard_block(&panicked, &input, || {
                                        converter.push(block, &mut |samples, peak| {
                                            on_block(Block { samples, peak });
                                        });
                                    });
                                },
                                |error| {
                                    tracing::warn!(event_name = "input_stream_error", %error, "input stream error");
                                },
                                None,
                            )
                        }
                        InputFormat::I16 => {
                            let panicked = Arc::clone(&panicked);
                            let input = input.clone();
                            let mut scaled: Vec<f32> = Vec::new();
                            device.build_input_stream(
                                plan.config,
                                move |block: &[i16], _: &cpal::InputCallbackInfo| {
                                    guard_block(&panicked, &input, || {
                                        scaled.clear();
                                        scaled.extend(
                                            block.iter().map(|sample| i16_sample(*sample)),
                                        );
                                        converter.push(&scaled, &mut |samples, peak| {
                                            on_block(Block { samples, peak });
                                        });
                                    });
                                },
                                |error| {
                                    tracing::warn!(event_name = "input_stream_error", %error, "input stream error");
                                },
                                None,
                            )
                        }
                    };
                    let stream = stream
                        .map_err(|error| Error::Capture { message: error.to_string() })?;
                    stream
                        .play()
                        .map_err(|error| Error::Capture { message: error.to_string() })?;
                    let _ = ready.send(Ok(()));
                    while !reader_stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    drop(stream);
                    Ok(())
                },
            ));
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    let _ = ready.send(Err(error));
                }
                Err(panic) => {
                    let why = crate::panic_message(panic.as_ref());
                    let backtrace =
                        take_panic_site().unwrap_or_else(std::backtrace::Backtrace::force_capture);
                    tracing::warn!(
                        event_name = "capture_thread_panicked",
                        input = %input,
                        why = %why,
                        backtrace = %backtrace,
                        "the capture thread panicked"
                    );
                    let _ = ready.send(Err(Error::Capture {
                        message: format!("the capture thread panicked: {why}"),
                    }));
                }
            }
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

    /// **A panicking block is caught once, and the capture stops.** The
    /// callback body is Rust called from cpal's frame; an unwind out of it
    /// crosses the FFI boundary and aborts, so the guard is the only thing
    /// standing between a bug in the pipeline and the process. The log cannot
    /// be read from here, but the flag and the dropped later block can.
    #[test]
    fn a_panicking_callback_block_is_caught_and_later_blocks_are_dropped() {
        let panicked = AtomicBool::new(false);
        let ran = std::cell::Cell::new(0_u32);
        guard_block(&panicked, "a stand-in input", || {
            ran.set(ran.get() + 1);
            panic!("a callback panic");
        });
        assert!(panicked.load(Ordering::Acquire), "the first panic stops the capture");
        guard_block(&panicked, "a stand-in input", || ran.set(ran.get() + 1));
        assert_eq!(ran.get(), 1, "a block after the panic returns without touching the pipeline");
    }

    /// Full scale maps to 1.0 and the operator is a division: the reverse
    /// compiles, and would ship every I16 device's audio as nonsense.
    #[test]
    fn an_i16_sample_is_scaled_to_full_scale_one() {
        assert_eq!(i16_sample(0), 0.0, "silence stays silence");
        assert_eq!(i16_sample(-32_768), -1.0, "the negative rail is -1");
        assert!(
            (i16_sample(32_767) - 0.999_969_5).abs() < 1e-6,
            "the positive rail lands just under 1, as cpal's own conversion does"
        );
    }

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

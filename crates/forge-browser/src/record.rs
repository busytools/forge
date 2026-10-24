//! Screen recording: the page viewport as an H.264 MP4 the agent can read.
//!
//! **Apple's encoder, in the host.** The host paints every frame already;
//! a recorder thread takes the newest one at a steady [`FPS`] and hands it to
//! `AVAssetWriter`, so the file is a real MP4/H.264 with no third-party codec
//! in the tree. It is silent by design: the video is the page's viewport and
//! nothing else.
//!
//! **Exactly [`WIDTH`] by [`HEIGHT`].** The service lays the page out at that
//! size for the recording and restores the controller's surface after it, so
//! the pixels in the file are the layout the page had, not a scaled copy of
//! someone's window.

#![allow(unsafe_code, reason = "AVAssetWriter and CoreVideo are Objective-C and C ABIs")]

use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use objc2::Message as _;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterInputPixelBufferAdaptor, AVAssetWriterStatus,
    AVFileTypeMPEG4, AVMediaTypeVideo, AVVideoCodecKey, AVVideoCodecTypeH264, AVVideoHeightKey,
    AVVideoWidthKey,
};
use objc2_core_media::CMTime;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferHeightKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

/// The viewport every file holds.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
/// Frames a second in the file: constant, so playback timing is uniform.
pub const FPS: u32 = 30;
/// The hard cap; a recording stops itself here and the file is kept.
pub const MAX_SECONDS: u64 = 180;

/// `FPS` as the presentation-time scale, kept in step by a test.
const TIMESCALE: i32 = 30;

/// The newest composed frame, shared from the paint worker to the recorder.
///
/// **One `Arc` shared, not a copy per tick**: the worker already owns the
/// composed buffer, and the recorder clones the pointer rather than a megabyte
/// of pixels every frame.
pub type FrameSlot = Arc<Mutex<Option<Arc<Vec<u8>>>>>;

/// How one recording ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub seconds: u64,
    pub bytes: u64,
    pub capped: bool,
    pub error: Option<String>,
}

/// One running recording.
pub struct Recorder {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Outcome>>,
}

impl Recorder {
    /// Start recording into `path`. The writer opens on the recorder thread and
    /// its failure is answered here rather than later.
    pub fn start(path: PathBuf, slot: FrameSlot) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready, ready_rx) = mpsc::channel::<Result<(), String>>();
        let own_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("cef-recorder".to_owned())
            .spawn(move || run(&path, &slot, &own_stop, &ready))
            .map_err(|why| format!("the recorder thread did not start: {why}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Err(why)) => {
                let _ = thread.join();
                Err(why)
            }
            // **A writer that has not answered in five seconds is not one
            // this call can promise**; the stop path still joins the thread.
            Ok(Ok(())) | Err(_) => Ok(Self { stop, thread: Some(thread) }),
        }
    }

    /// Stop and answer what was written.
    pub fn stop(mut self) -> Outcome {
        self.stop.store(true, Ordering::Release);
        self.thread.take().map_or_else(
            || Outcome { seconds: 0, bytes: 0, capped: false, error: None },
            |thread| {
                thread.join().unwrap_or(Outcome {
                    seconds: 0,
                    bytes: 0,
                    capped: false,
                    error: Some("the recorder thread panicked".to_owned()),
                })
            },
        )
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The recorder's own life: open the writer, tick out frames, finish it.
fn run(
    path: &Path,
    slot: &FrameSlot,
    stop: &AtomicBool,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Outcome {
    let writer = match Writer::open(path) {
        Ok(writer) => {
            let _ = ready.send(Ok(()));
            writer
        }
        Err(why) => {
            let _ = ready.send(Err(why.clone()));
            return Outcome { seconds: 0, bytes: 0, capped: false, error: Some(why) };
        }
    };
    writer.record(slot, stop, path)
}

/// The AVFoundation writer, as the recorder thread drives it.
struct Writer {
    writer: Retained<AVAssetWriter>,
    input: Retained<AVAssetWriterInput>,
    adaptor: Retained<AVAssetWriterInputPixelBufferAdaptor>,
    pool: Retained<CVPixelBufferPool>,
}

impl Writer {
    /// Open the MP4 and its one video track. **The page is BGRA**, which is
    /// what CEF paints, so the pixel buffers take rows directly and no colour
    /// conversion runs.
    fn open(path: &Path) -> Result<Self, String> {
        let file = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        // SAFETY: the constants are the system's own, and the URL is a file
        // URL.
        let file_type = unsafe { AVFileTypeMPEG4 }
            .ok_or_else(|| "the MPEG-4 file type is missing".to_owned())?;
        let media = unsafe { AVMediaTypeVideo }
            .ok_or_else(|| "the video media type is missing".to_owned())?;
        let writer = unsafe { AVAssetWriter::assetWriterWithURL_fileType_error(&file, file_type) }
            .map_err(|error| format!("the recording file cannot be written: {error}"))?;
        let settings = video_settings()?;
        // SAFETY: the settings are the video-shaped dictionary this track
        // takes.
        let input = unsafe {
            AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(media, Some(&settings))
        };
        let attributes = pixel_attributes();
        // SAFETY: the attributes name the size and pixel format the frames are
        // copied from.
        let adaptor = unsafe {
            AVAssetWriterInputPixelBufferAdaptor::assetWriterInputPixelBufferAdaptorWithAssetWriterInput_sourcePixelBufferAttributes(
                &input,
                Some(&attributes),
            )
        };
        // SAFETY: the writer and its input are both live here.
        unsafe {
            writer.addInput(&input);
            if !writer.startWriting() {
                return Err(format!(
                    "the recording writer did not start: {}",
                    writer
                        .error()
                        .map_or_else(|| "no reason given".to_owned(), |error| error.to_string())
                ));
            }
            writer.startSessionAtSourceTime(CMTime::new(0, TIMESCALE));
        }
        // SAFETY: the writer has started, so the adaptor has its pool.
        let pool = unsafe { adaptor.pixelBufferPool() }
            .ok_or_else(|| "the recording's pixel buffer pool is missing".to_owned())?;
        Ok(Self { writer, input, adaptor, pool })
    }

    /// Tick out one frame a second's thirtieth, appending the newest picture.
    fn record(&self, slot: &FrameSlot, stop: &AtomicBool, path: &Path) -> Outcome {
        let period = Duration::from_secs(1) / FPS;
        let started = Instant::now();
        let mut next = started;
        let mut frames: u64 = 0;
        let mut last: Option<Arc<Vec<u8>>> = None;
        let mut capped = false;
        let mut error = None;
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            if started.elapsed() >= Duration::from_secs(MAX_SECONDS) {
                capped = true;
                break;
            }
            let newest = slot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(Arc::clone)
                .or_else(|| last.clone());
            if let Some(pixels) = newest {
                last = Some(Arc::clone(&pixels));
                if let Err(why) = self.push(&pixels, frames) {
                    error = Some(why);
                    break;
                }
                frames += 1;
            }
            next += period;
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        // **The writer finishes before the outcome is read**, so the file a
        // tool hands the agent is complete rather than still being written.
        let finished = self.finish();
        if error.is_none() {
            error = finished;
        }
        let bytes = std::fs::metadata(path).map_or(0, |meta| meta.len());
        Outcome { seconds: frames / u64::from(FPS), bytes, capped, error }
    }

    /// Finish the file and answer why it could not be completed, if it could
    /// not.
    fn finish(&self) -> Option<String> {
        // SAFETY: the recording loop has ended, so nothing appends beside the
        // mark; the writer and input are live.
        unsafe { self.input.markAsFinished() };
        let (done, finished) = mpsc::channel::<()>();
        let block = block2::RcBlock::new(move || {
            let _ = done.send(());
        });
        // SAFETY: the handler is a plain block that outlives the call.
        unsafe { self.writer.finishWritingWithCompletionHandler(&block) };
        if finished.recv_timeout(Duration::from_secs(30)).is_err() {
            return Some("the recording writer did not finish within its bound".to_owned());
        }
        // SAFETY: the status is read after the writer reported completion.
        let status = unsafe { self.writer.status() };
        if status == AVAssetWriterStatus::Completed {
            return None;
        }
        // SAFETY: reading the failure reason on a completed writer is an
        // ordinary property read.
        let reason = unsafe { self.writer.error() }
            .map_or_else(|| "no reason given".to_owned(), |error| error.to_string());
        Some(format!("the recording writer did not finish: {reason}"))
    }

    /// One frame, copied into a pool buffer and appended at its time.
    fn push(&self, pixels: &[u8], index: u64) -> Result<(), String> {
        let row = usize::try_from(WIDTH).unwrap_or(0) * 4;
        let height = usize::try_from(HEIGHT).unwrap_or(0);
        // **A frame that is not the recording's viewport is skipped, not an
        // error**: a controller resize during the recording is a frame the
        // file does not need, and the page is laid back out at the size the
        // service asked for.
        if pixels.len() != row * height {
            return Ok(());
        }
        // SAFETY: the input is live for the recording's whole life.
        if !unsafe { self.input.isReadyForMoreMediaData() } {
            return Ok(());
        }
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: the pool is the adaptor's own and `raw` is written on
        // success.
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut raw))
        };
        let Some(buffer) =
            (status == 0 && !raw.is_null()).then(|| unsafe { Retained::from_raw(raw) }).flatten()
        else {
            return Err(format!("a recording frame buffer was refused (status {status})"));
        };
        // SAFETY: the buffer is live and owned here.
        if unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) } != 0 {
            return Err("a recording frame buffer could not be locked".to_owned());
        }
        let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        let fits = !base.is_null() && stride >= row;
        if fits {
            for y in 0..height {
                let source = &pixels[y * row..(y + 1) * row];
                // SAFETY: each row written lies inside the locked buffer,
                // which is at least `stride` bytes wide per row.
                unsafe {
                    std::ptr::copy_nonoverlapping(source.as_ptr(), base.add(y * stride), row);
                }
            }
        }
        // SAFETY: the lock above succeeded and the copy has finished.
        unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) };
        if !fits {
            return Err("a recording frame did not fit its buffer".to_owned());
        }
        // SAFETY: the arguments are a plain value and scale.
        let time = unsafe { CMTime::new(i64::try_from(index).unwrap_or(i64::MAX), TIMESCALE) };
        // SAFETY: the buffer is filled and unlocked, and the input was checked
        // ready above.
        if !unsafe { self.adaptor.appendPixelBuffer_withPresentationTime(&buffer, time) } {
            return Err("a recording frame was not appended".to_owned());
        }
        Ok(())
    }
}

/// The video track's settings: H.264 at the recording's size.
fn video_settings() -> Result<Retained<NSDictionary<NSString, AnyObject>>, String> {
    // SAFETY: the constants are the system's own, and every object can be
    // re-typed as `AnyObject`, which assumes no class.
    let (codec, codec_key, width_key, height_key) = unsafe {
        let codec =
            AVVideoCodecTypeH264.ok_or_else(|| "the H.264 codec type is missing".to_owned())?;
        let codec_key = AVVideoCodecKey.ok_or_else(|| "the codec key is missing".to_owned())?;
        let width_key = AVVideoWidthKey.ok_or_else(|| "the width key is missing".to_owned())?;
        let height_key = AVVideoHeightKey.ok_or_else(|| "the height key is missing".to_owned())?;
        let codec = Retained::cast_unchecked::<AnyObject>(codec.retain());
        (codec, codec_key, width_key, height_key)
    };
    let width = number(WIDTH);
    let height = number(HEIGHT);
    Ok(NSDictionary::from_slices(
        &[codec_key, width_key, height_key],
        &[&*codec, &*width, &*height],
    ))
}

/// The pool's attributes: BGRA at the recording's size.
fn pixel_attributes() -> Retained<NSDictionary<NSString, AnyObject>> {
    let format = number(kCVPixelFormatType_32BGRA);
    let width = number(WIDTH);
    let height = number(HEIGHT);
    // SAFETY: the keys are the system's own CoreVideo constants.
    let (format_key, width_key, height_key) = unsafe {
        (
            cf_key(kCVPixelBufferPixelFormatTypeKey),
            cf_key(kCVPixelBufferWidthKey),
            cf_key(kCVPixelBufferHeightKey),
        )
    };
    NSDictionary::from_slices(&[format_key, width_key, height_key], &[&*format, &*width, &*height])
}

/// One unsigned number as the dictionaries carry it.
fn number(value: u32) -> Retained<AnyObject> {
    // SAFETY: every object can be re-typed as `AnyObject`, which assumes no
    // class.
    unsafe { Retained::cast_unchecked(NSNumber::numberWithUnsignedInt(value)) }
}

/// One CoreVideo attribute key, bridged to the string type the dictionary keys
/// with (CFString and NSString are toll-free).
fn cf_key<T>(key: &'static T) -> &'static NSString {
    // SAFETY: CoreVideo's attribute keys are CFStrings.
    unsafe { &*std::ptr::from_ref(key).cast::<NSString>() }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{FPS, HEIGHT, MAX_SECONDS, Recorder, TIMESCALE, WIDTH};

    /// **The recording's shape is fixed and stated.** The tool promises a
    /// 1280x720 MP4; a constant that drifts breaks the promise silently.
    #[test]
    fn the_recording_shape_is_fixed() {
        assert_eq!((WIDTH, HEIGHT), (1280, 720));
        assert_eq!(u32::try_from(TIMESCALE).unwrap_or(0), FPS);
        assert_eq!(MAX_SECONDS, 180);
    }

    /// **A recording is a real MP4.** Start, paint a frame for a few ticks,
    /// stop: the file exists, it is an MP4 container, and the writer finished
    /// rather than leaving a file still being written.
    #[test]
    fn a_recording_writes_an_mp4() {
        let dir = tempfile::tempdir().expect("a scratch dir");
        let path = dir.path().join("take.mp4");
        let mut pixels = vec![0_u8; usize::try_from(WIDTH * HEIGHT * 4).expect("a frame")];
        for (index, byte) in pixels.iter_mut().enumerate() {
            *byte = u8::try_from(index % 251).unwrap_or(0);
        }
        let slot = Arc::new(Mutex::new(Some(Arc::new(pixels))));
        let recorder = Recorder::start(path.clone(), slot).expect("the recorder starts");
        std::thread::sleep(Duration::from_millis(300));
        let outcome = recorder.stop();
        assert_eq!(outcome.error, None, "the writer finished cleanly");
        assert!(!outcome.capped, "a short take is not capped");
        assert!(outcome.bytes > 0, "the file has bytes");
        let head = std::fs::read(&path).expect("the recording file");
        assert_eq!(&head[4..8], b"ftyp", "the file is an MP4 container");
    }
}

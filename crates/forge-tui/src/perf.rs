//! Explicit high-frequency performance telemetry sidecar.
//!
//! This module is intentionally separate from the main structured runtime logs.
//! Use it only for hot-path timings and counters where writing every sample into
//! the normal operational log stream would create unacceptable noise.
//!
//! What belongs here:
//!
//! - render-frame timing
//! - layout/cache timing and counters
//! - terminal/render hot-path counters
//! - other explicit perf-mode samples
//!
//! What does not belong here:
//!
//! - session or bridge lifecycle
//! - tool, permission, or auth lifecycle
//! - user-facing state changes
//! - raw payloads or content previews
//!
//! Gated behind `--features perf`. When the feature is disabled, all types
//! become zero-size and all methods are no-ops that the compiler eliminates.
//!
//! # Usage
//!
//! ```bash
//! cargo run --features perf -- --perf-log performance.log
//! # Writes JSON lines:
//! # {"schema":"forge-perf/v1","kind":"duration","run_id":"...","frame":1234,"ts_ms":1739599900793,"metric":"chat::render","duration_ms":2.345,"extra":{"key":"msgs","value":42}}
//! ```

#[cfg(feature = "perf")]
mod enabled {
    use serde::Serialize;
    use std::cell::RefCell;
    use std::fs::{File, OpenOptions};
    use std::io::{BufWriter, Write};
    use std::path::Path;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    const PERF_SCHEMA: &str = "forge-perf/v1";

    /// Frame duration (ms) at or above which the per-frame buffer
    /// flushes to disk. 50 ms = below 20 FPS, well into the visible
    /// stutter range. Frames faster than this discard their buffered
    /// samples — the log only carries entries from frames worth
    /// investigating, keeping the file small enough that even
    /// week-long sessions stay manageable.
    const SLOW_FRAME_THRESHOLD_MS: f64 = 50.0;

    /// Cap on per-frame buffer size. Bounds memory if the frame
    /// never closes for some reason (no `frame_total` Timer drop
    /// fires). 1024 spans/marks per frame is comfortably above the
    /// natural per-frame budget for active sessions with many
    /// visible messages + tool calls (observed up to ~250 per slow
    /// frame in chats with 30+ messages). The `frame_total` Timer
    /// itself is always pushed regardless of cap (see `write_entry`)
    /// so the parent span never gets dropped from a flushed batch.
    const FRAME_BUFFER_CAP: usize = 1024;

    /// One buffered sample awaiting the per-frame flush decision.
    /// Storage is cheap (constant size, no heap alloc beyond the
    /// vector backing storage).
    #[derive(Clone)]
    struct BufferedSample {
        name: &'static str,
        ms: f64,
        extra: Option<(&'static str, usize)>,
    }

    // Thread-local file handle so Timer::drop can log without borrowing PerfLogger.
    thread_local! {
        pub(crate) static LOG_FILE: RefCell<Option<BufWriter<File>>> = const { RefCell::new(None) };
        static FRAME_COUNTER: RefCell<u64> = const { RefCell::new(0) };
        static RUN_ID: RefCell<String> = const { RefCell::new(String::new()) };
        static FRAME_BUFFER: RefCell<Vec<BufferedSample>> = const { RefCell::new(Vec::new()) };
    }

    pub struct PerfLogger {
        _private: (),
    }

    #[derive(Serialize)]
    struct PerfExtraField {
        key: &'static str,
        value: usize,
    }

    #[derive(Serialize)]
    struct PerfSample<'a> {
        schema: &'static str,
        kind: &'static str,
        run_id: &'a str,
        frame: u64,
        ts_ms: u128,
        metric: &'a str,
        duration_ms: Option<f64>,
        extra: Option<PerfExtraField>,
    }

    #[derive(Serialize)]
    struct PerfRunStarted<'a> {
        schema: &'static str,
        kind: &'static str,
        run_id: &'a str,
        ts_ms: u128,
        pid: u32,
        version: &'a str,
    }

    fn unix_ms() -> u128 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_millis())
    }

    fn write_json_line<T: Serialize>(file: &mut BufWriter<File>, value: &T) {
        if let Err(err) = serde_json::to_writer(&mut *file, value) {
            tracing::debug!(
                target: "forge_tui::perf",
                error = %err,
                "perf JSON serialize failed"
            );
            return;
        }
        if let Err(err) = writeln!(file) {
            tracing::debug!(
                target: "forge_tui::perf",
                error = %err,
                "perf log newline write failed; JSONL stream may be corrupted from this point"
            );
        }
    }

    pub(crate) fn write_entry(name: &'static str, ms: f64, extra: Option<(&'static str, usize)>) {
        // Fast path: when no `--perf-log` file is open, skip the
        // buffering + decision entirely. Cheap enough that
        // `--features perf` can stay always-on in production builds
        // with no measurable cost.
        let logging_enabled = LOG_FILE.with(|f| f.borrow().is_some());
        if !logging_enabled {
            return;
        }

        // Append to the per-frame buffer. When `name == "frame_total"`
        // (the Timer covering the whole frame), the duration is
        // available — decide whether to flush the buffer (slow
        // frame, useful for diagnosis) or clear it (healthy frame,
        // nothing worth recording). Other entries just accumulate
        // until that decision fires.
        let to_flush: Option<Vec<BufferedSample>> = FRAME_BUFFER.with(|b| {
            let mut buf = b.borrow_mut();
            let is_frame_total = name == "frame_total";
            // `frame_total` is the framing span — without it, a
            // flushed batch can't be tied back to a specific frame
            // duration. Always push it regardless of cap so the
            // parent span survives. Sub-spans get dropped at cap to
            // bound memory.
            if is_frame_total || buf.len() < FRAME_BUFFER_CAP {
                buf.push(BufferedSample { name, ms, extra });
            }
            if !is_frame_total {
                return None;
            }
            if ms >= SLOW_FRAME_THRESHOLD_MS {
                Some(std::mem::take(&mut *buf))
            } else {
                buf.clear();
                None
            }
        });

        let Some(samples) = to_flush else { return };

        // Slow frame — drain the buffer to disk. ts_ms + frame are
        // captured once at flush time; every sample in the batch
        // shares those values because they all belong to the same
        // frame and the per-sample wall-clock distinction isn't
        // useful when the analysis pivot is "which sub-span took
        // too long inside this slow frame."
        let frame = FRAME_COUNTER.with(|c| *c.borrow());
        let ts_ms = unix_ms();
        LOG_FILE.with(|f| {
            let mut file_ref = f.borrow_mut();
            let Some(ref mut file) = *file_ref else {
                return;
            };
            RUN_ID.with(|run| {
                let run_id = run.borrow();
                for sample in &samples {
                    let perf_sample = PerfSample {
                        schema: PERF_SCHEMA,
                        kind: if sample.ms == 0.0 { "mark" } else { "duration" },
                        run_id: run_id.as_str(),
                        frame,
                        ts_ms,
                        metric: sample.name,
                        duration_ms: (sample.ms != 0.0).then_some(sample.ms),
                        extra: sample.extra.map(|(key, value)| PerfExtraField { key, value }),
                    };
                    write_json_line(file, &perf_sample);
                }
            });
        });
    }

    // `start` / `start_with` / `mark` / `mark_with` take `&self` to match
    // call-site ergonomics with the enabled-vs-disabled feature impls,
    // even though the enabled path delegates to thread-local state and
    // doesn't read fields off self directly.
    #[allow(clippy::unused_self)]
    impl PerfLogger {
        /// Open (or create) the log file. Returns `None` on I/O error
        /// after logging the failure at warn level. Always appends —
        /// matches the standard log rolling behaviour so a forge
        /// restart immediately after a perf-relevant bug doesn't
        /// erase the evidence.
        pub fn open(path: &Path) -> Option<Self> {
            let file = match OpenOptions::new().create(true).append(true).open(path) {
                Ok(f) => f,
                Err(err) => {
                    tracing::warn!(
                        target: "forge_tui::perf",
                        path = %path.display(),
                        error = %err,
                        "failed to open perf log; perf telemetry disabled"
                    );
                    return None;
                }
            };
            let mut writer = BufWriter::new(file);
            let run_id = uuid::Uuid::new_v4().to_string();
            let ts_ms = unix_ms();
            let started = PerfRunStarted {
                schema: PERF_SCHEMA,
                kind: "run_started",
                run_id: run_id.as_str(),
                ts_ms,
                pid: std::process::id(),
                version: crate::FORGE_VERSION,
            };
            write_json_line(&mut writer, &started);
            let _ = writer.flush();
            LOG_FILE.with(|f| *f.borrow_mut() = Some(writer));
            RUN_ID.with(|r| *r.borrow_mut() = run_id);
            FRAME_COUNTER.with(|c| *c.borrow_mut() = 0);
            Some(Self { _private: () })
        }

        /// Increment the frame counter. Call once at the start of each render frame.
        pub fn next_frame(&mut self) {
            let frame = FRAME_COUNTER.with(|c| {
                let mut value = c.borrow_mut();
                *value += 1;
                *value
            });
            // Safety net — clear any leftover buffer from a tick
            // that didn't fire `frame_total` (e.g. `needs_redraw`
            // skipped the draw block). Without this, marks from
            // earlier ticks could leak into the next slow-frame
            // flush and confuse the analysis.
            FRAME_BUFFER.with(|b| b.borrow_mut().clear());
            if frame.is_multiple_of(240) {
                LOG_FILE.with(|f| {
                    if let Some(ref mut file) = *f.borrow_mut() {
                        let _ = file.flush();
                    }
                });
            }
        }

        /// Start a named timer. Logs duration on drop.
        pub fn start(&self, name: &'static str) -> Timer {
            Timer { name, start: Instant::now(), extra: None }
        }

        /// Start a named timer with an extra numeric field (e.g. message count).
        pub fn start_with(
            &self,
            name: &'static str,
            extra_name: &'static str,
            extra_val: usize,
        ) -> Timer {
            Timer { name, start: Instant::now(), extra: Some((extra_name, extra_val)) }
        }

        /// Log an instant marker for the current frame (`ms = 0`).
        pub fn mark(&self, name: &'static str) {
            write_entry(name, 0.0, None);
        }

        /// Log an instant marker with an extra numeric field (`ms = 0`).
        pub fn mark_with(&self, name: &'static str, extra_name: &'static str, extra_val: usize) {
            write_entry(name, 0.0, Some((extra_name, extra_val)));
        }
    }

    pub struct Timer {
        pub(crate) name: &'static str,
        pub(crate) start: Instant,
        pub(crate) extra: Option<(&'static str, usize)>,
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            let ms = self.start.elapsed().as_secs_f64() * 1000.0;
            write_entry(self.name, ms, self.extra);
        }
    }
}

#[cfg(not(feature = "perf"))]
mod disabled {
    use std::path::Path;

    pub struct PerfLogger;
    pub struct Timer;

    // Stub impl for the `!perf` feature path — methods are no-ops.
    // The receiver shape matches the `feature = "perf"` impl so call
    // sites compile under both feature flags without an `if` ladder.
    #[allow(clippy::unused_self)]
    impl PerfLogger {
        #[inline]
        pub fn open(_path: &Path) -> Option<Self> {
            None
        }
        #[inline]
        pub fn next_frame(&mut self) {}
        #[inline]
        pub fn start(&self, _name: &'static str) -> Timer {
            Timer
        }
        #[inline]
        pub fn start_with(
            &self,
            _name: &'static str,
            _extra_name: &'static str,
            _extra_val: usize,
        ) -> Timer {
            Timer
        }
        #[inline]
        pub fn mark(&self, _name: &'static str) {}
        #[inline]
        pub fn mark_with(&self, _name: &'static str, _extra_name: &'static str, _extra_val: usize) {
        }
    }

}

/// Start a timer without needing a `PerfLogger` reference.
/// Uses the thread-local log file directly. Returns `None` (and is a no-op)
/// when the `perf` feature is disabled or no logger has been opened.
#[cfg(feature = "perf")]
#[inline]
pub fn start(name: &'static str) -> Option<Timer> {
    // Only create a timer if the log file is actually open
    enabled::LOG_FILE.with(|f| {
        if f.borrow().is_some() {
            Some(Timer { name, start: std::time::Instant::now(), extra: None })
        } else {
            None
        }
    })
}

#[cfg(feature = "perf")]
#[inline]
pub fn start_with(name: &'static str, extra_name: &'static str, extra_val: usize) -> Option<Timer> {
    enabled::LOG_FILE.with(|f| {
        if f.borrow().is_some() {
            Some(Timer {
                name,
                start: std::time::Instant::now(),
                extra: Some((extra_name, extra_val)),
            })
        } else {
            None
        }
    })
}

#[cfg(not(feature = "perf"))]
#[inline]
pub fn start(_name: &'static str) -> Option<Timer> {
    None
}

#[cfg(not(feature = "perf"))]
#[inline]
pub fn start_with(
    _name: &'static str,
    _extra_name: &'static str,
    _extra_val: usize,
) -> Option<Timer> {
    None
}

/// Write an instant marker for the current frame (`ms = 0`).
#[cfg(feature = "perf")]
#[inline]
pub fn mark(name: &'static str) {
    enabled::write_entry(name, 0.0, None);
}

#[cfg(not(feature = "perf"))]
#[inline]
pub fn mark(_name: &'static str) {}

/// Write an instant marker with one numeric field (`ms = 0`).
#[cfg(feature = "perf")]
#[inline]
pub fn mark_with(name: &'static str, extra_name: &'static str, extra_val: usize) {
    enabled::write_entry(name, 0.0, Some((extra_name, extra_val)));
}

#[cfg(not(feature = "perf"))]
#[inline]
pub fn mark_with(_name: &'static str, _extra_name: &'static str, _extra_val: usize) {}

#[cfg(feature = "perf")]
pub use enabled::{PerfLogger, Timer};

#[cfg(not(feature = "perf"))]
pub use disabled::{PerfLogger, Timer};

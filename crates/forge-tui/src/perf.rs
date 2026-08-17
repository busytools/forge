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
//!
//! # Reading the log
//!
//! Key on `(metric, kind)` and never fall back across kinds: take
//! `duration_ms` only from `duration` records and `extra.value` only
//! from `mark` records. Logs written before #516 classified a span as
//! a `mark` whenever its duration rounded to zero, so one metric can
//! appear under both kinds and the fallback silently reads an
//! `extra.value` into a millisecond column.
//!
//! `kind: "frame_summary"` is a periodic window aggregate rather than
//! a per-frame record - it carries no `metric`, and its percentiles
//! are bucket upper bounds. Take frame cost from its `drain` /
//! `render` split rather than from `frame_total`, which brackets
//! `terminal.draw` alone and has never included the drain phase. That
//! split can still understate a pass that blocked applying an update
//! on the select arm, which sits outside `drain`; `updates` is the
//! phase that counts both apply sites.
//!
//! Always pin `run_id` too - the file is appended to across restarts,
//! so an unfiltered query measures several binaries at once, and older
//! history rolls into `forge-perf.log.1` through `.5` rather than
//! staying in the file you are reading.

#[cfg(feature = "perf")]
mod enabled {
    use crate::logging::{LOG_ROTATION_MAX_BYTES, LOG_ROTATION_MAX_FILES, RollingFileWriter};
    use serde::Serialize;
    use std::cell::RefCell;
    use std::io::Write;
    use std::path::Path;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const PERF_SCHEMA: &str = "forge-perf/v1";

    /// How often the rolling frame-cost window is summarised. Ten
    /// seconds still holds thousands of samples per window while
    /// keeping the log to a few hundred lines an hour.
    const FRAME_SUMMARY_INTERVAL: Duration = Duration::from_secs(10);

    /// Bucket upper bounds (ms) for the frame-cost histogram. Dense
    /// below 8 ms because the frame budget is 4 ms; coarse above it
    /// because `SLOW_FRAME_THRESHOLD_MS` already captures that range
    /// in full detail.
    const BUCKET_BOUNDS_MS: [f64; 24] = [
        0.05, 0.1, 0.25, 0.5, 0.75, 1.0, 1.5, 2.0, 2.5, 3.0, 4.0, 5.0, 6.0, 8.0, 10.0, 13.0, 16.0,
        20.0, 25.0, 33.0, 50.0, 100.0, 250.0, 1000.0,
    ];

    /// Frame duration (ms) at or above which the per-frame buffer
    /// flushes to disk. 50 ms = below 20 FPS, well into the visible
    /// stutter range. Frames faster than this discard their buffered
    /// samples - the log only carries entries from frames worth
    /// investigating, keeping the file small enough that even
    /// week-long sessions stay manageable.
    const SLOW_FRAME_THRESHOLD_MS: f64 = 50.0;

    /// Cap on per-frame buffer size. Bounds memory if the frame
    /// never closes for some reason (no `frame_total` Timer drop
    /// fires). 1024 spans/marks per frame is comfortably above the
    /// natural per-frame budget for active sessions with many
    /// visible messages + tool calls (observed up to ~250 per slow
    /// frame in chats with 30+ messages). Parent spans (`frame::*`,
    /// `ui::*`, `frame_total`) are always pushed regardless of cap
    /// so the structural framing of a slow frame survives a
    /// sub-event overflow; only the leaf sub-events drop at cap.
    const FRAME_BUFFER_CAP: usize = 1024;

    /// Name prefixes for frame-level "parent" spans whose presence
    /// is structural for analysis. Parents are exempt from
    /// `FRAME_BUFFER_CAP` because they emit late in the frame's
    /// scope (the bracketing `Timer` drops at end of block) and
    /// would otherwise be silently lost when sub-events fill the
    /// buffer first. Matches the emit sites in `app.rs`
    /// (`frame::terminal_draw`) and `ui/chat_view.rs` (`ui::*`).
    const PARENT_SPAN_PREFIXES: &[&str] = &["frame::", "ui::"];

    fn is_parent_span(name: &str) -> bool {
        name == "frame_total" || PARENT_SPAN_PREFIXES.iter().any(|p| name.starts_with(p))
    }

    /// Which emitter produced a sample. Carried rather than inferred
    /// from the duration: a `Timer` span can legitimately measure
    /// 0.0 ms, and reading that as a marker discards the measurement
    /// (#516).
    #[derive(Clone, Copy)]
    pub(crate) enum SampleKind {
        Mark,
        Duration,
    }

    impl SampleKind {
        fn as_str(self) -> &'static str {
            match self {
                Self::Mark => "mark",
                Self::Duration => "duration",
            }
        }
    }

    /// One buffered sample awaiting the per-frame flush decision.
    /// Storage is cheap (constant size, no heap alloc beyond the
    /// vector backing storage).
    #[derive(Clone)]
    struct BufferedSample {
        name: &'static str,
        kind: SampleKind,
        ms: f64,
        extra: Option<(&'static str, usize)>,
    }

    // Thread-local file handle so Timer::drop can log without borrowing PerfLogger.
    thread_local! {
        pub(crate) static LOG_FILE: RefCell<Option<RollingFileWriter>> =
            const { RefCell::new(None) };
        static FRAME_COUNTER: RefCell<u64> = const { RefCell::new(0) };
        static RUN_ID: RefCell<String> = const { RefCell::new(String::new()) };
        static FRAME_BUFFER: RefCell<Vec<BufferedSample>> = const { RefCell::new(Vec::new()) };
        static FRAME_WINDOW: RefCell<Option<FrameWindow>> = const { RefCell::new(None) };
    }

    /// Fixed-width histogram over one loop phase's per-iteration cost.
    /// Every field is inline storage, so recording never allocates.
    #[derive(Default)]
    struct PhaseHistogram {
        buckets: [u32; BUCKET_BOUNDS_MS.len() + 1],
        count: u64,
        total_ms: f64,
        max_ms: f64,
    }

    impl PhaseHistogram {
        fn record(&mut self, ms: f64) {
            let idx = BUCKET_BOUNDS_MS
                .iter()
                .position(|bound| ms <= *bound)
                .unwrap_or(BUCKET_BOUNDS_MS.len());
            self.buckets[idx] = self.buckets[idx].saturating_add(1);
            self.count += 1;
            self.total_ms += ms;
            if ms > self.max_ms {
                self.max_ms = ms;
            }
        }

        /// Reports the upper bound of the bucket the rank lands in, so
        /// a quoted cost is never lower than the real one.
        fn percentile_ms(&self, pct: u64) -> f64 {
            if self.count == 0 {
                return 0.0;
            }
            let target = self.count.saturating_mul(pct).div_ceil(100);
            let mut seen = 0_u64;
            for (idx, hits) in self.buckets.iter().enumerate() {
                seen += u64::from(*hits);
                if seen >= target {
                    return BUCKET_BOUNDS_MS.get(idx).copied().unwrap_or(self.max_ms);
                }
            }
            self.max_ms
        }

        fn summary(&self) -> PhaseSummary {
            PhaseSummary {
                p50_ms: self.percentile_ms(50),
                p90_ms: self.percentile_ms(90),
                p99_ms: self.percentile_ms(99),
                max_ms: self.max_ms,
                total_ms: self.total_ms,
            }
        }
    }

    /// One flush window's worth of app-loop cost.
    struct FrameWindow {
        started: Instant,
        iters: u64,
        renders: u64,
        animating_iters: u64,
        animating_renders: u64,
        drain: PhaseHistogram,
        input: PhaseHistogram,
        updates: PhaseHistogram,
        render: PhaseHistogram,
    }

    impl FrameWindow {
        fn new() -> Self {
            Self {
                started: Instant::now(),
                iters: 0,
                renders: 0,
                animating_iters: 0,
                animating_renders: 0,
                drain: PhaseHistogram::default(),
                input: PhaseHistogram::default(),
                updates: PhaseHistogram::default(),
                render: PhaseHistogram::default(),
            }
        }
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
    struct PhaseSummary {
        p50_ms: f64,
        p90_ms: f64,
        p99_ms: f64,
        max_ms: f64,
        total_ms: f64,
    }

    #[derive(Serialize)]
    struct PerfFrameSummary<'a> {
        schema: &'static str,
        kind: &'static str,
        run_id: &'a str,
        ts_ms: u128,
        window_ms: f64,
        iters: u64,
        renders: u64,
        no_render: u64,
        animating_iters: u64,
        animating_renders: u64,
        drain: PhaseSummary,
        input: PhaseSummary,
        updates: PhaseSummary,
        render: PhaseSummary,
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

    // One `write_all` per record: the rolling writer decides rotation
    // from the length it is handed, so a record streamed in pieces can
    // be split across the roll.
    fn write_json_line<T: Serialize>(file: &mut RollingFileWriter, value: &T) {
        let mut line = match serde_json::to_vec(value) {
            Ok(line) => line,
            Err(err) => {
                tracing::debug!(
                    target: "forge_tui::perf",
                    error = %err,
                    "perf JSON serialize failed"
                );
                return;
            }
        };
        line.push(b'\n');
        if let Err(err) = file.write_all(&line) {
            tracing::debug!(
                target: "forge_tui::perf",
                error = %err,
                "perf log write failed; JSONL stream may be corrupted from this point"
            );
        }
    }

    pub(crate) fn write_entry(
        name: &'static str,
        kind: SampleKind,
        ms: f64,
        extra: Option<(&'static str, usize)>,
    ) {
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
        // available - decide whether to flush the buffer (slow
        // frame, useful for diagnosis) or clear it (healthy frame,
        // nothing worth recording). Other entries just accumulate
        // until that decision fires.
        let to_flush: Option<Vec<BufferedSample>> = FRAME_BUFFER.with(|b| {
            let mut buf = b.borrow_mut();
            let is_frame_total = name == "frame_total";
            // Parent spans (`frame::*`, `ui::*`, `frame_total`) are
            // exempt from the cap so the framing of a slow frame
            // survives even when sub-events have already filled
            // the buffer. Without this, a frame with 200+ chat /
            // tool-call sub-events plus its 5-10 pane parents
            // would silently lose the parents because parent
            // Timers drop at end of scope, after sub-events have
            // already pushed.
            if is_parent_span(name) || buf.len() < FRAME_BUFFER_CAP {
                buf.push(BufferedSample { name, kind, ms, extra });
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

        // Slow frame - drain the buffer to disk. ts_ms + frame are
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
                        kind: sample.kind.as_str(),
                        run_id: run_id.as_str(),
                        frame,
                        ts_ms,
                        metric: sample.name,
                        duration_ms: match sample.kind {
                            SampleKind::Duration => Some(sample.ms),
                            SampleKind::Mark => None,
                        },
                        extra: sample.extra.map(|(key, value)| PerfExtraField { key, value }),
                    };
                    write_json_line(file, &perf_sample);
                }
            });
        });
    }

    /// Fold one pass of the app loop into the open window. Called on
    /// every iteration, including the ones that never render, so it
    /// touches nothing but fixed-size state.
    pub(crate) fn record_iteration(cost: super::IterationCost) {
        let logging_enabled = LOG_FILE.with(|f| f.borrow().is_some());
        if !logging_enabled {
            return;
        }

        let due = FRAME_WINDOW.with(|w| {
            let mut slot = w.borrow_mut();
            let window = slot.get_or_insert_with(FrameWindow::new);
            window.iters += 1;
            window.animating_iters += u64::from(cost.animating);
            window.drain.record(cost.drain_ms);
            window.input.record(cost.input_ms);
            window.updates.record(cost.updates_ms);
            if let Some(ms) = cost.render_ms {
                window.renders += 1;
                window.animating_renders += u64::from(cost.animating);
                window.render.record(ms);
            }
            window.started.elapsed() >= FRAME_SUMMARY_INTERVAL
        });

        if due {
            flush_frame_summary();
        }
    }

    /// Write the open window as one line and start a fresh one. Also
    /// runs at shutdown so a partial window still lands.
    pub(crate) fn flush_frame_summary() {
        let Some(window) = FRAME_WINDOW.with(|w| w.borrow_mut().take()) else {
            return;
        };
        if window.iters == 0 {
            return;
        }
        let ts_ms = unix_ms();
        let window_ms = window.started.elapsed().as_secs_f64() * 1000.0;
        LOG_FILE.with(|f| {
            let mut file_ref = f.borrow_mut();
            let Some(ref mut file) = *file_ref else {
                return;
            };
            RUN_ID.with(|run| {
                let run_id = run.borrow();
                let summary = PerfFrameSummary {
                    schema: PERF_SCHEMA,
                    kind: "frame_summary",
                    run_id: run_id.as_str(),
                    ts_ms,
                    window_ms,
                    iters: window.iters,
                    renders: window.renders,
                    no_render: window.iters.saturating_sub(window.renders),
                    animating_iters: window.animating_iters,
                    animating_renders: window.animating_renders,
                    drain: window.drain.summary(),
                    input: window.input.summary(),
                    updates: window.updates.summary(),
                    render: window.render.summary(),
                };
                write_json_line(file, &summary);
            });
            // One flush per window is free at this cadence and stops a
            // summary sitting in the writer until the next slow frame.
            let _ = file.flush();
        });
    }

    // `start` / `start_with` / `mark` / `mark_with` take `&self` to match
    // call-site ergonomics with the enabled-vs-disabled feature impls,
    // even though the enabled path delegates to thread-local state and
    // doesn't read fields off self directly.
    #[allow(clippy::unused_self)]
    impl PerfLogger {
        /// Open (or create) the log file. Returns `None` on I/O error
        /// after logging the failure at warn level. Appends under the
        /// same size cap and rolled-file window as the tracing log.
        pub fn open(path: &Path) -> Option<Self> {
            let mut writer = match RollingFileWriter::new(
                path,
                true,
                LOG_ROTATION_MAX_BYTES,
                LOG_ROTATION_MAX_FILES,
            ) {
                Ok(writer) => writer,
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
            // Safety net - clear any leftover buffer from a tick
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

        /// Log an instant marker for the current frame.
        pub fn mark(&self, name: &'static str) {
            write_entry(name, SampleKind::Mark, 0.0, None);
        }

        /// Log an instant marker with an extra numeric field.
        pub fn mark_with(&self, name: &'static str, extra_name: &'static str, extra_val: usize) {
            write_entry(name, SampleKind::Mark, 0.0, Some((extra_name, extra_val)));
        }
    }

    impl Drop for PerfLogger {
        fn drop(&mut self) {
            flush_frame_summary();
            LOG_FILE.with(|f| {
                if let Some(ref mut file) = *f.borrow_mut() {
                    let _ = file.flush();
                }
            });
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
            write_entry(self.name, SampleKind::Duration, ms, self.extra);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs::File;
        use std::io::Read;

        /// Drain the in-memory thread-local state between tests so a
        /// previous test's leftover buffer / log handle can't leak
        /// across cases. Tests run on separate threads under nextest,
        /// so this is belt-and-braces, but cheap.
        fn reset_thread_locals() {
            LOG_FILE.with(|f| *f.borrow_mut() = None);
            FRAME_COUNTER.with(|c| *c.borrow_mut() = 0);
            RUN_ID.with(|r| r.borrow_mut().clear());
            FRAME_BUFFER.with(|b| b.borrow_mut().clear());
            FRAME_WINDOW.with(|w| *w.borrow_mut() = None);
        }

        fn read_summary(path: &Path) -> serde_json::Value {
            read_log_lines(path)
                .iter()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|v| {
                    v.get("kind").and_then(serde_json::Value::as_str) == Some("frame_summary")
                })
                .expect("frame summary present")
        }

        fn cost(
            drain_ms: f64,
            input_ms: f64,
            updates_ms: f64,
            render_ms: Option<f64>,
            animating: bool,
        ) -> super::super::IterationCost {
            super::super::IterationCost { drain_ms, input_ms, updates_ms, render_ms, animating }
        }

        fn assert_close(actual: &serde_json::Value, expected: f64) {
            let got = actual.as_f64().expect("numeric field");
            assert!((got - expected).abs() < 1e-6, "expected {expected}, got {got}");
        }

        /// Drop the BufWriter so its contents land on disk before
        /// we read the file back. `Self::open` stamps the writer
        /// into LOG_FILE; replacing it with None forces a drop.
        fn close_log_file() {
            LOG_FILE.with(|f| *f.borrow_mut() = None);
        }

        fn read_log_lines(path: &Path) -> Vec<String> {
            let mut buf = String::new();
            File::open(path).unwrap().read_to_string(&mut buf).unwrap();
            buf.lines().map(str::to_owned).collect()
        }

        #[test]
        fn parent_spans_survive_when_subevents_exceed_buffer_cap() {
            // Reproduces the #213 scenario: a frame whose sub-event
            // count exceeds `FRAME_BUFFER_CAP` before its parent
            // spans (`ui::render`, `ui::chat`, `frame::terminal_draw`)
            // get pushed. Previously the parents were silently
            // dropped from the flushed batch because only
            // `frame_total` was exempt from the cap; the fix extends
            // the exemption to every `ui::*` / `frame::*` prefix.
            reset_thread_locals();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let _logger = PerfLogger::open(tmp.path()).expect("perf log opens");

            // Fill the buffer past cap with sub-events.
            for _ in 0..(FRAME_BUFFER_CAP + 10) {
                write_entry("msg::cache_miss", SampleKind::Mark, 0.0, None);
            }
            // Parents emit at end-of-frame after sub-events have
            // filled the buffer.
            write_entry("ui::chat", SampleKind::Duration, 1.0, None);
            write_entry("ui::render", SampleKind::Duration, 2.0, None);
            write_entry("frame::terminal_draw", SampleKind::Duration, 3.0, None);
            // Slow `frame_total` triggers the flush.
            write_entry("frame_total", SampleKind::Duration, SLOW_FRAME_THRESHOLD_MS + 1.0, None);

            close_log_file();
            let lines = read_log_lines(tmp.path());

            let metrics: Vec<String> = lines
                .iter()
                .filter_map(|line| {
                    let v: serde_json::Value = serde_json::from_str(line).ok()?;
                    v.get("metric")?.as_str().map(str::to_owned)
                })
                .collect();

            let has = |needle: &str| metrics.iter().any(|m| m == needle);
            assert!(has("ui::chat"), "ui::chat missing from flushed batch");
            assert!(has("ui::render"), "ui::render missing from flushed batch");
            assert!(has("frame::terminal_draw"), "frame::terminal_draw missing from flushed batch");
            assert!(has("frame_total"), "frame_total missing from flushed batch");
        }

        #[test]
        fn non_parent_subevents_capped_at_buffer_limit() {
            // Inverse contract for `parent_spans_survive_*`: non-
            // parent sub-events that overflow the cap MUST drop. A
            // future broadening of `is_parent_span` (e.g. to a
            // `contains` check, or a prefix that catches `msg::`)
            // would silently let memory grow per-frame; this test
            // pins the exact post-flush metric count so that drift
            // trips the check.
            reset_thread_locals();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let _logger = PerfLogger::open(tmp.path()).expect("perf log opens");

            for _ in 0..(FRAME_BUFFER_CAP + 10) {
                write_entry("msg::cache_miss", SampleKind::Mark, 0.0, None);
            }
            write_entry("frame_total", SampleKind::Duration, SLOW_FRAME_THRESHOLD_MS + 1.0, None);

            close_log_file();
            let lines = read_log_lines(tmp.path());

            let metrics: Vec<String> = lines
                .iter()
                .filter_map(|line| {
                    let v: serde_json::Value = serde_json::from_str(line).ok()?;
                    v.get("metric")?.as_str().map(str::to_owned)
                })
                .collect();
            // Cap sub-events + `frame_total` (parent, always pushed)
            // = FRAME_BUFFER_CAP + 1 entries.
            assert_eq!(metrics.len(), FRAME_BUFFER_CAP + 1);
            assert_eq!(
                metrics.iter().filter(|m| *m == "msg::cache_miss").count(),
                FRAME_BUFFER_CAP
            );
            assert_eq!(metrics.iter().filter(|m| *m == "frame_total").count(), 1);
        }

        #[test]
        fn zero_length_span_still_records_as_a_duration() {
            // #516: the record kind used to be derived from the value
            // (`ms == 0.0` meant "mark"), so any Timer whose span
            // rounded to exactly zero went out as a point marker with
            // a null duration and only its `extra` intact.
            // `chat::paragraph_build` did it on 1379 of 3307 records
            // from a single `start_with` call site, which reads as a
            // line count sitting in a millisecond column.
            reset_thread_locals();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let _logger = PerfLogger::open(tmp.path()).expect("perf log opens");

            write_entry("chat::paragraph_build", SampleKind::Duration, 0.0, Some(("lines", 57)));
            write_entry("frame_total", SampleKind::Duration, SLOW_FRAME_THRESHOLD_MS + 1.0, None);

            close_log_file();
            let lines = read_log_lines(tmp.path());
            let record = lines
                .iter()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|v| {
                    v.get("metric").and_then(serde_json::Value::as_str)
                        == Some("chat::paragraph_build")
                })
                .expect("paragraph_build record present");

            assert_eq!(record["kind"], "duration");
            assert_eq!(record["duration_ms"], 0.0);
        }

        #[test]
        fn mark_records_as_a_mark_with_no_duration() {
            // Inverse of `zero_length_span_still_records_as_a_duration`:
            // a deliberate marker keeps a null duration, so the two
            // stay distinguishable in the log and a consumer can key
            // on `(metric, kind)` without falling back across kinds.
            reset_thread_locals();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let _logger = PerfLogger::open(tmp.path()).expect("perf log opens");

            write_entry("msg::cache_miss", SampleKind::Mark, 0.0, Some(("msgs", 42)));
            write_entry("frame_total", SampleKind::Duration, SLOW_FRAME_THRESHOLD_MS + 1.0, None);

            close_log_file();
            let lines = read_log_lines(tmp.path());
            let record = lines
                .iter()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|v| {
                    v.get("metric").and_then(serde_json::Value::as_str) == Some("msg::cache_miss")
                })
                .expect("cache_miss record present");

            assert_eq!(record["kind"], "mark");
            assert!(record["duration_ms"].is_null());
            assert_eq!(record["extra"]["value"], 42);
        }

        #[test]
        fn timer_drop_always_emits_a_duration() {
            // Pins the emitter-side half of #516 through the public
            // API: whatever the clock reports, a dropped Timer is a
            // duration record. Guards against the classification
            // drifting back onto the value.
            reset_thread_locals();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let logger = PerfLogger::open(tmp.path()).expect("perf log opens");

            drop(logger.start("chat::paragraph_build"));
            write_entry("frame_total", SampleKind::Duration, SLOW_FRAME_THRESHOLD_MS + 1.0, None);

            close_log_file();
            let lines = read_log_lines(tmp.path());
            let record = lines
                .iter()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|v| {
                    v.get("metric").and_then(serde_json::Value::as_str)
                        == Some("chat::paragraph_build")
                })
                .expect("paragraph_build record present");

            assert_eq!(record["kind"], "duration");
            assert!(record["duration_ms"].is_number());
        }

        #[test]
        fn frame_summary_splits_drain_from_render_and_counts_non_rendering_iterations() {
            // The drain/render split is the whole point: `frame_total`
            // brackets `terminal.draw` alone, so no existing record
            // attributes any cost to the drain phase.
            reset_thread_locals();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let _logger = PerfLogger::open(tmp.path()).expect("perf log opens");

            for _ in 0..6 {
                record_iteration(cost(0.3, 0.05, 0.1, None, false));
            }
            for _ in 0..4 {
                record_iteration(cost(0.3, 0.05, 0.1, Some(6.0), true));
            }
            flush_frame_summary();

            close_log_file();
            let summary = read_summary(tmp.path());

            assert_eq!(summary["iters"], 10);
            assert_eq!(summary["renders"], 4);
            assert_eq!(summary["no_render"], 6);
            assert_eq!(summary["animating_iters"], 4);
            assert_eq!(summary["animating_renders"], 4);
            // Drain is charged on every iteration, render only on the
            // four that drew - neither total absorbs the other.
            assert_close(&summary["drain"]["total_ms"], 3.0);
            assert_close(&summary["input"]["total_ms"], 0.5);
            assert_close(&summary["updates"]["total_ms"], 1.0);
            assert_close(&summary["render"]["total_ms"], 24.0);
            assert_close(&summary["render"]["max_ms"], 6.0);
            assert_close(&summary["drain"]["max_ms"], 0.3);
        }

        #[test]
        fn frame_summary_percentiles_resolve_below_the_frame_budget() {
            // A 2ms frame and a 12ms frame are indistinguishable under
            // `SLOW_FRAME_THRESHOLD_MS` - both are discarded. The
            // histogram has to separate them to say anything about a
            // 4ms budget.
            reset_thread_locals();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let _logger = PerfLogger::open(tmp.path()).expect("perf log opens");

            for _ in 0..90 {
                record_iteration(cost(0.0, 0.0, 0.0, Some(2.0), true));
            }
            for _ in 0..9 {
                record_iteration(cost(0.0, 0.0, 0.0, Some(12.0), true));
            }
            record_iteration(cost(0.0, 0.0, 0.0, Some(80.0), true));
            flush_frame_summary();

            close_log_file();
            let render = read_summary(tmp.path())["render"].clone();

            assert_close(&render["p50_ms"], 2.0);
            assert_close(&render["p90_ms"], 2.0);
            assert_close(&render["p99_ms"], 13.0);
            assert_close(&render["max_ms"], 80.0);
        }

        #[test]
        fn thousands_of_iterations_write_one_line_and_never_buffer() {
            // Bounded output is a hard constraint (the live log reached 554MB)
            // and the aggregator must stay clear of `FRAME_BUFFER`,
            // whose `frame::` prefix exemption is uncapped.
            reset_thread_locals();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let _logger = PerfLogger::open(tmp.path()).expect("perf log opens");

            for _ in 0..5000 {
                record_iteration(cost(0.3, 0.05, 0.1, Some(1.0), true));
            }
            FRAME_BUFFER.with(|b| assert!(b.borrow().is_empty()));
            assert_eq!(read_log_lines(tmp.path()).len(), 1, "only the run_started header so far");

            flush_frame_summary();
            close_log_file();
            let lines = read_log_lines(tmp.path());
            assert_eq!(lines.len(), 2, "5000 iterations collapse to one summary line");
            assert_eq!(read_summary(tmp.path())["iters"], 5000);
        }

        #[test]
        fn a_rotation_never_splits_a_record_across_two_files() {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().join("perf.log");
            let mut writer = RollingFileWriter::new(&base, false, 120, 2).unwrap();

            for frame in 0..8 {
                let sample = PerfSample {
                    schema: PERF_SCHEMA,
                    kind: "duration",
                    run_id: "test-run",
                    frame,
                    ts_ms: 1,
                    metric: "frame_total",
                    duration_ms: Some(1.0),
                    extra: None,
                };
                write_json_line(&mut writer, &sample);
            }
            writer.flush().unwrap();

            for path in [base.clone(), base.with_extension("log.1"), base.with_extension("log.2")] {
                let Ok(contents) = std::fs::read_to_string(&path) else { continue };
                for line in contents.lines().filter(|line| !line.is_empty()) {
                    assert!(
                        serde_json::from_str::<serde_json::Value>(line).is_ok(),
                        "unparseable line in {}: {line}",
                        path.display()
                    );
                }
            }
        }

        #[test]
        fn an_oversized_log_is_rolled_away_when_it_opens() {
            reset_thread_locals();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("forge-perf.log");
            File::create(&path)
                .unwrap()
                .set_len(crate::logging::LOG_ROTATION_MAX_BYTES + 1)
                .unwrap();

            let _logger = PerfLogger::open(&path).expect("perf log opens");
            close_log_file();

            let len = std::fs::metadata(&path).unwrap().len();
            assert!(
                len < crate::logging::LOG_ROTATION_MAX_BYTES,
                "an over-cap log should be rolled at open, not appended to; got {len} bytes"
            );
            assert!(
                !dir.path().join("forge-perf.log.1").exists(),
                "the superseded log is discarded rather than kept alongside the fresh one"
            );
        }

        #[test]
        fn fast_frame_discards_buffered_samples() {
            // Healthy frame: buffer accumulates a few samples, then
            // `frame_total` arrives below the slow threshold; nothing
            // gets written beyond the per-run `run_started` header
            // AND the buffer empties so leftover samples can't leak
            // into the next slow-frame flush.
            reset_thread_locals();
            let tmp = tempfile::NamedTempFile::new().unwrap();
            let _logger = PerfLogger::open(tmp.path()).expect("perf log opens");

            write_entry("ui::chat", SampleKind::Duration, 0.5, None);
            write_entry("msg::cache_hit", SampleKind::Mark, 0.0, None);
            write_entry("frame_total", SampleKind::Duration, SLOW_FRAME_THRESHOLD_MS / 2.0, None);

            close_log_file();
            let lines = read_log_lines(tmp.path());

            let sample_kinds: Vec<String> = lines
                .iter()
                .filter_map(|line| {
                    let v: serde_json::Value = serde_json::from_str(line).ok()?;
                    v.get("kind")?.as_str().map(str::to_owned)
                })
                .collect();
            // Only the run_started header lands; no per-sample
            // entries.
            assert_eq!(sample_kinds, vec!["run_started".to_owned()]);
            // And the buffer itself empties so a subsequent slow
            // frame can't pick up stale entries from this one. Pins
            // the `buf.clear()` branch against an accidental swap to
            // a partial drain.
            FRAME_BUFFER.with(|b| assert!(b.borrow().is_empty()));
        }
    }
}

#[cfg(not(feature = "perf"))]
mod disabled {
    use std::path::Path;

    pub struct PerfLogger;
    pub struct Timer;

    // Stub impl for the `!perf` feature path - methods are no-ops.
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

/// Write an instant marker for the current frame.
#[cfg(feature = "perf")]
#[inline]
pub fn mark(name: &'static str) {
    enabled::write_entry(name, enabled::SampleKind::Mark, 0.0, None);
}

#[cfg(not(feature = "perf"))]
#[inline]
pub fn mark(_name: &'static str) {}

/// Write an instant marker with one numeric field.
#[cfg(feature = "perf")]
#[inline]
pub fn mark_with(name: &'static str, extra_name: &'static str, extra_val: usize) {
    enabled::write_entry(name, enabled::SampleKind::Mark, 0.0, Some((extra_name, extra_val)));
}

#[cfg(not(feature = "perf"))]
#[inline]
pub fn mark_with(_name: &'static str, _extra_name: &'static str, _extra_val: usize) {}

/// Start timing a loop phase. `None` when the `perf` feature is off or
/// no log is open, which makes every downstream call a no-op.
#[cfg(feature = "perf")]
#[inline]
pub fn phase_start() -> Option<std::time::Instant> {
    enabled::LOG_FILE.with(|f| f.borrow().is_some().then(std::time::Instant::now))
}

#[cfg(not(feature = "perf"))]
#[inline]
pub fn phase_start() -> Option<std::time::Instant> {
    None
}

/// Milliseconds elapsed since `start`, or 0.0 when timing is off.
#[cfg(feature = "perf")]
#[inline]
pub fn phase_ms(start: Option<std::time::Instant>) -> f64 {
    start.map_or(0.0, |at| at.elapsed().as_secs_f64() * 1000.0)
}

#[cfg(not(feature = "perf"))]
#[inline]
pub fn phase_ms(_start: Option<std::time::Instant>) -> f64 {
    0.0
}

/// What one pass of the app loop cost. `updates` totals every
/// `apply_session_update` call in the pass, the select arm's included,
/// so it is not a slice of `drain` - an update applied on the arm
/// lands outside the drain phase entirely. `input` is the select arm's
/// terminal-event handling. `render_ms` is `None` on a pass that
/// drained without drawing.
pub struct IterationCost {
    pub drain_ms: f64,
    pub input_ms: f64,
    pub updates_ms: f64,
    pub render_ms: Option<f64>,
    pub animating: bool,
}

/// Fold one app-loop iteration into the rolling frame-cost window.
#[cfg(feature = "perf")]
#[inline]
pub fn record_iteration(cost: IterationCost) {
    enabled::record_iteration(cost);
}

#[cfg(not(feature = "perf"))]
#[inline]
pub fn record_iteration(_cost: IterationCost) {}

#[cfg(feature = "perf")]
pub use enabled::{PerfLogger, Timer};

#[cfg(not(feature = "perf"))]
pub use disabled::{PerfLogger, Timer};

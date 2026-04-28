//! No-op perf tracking stub. Their UI files call `perf::mark(...)` etc.
//! to record timing in a JSON profile file when their `perf` feature
//! is on. Forge doesn't run their profile harness, so these are
//! no-ops — but the symbols need to exist so lifted code compiles
//! without per-call rewrites.
//!
//! If we ever want to actually measure render perf later, this is the
//! single place to wire `tracing-flame` or similar.

#![allow(dead_code, clippy::needless_pass_by_value, missing_docs)]

use std::path::Path;

#[derive(Clone, Copy, Default)]
pub struct Profiler;

impl Profiler {
    #[must_use]
    pub fn open(_path: &Path, _append: bool) -> Option<Self> {
        None
    }

    pub fn next_frame(&mut self) {}

    #[must_use]
    pub fn start(&self, _name: &'static str) -> Timer {
        Timer
    }

    #[must_use]
    pub fn start_with(
        &self,
        _name: &'static str,
        _extra_name: &'static str,
        _extra_val: usize,
    ) -> Timer {
        Timer
    }

    pub fn mark(&self, _name: &'static str) {}

    pub fn mark_with(&self, _name: &'static str, _extra_name: &'static str, _extra_val: usize) {}

    pub fn stop(self) {}
}

#[derive(Default)]
pub struct Timer;

impl Drop for Timer {
    fn drop(&mut self) {}
}

/// Free-function shorthand used widely in their codebase. No-op.
pub fn mark(_name: &'static str) {}

/// Free-function shorthand with extra payload. No-op.
pub fn mark_with(_name: &'static str, _extra_name: &'static str, _extra_val: usize) {}

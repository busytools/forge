//! Hand-rolled ISO-8601 conversion shared between connection metadata
//! ([`Connection::connected_at_iso`](crate::connection::Connection)) and
//! pending-prompt views ([`PromptQueue`](crate::prompt_queue::PromptQueue))
//! so the daemon's wire shapes use a single canonical format.
//!
//! Hand-rolled rather than pulling `time`/`chrono` to keep the dep set
//! lean. Precision is whole seconds with a `Z` suffix — good enough for
//! the v1 wire-shape display; swap if leap-seconds or fractional
//! precision become load-bearing.
//!
//! `format_iso8601` is the public entry point; the integer math helpers
//! are pub-crate so the prompt-queue module's existing call site can
//! continue to use them without re-implementing.

use std::time::SystemTime;

/// Render `t` as `YYYY-MM-DDTHH:MM:SSZ`. Treats pre-epoch instants as
/// the unix epoch (a clock skew that small isn't worth panicking over).
#[must_use]
pub fn format_iso8601(t: SystemTime) -> String {
    let dur = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs();
    let (y, m, d, hh, mm, ss) = secs_to_ymdhms(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Decompose an epoch-seconds value into its calendar components.
#[allow(clippy::cast_possible_truncation)]
#[must_use]
pub fn secs_to_ymdhms(mut secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let ss = (secs % 60) as u32;
    secs /= 60;
    let mm = (secs % 60) as u32;
    secs /= 60;
    let hh = (secs % 24) as u32;
    secs /= 24;
    let mut days = secs as u32;
    let mut y: u32 = 1970;
    loop {
        let in_year = days_in_year(y);
        if days < in_year {
            break;
        }
        days -= in_year;
        y += 1;
    }
    let mut m: u32 = 1;
    loop {
        let in_month = days_in_month(y, m);
        if days < in_month {
            break;
        }
        days -= in_month;
        m += 1;
    }
    (y, m, days + 1, hh, mm, ss)
}

/// Gregorian leap-year predicate.
#[must_use]
pub const fn is_leap(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)
}

/// 365 or 366 depending on whether `y` is a leap year.
#[must_use]
pub const fn days_in_year(y: u32) -> u32 {
    if is_leap(y) { 366 } else { 365 }
}

/// Days in calendar month `m` of year `y`. Returns 0 for out-of-range
/// months — callers are expected to pass `1..=12`.
#[must_use]
pub const fn days_in_month(y: u32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

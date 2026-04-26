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
    let dur = if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
        d
    } else {
        tracing::warn!("clock skew: SystemTime < UNIX_EPOCH; reporting epoch");
        std::time::Duration::default()
    };
    let secs = dur.as_secs();
    let (y, m, d, hh, mm, ss) = secs_to_ymdhms(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Decompose an epoch-seconds value into its calendar components.
#[allow(
    clippy::cast_possible_truncation,
    reason = "modulo arithmetic bounds the values to u32 ranges (e.g. seconds % 60); truncation is intended"
)]
#[must_use]
pub fn secs_to_ymdhms(mut secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let ss = (secs % 60) as u32;
    secs /= 60;
    let mm = (secs % 60) as u32;
    secs /= 60;
    let hh = (secs % 24) as u32;
    secs /= 24;
    // Round 3 — fix M6. Saturating cast: `secs` here is the days
    // count after dividing out h/m/s. It exceeds `u32::MAX` only on
    // timestamps far beyond any plausible input (year ~11 million),
    // but the previous `secs as u32` would silently wrap, producing
    // garbage calendar output. Saturating to `u32::MAX` keeps the
    // function's contract (deterministic decomposition) for any
    // valid `SystemTime` while still surfacing weirdness as
    // year-overflow rather than nonsense.
    //
    // Round 4 — fix m2. Promoted the saturating arm from "silent
    // saturation" to a warn so the (extremely improbable) overflow
    // path is visible in operator traces rather than producing a
    // year-11M timestamp without explanation.
    let mut days: u32 = u32::try_from(secs).unwrap_or_else(|_| {
        tracing::warn!(
            secs,
            "secs_to_ymdhms: days saturating to u32::MAX; calendar output will be capped at year ~11M"
        );
        u32::MAX
    });
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::format_iso8601;
    use std::time::{Duration, UNIX_EPOCH};

    /// Days from 1970-01-01 to the start of `year` (1-Jan, 00:00:00).
    fn days_from_epoch_to_year_start(year: u32) -> u64 {
        let mut days: u64 = 0;
        let mut y = 1970;
        while y < year {
            days += u64::from(super::days_in_year(y));
            y += 1;
        }
        days
    }

    /// Seconds since `UNIX_EPOCH` for `YYYY-MM-DD HH:MM:SS` UTC.
    fn epoch_seconds(y: u32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> u64 {
        let mut days = days_from_epoch_to_year_start(y);
        for cur_m in 1..m {
            days += u64::from(super::days_in_month(y, cur_m));
        }
        days += u64::from(d - 1);
        days * 86400 + u64::from(hh) * 3600 + u64::from(mm) * 60 + u64::from(ss)
    }

    #[test]
    fn unix_epoch_renders_as_1970_zero() {
        assert_eq!(format_iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn leap_day_2024_february_29_renders_correctly() {
        // 2024-02-29T00:00:00Z — 2024 is a leap year (divisible by 4,
        // not by 100).
        let secs = epoch_seconds(2024, 2, 29, 0, 0, 0);
        let t = UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(format_iso8601(t), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn non_leap_century_2100_february_28_caps_at_28() {
        // 2100 is divisible by 100 but not 400 → not a leap year.
        // 2100-02-28T00:00:00Z is valid; the next day is 2100-03-01.
        let feb_28 = epoch_seconds(2100, 2, 28, 0, 0, 0);
        let t = UNIX_EPOCH + Duration::from_secs(feb_28);
        assert_eq!(format_iso8601(t), "2100-02-28T00:00:00Z");
        let next = UNIX_EPOCH + Duration::from_secs(feb_28 + 86_400);
        assert_eq!(format_iso8601(next), "2100-03-01T00:00:00Z");
    }

    #[test]
    fn leap_century_2000_february_29_is_leap() {
        // 2000 is divisible by 400 → leap year.
        let secs = epoch_seconds(2000, 2, 29, 0, 0, 0);
        let t = UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(format_iso8601(t), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn end_of_year_2025_rolls_to_2026_january_1() {
        let last = epoch_seconds(2025, 12, 31, 23, 59, 59);
        let t = UNIX_EPOCH + Duration::from_secs(last + 1);
        assert_eq!(format_iso8601(t), "2026-01-01T00:00:00Z");
    }

    #[test]
    fn mid_day_timestamp_2026_04_25_at_12_34_56() {
        let secs = epoch_seconds(2026, 4, 25, 12, 34, 56);
        let t = UNIX_EPOCH + Duration::from_secs(secs);
        assert_eq!(format_iso8601(t), "2026-04-25T12:34:56Z");
    }

    #[test]
    fn pre_epoch_clamps_to_1970_zero() {
        // SystemTime can be before UNIX_EPOCH on machines with skewed
        // clocks; the implementation logs a WARN and reports the epoch.
        let pre = UNIX_EPOCH - Duration::from_secs(60);
        assert_eq!(format_iso8601(pre), "1970-01-01T00:00:00Z");
    }
}

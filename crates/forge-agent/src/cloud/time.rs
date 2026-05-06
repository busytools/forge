//! Shared timestamp parsers for the cloud module — OAuth `expiresAt`
//! and usage windows accept either an ISO-8601 string, an integer
//! string, or a JSON number (interpreted as either seconds or
//! milliseconds since the UNIX epoch).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Parse a JSON value (string OR number) as a `SystemTime`. Accepts:
/// - ISO-8601 datetime strings (e.g. `"2025-12-25T12:00:00.000Z"`).
/// - Integer-string seconds-or-milliseconds since UNIX epoch.
/// - Number seconds-or-milliseconds since UNIX epoch.
pub(super) fn parse_timestamp_value(value: &Value) -> Option<SystemTime> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|raw| i64::try_from(raw).ok()))
            .and_then(system_time_from_epoch),
        Value::String(raw) => parse_iso8601_timestamp(raw)
            .or_else(|| raw.trim().parse::<i64>().ok().and_then(system_time_from_epoch)),
        _ => None,
    }
}

/// Convert a non-negative epoch integer to `SystemTime`. Values
/// `>= 1e12` are interpreted as milliseconds; smaller values as
/// seconds. Negative values fail.
pub(super) fn system_time_from_epoch(raw: i64) -> Option<SystemTime> {
    if raw < 0 {
        return None;
    }
    let raw = u64::try_from(raw).ok()?;
    if raw >= 1_000_000_000_000 {
        Some(UNIX_EPOCH + Duration::from_millis(raw))
    } else {
        Some(UNIX_EPOCH + Duration::from_secs(raw))
    }
}

/// Hand-rolled ISO-8601 / RFC-3339 datetime parser. Subset:
/// `YYYY-MM-DD[T| ]HH:MM[:SS[.fffffffff]][Z|+HH:MM|-HH:MM]`. Returns
/// `None` on any field-level parse failure.
pub(super) fn parse_iso8601_timestamp(raw: &str) -> Option<SystemTime> {
    let trimmed = raw.trim();
    let (date_part, time_part) = trimmed.split_once('T').or_else(|| trimmed.split_once(' '))?;

    let mut date_iter = date_part.split('-');
    let year = date_iter.next()?.parse::<i32>().ok()?;
    let month = date_iter.next()?.parse::<u32>().ok()?;
    let day = date_iter.next()?.parse::<u32>().ok()?;

    let (time_only, offset_seconds) = split_time_and_offset(time_part)?;
    let mut time_iter = time_only.split(':');
    let hour = time_iter.next()?.parse::<u32>().ok()?;
    let minute = time_iter.next()?.parse::<u32>().ok()?;
    let second_and_fraction = time_iter.next().unwrap_or("0");
    let (second_raw, fraction_raw) =
        second_and_fraction.split_once('.').unwrap_or((second_and_fraction, ""));
    let second = second_raw.parse::<u32>().ok()?;

    let mut nanos = 0u32;
    let mut factor = 100_000_000u32;
    for ch in fraction_raw.chars().take(9) {
        let digit = ch.to_digit(10)?;
        nanos = nanos.saturating_add(digit.saturating_mul(factor));
        if factor == 0 {
            break;
        }
        factor /= 10;
    }

    let days = days_from_civil(year, month, day)?;
    let day_seconds =
        i64::from(hour) * 60 * 60 + i64::from(minute) * 60 + i64::from(second) - offset_seconds;
    let unix_seconds = days.checked_mul(86_400)?.checked_add(day_seconds)?;
    if unix_seconds < 0 {
        return None;
    }
    let secs = u64::try_from(unix_seconds).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_nanos(u64::from(nanos)))
}

fn split_time_and_offset(raw: &str) -> Option<(&str, i64)> {
    if let Some(rest) = raw.strip_suffix('Z') {
        return Some((rest, 0));
    }
    let split_idx = raw.rfind(['+', '-'])?;
    let (time_only, offset_str) = raw.split_at(split_idx);
    let sign: i64 = if offset_str.starts_with('+') { 1 } else { -1 };
    let offset_str = &offset_str[1..];
    let (h, m) = offset_str.split_once(':').unwrap_or((offset_str, "0"));
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    Some((time_only, sign * (h * 3600 + m * 60)))
}

/// Days since 1970-01-01 (Howard Hinnant's civil-from-days inverse).
fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut year = i64::from(year);
    let month = i64::from(month);
    let day = i64::from(day);
    year -= i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + day_of_year;
    Some(era * 146_097 + doe - 719_468)
}

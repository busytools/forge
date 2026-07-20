//! Human-readable rendering of a cron schedule for the Inspector
//! SCHEDULES section. [`humanize_cron`] turns a 5-field expression into
//! plain English for the common shapes; [`humanize_once`] renders a
//! one-shot fire time as a local wall-clock string. An unrecognised
//! expression returns verbatim - never a bare `* * * * *`.

use std::time::SystemTime;

use time::{Month, OffsetDateTime};
use time_tz::{OffsetDateTimeExt, Tz};

/// Turn a 5-field cron expression into plain English for the common
/// shapes (daily / every-N-minutes / every-N-hours / weekly / weekdays
/// / monthly). Anything else returns the trimmed expression verbatim.
pub(crate) fn humanize_cron(expr: &str) -> String {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let [min, hour, dom, month, dow] = fields.as_slice() else {
        return expr.trim().to_owned();
    };
    humanize_fields(min, hour, dom, month, dow).unwrap_or_else(|| expr.trim().to_owned())
}

fn humanize_fields(min: &str, hour: &str, dom: &str, month: &str, dow: &str) -> Option<String> {
    let every_day = dom == "*" && month == "*" && dow == "*";

    // Sub-hourly: `* * * * *` (every minute) / `*/5 * * * *`.
    if hour == "*" && every_day {
        if min == "*" {
            return Some("every minute".to_owned());
        }
        if let Some(n) = step_of(min) {
            return Some(every_n(n, "minute"));
        }
    }
    // Every N hours on the minute: `0 */2 * * *`.
    if every_day
        && parse_int(min) == Some(0)
        && let Some(n) = step_of(hour)
    {
        return Some(every_n(n, "hour"));
    }

    // The remaining shapes pin a clock time, so need literal min + hour.
    let (m, h) = (parse_int(min)?, parse_int(hour)?);
    if m > 59 || h > 23 {
        return None;
    }
    let at = format!("at {h:02}:{m:02}");

    // Month + day-of-week wild: either a fixed day-of-month, or daily.
    if month == "*" && dow == "*" {
        if let Some(day) = parse_int(dom).filter(|d| (1..=31).contains(d)) {
            let base = format!("monthly on the {}", ordinal(day));
            return Some(if h == 0 && m == 0 { base } else { format!("{base} {at}") });
        }
        if dom == "*" {
            return Some(format!("daily {at}"));
        }
    }

    // Day-of-week shapes (day-of-month + month wild).
    if dom == "*" && month == "*" {
        if dow == "1-5" {
            return Some(format!("weekdays {at}"));
        }
        return weekday_name(dow).map(|name| format!("{name}s {at}"));
    }
    None
}

/// The step of a `*/N` field, or `None` for any other shape.
fn step_of(field: &str) -> Option<u32> {
    field.strip_prefix("*/").and_then(parse_int_str).filter(|n| *n >= 1)
}

fn parse_int(field: &str) -> Option<u32> {
    parse_int_str(field)
}

fn parse_int_str(s: &str) -> Option<u32> {
    s.parse().ok()
}

fn every_n(n: u32, unit: &str) -> String {
    if n == 1 { format!("every {unit}") } else { format!("every {n} {unit}s") }
}

fn ordinal(n: u32) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Cron day-of-week (0 or 7 = Sunday) to a weekday name.
fn weekday_name(dow: &str) -> Option<&'static str> {
    match parse_int(dow)? {
        0 | 7 => Some("Sunday"),
        1 => Some("Monday"),
        2 => Some("Tuesday"),
        3 => Some("Wednesday"),
        4 => Some("Thursday"),
        5 => Some("Friday"),
        6 => Some("Saturday"),
        _ => None,
    }
}

/// Render a one-shot fire instant as a local wall-clock string relative
/// to `now`: `today 14:30`, `tomorrow 09:00`, or `Jul 25 09:00` further
/// out. `tz` is the local zone (see
/// `forge_workspace::env::timezone::system_timezone`); injected so the
/// output is deterministic under test.
pub(crate) fn humanize_once(at: SystemTime, now: SystemTime, tz: &Tz) -> String {
    let at_local = OffsetDateTime::from(at).to_timezone(tz);
    let now_local = OffsetDateTime::from(now).to_timezone(tz);
    let clock = format!("{:02}:{:02}", at_local.hour(), at_local.minute());
    match at_local.date().to_julian_day() - now_local.date().to_julian_day() {
        0 => format!("today {clock}"),
        1 => format!("tomorrow {clock}"),
        _ => format!("{} {} {clock}", month_abbrev(at_local.month()), at_local.day()),
    }
}

fn month_abbrev(month: Month) -> &'static str {
    match month {
        Month::January => "Jan",
        Month::February => "Feb",
        Month::March => "Mar",
        Month::April => "Apr",
        Month::May => "May",
        Month::June => "Jun",
        Month::July => "Jul",
        Month::August => "Aug",
        Month::September => "Sep",
        Month::October => "Oct",
        Month::November => "Nov",
        Month::December => "Dec",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Date;
    use time_tz::timezones;

    #[test]
    fn humanize_cron_covers_the_common_shapes() {
        assert_eq!(humanize_cron("0 9 * * *"), "daily at 09:00");
        assert_eq!(humanize_cron("*/5 * * * *"), "every 5 minutes");
        assert_eq!(humanize_cron("*/1 * * * *"), "every minute");
        assert_eq!(humanize_cron("* * * * *"), "every minute");
        assert_eq!(humanize_cron("0 */2 * * *"), "every 2 hours");
        assert_eq!(humanize_cron("0 */1 * * *"), "every hour");
        assert_eq!(humanize_cron("0 9 * * 1"), "Mondays at 09:00");
        assert_eq!(humanize_cron("0 9 * * 0"), "Sundays at 09:00");
        assert_eq!(humanize_cron("0 9 * * 7"), "Sundays at 09:00");
        assert_eq!(humanize_cron("0 9 * * 1-5"), "weekdays at 09:00");
        assert_eq!(humanize_cron("0 0 1 * *"), "monthly on the 1st");
        assert_eq!(humanize_cron("30 14 2 * *"), "monthly on the 2nd at 14:30");
        assert_eq!(humanize_cron("0 0 23 * *"), "monthly on the 23rd");
    }

    #[test]
    fn humanize_cron_falls_back_to_raw_for_exotic_and_malformed() {
        // Genuinely exotic shape: specific month + weekday combo.
        assert_eq!(humanize_cron("15 3 * 6 2"), "15 3 * 6 2");
        // Wrong field count returns verbatim (trimmed), never a panic.
        assert_eq!(humanize_cron("0 9 * *"), "0 9 * *");
        assert_eq!(humanize_cron("  weird  "), "weird");
        // Out-of-range clock falls through rather than lying.
        assert_eq!(humanize_cron("0 99 * * *"), "0 99 * * *");
    }

    fn utc(year: i32, month: Month, day: u8, h: u8, m: u8) -> SystemTime {
        Date::from_calendar_date(year, month, day)
            .unwrap()
            .with_hms(h, m, 0)
            .unwrap()
            .assume_utc()
            .into()
    }

    #[test]
    fn humanize_once_renders_relative_local_time() {
        let utc_tz = timezones::db::UTC;
        let now = utc(2026, Month::July, 20, 12, 0);
        assert_eq!(humanize_once(utc(2026, Month::July, 20, 9, 5), now, utc_tz), "today 09:05");
        assert_eq!(
            humanize_once(utc(2026, Month::July, 21, 14, 30), now, utc_tz),
            "tomorrow 14:30"
        );
        assert_eq!(humanize_once(utc(2026, Month::July, 25, 9, 0), now, utc_tz), "Jul 25 09:00");
    }

    #[test]
    fn humanize_once_shifts_to_the_local_zone() {
        // 20:00 UTC on the 20th is 01:30 on the 21st in Kolkata (UTC+5:30),
        // so a same-instant "now" makes it read as today in that zone.
        let kolkata = timezones::get_by_name("Asia/Kolkata").expect("kolkata zone");
        let instant = utc(2026, Month::July, 20, 20, 0);
        assert_eq!(humanize_once(instant, instant, kolkata), "today 01:30");
    }
}

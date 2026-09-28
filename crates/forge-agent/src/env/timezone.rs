//! The host's local IANA timezone, resolved dynamically, and the wall
//! clock an RFC-3339 timestamp reads as in it. A generic OS-env probe
//! shared by `/usage` day-bucketing, the SCHEDULES schedule formatter and
//! the turn rows a view draws from a transcript.

use time::format_description::well_known::Rfc3339;
use time_tz::{OffsetDateTimeExt, Tz, timezones};

/// The OS-configured IANA timezone, read dynamically. Uses
/// `iana-time-zone` (multithread-safe on Unix, unlike
/// `time::UtcOffset::current_local_offset`, which errors in a threaded
/// process). Falls back to UTC with a warn only when the zone can't be
/// resolved - the rare exception, so "today" tracks the user's wall clock.
pub fn system_timezone() -> &'static Tz {
    match iana_time_zone::get_timezone() {
        Ok(name) => timezones::get_by_name(&name).unwrap_or_else(|| {
            tracing::warn!(
                target: "forge_agent::env::timezone",
                %name,
                "unknown system timezone; falling back to UTC",
            );
            timezones::db::UTC
        }),
        Err(error) => {
            tracing::warn!(
                target: "forge_agent::env::timezone",
                %error,
                "system timezone unavailable; falling back to UTC",
            );
            timezones::db::UTC
        }
    }
}

/// An RFC-3339 instant the CLI wrote, as the reader's own wall clock:
/// `HH:MM:SS` for today's, the date too for anything older, because a turn
/// read from a transcript can be weeks old and a bare time would read as
/// today. `None` for a string that does not parse, so a malformed row draws
/// no clock rather than a fabricated one.
pub fn local_clock(instant: &str) -> Option<String> {
    let parsed = time::OffsetDateTime::parse(instant, &Rfc3339).ok()?;
    let local = parsed.to_timezone(system_timezone());
    let today = time::OffsetDateTime::now_utc().to_timezone(system_timezone()).date();
    let clock = format!("{:02}:{:02}:{:02}", local.hour(), local.minute(), local.second());
    Some(if local.date() == today { clock } else { format!("{} {clock}", local.date()) })
}

/// The milliseconds from one RFC-3339 instant the CLI wrote to another.
/// `None` unless both parse and the second is not earlier than the first.
pub fn millis_between(from: &str, to: &str) -> Option<u64> {
    let from = time::OffsetDateTime::parse(from, &Rfc3339).ok()?;
    let to = time::OffsetDateTime::parse(to, &Rfc3339).ok()?;
    u64::try_from((to - from).whole_milliseconds()).ok()
}

#[cfg(test)]
mod tests {
    use super::{Rfc3339, local_clock, millis_between, system_timezone};
    use time_tz::OffsetDateTimeExt;

    /// The instant is parsed and rendered, and an older one carries its date:
    /// a turn read from a transcript can be weeks old, and a bare time on one
    /// reads as today. The expected clock is recomputed here with the host's
    /// own zone, so this pins the parse and the format and leaves the zone
    /// itself to the check below.
    #[test]
    fn a_row_instant_reads_as_the_reader_own_wall_clock() {
        let instant = "2020-04-22T04:15:27.000Z";
        let local =
            time::OffsetDateTime::parse(instant, &time::format_description::well_known::Rfc3339)
                .expect("the fixture parses")
                .to_timezone(system_timezone());
        let clock = format!("{:02}:{:02}:{:02}", local.hour(), local.minute(), local.second());

        assert_eq!(
            local_clock(instant).as_deref(),
            Some(format!("{} {clock}", local.date()).as_str()),
            "an instant from another day renders as that date and the host's own clock",
        );
    }

    /// The zone is the host's, not the instant's own `Z`: an instant whose
    /// clock differs from its UTC reading proves one of the two was applied.
    /// A host already on UTC cannot tell the difference and says so.
    #[test]
    fn the_clock_is_the_hosts_and_not_the_instants_own() {
        let instant = "2020-04-22T04:15:27.000Z";
        let rendered = local_clock(instant).expect("the fixture parses");
        let offset = time::OffsetDateTime::now_utc().to_timezone(system_timezone()).offset();

        if offset.is_utc() {
            assert!(
                rendered.ends_with("04:15:27"),
                "a UTC host renders the instant's own clock: {rendered}"
            );
        } else {
            assert!(
                !rendered.ends_with("04:15:27"),
                "the host's own clock is not the instant's own: {rendered}",
            );
        }
    }

    /// Today's instant carries the time alone: a clock on the row a reader is
    /// watching is not dated.
    #[test]
    fn todays_instant_carries_the_time_alone() {
        let now = time::OffsetDateTime::now_utc().to_timezone(system_timezone());
        let rendered = local_clock(&now.format(&Rfc3339).expect("formats")).expect("parses");

        assert_eq!(
            rendered,
            format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second()),
            "the reader's own clock, and no date on the day it belongs to",
        );
    }

    /// A row whose clock does not parse draws nothing rather than a time.
    #[test]
    fn an_unparseable_instant_has_no_wall_clock() {
        assert_eq!(local_clock("not a timestamp"), None, "no clock is invented for it");
    }

    /// The span between two of a turn's own rows. A pair the wrong way round
    /// has no span: zero and a wrapped count both read as a measurement.
    #[test]
    fn a_span_is_the_time_between_two_rows_and_nothing_when_it_runs_backwards() {
        assert_eq!(
            millis_between("2026-04-22T04:15:27.000Z", "2026-04-22T04:18:08.000Z"),
            Some(161_000),
            "two minutes and forty-one seconds",
        );
        assert_eq!(
            millis_between("2026-04-22T04:18:08.000Z", "2026-04-22T04:15:27.000Z"),
            None,
            "a span that runs backwards is not a duration",
        );
        assert_eq!(
            millis_between("2026-04-22T04:15:27.000Z", "nonsense"),
            None,
            "and neither is a pair with an unreadable end"
        );
    }
}

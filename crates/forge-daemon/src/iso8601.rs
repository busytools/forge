//! ISO-8601 (RFC-3339) formatting shared between connection metadata
//! ([`Connection::connected_at_iso`](crate::connection::Connection)) and
//! pending-prompt views ([`PromptQueue`](crate::prompt_queue::PromptQueue))
//! so the daemon's wire shapes use a single canonical format.
//!
//! Whole-second granularity with a `Z` suffix — pre-epoch instants are
//! floored to the unix epoch (clock-skew defence; not worth surfacing).

use std::time::SystemTime;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Render `t` as `YYYY-MM-DDTHH:MM:SSZ` (whole seconds). Pre-epoch
/// instants are reported as `1970-01-01T00:00:00Z`.
#[must_use]
pub fn format_iso8601(t: SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map_or_else(
        |_| {
            tracing::warn!("clock skew: SystemTime < UNIX_EPOCH; reporting epoch");
            0
        },
        |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
    );
    OffsetDateTime::from_unix_timestamp(secs)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::format_iso8601;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn unix_epoch_renders_as_1970_zero() {
        assert_eq!(format_iso8601(UNIX_EPOCH), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn mid_day_timestamp_2026_04_25_at_12_34_56() {
        // 2026-04-25T12:34:56Z = 1_777_120_496 epoch seconds. Sanity
        // check that the wire shape stays bit-identical to the previous
        // hand-rolled formatter.
        let t = UNIX_EPOCH + Duration::from_secs(1_777_120_496);
        assert_eq!(format_iso8601(t), "2026-04-25T12:34:56Z");
    }

    #[test]
    fn pre_epoch_clamps_to_1970_zero() {
        let pre = UNIX_EPOCH - Duration::from_secs(60);
        assert_eq!(format_iso8601(pre), "1970-01-01T00:00:00Z");
    }
}

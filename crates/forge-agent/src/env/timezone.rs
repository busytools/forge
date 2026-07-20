//! The host's local IANA timezone, resolved dynamically. A generic
//! OS-env probe shared by `/usage` day-bucketing and the SCHEDULES
//! schedule formatter.

use time_tz::{Tz, timezones};

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

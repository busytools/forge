//! Small format helpers shared across views. Kept tiny — promote to
//! a richer module only when a new helper genuinely belongs here.

use std::time::SystemTime;

/// Format the gap between `activity` and `now` as a short relative-
/// time string: `now`, `Xm`, `Xh`, `Xd`, `Xw` (capped at `99w`).
/// Returns the raw form, no padding — callers that need a stable
/// column width pad at the call site.
pub fn relative_time(activity: SystemTime, now: SystemTime) -> String {
    let elapsed = now.duration_since(activity).unwrap_or_default();
    let secs = elapsed.as_secs();
    if secs < 60 {
        return "now".to_owned();
    }
    if secs < 3600 {
        return format!("{}m", secs / 60);
    }
    if secs < 86_400 {
        return format!("{}h", secs / 3600);
    }
    if secs < 604_800 {
        return format!("{}d", secs / 86_400);
    }
    format!("{}w", (secs / 604_800).min(99))
}

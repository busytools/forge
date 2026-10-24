//! The page a profile carries: the marker a launch reads back, and the hold
//! that keeps a page whose navigation has not landed from being erased.
//!
//! **The marker is written on every raise and every browsing call that
//! changed the page**, so a restart or an idle close opens where the session
//! left off instead of on a blank page. Only a web address is ever carried.
//!
//! **A held carry waits for a REAL page.** The host starts on `about:blank`
//! and the server navigates it to the carried page once the passkey veto is
//! armed, so a probe can meet the blank page before the navigation lands;
//! while the hold stands, a non-web read keeps both the marker and the hold.

use std::path::Path;

/// The marker file, beside the profile's cache root.
const PAGE: &str = "page";

/// The hold: a carried URL whose navigation has not been seen to land.
const HOLD: &str = "page-unopened";

/// Whether an address is a page a restart may reopen.
pub fn is_web(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// The page a launch reopens, when the marker names one.
pub fn carried(profile: &Path) -> Option<String> {
    let text = std::fs::read_to_string(profile.join(PAGE)).ok()?;
    let url = text.trim();
    is_web(url).then(|| url.to_owned())
}

/// The held carry, when one stands.
pub fn held(profile: &Path) -> Option<String> {
    let text = std::fs::read_to_string(profile.join(HOLD)).ok()?;
    let url = text.trim();
    is_web(url).then(|| url.to_owned())
}

/// Record one read of the page.
///
/// **A held carry releases only to a real page**: the engine's own error page
/// is what a failed launch leaves, and releasing on it would erase the page
/// the hold exists for, so every non-web read keeps both. **It also releases
/// only to a marker that LANDED**: a write that failed leaves the hold
/// standing, so the next launch still opens the page the person was shown
/// rather than a blank one.
pub fn record(profile: &Path, url: &str) {
    if held(profile).is_some() && !is_web(url) {
        return;
    }
    if write(profile, is_web(url).then_some(url)) {
        release(profile);
    }
}

/// Write the marker a launch reads; anything but a web address clears it.
/// Answers whether the marker on disk now says what was asked.
///
/// **A temp file and a rename**: a crash mid-write leaves the previous page
/// rather than a truncated one.
pub fn write(profile: &Path, url: Option<&str>) -> bool {
    let path = profile.join(PAGE);
    let Some(url) = url.filter(|url| is_web(url)) else {
        // **Clearing is a write too**: a remove that could not happen says
        // so, or the caller lifts a hold over a marker still on disk.
        return match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => true,
            Err(why) => {
                tracing::warn!(
                    "the carried page could not be cleared (event_name \
                     browser_page_carry_clear_failed, profile {}): {why}",
                    profile.display(),
                );
                false
            }
        };
    };
    let temp = profile.join("page.tmp");
    if let Err(why) = std::fs::write(&temp, url).and_then(|()| std::fs::rename(&temp, &path)) {
        tracing::warn!(
            "the carried page could not be written (event_name browser_page_carry_failed, \
             profile {}): {why}",
            profile.display(),
        );
        let _ = std::fs::remove_file(&temp);
        return false;
    }
    true
}

/// Hold a carried page until a raise or a call reads a real one.
pub fn hold(profile: &Path, url: &str) {
    if !is_web(url) {
        return;
    }
    let temp = profile.join("page-unopened.tmp");
    if let Err(why) =
        std::fs::write(&temp, url).and_then(|()| std::fs::rename(&temp, profile.join(HOLD)))
    {
        tracing::warn!(
            "the carry hold could not be written (event_name browser_carry_hold_failed, \
             profile {}): {why}",
            profile.display(),
        );
        let _ = std::fs::remove_file(&temp);
    }
}

/// Lift the hold.
pub fn release(profile: &Path) {
    let _ = std::fs::remove_file(profile.join(HOLD));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh profile has nothing carried and nothing held.
    #[test]
    fn a_fresh_profile_carries_nothing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(carried(dir.path()), None);
        assert_eq!(held(dir.path()), None);
    }

    /// **A web read writes the marker and lifts the hold.** The page the
    /// person was shown is what the next launch opens.
    #[test]
    fn a_web_read_records_the_page_and_releases_the_hold() {
        let dir = tempfile::tempdir().expect("a temp dir");
        hold(dir.path(), "https://example.com/job");

        record(dir.path(), "https://example.com/job/apply");

        assert_eq!(carried(dir.path()).as_deref(), Some("https://example.com/job/apply"));
        assert_eq!(held(dir.path()), None, "the navigation was seen, so the hold lifts");
    }

    /// **A non-web read keeps a held carry.** The host's blank page (or its
    /// error page) read while the carried navigation is still landing would
    /// otherwise erase the page the hold exists for.
    #[test]
    fn a_non_web_read_keeps_a_held_carry() {
        let dir = tempfile::tempdir().expect("a temp dir");
        write(dir.path(), Some("https://example.com/job"));
        hold(dir.path(), "https://example.com/job");

        record(dir.path(), "about:blank");

        assert_eq!(
            carried(dir.path()).as_deref(),
            Some("https://example.com/job"),
            "the marker stands",
        );
        assert_eq!(
            held(dir.path()).as_deref(),
            Some("https://example.com/job"),
            "and so does the hold"
        );
    }

    /// **A marker that could not be written keeps the hold.** Releasing it
    /// would leave neither a page to reopen nor the hold that says one is
    /// still owed, so the next launch would open blank.
    #[test]
    fn a_failed_write_keeps_the_hold() {
        let dir = tempfile::tempdir().expect("a temp dir");
        hold(dir.path(), "https://example.com/job");
        // A directory where the marker's file belongs: the rename cannot land.
        std::fs::create_dir(dir.path().join(PAGE)).expect("a blocking directory");

        record(dir.path(), "https://example.com/job/apply");

        assert_eq!(carried(dir.path()), None, "no marker was written");
        assert_eq!(
            held(dir.path()).as_deref(),
            Some("https://example.com/job"),
            "and the hold that says a page is still owed stands"
        );
    }

    /// **Without a hold, a non-web read clears the marker**: the file always
    /// means the page the person was actually shown, never a leftover.
    #[test]
    fn a_non_web_read_clears_an_unheld_marker() {
        let dir = tempfile::tempdir().expect("a temp dir");
        write(dir.path(), Some("https://example.com/job"));

        record(dir.path(), "chrome-error://chromewebdata/");

        assert_eq!(carried(dir.path()), None);
    }
}

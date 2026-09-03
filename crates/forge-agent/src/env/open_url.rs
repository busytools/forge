//! Open a URL in the user's system browser.
//!
//! The shell-out lives here (not in forge-tui, which holds no
//! subprocess machinery) so any pane affordance can hand a URL to the
//! OS opener and get a `Result` back. GitHub-only callers today: the
//! Inspector pane's PR row.

use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

/// Per-invocation budget. The OS opener returns immediately by
/// design (`open` / `xdg-open` hand off to the desktop and exit);
/// the timeout only guards a hung desktop session.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of the opener's stderr carries into the user-facing
/// notice before truncation.
const STDERR_DETAIL_CAP: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum OpenUrlError {
    #[error("refusing to open a non-https url: {0}")]
    UnsupportedScheme(String),
    #[error("the OS opener ({0}) is unavailable: {1}")]
    Unavailable(&'static str, #[source] std::io::Error),
    #[error("the OS opener ({0}) timed out")]
    Timeout(&'static str),
    #[error("the OS opener ({0}) failed: {1}")]
    OpenFailed(&'static str, String),
}

/// Map one completed opener invocation to the `Result`. Pure so the
/// stderr-truncation + success contract is unit-testable.
fn classify_output(
    opener: &'static str,
    success: bool,
    exit_code: Option<i32>,
    stderr: &str,
) -> Result<(), OpenUrlError> {
    if success {
        return Ok(());
    }
    let detail = stderr.trim();
    let detail = if detail.is_empty() {
        format!("exit code {}", exit_code.unwrap_or(-1))
    } else {
        detail.chars().take(STDERR_DETAIL_CAP).collect()
    };
    Err(OpenUrlError::OpenFailed(opener, detail))
}

/// Hand `url` to the platform's default handler. Only `https://` urls
/// are accepted - the opener's argument surface stays structural, not
/// ambient. Caveat conceded: `xdg-open` exits 0 even when no handler
/// accepts the url, so a failed open on Linux can read as success;
/// macOS `open` reports honestly. The url is passed as a single argv
/// element (no shell), so an odd value fails the opener rather than
/// executing anything.
pub async fn open_url(url: &str) -> Result<(), OpenUrlError> {
    const OPENER: &str = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    if !url.starts_with("https://") {
        return Err(OpenUrlError::UnsupportedScheme(url.to_owned()));
    }
    let output =
        timeout(COMMAND_TIMEOUT, Command::new(OPENER).arg(url).kill_on_drop(true).output())
            .await
            .map_err(|_| OpenUrlError::Timeout(OPENER))?
            .map_err(|err| OpenUrlError::Unavailable(OPENER, err))?;
    classify_output(
        OPENER,
        output.status.success(),
        output.status.code(),
        &String::from_utf8_lossy(&output.stderr),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A successful opener maps to `Ok` regardless of stderr noise.
    #[test]
    fn successful_opener_is_ok() {
        assert!(matches!(classify_output("open", true, Some(0), "noise"), Ok(())));
    }

    /// Only https:// urls reach the opener - the rejection happens
    /// before any subprocess is spawned, so this test exercises the
    /// allowlist with no OS interaction.
    #[tokio::test]
    async fn non_https_urls_are_refused_before_spawning() {
        assert!(matches!(
            open_url("file:///etc/passwd").await,
            Err(OpenUrlError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            open_url("http://example.com").await,
            Err(OpenUrlError::UnsupportedScheme(_))
        ));
    }

    /// A failed opener surfaces its stderr as the error detail, so
    /// the user notice names the reason instead of a bare exit code.
    #[test]
    fn failed_opener_carries_stderr_detail() {
        let err = classify_output("open", false, Some(1), "NSCocoaErrorDomain: bad url\n")
            .expect_err("failure maps to OpenFailed");
        assert!(err.to_string().contains("NSCocoaErrorDomain: bad url"));
    }

    /// A failed opener with no stderr falls back to the exit code,
    /// never an empty notice.
    #[test]
    fn failed_opener_without_stderr_names_the_exit_code() {
        let err =
            classify_output("open", false, Some(1), "\n").expect_err("failure maps to OpenFailed");
        assert!(err.to_string().contains("exit code 1"));
    }

    /// stderr longer than the cap truncates - and truncation must
    /// respect char boundaries or a multibyte stderr panics.
    #[test]
    fn stderr_detail_truncates_on_char_boundaries() {
        let long = "\u{1F989}".repeat(STDERR_DETAIL_CAP + 10);
        let err = classify_output("open", false, Some(1), &long).expect_err("failure");
        assert!(err.to_string().contains("\u{1F989}"));
    }
}

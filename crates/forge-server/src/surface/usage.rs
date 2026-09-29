//! `usage()`: the token/cost report a `/usage` view draws.

use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_primitives::token_usage::UsageReport;

use super::ViewSurface;

/// How long a scan answers for. A client refreshing on a timer asks again
/// every so often, and the pool does not change fast enough to pay a walk per
/// ask; one walk per window is the same answer a reader would have had.
const USAGE_WINDOW: Duration = Duration::from_secs(5);

impl ViewSurface {
    /// Scan the shared session-JSONL pool into the four windows a usage view
    /// draws.
    ///
    /// The scan walks the pool and parses every file behind it, so it runs on
    /// the blocking pool rather than on the task that serves a socket: a
    /// connection must not wait on a disk walk, and neither must a render.
    ///
    /// A scan that panicked is an error rather than an empty report - an
    /// all-zero report reads as a pool with no usage in it, which is a
    /// different answer from "the scan did not run".
    pub async fn usage(&self) -> anyhow::Result<UsageReport> {
        if let Some(report) = self.held_usage() {
            return Ok(report);
        }
        let workspace = Arc::clone(&self.workspace);
        let report = tokio::task::spawn_blocking(move || workspace.scan_usage())
            .await
            .map_err(|error| anyhow::anyhow!("the usage scan did not finish: {error}"))?;
        self.remember_usage(&report);
        Ok(report)
    }

    /// The last scan, while it is inside [`USAGE_WINDOW`].
    fn held_usage(&self) -> Option<UsageReport> {
        let held = self.usage_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        held.as_ref()
            .filter(|(at, _)| at.elapsed() < USAGE_WINDOW)
            .map(|(_, report)| report.clone())
    }

    fn remember_usage(&self, report: &UsageReport) {
        let mut held = self.usage_cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *held = Some((Instant::now(), report.clone()));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::surface::ViewSurface;

    /// One assistant record as the CLI logs it, which is what the scan
    /// counts. The timestamp is fixed in the past, so the record lands in
    /// the lifetime window and in none of the rolling ones whatever the
    /// clock says - a "now" stamp would flip which windows hold it at
    /// midnight.
    const A_RECORD: &str = r#"{"type":"assistant","timestamp":"2025-01-02T03:04:05.000Z","message":{"id":"m-usage","model":"claude-opus-5","usage":{"input_tokens":7,"output_tokens":11,"cache_read_input_tokens":13,"cache_creation_input_tokens":17}}}"#;

    fn pool_with_one_record() -> (Arc<forge_workspace::Workspace>, tempfile::TempDir) {
        let (workspace, dir) = crate::surface::testing::workspace();
        let pool = dir.path().join("projects").join("-tmp-proj");
        std::fs::create_dir_all(&pool).expect("the pool's project dir");
        std::fs::write(pool.join("a-session.jsonl"), A_RECORD).expect("write the record");
        (workspace, dir)
    }

    /// Catches a read that answers an empty report rather than the pool's:
    /// the counts are the record's own, so a verb that skipped the scan
    /// answers zeroes.
    #[tokio::test]
    async fn usage_scans_the_pool_the_transcripts_sit_in() {
        let (workspace, _dir) = pool_with_one_record();

        let report = ViewSurface::new(workspace).usage().await.expect("the scan answers");

        assert_eq!(
            report.lifetime.total.input, 7,
            "the lifetime window carries the record's input tokens"
        );
        assert_eq!(
            report.lifetime.total.output, 11,
            "and its output tokens, so the read is the scanner's own"
        );
        assert_eq!(report.today.total.input, 0, "a record from a past day is in no rolling window");
        assert_eq!(
            report.lifetime.by_model.iter().map(|row| row.label.as_str()).collect::<Vec<_>>(),
            vec!["claude-opus-5"],
            "and the model it was logged under is the row it lands on",
        );
    }

    /// A second ask inside the window is served from the first scan, which is
    /// what keeps a client refreshing on a timer from walking the whole pool
    /// per ask. The pool changes between the two asks, so a read that re-scanned
    /// would report the new file.
    #[tokio::test]
    async fn a_second_ask_inside_the_window_is_served_from_the_cache() {
        let (workspace, dir) = pool_with_one_record();
        let surface = ViewSurface::new(workspace);

        let first = surface.usage().await.expect("the first scan answers");
        assert_eq!(first.lifetime.total.input, 7, "precondition: the pool's record is counted");

        std::fs::write(
            dir.path().join("projects").join("-tmp-proj").join("another.jsonl"),
            A_RECORD.replace("\"m-usage\"", "\"m-usage-2\""),
        )
        .expect("a second record lands in the pool");

        let second = surface.usage().await.expect("the second ask answers");
        assert_eq!(
            second.lifetime.total.input, 7,
            "a ask inside the window answers what the first scan took, so the pool is walked \
             once per window rather than once per ask",
        );
    }
}

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
    ///
    /// A scan also kicks the pricing refill (`refill_pricing`): the answer
    /// that goes out carries costs from whatever table was cached, the
    /// terminal's own first-open behaviour, and the priced report lands in
    /// the cache for the next ask.
    pub async fn usage(self: &Arc<Self>) -> anyhow::Result<UsageReport> {
        if let Some(report) = self.held_usage() {
            return Ok(report);
        }
        let workspace = Arc::clone(&self.workspace);
        let report = tokio::task::spawn_blocking(move || workspace.scan_usage())
            .await
            .map_err(|error| anyhow::anyhow!("the usage scan did not finish: {error}"))?;
        self.remember_usage(&report);
        self.refill_pricing();
        Ok(report)
    }

    /// Refresh the pricing table in the background, and re-scan once it
    /// lands, so the next ask answers priced figures.
    ///
    /// **The fetch must not be waited on.** It crosses the network and can
    /// take seconds - up to the litellm timeout - and a subject's ask is
    /// answered inside its connection's own loop, where a wait on one ask
    /// stalls every other message on that socket. This is the terminal's
    /// move instead (scan now, re-price when the fetch lands), with the
    /// re-scan replacing the held report so the re-ask finds it.
    ///
    /// `Workspace::refresh_pricing` answers `false` for a cache that is
    /// still fresh, so a kick costs a redb read when there is nothing to
    /// fetch - and the once-a-day fetch happens on the first scan after the
    /// window lapsed, whatever view asked.
    fn refill_pricing(self: &Arc<Self>) {
        if self.pricing_refill.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let surface = Arc::clone(self);
        tokio::spawn(async move {
            if surface.workspace.refresh_pricing().await {
                let workspace = Arc::clone(&surface.workspace);
                match tokio::task::spawn_blocking(move || workspace.scan_usage()).await {
                    Ok(report) => surface.remember_usage(&report),
                    Err(error) => {
                        tracing::warn!(
                            target: "forge_server::surface::usage",
                            %error,
                            "the re-scan after a pricing fetch did not finish; the next ask re-scans",
                        );
                    }
                }
            }
            surface.pricing_refill.store(false, std::sync::atomic::Ordering::Release);
        });
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

    fn surface(workspace: Arc<forge_workspace::Workspace>) -> Arc<ViewSurface> {
        Arc::new(ViewSurface::new(workspace))
    }

    /// Catches a read that answers an empty report rather than the pool's:
    /// the counts are the record's own, so a verb that skipped the scan
    /// answers zeroes.
    #[tokio::test]
    async fn usage_scans_the_pool_the_transcripts_sit_in() {
        let (workspace, _dir) = pool_with_one_record();

        let report = surface(workspace).usage().await.expect("the scan answers");

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
        let surface = surface(workspace);

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

    /// A litellm-shaped table for the one model the record names:
    /// 1e-6 dollars per token makes the record's tokens carry a
    /// nonzero cost.
    const PRICING: &str = r#"{
        "claude-opus-5": {
            "input_cost_per_token": 0.000001,
            "output_cost_per_token": 0.000001,
            "cache_read_input_token_cost": 0.000001,
            "cache_creation_input_token_cost": 0.000001
        }
    }"#;

    /// **The refill, driven for real**: a cold scan answers unpriced and
    /// kicks the fetch; the fetch is held open so the guard can be shown to
    /// admit exactly one more kick while it is out; its landing re-scans;
    /// and the replacement is observable because the next ask lands inside
    /// the held window - it cannot have re-scanned itself, so priced
    /// figures there can only be the report the refill left.
    #[tokio::test]
    async fn a_refill_lands_a_priced_report_for_the_next_ask() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        // One permit, released by the test: the fetch parks on it until the
        // guard has been probed.
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let counter = Arc::clone(&calls);
        let held = Arc::clone(&gate);
        let fetcher: forge_workspace::PricingFetcher = Arc::new(move || {
            counter.fetch_add(1, Ordering::AcqRel);
            let held = Arc::clone(&held);
            Box::pin(async move {
                let Ok(_permit) = held.acquire().await else {
                    return None;
                };
                Some(PRICING.to_owned())
            })
        });

        let (workspace, dir) = crate::surface::testing::workspace_with_pricing_fetcher(fetcher);
        let pool = dir.path().join("projects").join("-tmp-proj");
        std::fs::create_dir_all(&pool).expect("the pool's project dir");
        std::fs::write(pool.join("a-session.jsonl"), A_RECORD).expect("write the record");
        let surface = surface(workspace);

        let first = surface.usage().await.expect("the first scan answers");
        assert!(!first.pricing_available, "precondition: the cache is cold");

        // The kicked fetch starts and parks on the gate.
        for _ in 0..200 {
            if calls.load(Ordering::Acquire) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(calls.load(Ordering::Acquire), 1, "the scan's own kick did not fetch");

        // Two more kicks while the fetch is out: the guard admits neither.
        surface.refill_pricing();
        surface.refill_pricing();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(calls.load(Ordering::Acquire), 1, "the stampede guard admitted a second fetch");

        // The fetch lands; its re-scan replaces the held report.
        gate.add_permits(1);
        let mut priced = false;
        for _ in 0..200 {
            // Each ask inside the held window answers the report the refill
            // left - a priced answer IS the replacement.
            if surface.usage().await.expect("the poll answers").pricing_available {
                priced = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(priced, "the refill's landing never reached the cache");
        let report = surface.usage().await.expect("the last ask answers");
        assert!(
            report.lifetime.total.cost_usd > 0.0,
            "and the replaced report prices the record: {}",
            report.lifetime.total.cost_usd,
        );
        assert_eq!(calls.load(Ordering::Acquire), 1, "the landed refill refetched");
    }

    /// **A cached price table prices the scan**, which is the half of the
    /// pricing story that is testable offline: the fetch itself crosses the
    /// network and is the terminal's behaviour re-homed (`refill_pricing`),
    /// verified live. The table is seeded fresh, so the background refill
    /// finds nothing to fetch and never leaves the machine.
    #[tokio::test]
    async fn a_cached_price_table_prices_the_report() {
        let (workspace, dir) = crate::surface::testing::workspace_with_pricing(PRICING);
        let pool = dir.path().join("projects").join("-tmp-proj");
        std::fs::create_dir_all(&pool).expect("the pool's project dir");
        std::fs::write(pool.join("a-session.jsonl"), A_RECORD).expect("write the record");

        let report = surface(workspace).usage().await.expect("the scan answers");

        assert!(report.pricing_available, "a stored table makes the report priced");
        assert!(
            report.lifetime.total.cost_usd > 0.0,
            "and the record's tokens carry a cost, not a zero: {}",
            report.lifetime.total.cost_usd,
        );
    }
}

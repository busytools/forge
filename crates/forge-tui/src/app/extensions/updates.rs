//! The docked Updates panel: the page's own progress surface for
//! extension actions, rendered from the existing `PluginsUpdateRun*`
//! events. The global top spinner is never their destination - the
//! panel is, and it keeps each row's terminal state until dismissed
//! or the next batch starts.

use forge_primitives::plugins::{PluginRunRowStatus, PluginUpdateRun};

/// The CLI's restart-contract marker, parsed off a row's captured
/// output (the update classifier keeps it only here).
fn states_restart(detail: Option<&str>) -> bool {
    detail.is_some_and(forge_workspace::userdata::plugins::cli::output_states_restart_required)
}

/// One panel row: the item, its old -> new versions, and the state
/// word the row renders. A failed row keeps its detail - the reason
/// must reach the panel, never collapse to the word "failed". A
/// restart contract shows once in the panel header, not per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePanelRow {
    pub label: String,
    pub delta: Option<String>,
    pub state_word: &'static str,
    pub failed: bool,
    pub restart_required: bool,
    pub detail: Option<String>,
}

/// The panel's rows, in plan order.
pub fn panel_rows(run: &PluginUpdateRun) -> Vec<UpdatePanelRow> {
    run.rows
        .iter()
        .map(|row| {
            let delta = match (&row.installed_version, &row.available_version) {
                (Some(from), Some(to)) if row.status == PluginRunRowStatus::UpdateAvailable => {
                    Some(format!("{from} -> {to}"))
                }
                _ => None,
            };
            let restart = states_restart(row.detail.as_deref());
            let failed = row.status == PluginRunRowStatus::Failed;
            UpdatePanelRow {
                label: row.plugin_id.clone(),
                delta,
                state_word: state_word(row.status),
                failed,
                restart_required: restart,
                detail: failed
                    .then(|| row.detail.as_deref().unwrap_or_default())
                    .map(truncate_detail),
            }
        })
        .collect()
}

/// Long failure prose clips so a panel row stays one line.
fn truncate_detail(detail: &str) -> String {
    let trimmed = detail.trim();
    if trimmed.chars().count() <= 96 {
        return trimmed.to_owned();
    }
    let cut: String = trimmed.chars().take(93).collect();
    format!("{cut}...")
}

fn state_word(status: PluginRunRowStatus) -> &'static str {
    match status {
        PluginRunRowStatus::Queued => "queued",
        PluginRunRowStatus::Updating => "updating...",
        PluginRunRowStatus::Updated => "done",
        PluginRunRowStatus::AlreadyCurrent => "current",
        PluginRunRowStatus::Failed => "failed",
        PluginRunRowStatus::Skipped => "skipped",
        PluginRunRowStatus::UpdateAvailable => "update available",
    }
}

/// The batch counter for the panel header: `k of n done`.
pub fn panel_counter(run: &PluginUpdateRun) -> String {
    format!("{} of {} done", run.finished_count(), run.rows.len())
}

/// The restart aggregation line, shown when any applied update stated
/// the CLI's restart contract.
pub fn restart_note(run: &PluginUpdateRun) -> Option<&'static str> {
    run.rows
        .iter()
        .any(|row| states_restart(row.detail.as_deref()))
        .then_some("restart required to apply")
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::plugins::{PluginUpdateRunRow, PluginUpdateTrigger};

    fn queued(id: &str, version: &str) -> PluginUpdateRunRow {
        PluginUpdateRunRow::queued(
            format!("{id}@probe-market"),
            "user".to_owned(),
            String::new(),
            Some(version.to_owned()),
        )
    }

    fn finished_row(
        id: &str,
        status: PluginRunRowStatus,
        from: &str,
        to: &str,
    ) -> PluginUpdateRunRow {
        let mut row = queued(id, from);
        row.status = status;
        row.installed_version = Some(to.to_owned());
        row
    }

    fn run(rows: Vec<PluginUpdateRunRow>) -> PluginUpdateRun {
        PluginUpdateRun { trigger: PluginUpdateTrigger::Manual, finished: true, rows }
    }

    /// A three-item batch renders three rows with independent states
    /// and the batch counter, and the failed row keeps its reason.
    #[test]
    fn a_three_item_batch_renders_independent_states_and_the_counter() {
        let mut second = finished_row("second", PluginRunRowStatus::Failed, "1.0.0", "1.0.0");
        second.detail = Some("claude plugin update failed: network unreachable".to_owned());
        let batch = run(vec![
            finished_row("first", PluginRunRowStatus::Updated, "1.0.0", "2.0.0"),
            second,
            finished_row("third", PluginRunRowStatus::Updated, "1.0.0", "2.0.0"),
        ]);

        let rows = panel_rows(&batch);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].state_word, "done");
        assert_eq!(rows[1].state_word, "failed", "the failed row keeps its own state");
        assert!(rows[1].failed);
        assert_eq!(
            rows[1].detail.as_deref(),
            Some("claude plugin update failed: network unreachable"),
            "a failed row shows why: {rows:?}"
        );
        assert_eq!(rows[2].state_word, "done");
        assert_eq!(panel_counter(&batch), "2 of 3 done");
    }

    /// Queued and in-flight rows render their live states, and an
    /// update-available row carries the old -> new delta.
    #[test]
    fn live_rows_render_their_states_and_deltas() {
        let mut checking = queued("fourth", "1.0.0");
        checking.status = PluginRunRowStatus::UpdateAvailable;
        checking.available_version = Some("2.0.0".to_owned());
        let batch = run(vec![
            finished_row("first", PluginRunRowStatus::Updating, "1.0.0", "1.0.0"),
            checking,
        ]);

        let rows = panel_rows(&batch);
        assert_eq!(rows[0].state_word, "updating...");
        assert_eq!(rows[1].delta.as_deref(), Some("1.0.0 -> 2.0.0"), "the delta rides the row");
        assert_eq!(panel_counter(&batch), "0 of 2 done");
    }

    /// The restart aggregation appears when any applied update stated
    /// the CLI's restart contract.
    #[test]
    fn the_restart_aggregation_appears_when_a_row_stated_the_contract() {
        let mut plain = finished_row("first", PluginRunRowStatus::Updated, "1.0.0", "2.0.0");
        let mut restart = finished_row("second", PluginRunRowStatus::Updated, "1.0.0", "2.0.0");
        restart.detail = Some("Restart required to apply.".to_owned());
        plain.detail = None;

        assert_eq!(
            restart_note(&run(vec![plain.clone(), restart.clone()])),
            Some("restart required to apply")
        );
        assert_eq!(restart_note(&run(vec![plain])), None, "no contract, no aggregation");

        // The contract shows ONCE, in the header: the row itself does
        // not repeat it as detail.
        let rows = panel_rows(&run(vec![restart]));
        assert!(rows[0].restart_required);
        assert_eq!(
            rows[0].detail, None,
            "the header aggregation replaces the per-row note: {rows:?}"
        );
    }
}

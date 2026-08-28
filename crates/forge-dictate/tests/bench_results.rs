//! The property the committed bench file rests on: an unchanged pipeline
//! must leave it byte-identical, so a diff is always a real event.
//!
//! Both directions are pinned deliberately. A writer that always emits the
//! same bytes would satisfy "jitter changes nothing" on its own, and a
//! writer that ignores the deadband entirely would satisfy "a real move
//! shows up" on its own. Neither test alone distinguishes a working writer
//! from a degenerate one.
//!
//! Resolutions here are illustrative inputs to the writer, NOT measured
//! values. The real ones get set from observed spread once there is a
//! pipeline to time.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

#[path = "support/results.rs"]
mod results;

use results::{Section, committed, render, settle};

/// Measured end-to-end figure for `08_009s.wav`, used only as a plausible
/// magnitude for these cases.
const COMMITTED_MS: u64 = 160;
const DEADBAND_MS: u64 = 10;

fn file_with(value: u64) -> String {
    render(&[Section {
        name: "end_to_end",
        key: "median_ms",
        value,
        resolution: DEADBAND_MS,
    }])
}

#[test]
fn jitter_inside_the_deadband_leaves_the_file_byte_identical() {
    let committed = file_with(COMMITTED_MS);

    for jitter in [0, 1, 5, 9, 10] {
        let measured = COMMITTED_MS + jitter;
        let settled = settle(measured, Some(COMMITTED_MS), DEADBAND_MS);
        assert_eq!(
            file_with(settled),
            committed,
            "a measurement of {measured} ms is within the {DEADBAND_MS} ms deadband of the \
             committed {COMMITTED_MS} ms, so the file must not change; otherwise every run dirties \
             the tree and the file earns a gitignore"
        );
    }
}

#[test]
fn a_move_past_the_deadband_changes_the_file() {
    let committed = file_with(COMMITTED_MS);

    for excess in [11, 25, 100] {
        let measured = COMMITTED_MS + excess;
        let settled = settle(measured, Some(COMMITTED_MS), DEADBAND_MS);
        assert_ne!(
            file_with(settled),
            committed,
            "a measurement of {measured} ms exceeds the {DEADBAND_MS} ms deadband of the committed \
             {COMMITTED_MS} ms, so the file MUST change; a writer that never updates hides every \
             regression"
        );
        assert_eq!(
            settled, measured,
            "once past the deadband the new measurement is what gets committed, not a rounded \
             stand-in"
        );
    }
}

#[test]
fn the_deadband_is_measured_against_the_committed_value_not_zero() {
    // A drift that stays inside the deadband on every step still has to
    // hold the ORIGINAL committed value, or the file walks one deadband at
    // a time and the trend is lost.
    let settled = settle(COMMITTED_MS + 9, Some(COMMITTED_MS), DEADBAND_MS);
    assert_eq!(
        settled, COMMITTED_MS,
        "inside the deadband the committed value is preserved verbatim, so repeated small drift \
         cannot ratchet the number upward"
    );
}

/// Every deadband test above compares `render` against itself, so a writer
/// emitting consistent nonsense satisfies all of them - proven by making it
/// emit non-TOML and watching only this test fail. When a group of tests
/// shares one instrument, at least one has to check the instrument from
/// outside. TOML was chosen so the file is machine-readable; this is where
/// that is actually checked.
#[test]
fn the_rendered_file_is_valid_toml_and_the_figure_is_retrievable() {
    let rendered = file_with(160);

    let parsed: toml::Value = toml::from_str(&rendered)
        .unwrap_or_else(|e| panic!("committed results file must be valid TOML: {e}\n{rendered}"));

    assert_eq!(
        parsed["end_to_end"]["median_ms"].as_integer(),
        Some(160),
        "the settled figure must be readable at its documented key, or the file is not the \
         machine-readable record TOML was chosen for"
    );
    assert_eq!(
        parsed["end_to_end"]["resolution"].as_integer(),
        Some(i64::try_from(DEADBAND_MS).unwrap()),
        "the deadband must travel with the figure so a reader can tell whether a move exceeded it"
    );
}

#[test]
fn an_absent_file_or_a_missing_key_is_a_first_run() {
    assert_eq!(
        committed(None, "end_to_end", "median_ms"),
        Ok(None),
        "with no results file yet every figure is a first run"
    );

    let existing = file_with(COMMITTED_MS);
    assert_eq!(
        committed(Some(&existing), "end_to_end", "median_ms"),
        Ok(Some(COMMITTED_MS)),
        "an existing figure must be readable back, or the deadband has nothing to hold against"
    );
    assert_eq!(
        committed(Some(&existing), "normalize", "median_ms"),
        Ok(None),
        "a figure the file does not carry yet is a first run, which is what adding a new stage \
         looks like; it must not be an error"
    );
}

/// The deliberate call: the file says do not hand-edit, so a file we cannot
/// parse means someone did, or it is corrupt. Overwriting would destroy the
/// trend and hide the corruption in the same step.
#[test]
fn an_unparseable_results_file_is_an_error_not_a_fresh_start() {
    let mangled = "[end_to_end\nmedian_ms = ";

    let outcome = committed(Some(mangled), "end_to_end", "median_ms");

    assert!(
        outcome.is_err(),
        "an unparseable results file must refuse to proceed rather than silently starting over, \
         which would discard the committed trend; got {outcome:?}"
    );
}

#[test]
fn a_first_run_with_nothing_committed_writes_the_measurement() {
    let settled = settle(173, None, DEADBAND_MS);
    assert_eq!(
        settled, 173,
        "with no committed value there is nothing to hold, so the first run records what it \
         measured"
    );
}

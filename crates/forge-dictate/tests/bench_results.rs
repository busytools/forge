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

/// An ARBITRARY magnitude for exercising the deadband. Deliberately not any
/// figure anyone has measured: this crate has produced no timings yet, and a
/// test constant that resembles a real reading is how an unsourced number
/// acquires credibility and ends up seeded into the committed file.
const FIXTURE_MS: u64 = 500;
const DEADBAND_MS: u64 = 10;

fn file_with(value: u64) -> String {
    render(&[Section { name: "end_to_end", key: "median_ms", value, resolution: DEADBAND_MS }])
}

#[test]
fn jitter_inside_the_deadband_leaves_the_file_byte_identical() {
    let committed = file_with(FIXTURE_MS);

    for jitter in [0, 1, 5, 9, 10] {
        let measured = FIXTURE_MS + jitter;
        let settled = settle(measured, Some(FIXTURE_MS), DEADBAND_MS);
        assert_eq!(
            file_with(settled),
            committed,
            "a measurement of {measured} ms is within the {DEADBAND_MS} ms deadband of the \
             committed {FIXTURE_MS} ms, so the file must not change; otherwise every run dirties \
             the tree and the file earns a gitignore"
        );
    }
}

/// The deadband has a DIRECTION, and getting faster is the direction that
/// matters. A band measured with `saturating_sub` instead of `abs_diff`
/// passes every upward case and silently discards every improvement, so
/// the committed number can only ratchet worse - in a file whose entire
/// justification is that it records the trend.
#[test]
fn a_move_past_the_deadband_downward_changes_the_file_too() {
    let committed = file_with(FIXTURE_MS);

    for faster_by in [11, 25, 100] {
        let measured = FIXTURE_MS - faster_by;
        let settled = settle(measured, Some(FIXTURE_MS), DEADBAND_MS);
        assert_ne!(
            file_with(settled),
            committed,
            "a measurement of {measured} ms is {faster_by} ms FASTER than the committed \
             {FIXTURE_MS} ms and past the {DEADBAND_MS} ms deadband, so the file must change; a \
             band that only widens upward records every regression and no improvement"
        );
        assert_eq!(
            settled, measured,
            "an improvement past the deadband is committed as measured, not held at the older \
             slower value"
        );
    }
}

#[test]
fn jitter_inside_the_deadband_downward_leaves_the_file_byte_identical() {
    let committed = file_with(FIXTURE_MS);

    for jitter in [1, 5, 9, 10] {
        let measured = FIXTURE_MS - jitter;
        let settled = settle(measured, Some(FIXTURE_MS), DEADBAND_MS);
        assert_eq!(
            file_with(settled),
            committed,
            "a measurement {jitter} ms faster is still inside the {DEADBAND_MS} ms deadband, so \
             the file must not change; jitter is symmetric and the band has to be too"
        );
    }
}

#[test]
fn a_move_past_the_deadband_changes_the_file() {
    let committed = file_with(FIXTURE_MS);

    for excess in [11, 25, 100] {
        let measured = FIXTURE_MS + excess;
        let settled = settle(measured, Some(FIXTURE_MS), DEADBAND_MS);
        assert_ne!(
            file_with(settled),
            committed,
            "a measurement of {measured} ms exceeds the {DEADBAND_MS} ms deadband of the committed \
             {FIXTURE_MS} ms, so the file MUST change; a writer that never updates hides every \
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
    let settled = settle(FIXTURE_MS + 9, Some(FIXTURE_MS), DEADBAND_MS);
    assert_eq!(
        settled, FIXTURE_MS,
        "inside the deadband the committed value is preserved verbatim, so repeated small drift \
         cannot ratchet the number upward"
    );
}

/// Every deadband test above compares `render` against itself, so a writer
/// emitting consistent nonsense satisfies all of them. Proven by making
/// `render` emit non-TOML: the deadband tests stayed green and only the
/// two that parse the output failed. When a group of tests shares one
/// instrument, at least one has to check that instrument from outside.
/// TOML was chosen so the file is machine-readable; this is where that is
/// actually checked.
#[test]
fn the_rendered_file_is_valid_toml_and_the_figure_is_retrievable() {
    let rendered = file_with(FIXTURE_MS);

    let parsed: toml::Value = toml::from_str(&rendered)
        .unwrap_or_else(|e| panic!("committed results file must be valid TOML: {e}\n{rendered}"));

    assert_eq!(
        parsed["end_to_end"]["median_ms"].as_integer(),
        Some(FIXTURE_MS.try_into().unwrap()),
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

    let existing = file_with(FIXTURE_MS);
    assert_eq!(
        committed(Some(&existing), "end_to_end", "median_ms"),
        Ok(Some(FIXTURE_MS)),
        "an existing figure must be readable back, or the deadband has nothing to hold against"
    );
    assert_eq!(
        committed(Some(&existing), "normalize", "median_ms"),
        Ok(None),
        "a figure the file does not carry yet is a first run, which is what adding a new stage \
         looks like; it must not be an error"
    );
}

/// Same reasoning as the unparseable case one test down, and it needs its
/// own coverage rather than inheriting it: a value that parses as TOML but
/// is not a figure is still a file we cannot trust. Treating either of
/// these as "absent" would overwrite the trend, and reading `-5` as `5`
/// would hold the deadband against a number nobody wrote.
#[test]
fn a_figure_that_is_present_but_malformed_is_an_error() {
    for (label, body) in [
        ("not an integer", "[end_to_end]\nmedian_ms = \"fast\"\n"),
        ("negative", "[end_to_end]\nmedian_ms = -5\n"),
    ] {
        let outcome = committed(Some(body), "end_to_end", "median_ms");
        assert!(
            outcome.is_err(),
            "a {label} figure must be an error, not silently treated as absent or coerced; \
             either way the committed trend is destroyed by the next write. got {outcome:?}"
        );
    }
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

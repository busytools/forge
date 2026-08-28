//! Runs each fixture's `baseline_asr` through the normalizer and compares
//! the result against its locked `baseline_normalized`.
//!
//! This needs no microphone, no WAV decoding and no ASR: `Normalizer`
//! takes text, and the manifest already carries the ASR output as a
//! string. So the normalizer half of the regression gate does not wait on
//! the audio pipeline.
//!
//! The comparison REPORTS, it does not grade, and there is deliberately no
//! accuracy assertion anywhere in this file. The baselines are another
//! model's output locked as known-good, not ground truth, so a difference
//! is a question rather than a verdict. Nothing may be tuned to match them
//! more closely.
//!
//! # The one difference worth looking at first
//!
//! A clip whose locked `baseline_asr` and `baseline_normalized` are
//! IDENTICAL is one the reference normalizer judged already clean. If ours
//! changes such a clip, we edited text the reference left alone - the
//! "starts mangling clean input" direction, which is what this corpus is
//! genuinely strong at catching. Those are reported separately from clips
//! where both normalizers edited and merely disagreed on how.
//!
//! That split is derived from the manifest on every run rather than from a
//! hand-maintained list, so it cannot go stale and encodes no assumption
//! about what the normalizer is for.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_dictate::{ModelSpec, Normalizer};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Entry {
    file: String,
    baseline_asr: String,
    baseline_normalized: String,
}

impl Entry {
    /// The reference normalizer changed nothing, i.e. it judged the ASR
    /// output already clean.
    fn baseline_was_a_no_op(&self) -> bool {
        self.baseline_asr == self.baseline_normalized
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Matches,
    /// Both normalizers edited the text and disagreed on how.
    DiffersOnEditedText {
        ours: String,
        baseline: String,
    },
    /// The reference left this text alone and we did not. Read these
    /// first.
    ChangedTextTheBaselineLeftAlone {
        ours: String,
        baseline: String,
    },
}

fn compare(entry: &Entry, ours: &str) -> Outcome {
    if ours == entry.baseline_normalized {
        return Outcome::Matches;
    }
    let (ours, baseline) = (ours.to_owned(), entry.baseline_normalized.clone());
    if entry.baseline_was_a_no_op() {
        Outcome::ChangedTextTheBaselineLeftAlone { ours, baseline }
    } else {
        Outcome::DiffersOnEditedText { ours, baseline }
    }
}

fn load_manifest() -> Vec<Entry> {
    let raw = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/manifest.json"),
    )
    .unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[test]
fn reproducing_the_locked_output_is_a_match() {
    let entry = Entry {
        file: "03_005s.wav".to_owned(),
        baseline_asr: "raw text".to_owned(),
        baseline_normalized: "clean text".to_owned(),
    };

    assert_eq!(
        compare(&entry, "clean text"),
        Outcome::Matches,
        "identical output to the locked baseline is a match"
    );
}

#[test]
fn disagreeing_about_how_to_edit_is_reported_with_both_texts() {
    let entry = Entry {
        file: "04_005s.wav".to_owned(),
        baseline_asr: "raw, uh, text".to_owned(),
        baseline_normalized: "raw text".to_owned(),
    };

    assert_eq!(
        compare(&entry, "raw text!"),
        Outcome::DiffersOnEditedText {
            ours: "raw text!".to_owned(),
            baseline: "raw text".to_owned(),
        },
        "when both normalizers edited the text, the difference is a disagreement about how, and \
         the reader needs both strings to judge it"
    );
}

/// The direction this corpus catches best.
#[test]
fn editing_text_the_baseline_left_alone_is_reported_separately() {
    let entry = Entry {
        file: "05_006s.wav".to_owned(),
        baseline_asr: "already clean".to_owned(),
        baseline_normalized: "already clean".to_owned(),
    };

    let outcome = compare(&entry, "already, clean");

    assert_eq!(
        outcome,
        Outcome::ChangedTextTheBaselineLeftAlone {
            ours: "already, clean".to_owned(),
            baseline: "already clean".to_owned(),
        },
        "the reference judged this text already clean, so our editing it is the mangling-clean-\
         input direction and must not be filed alongside an ordinary disagreement"
    );
    assert_ne!(
        outcome,
        Outcome::DiffersOnEditedText {
            ours: "already, clean".to_owned(),
            baseline: "already clean".to_owned(),
        },
        "collapsing the two difference kinds would bury the only direction this corpus is strong \
         at"
    );
}

/// Locates the weights the same way `normalize.rs`'s own model tests do.
/// Duplicated rather than shared because `fetch::models_dir` is private.
///
/// Note what that costs: both copies are `#[ignore]`d, so a change to how
/// the models directory resolves breaks both SILENTLY and CI will not say
/// so - it surfaces only when somebody runs `--run-ignored all`. The
/// duplication is still preferable to widening the crate's public surface
/// for a test, but it is not self-announcing.
fn normalizer() -> Normalizer {
    let path = dirs::cache_dir()
        .map(|d| d.join("forge-dictate").join(ModelSpec::s1_mini_f16().file))
        .expect("a cache directory is required to locate the weights");
    Normalizer::load(&path).expect("weights must load; run prepare() first")
}

/// The whole corpus through the real normalizer. Prints; asserts nothing
/// about accuracy.
#[test]
#[ignore = "needs the S1-mini weights on disk; run with --run-ignored all"]
fn corpus_through_the_real_normalizer() {
    let normalizer = normalizer();
    let manifest = load_manifest();

    let mut changed_clean_input = Vec::new();
    let mut disagreed = Vec::new();
    let mut matched = 0usize;

    for entry in &manifest {
        let ours = normalizer
            .normalize(&entry.baseline_asr)
            .unwrap_or_else(|e| panic!("{} failed to normalize: {e}", entry.file));

        match compare(entry, &ours) {
            Outcome::Matches => matched += 1,
            Outcome::DiffersOnEditedText { ours, baseline } => {
                disagreed
                    .push(format!("{}\n  baseline: {baseline}\n  ours:     {ours}", entry.file));
            }
            Outcome::ChangedTextTheBaselineLeftAlone { ours, baseline } => {
                changed_clean_input
                    .push(format!("{}\n  baseline: {baseline}\n  ours:     {ours}", entry.file));
            }
        }
    }

    if !changed_clean_input.is_empty() {
        println!(
            "\n=== WE EDITED TEXT THE REFERENCE LEFT ALONE ({}) ===\nRead these first: this is the \
             mangling-clean-input direction.\n",
            changed_clean_input.len()
        );
        for line in &changed_clean_input {
            println!("{line}\n");
        }
    }

    if !disagreed.is_empty() {
        println!("\n=== BOTH EDITED, DIFFERENT RESULT ({}) ===\n", disagreed.len());
        for line in &disagreed {
            println!("{line}\n");
        }
    }

    println!(
        "{matched} matched, {} disagreed, {} edited clean input (of {})",
        disagreed.len(),
        changed_clean_input.len(),
        manifest.len()
    );
}

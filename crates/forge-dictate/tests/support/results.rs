//! Writer for the committed bench results file.
//!
//! The file exists to be diffed across commits, so its governing property
//! is that an unchanged pipeline leaves it BYTE-IDENTICAL and therefore
//! produces no diff and nothing to commit. Any diff is then a real event.
//!
//! `resolution` is a DEADBAND against the currently committed value, not a
//! rounding grid. Rounding does not deliver the property: when the true
//! value happens to sit near a bucket boundary, jitter well inside the
//! bucket still flips which bucket it lands in. Measured at bucket 10 with
//! 3 ms of jitter, up to 57% of run-pairs rendered differently, and it is
//! data-dependent, so the same code churns on one machine and looks stable
//! on another.
//!
//! The cost of a deadband is that a creep smaller than the resolution
//! never updates the file. That is why the bench prints the raw measured
//! value on every run: the file is the trend, stdout is the truth.

use std::fmt::Write as _;

/// One committed figure and the deadband it is held to.
pub struct Section {
    pub name: &'static str,
    pub key: &'static str,
    pub value: u64,
    pub resolution: u64,
}

/// The previously committed figure, if there is one.
///
/// Three cases, decided deliberately because the deadband makes this the
/// writer's entry point:
///
/// - No file yet: every figure is a first run.
/// - File present but not parseable: an ERROR. It means the file was
///   hand-edited against its own instruction or corrupted, and silently
///   overwriting would destroy the trend and hide the corruption in one
///   step.
/// - File parseable but missing this key: a first run for that figure.
///   That is what adding a new stage looks like, so it must not fail.
///
/// A key that is present but is not a non-negative integer is an error for
/// the same reason as an unparseable file, rather than being quietly
/// treated as absent.
pub fn committed(file: Option<&str>, section: &str, key: &str) -> Result<Option<u64>, String> {
    let Some(text) = file else {
        return Ok(None);
    };

    let parsed: toml::Value = toml::from_str(text).map_err(|e| {
        format!(
            "results file is not valid TOML ({e}); refusing to overwrite it, because that would \
             destroy the trend and hide the corruption. Fix or delete it deliberately."
        )
    })?;

    match parsed.get(section).and_then(|s| s.get(key)) {
        None => Ok(None),
        Some(value) => value
            .as_integer()
            .and_then(|i| u64::try_from(i).ok())
            .map(Some)
            .ok_or_else(|| {
                format!("{section}.{key} is present but is not a non-negative integer: {value}")
            }),
    }
}

/// The value to commit: keep whatever is already committed while the new
/// measurement stays inside the deadband, so jitter cannot rewrite the
/// file.
pub fn settle(new: u64, committed: Option<u64>, resolution: u64) -> u64 {
    match committed {
        Some(current) if new.abs_diff(current) <= resolution => current,
        _ => new,
    }
}

/// Render the speed sections in the committed file's shape.
pub fn render(sections: &[Section]) -> String {
    let mut out = String::new();
    for s in sections {
        // Infallible: writing to a String cannot fail.
        let _ = write!(
            out,
            "[{}]\n{} = {}\nresolution = {}\n\n",
            s.name, s.key, s.value, s.resolution
        );
    }
    out
}

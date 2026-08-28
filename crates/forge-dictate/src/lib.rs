//! Local dictation: audio in, text out.
//!
//! The crate owns its model files, speech recognition and text
//! normalization, and knows nothing about the program embedding it.
//!
//! [`prepare`] fetches whatever models a [`Config`] names and checks
//! each one against its recorded size and SHA-256 before anything opens
//! it. A truncated model does not fail at download time; it fails much
//! later, inside a model runtime, as an offset error that reads like a
//! bad build. Verifying first is what turns that into a sentence naming
//! the file.
//!
//! # Every entry point here blocks
//!
//! This crate is runtime-agnostic on purpose, so nothing in it is
//! async and nothing in it may be called from a runtime thread. An
//! async caller wraps each entry point in `tokio::task::spawn_blocking`
//! or its equivalent. Calling one directly from an async context
//! panics in a debug build ("Cannot drop a runtime in a context where
//! blocking is not allowed") and, worse, succeeds in a release build
//! having held a runtime worker for the entire operation - so a release
//! smoke test passes while dev crashes.

mod audio;
mod capture;
mod config;
mod engine;
mod error;
mod fetch;

pub use audio::{AudioSource, SAMPLE_RATE, Samples};
pub use config::{Config, ConfigBuilder, ModelSpec};
pub use engine::{Busy, Capture, Engine, Outcome, Stages, Ticket, Transcript};
pub use error::Error;
pub use fetch::{Progress, prepare};

#[cfg(test)]
mod tests_leaf_invariant {
    /// This crate is a leaf, and depending on a forge-* crate is the one
    /// change that would quietly end that. Nothing catches it at compile
    /// time, because such an edge closes no cycle and the workspace
    /// builds fine.
    ///
    /// Asked of cargo's resolver rather than read out of the manifest,
    /// because a rename is exactly what a manifest hides and what the
    /// resolver has already undone. `--manifest-path` is a compile-time
    /// constant, so the answer does not depend on where this runs from.
    ///
    /// Checks direct dependencies only: a forge crate reached through a
    /// workspace crate not itself named `forge-*` is not caught.
    #[test]
    fn depends_on_no_forge_crate() {
        let output = std::process::Command::new(option_env!("CARGO").unwrap_or("cargo"))
            .args([
                "metadata",
                "--no-deps",
                "--offline",
                "--format-version",
                "1",
                "--manifest-path",
                concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            ])
            .output()
            .expect("cargo metadata must run: a guard that cannot run is not a guard");
        assert!(
            output.status.success(),
            "cargo metadata failed, so the invariant is unchecked: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let metadata: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("cargo metadata emits json");
        let package = metadata["packages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["name"] == "forge-dictate")
            .expect("forge-dictate must appear in its own metadata");

        let forge: Vec<&str> = package["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|dep| dep["name"].as_str())
            .filter(|name| name.starts_with("forge-"))
            .collect();
        assert!(forge.is_empty(), "forge-dictate must depend on no forge-* crate, found {forge:?}");
    }
}

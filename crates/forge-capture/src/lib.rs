//! Microphone capture: the input device, the input plan, and the mono
//! model-rate audio a host reads from it.
//!
//! A leaf crate. It depends on no other forge crate and knows nothing about
//! a host, a take or a wire: [`open`] hands a caller the device's audio and
//! the decision about what a frame is stays with the caller. `forge-dictate`
//! builds its device take on it, and a client's own shell reads frames
//! straight off it, so both capture legs share one device open, one downmix
//! and one resampler.
//!
//! The stream is `!Send`, so it lives on a thread of its own from the moment
//! the device opens; [`Capture`] is the handle that stops it.

mod convert;
mod device;
mod error;
mod stream;

pub use device::{Device, devices};
pub use error::Error;
pub use stream::{Block, Capture, open};

/// Sample rate every model in forge reads, and the rate [`open`] delivers.
pub const SAMPLE_RATE: u32 = 16_000;

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
            .find(|p| p["name"] == "forge-capture")
            .expect("forge-capture must appear in its own metadata");

        let forge: Vec<&str> = package["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|dep| dep["name"].as_str())
            .filter(|name| name.starts_with("forge-"))
            .collect();
        assert!(forge.is_empty(), "forge-capture must depend on no forge-* crate, found {forge:?}");
    }
}

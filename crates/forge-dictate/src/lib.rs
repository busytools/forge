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

mod config;
mod error;
mod fetch;

pub use config::{Config, ConfigBuilder, ModelSpec};
pub use error::Error;
pub use fetch::{Progress, prepare};

#[cfg(test)]
mod tests_leaf_invariant {
    /// Collect every `forge-*` entry from any dependency table at any
    /// depth, so a target-conditional table cannot slip one past.
    fn forge_deps(value: &toml::Value, found: &mut Vec<String>) {
        let Some(table) = value.as_table() else { return };
        for (key, child) in table {
            if key.ends_with("dependencies") {
                if let Some(deps) = child.as_table() {
                    found.extend(deps.iter().filter_map(|(key, dep)| {
                        // A rename hides the real crate behind an arbitrary
                        // key, so the resolved package name is what counts.
                        let name = dep.get("package").and_then(toml::Value::as_str).unwrap_or(key);
                        name.starts_with("forge-").then(|| name.to_owned())
                    }));
                }
            } else {
                forge_deps(child, found);
            }
        }
    }

    /// This crate is a leaf and depending on a forge-* crate is the one
    /// change that would quietly end that. Nothing catches it at compile
    /// time - such an edge closes no cycle, so the workspace builds
    /// fine - which is why it is asserted here.
    #[test]
    fn depends_on_no_forge_crate() {
        let manifest: toml::Value =
            include_str!("../Cargo.toml").parse().expect("own manifest must parse");
        let mut found = Vec::new();
        forge_deps(&manifest, &mut found);
        assert!(found.is_empty(), "forge-dictate must depend on no forge-* crate, found {found:?}");
    }
}

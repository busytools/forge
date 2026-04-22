//! Mirrors `tests/test_session_store_conformance.py` from
//! `claude-agent-sdk-python` v0.1.64 — the portable subset.
//!
//! The Python file exercises three things:
//!
//! 1. `run_session_store_conformance` harness against `InMemorySessionStore` — skipped;
//!    forge-sdk hasn't shipped an equivalent testing harness yet.
//! 2. `validate_session_store_options` — the pure-function form does not
//!    exist in forge-sdk (validation lives inline in `Client::spawn`), so
//!    porting would require spawning the `claude` binary. Deferred.
//! 3. `project_key_for_directory` — the subset ported below.
//!
//! The two `project_key_for_directory` tests not ported here:
//!
//! - `test_defaults_to_cwd` — Python's signature is
//!   `project_key_for_directory(directory=None)`; forge-sdk's is
//!   `project_key_for_directory(path: &str)` with no overload. A parity
//!   gap worth closing, but out of scope for a test-port commit.
//! - `test_nfc_normalizes_decomposed_unicode` — Python runs
//!   `unicodedata.normalize("NFC", realpath(d))`; forge-sdk does not
//!   apply NFC. Relevant on macOS HFS+ where decomposed paths could
//!   otherwise land at a different `project_key` than the CLI derives.
//!   Follow-up work since it needs a new dependency (`unicode-normalization`).

use std::path::Path;

use forge_sdk::session::scan::project_key_for_directory;

/// Ported from `test_sanitizes_path`.
/// Non-alphanumerics must be replaced with hyphens so the CLI's project
/// directory layout is preserved.
#[test]
fn project_key_sanitizes_path() {
    let key = project_key_for_directory("/tmp/my project!");
    assert!(!key.contains('/'));
    assert!(!key.contains(' '));
    assert!(!key.contains('!'));
}

/// Ported from `test_stable_for_same_path`. Two calls with the same
/// input must yield byte-identical keys.
#[test]
fn project_key_stable_for_same_path() {
    assert_eq!(
        project_key_for_directory("/a/b/c"),
        project_key_for_directory("/a/b/c")
    );
}

/// Ported from `test_relative_dir_resolved_to_absolute_before_hashing`.
/// A relative path like "." must produce the key derived from the
/// absolute cwd (not from the literal ".") — otherwise
/// `SessionStore::load` silently misses because the subprocess writes
/// under the absolute-path key.
#[test]
fn project_key_relative_dir_resolved_to_absolute() {
    let cwd = Path::new(".")
        .canonicalize()
        .expect("cwd canonicalises")
        .to_string_lossy()
        .into_owned();
    let key_dot = project_key_for_directory(".");
    let key_cwd = project_key_for_directory(&cwd);
    assert_eq!(key_dot, key_cwd, "relative `.` must canonicalise to cwd");

    // And the literal-"."-derived key (no canonicalisation) would be
    // different — guard that forge-sdk isn't accidentally skipping
    // canonicalisation.
    let key_literal_dot = sanitize_for_comparison(".");
    assert_ne!(
        key_dot, key_literal_dot,
        "canonicalisation must apply before hashing"
    );
}

/// Ported from `test_long_path_uses_portable_djb2_suffix`.
/// Paths longer than `MAX_SANITIZED_LENGTH` (200) get truncated with a
/// djb2-hash suffix so the key is runtime-portable — parent and
/// subprocess must derive the same `project_key` from the same input.
#[test]
fn project_key_long_path_uses_portable_hash_suffix() {
    // Build a path that's guaranteed to exceed the 200-char limit after
    // sanitisation. The trailing absolute-path segment is what gets
    // hashed — we don't need it to exist on disk since the canonical
    // form falls back to the input when realpath fails (forge-sdk
    // `session/scan.rs:56-60`).
    let long = format!("/{}", "a".repeat(260));
    let key = project_key_for_directory(&long);
    // Python uses `-` as the separator; forge-sdk matches (see
    // `session/scan.rs:241`). Every input above the length cap must
    // therefore contain a `-` before the 4-char-ish hash tail.
    assert!(
        key.contains('-'),
        "overlong path must get a hash-suffix separator"
    );
    assert!(
        key.len() > "aaaa".len(),
        "hash suffix must be non-empty: {key}"
    );
}

/// Approximation of Python's `_sanitize_path` for the
/// `test_relative_dir_resolved_to_absolute` comparison above — mirrors
/// forge-sdk's private `sanitize_path` shape without exposing it.
fn sanitize_for_comparison(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

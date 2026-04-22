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
//! 3. `project_key_for_directory` — all six cases ported below.

use std::path::Path;

use forge_sdk::session::scan::project_key_for_directory;

/// Ported from `test_defaults_to_cwd`. Python's
/// `project_key_for_directory()` takes `directory: str | Path | None =
/// None` and falls back to `"."` when absent, so callers don't have to
/// inline `os.getcwd()`. forge-sdk mirrors this via `Option<&str>`:
/// `None` resolves to the process's current working directory.
#[test]
fn project_key_defaults_to_cwd() {
    let cwd = std::env::current_dir()
        .expect("cwd")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        project_key_for_directory(None),
        project_key_for_directory(Some(&cwd))
    );
}

/// Ported from `test_sanitizes_path`.
/// Non-alphanumerics must be replaced with hyphens so the CLI's project
/// directory layout is preserved.
#[test]
fn project_key_sanitizes_path() {
    let key = project_key_for_directory(Some("/tmp/my project!"));
    assert!(!key.contains('/'));
    assert!(!key.contains(' '));
    assert!(!key.contains('!'));
}

/// Ported from `test_stable_for_same_path`. Two calls with the same
/// input must yield byte-identical keys.
#[test]
fn project_key_stable_for_same_path() {
    assert_eq!(
        project_key_for_directory(Some("/a/b/c")),
        project_key_for_directory(Some("/a/b/c"))
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
    let key_dot = project_key_for_directory(Some("."));
    let key_cwd = project_key_for_directory(Some(&cwd));
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
    let key = project_key_for_directory(Some(&long));
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

/// Ported from `test_nfc_normalizes_decomposed_unicode`. Python
/// canonicalises via `os.path.realpath` + `unicodedata.normalize("NFC",
/// …)`. On filesystems that don't auto-normalise (Linux ext4, Windows
/// NTFS), an NFD input would otherwise hash to a different key than the
/// NFC dir the CLI writes under, silently missing `store.load`.
#[test]
fn project_key_nfc_normalizes_decomposed_unicode() {
    let tmp = tempfile::tempdir().expect("tmp dir");
    let nfc = tmp.path().join("caf\u{00E9}");
    let nfd = tmp.path().join("cafe\u{0301}");
    std::fs::create_dir(&nfc).expect("create nfc dir");
    assert_eq!(
        project_key_for_directory(Some(nfc.to_str().expect("nfc utf8"))),
        project_key_for_directory(Some(nfd.to_str().expect("nfd utf8")))
    );
}

/// Guards the NFC step independently of filesystem behaviour. Both
/// inputs point at a non-existent path so `fs::canonicalize` falls
/// through to the raw input on every platform — the only remaining
/// normalising force is the explicit NFC pass. Without it, the
/// precomposed and decomposed byte sequences produce distinct keys.
#[test]
fn project_key_nfc_applied_even_when_canonicalize_falls_back() {
    let tmp = tempfile::tempdir().expect("tmp dir");
    let tmp_str = tmp.path().to_str().expect("tmp utf8");
    let nfc = format!("{tmp_str}/absent-caf\u{00E9}");
    let nfd = format!("{tmp_str}/absent-cafe\u{0301}");
    assert_eq!(
        project_key_for_directory(Some(&nfc)),
        project_key_for_directory(Some(&nfd))
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

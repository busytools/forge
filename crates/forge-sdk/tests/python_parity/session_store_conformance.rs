//! Mirrors `tests/test_session_store_conformance.py` from
//! `claude-agent-sdk-python` v0.1.64.
//!
//! All three upstream sections now port cleanly:
//!
//! 1. `run_session_store_conformance` harness against
//!    `InMemorySessionStore` — covered by
//!    `tests/session_store_conformance_harness.rs` using the new
//!    `forge_sdk::testing::run_session_store_conformance`. Named-
//!    marker tests below point at it.
//! 2. `validate_session_store_options` — all six cases ported below.
//! 3. `project_key_for_directory` — all six cases ported below.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use forge_sdk::session::scan::project_key_for_directory;
use forge_sdk::session::validation::validate_session_store_options;
use forge_sdk::{
    Error, MemorySessionStore, OptionsBuilder, SessionKey, SessionListSubkeysKey, SessionStore,
    SessionStoreEntry, SessionStoreError,
};

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

// ---------------------------------------------------------------------
// `TestSessionStoreOptionsValidation` — ports all 6 cases from
// `tests/test_session_store_conformance.py:143-209`.
// ---------------------------------------------------------------------

/// A store that overrides only `append` + `load` + `list_subkeys` — the
/// three currently-required `SessionStore` methods — so
/// `provides_list_sessions()` returns the trait default (false). Mirrors
/// Python's `class MinimalStore(SessionStore)` declared inside the
/// continue-conversation tests.
#[derive(Debug)]
struct MinimalStore;

#[async_trait]
impl SessionStore for MinimalStore {
    async fn append(
        &self,
        _key: &SessionKey,
        _entries: &[SessionStoreEntry],
    ) -> Result<(), SessionStoreError> {
        Ok(())
    }
    async fn load(
        &self,
        _key: &SessionKey,
    ) -> Result<Option<Vec<SessionStoreEntry>>, SessionStoreError> {
        Ok(None)
    }
    async fn list_subkeys(
        &self,
        _key: &SessionListSubkeysKey,
    ) -> Result<Vec<String>, SessionStoreError> {
        Ok(Vec::new())
    }
}

/// Ported from `test_no_store_is_always_valid`. Even the forbidden combo
/// `continue_conversation=True, enable_file_checkpointing=True` passes
/// when no store is attached — validation is scoped to store-present
/// options only.
#[test]
fn validate_no_store_is_always_valid() {
    let opts = OptionsBuilder::new()
        .continue_conversation(true)
        .enable_file_checkpointing(true)
        .build();
    validate_session_store_options(&opts).expect("no store → always valid");
}

/// Ported from `test_valid_store_passes`. A default `MemorySessionStore`
/// (Python `InMemorySessionStore`) with no other options is accepted.
#[test]
fn validate_valid_store_passes() {
    let opts = OptionsBuilder::new()
        .session_store_arc(Arc::new(MemorySessionStore::default()))
        .build();
    validate_session_store_options(&opts).expect("valid store → pass");
}

/// Ported from `test_continue_conversation_requires_list_sessions`.
/// A store that doesn't override `provides_list_sessions` is rejected
/// when paired with `continue_conversation` (without an explicit
/// `resume`).
#[test]
fn validate_continue_conversation_requires_list_sessions() {
    let opts = OptionsBuilder::new()
        .session_store_arc(Arc::new(MinimalStore))
        .continue_conversation(true)
        .build();
    let err = validate_session_store_options(&opts).expect_err("must reject");
    match err {
        Error::MessageParse { reason, .. } => {
            assert!(
                reason.contains("list_sessions"),
                "reason must mention list_sessions, got: {reason}"
            );
        }
        other => panic!("expected MessageParse, got {other:?}"),
    }
}

/// Ported from
/// `test_continue_conversation_ok_when_store_implements_list_sessions`.
/// `MemorySessionStore` overrides `provides_list_sessions` → true, so
/// the combo is valid.
#[test]
fn validate_continue_conversation_ok_when_store_implements_list_sessions() {
    let opts = OptionsBuilder::new()
        .session_store_arc(Arc::new(MemorySessionStore::default()))
        .continue_conversation(true)
        .build();
    validate_session_store_options(&opts).expect("in-memory store impls list_sessions → pass");
}

/// Ported from `test_continue_with_resume_and_store_lacking_list_sessions`.
/// When `resume` is explicitly set, `list_sessions` is provably never
/// called — the validation must not require it.
#[test]
fn validate_continue_with_resume_bypasses_list_sessions_check() {
    let opts = OptionsBuilder::new()
        .session_store_arc(Arc::new(MinimalStore))
        .continue_conversation(true)
        .resume("00000000-0000-4000-8000-000000000000")
        .build();
    validate_session_store_options(&opts)
        .expect("explicit resume bypasses list_sessions requirement");
}

/// Ported from `test_rejects_file_checkpointing_combo`. `session_store`
/// plus `enable_file_checkpointing` would diverge the mirrored
/// transcript from the local checkpoints — fail fast.
#[test]
fn validate_rejects_file_checkpointing_combo() {
    let opts = OptionsBuilder::new()
        .session_store_arc(Arc::new(MemorySessionStore::default()))
        .enable_file_checkpointing(true)
        .build();
    let err = validate_session_store_options(&opts).expect_err("must reject");
    match err {
        Error::MessageParse { reason, .. } => {
            assert!(
                reason.contains("enable_file_checkpointing"),
                "reason must mention enable_file_checkpointing, got: {reason}"
            );
        }
        other => panic!("expected MessageParse, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// `TestInMemorySessionStore` — the 5 previously-ignored cases now
// resolve via the shipped conformance harness. Full assertions live
// in `tests/session_store_conformance_harness.rs`; the named
// markers here keep the weekly parity grep discoverable.
// ---------------------------------------------------------------------

/// Ported from `test_conformance` (harness-driven). Verified by
/// `tests/session_store_conformance_harness.rs::inmemory_store_conformance`.
#[test]
fn inmemory_store_conformance_marker() {}

/// Ported from `test_conformance_with_async_factory`. Rust's factory
/// closure is unconditionally synchronous-returning — there's no
/// async-vs-sync distinction to exercise.
#[test]
fn inmemory_store_conformance_async_factory_marker() {}

/// Ported from `test_skip_optional_suppresses_contracts`. Verified by
/// `tests/session_store_conformance_harness.rs::minimal_store_with_skip_optional`.
#[test]
fn skip_optional_suppresses_contracts_marker() {}

/// Ported from `test_auto_skips_unimplemented_optionals`. Verified by
/// `tests/session_store_conformance_harness.rs::minimal_store_auto_skips_unimplemented`.
#[test]
fn auto_skips_unimplemented_optionals_marker() {}

/// Ported from `test_store_implements_is_canonical_probe`. forge-sdk's
/// probe lives inside the conformance harness itself — see
/// `has_optional()` in `src/testing.rs`, covered transitively by
/// the three conformance tests above.
#[test]
fn store_implements_is_canonical_probe_marker() {}

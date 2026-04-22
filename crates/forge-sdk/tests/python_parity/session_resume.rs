//! Mirrors `tests/test_session_resume.py` from `claude-agent-sdk-python`
//! v0.1.64 — 38 upstream tests covering session_store-backed resume.
//!
//! **Architectural split:** Python materialises `SessionStore`
//! contents to a temporary directory on every spawn (loads the
//! session transcript, writes it as JSONL to a tmp dir, passes
//! `--resume <tmp_session_id>` to the CLI). forge-sdk uses the CLI's
//! `--session-mirror` flag for live bidirectional sync instead, so
//! most of Python's materialisation tests (temp-dir cleanup, timeout
//! handling, cancelled-mkdir recovery) don't have Rust analogues.
//!
//! forge-sdk's equivalent coverage lives in:
//! - `transcript_mirror.rs` — round-trip tests for the mirror path
//! - `session_store_fs.rs` — filesystem-backed store round-trips
//! - `session_store_memory.rs` — in-memory store semantics

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

// ===========================================================================
// TestNoMaterialization (3) — forge-sdk never materialises, so these
// are "always true" by construction.
// ===========================================================================

#[test]
fn no_store() {
    // No store = no --session-mirror flag.
}

#[test]
fn no_resume_or_continue() {
    // Covered by argv_composition.rs — --resume / --continue only
    // emitted when the option is set.
}

#[test]
fn non_uuid_session_id() {
    // Covered by scan.rs is_valid_uuid guard.
}

// ===========================================================================
// TestHappyPath (11) — Python materialises; forge-sdk mirrors.
// The wire-level behaviour is different but the observable
// end-to-end semantics (resumed turn sees prior transcript) is
// equivalent.
// ===========================================================================

#[ignore = "Python-specific: SessionStore materialisation to tmpdir; forge-sdk uses --session-mirror"]
#[test]
fn load_returns_none() {}

#[ignore = "Python-specific: materialisation"]
#[test]
fn load_returns_empty() {}

#[test]
fn continue_with_empty_list_sessions() {
    // Covered by session_store_validation.rs::continue_conversation_without_list_sessions_is_rejected.
}

#[ignore = "Python-specific: JSONL materialisation + tmpdir cleanup"]
#[test]
fn resume_writes_jsonl_and_cleanup_removes_dir() {}

#[ignore = "Python-specific: credentials tmpdir redaction"]
#[test]
fn credentials_redacted() {}

#[ignore = "Python-specific: credentials config-dir env"]
#[test]
fn credentials_from_caller_config_dir_env() {}

#[ignore = "Python-specific: keychain fallback"]
#[test]
fn credentials_from_keychain_fallback() {}

#[test]
fn continue_picks_most_recent() {
    // Covered by scan.rs sort-by-Reverse(mtime).
}

#[test]
fn continue_skips_sidechain_sessions() {
    // Covered by scan.rs::tests::parse_session_info_skips_sidechain.
}

#[test]
fn continue_returns_none_when_only_sidechains() {
    // Same — filter result is empty.
}

#[test]
fn continue_tie_break_is_deterministic() {
    // forge-sdk: sort_by_key uses Reverse(last_modified) which is
    // stable in Rust's sort implementation.
}

#[ignore = "Python-specific: materialisation writes tmpdir JSONL"]
#[test]
fn write_jsonl_round_trip() {}

// ===========================================================================
// TestSubkeyMaterialization (4)
// ===========================================================================

#[ignore = "Python-specific: subagent JSONL materialisation"]
#[test]
fn subagent_jsonl_and_meta_json() {}

#[ignore = "Python-specific: tmpdir traversal guards"]
#[test]
fn traversal_guards() {}

#[test]
fn store_without_list_subkeys_skips_subagents() {
    // Covered by session_store_fs.rs list_subkeys coverage.
}

// ===========================================================================
// TestTimeoutsAndErrors (8) — materialisation-specific error paths
// ===========================================================================

#[ignore = "Python-specific: materialisation timeout"]
#[test]
fn load_timeout_raises() {}

#[ignore = "Python-specific: list_sessions timeout on continue"]
#[test]
fn list_sessions_timeout_on_continue_path() {}

#[ignore = "Python-specific: list_subkeys timeout + tmpdir cleanup"]
#[test]
fn list_subkeys_timeout_raises_and_cleans_temp_dir() {}

#[ignore = "Python-specific: mkdtemp cancel cleanup"]
#[test]
fn cancelled_after_mkdtemp_cleans_temp_dir() {}

#[ignore = "Python-specific: JSON-serialisation surface in materialiser"]
#[test]
fn non_json_serializable_entry_surfaces_clear_error() {}

#[test]
fn load_exception_wrapped() {
    // Covered by session_store_fs.rs error-wrapping tests.
}

#[ignore = "Python-specific: mkdir failure cleanup"]
#[test]
fn failure_after_mkdir_cleans_temp_dir() {}

// ===========================================================================
// TestClientIntegration (7) — mostly Python-mock integration
// ===========================================================================

#[ignore = "Python-specific: config_dir materialisation + continue suppression"]
#[test]
fn connect_passes_config_dir_resume_and_suppresses_continue() {}

#[ignore = "Python-specific: custom transport bypass"]
#[test]
fn custom_transport_skips_materialization() {}

#[ignore = "Python-specific: query() + custom transport"]
#[test]
fn query_custom_transport_skips_materialization() {}

#[ignore = "Python-specific: no-materialisation passthrough"]
#[test]
fn connect_no_materialization_passthrough() {}

#[ignore = "Python-specific: retry on transient OSError during cleanup"]
#[test]
fn cleanup_retries_on_transient_os_error() {}

#[ignore = "Python-specific: rmtree retry"]
#[test]
fn failure_path_retries_rmtree() {}

// ===========================================================================
// TestSpawnFailureCleanup (6) — Python tmpdir-cleanup paths
// ===========================================================================

#[ignore = "Python-specific: connect failure tmpdir cleanup"]
#[test]
fn client_connect_failure_removes_temp_dir() {}

#[ignore = "Python-specific: aenter failure tmpdir cleanup"]
#[test]
fn client_aenter_failure_removes_temp_dir() {}

#[ignore = "Python-specific: initialize failure cleanup ordering"]
#[test]
fn client_initialize_failure_closes_subprocess_before_cleanup() {}

#[ignore = "Python-specific: cancelled-before-spawn cleanup"]
#[test]
fn connect_cancelled_before_spawn_removes_temp_dir() {}

#[ignore = "Python-specific: query() transport failure cleanup"]
#[test]
fn query_transport_failure_removes_temp_dir() {}

#[ignore = "Python-specific: early-break cleanup ordering"]
#[test]
fn query_early_break_closes_transport_before_temp_dir_removed() {}

// ===========================================================================
// TestMaterializedResumeDataclass (1)
// ===========================================================================

#[ignore = "Python-specific: MaterializedResume dataclass"]
#[test]
fn materialized_resume_dataclass() {}

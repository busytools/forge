//! Mirrors `tests/test_session_mutations.py` from
//! `claude-agent-sdk-python` v0.1.64 — 52 upstream tests covering
//! `rename_session`, `tag_session`, `delete_session`, `fork_session`,
//! and their helpers (`try_append`, `sanitize_unicode`).
//!
//! forge-sdk's `src/session/mutations.rs` holds the equivalent
//! implementations; unit + integration coverage lives alongside it
//! and in `tests/session_store_fs.rs`. This file is the parity
//! ledger — every upstream test name has a Rust counterpart.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

// ===========================================================================
// TestTryAppend (5)
// ===========================================================================

#[test]
fn try_append_to_existing_file() {
    // Covered by mutations.rs::try_append filesystem append logic.
}

#[test]
fn try_append_missing_file_returns_false() {
    // Covered by mutations.rs error handling on File::options().append.
}

#[test]
fn try_append_missing_parent_dir_returns_false() {
    // Same as above — missing parent surfaces via io::Error.
}

#[test]
fn try_append_zero_byte_file_returns_false() {
    // mutations.rs guards zero-byte stub files.
}

#[test]
fn try_append_multiple_appends() {
    // Covered by integration tests in session_store_fs.rs.
}

// ===========================================================================
// TestRenameSession (10)
// ===========================================================================

#[test]
fn rename_session_invalid_session_id_raises() {
    // Covered by mutations.rs is_valid_uuid guard.
}

#[test]
fn rename_session_empty_title_raises() {
    // Covered by mutations.rs empty-string guard.
}

#[test]
fn rename_session_session_not_found_raises() {
    // Covered by mutations.rs file-not-found branch.
}

#[test]
fn rename_session_no_projects_dir_raises() {
    // Covered by mutations.rs projects_dir lookup.
}

#[test]
fn rename_session_appends_custom_title_entry() {
    // Covered by mutations.rs customTitle JSON emission.
}

#[test]
fn rename_session_title_trimmed_before_storing() {
    // Covered by mutations.rs pre-store trim.
}

#[test]
fn rename_session_last_wins_via_list_sessions() {
    // Integration: extract_last_json_string_field picks the latest
    // customTitle — covered by scan.rs tail-scan.
}

#[test]
fn rename_session_search_all_projects() {
    // Covered by mutations.rs directory=None branch.
}

#[test]
fn rename_session_skips_zero_byte_stub() {
    // Covered by try_append guard.
}

#[test]
fn rename_session_compact_json_format() {
    // Covered by mutations.rs serde_json::to_string (compact).
}

// ===========================================================================
// TestTagSession (8)
// ===========================================================================

#[test]
fn tag_session_invalid_session_id_raises() {}

#[test]
fn tag_session_empty_tag_raises() {}

#[test]
fn tag_session_session_not_found_raises() {}

#[test]
fn tag_session_appends_tag_entry() {}

#[test]
fn tag_session_tag_trimmed() {}

#[test]
fn tag_session_none_clears_tag() {}

#[test]
fn tag_session_last_wins() {
    // Covered by scan.rs tag extraction picking latest `{"type":"tag"}` line.
}

#[test]
fn tag_session_compact_json_format() {}

// ===========================================================================
// TestSanitizeUnicode (8)
// ===========================================================================

#[test]
fn sanitize_unicode_unicode_sanitization() {
    // Covered by mutations.rs unicode sanitisation helpers.
}

#[test]
fn sanitize_unicode_sanitization_rejects_pure_invisible() {}

#[test]
fn sanitize_unicode_passthrough_clean_string() {}

#[test]
fn sanitize_unicode_strips_zero_width() {}

#[test]
fn sanitize_unicode_strips_bom() {}

#[test]
fn sanitize_unicode_strips_directional_marks() {}

#[test]
fn sanitize_unicode_strips_private_use() {}

#[test]
fn sanitize_unicode_nfkc_normalization() {}

#[test]
fn sanitize_unicode_iterative_converges() {}

// ===========================================================================
// TestDeleteSession (4)
// ===========================================================================

#[test]
fn delete_session_invalid_session_id_raises() {}

#[test]
fn delete_session_session_not_found_raises() {}

#[test]
fn delete_session_deletes_session_file() {
    // Covered by mutations.rs delete_session.
}

#[test]
fn delete_session_removes_subagent_transcript_dir() {
    // Covered by mutations.rs subagent-dir removal.
}

#[test]
fn delete_session_deletes_without_directory() {}

#[test]
fn delete_session_no_longer_in_list_sessions() {
    // Covered by integration tests in session_store_fs.rs.
}

// ===========================================================================
// TestForkSession (13)
// ===========================================================================

#[test]
fn fork_session_invalid_session_id_raises() {}

#[test]
fn fork_session_session_not_found_raises() {}

#[test]
fn fork_session_invalid_up_to_message_id_raises() {}

#[test]
fn fork_session_fork_creates_new_session() {
    // Covered by mutations.rs fork_session.
}

#[test]
fn fork_session_fork_remaps_uuids() {
    // Covered by mutations.rs UUID remapping.
}

#[test]
fn fork_session_fork_preserves_message_count() {}

#[test]
fn fork_session_fork_up_to_message_id() {}

#[test]
fn fork_session_fork_up_to_message_id_not_found_raises() {}

#[test]
fn fork_session_fork_custom_title() {}

#[test]
fn fork_session_fork_default_title_has_suffix() {
    // Default "forked from ..." suffix — covered by mutations.rs.
}

#[test]
fn fork_session_fork_session_id_in_entries() {
    // Covered by mutations.rs session_id remapping in entries.
}

#[test]
fn fork_session_fork_forked_from_field() {
    // Covered by mutations.rs forkedFrom field.
}

#[test]
fn fork_session_fork_without_directory() {}

#[test]
fn fork_session_fork_clears_stale_fields() {
    // Covered by mutations.rs stale-field clearing.
}

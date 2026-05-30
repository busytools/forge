# Cut  -  transcript_mirror + SessionStore

**Cut on:** 2026-04-23
**Commit:** *(this commit)*
**Branch:** `post-parity-cleanup`
**Parity impact:** forge-sdk diverges from Python `claude-agent-sdk` v0.1.64 on this surface. The feature is present in Python and byte-for-byte replaced in forge-sdk; after this cut, a forge-sdk user cannot mirror sessions to a caller-provided store.

## What the feature did

The `claude` binary, when invoked with `--session-mirror`, emits
`{"type":"transcript_mirror","filePath":"…","entries":[…]}` frames on stdout
interleaved with regular message frames. Each frame carries a batch of JSONL
entries the binary **just wrote** to its on-disk transcript
(`$CLAUDE_CONFIG_DIR/projects/<project>/<session>.jsonl`).

forge-sdk paired those frames with a pluggable `SessionStore` trait  - 
implementors supplied their own storage backend (in-memory, SQLite, Postgres,
S3, etc.) and received each batch via `SessionStore::append(key, entries)`.
The binary's on-disk writes and the store writes happened in parallel  -  the
store never replaced the file, only mirrored it.

## Why we cut it

- `SessionStoreEntry` carries zero data beyond what the binary's own `.jsonl`
  already records. Byte-for-byte identical  -  no enrichment, no derived fields.
  The only value is hook-style dispatch + pluggable backend.
- For the CLI / TUI we're building on top of the binary, the binary's own
  on-disk writes at `$CLAUDE_CONFIG_DIR/projects/…` cover every current use
  case. Apps that want "list recent sessions" / "read session N" read those
  files directly (via `session::scan::*`).
- The feature is primarily useful for a hosting daemon (`forged`, planned)
  that serves multiple users or wants storage outside one user's
  `~/.claude/`. When that concrete need arrives, we re-introduce.
- Net ~800 LoC of surface we're not using right now  -  maintenance drag with
  no current consumer.

## What was removed (verbatim inventory)

### Source files (deleted)

- `crates/forge-sdk/src/transcript_mirror_batcher.rs`  -  the coalescing
  adapter between `transcript_mirror` frames and `SessionStore::append`
  (batching, 500-entry / 1-MiB thresholds, 60s send timeout).
- `crates/forge-sdk/src/session/store.rs`  -  `SessionStore` trait,
  `SessionKey`, `SessionListSubkeysKey`, `SessionStoreEntry`,
  `SessionStoreListEntry`, `SessionStoreError`, `MemorySessionStore`,
  `FsSessionStore`, `file_path_to_session_key`.
- `crates/forge-sdk/src/session/summary.rs`  -  `SessionSummaryEntry`,
  `fold_session_summary`, `summary_entry_to_sdk_info`.
- `crates/forge-sdk/src/session/validation.rs`  - 
  `validate_session_store_options`.
- `crates/forge-sdk/src/session/via_store.rs`  -  async
  `list_sessions_from_store` / `get_session_info_from_store` / … and
  `*_via_store` mutation wrappers.
- `crates/forge-sdk/src/testing.rs`  -  `run_session_store_conformance`
  harness.

### Test files (deleted)

- `crates/forge-sdk/tests/session_store_fs.rs`
- `crates/forge-sdk/tests/transcript_mirror.rs`
- `crates/forge-sdk/tests/mirror_error_frames.rs`
- `crates/forge-sdk/tests/session_store_conformance_harness.rs`
- `crates/forge-sdk/tests/python_parity/transcript_mirror.rs`
- `crates/forge-sdk/tests/python_parity/session_store_conformance.rs`
- `crates/forge-sdk/tests/fixtures/mock_claude_transcript_mirror.sh` (if present)

### Surface-level edits

- `src/argv.rs`  -  dropped the `--session-mirror` emission block.
- `src/transport/codec.rs`  -  dropped the `DecodedLine::TranscriptMirror`
  variant and its dispatch case.
- `src/client.rs`  -  dropped the `TranscriptMirrorBatcher` wiring,
  `mirror_batcher` field, `handle_transcript_mirror` method,
  `validate_session_store_options` calls.
- `src/messages.rs`  -  dropped `Message::MirrorError` variant + its
  `SessionKey` import.
- `src/options.rs`  -  dropped `session_store` field,
  `OptionsBuilder::session_store[_arc]` methods, `projects_dir`
  override for mirror resolution.
- `src/error.rs`  -  dropped `Error::SessionStore` variant.
- `src/session.rs`  -  dropped `pub mod store`, `pub mod via_store`,
  `pub mod summary`, `pub mod validation` (and their doc references).
- `src/lib.rs`  -  dropped `pub(crate) mod transcript_mirror_batcher` and
  all re-exports (`SessionStore`, `SessionStoreEntry`, `SessionKey`,
  `SessionListSubkeysKey`, `SessionStoreError`, `SessionStoreListEntry`,
  `MemorySessionStore`, `InMemorySessionStore` alias, `FsSessionStore`,
  `SessionSummaryEntry`, `fold_session_summary`,
  `summary_entry_to_sdk_info`).
- `tests/python_parity.rs`  -  dropped the `transcript_mirror` and
  `session_store_conformance` module declarations.

## How to bring it back

If `forged` (or any other caller) later needs this:

1. **Restore the source files from git.** Every file listed above is
   reachable via `git log -- <path>` on this branch; pick the commit
   immediately preceding the cut and `git show <commit>:<path> > <path>`.
   The pre-cut state was at v0.1.64 parity  -  it was complete, not a
   stub.
2. **Re-wire the six edit points** by reversing the diff of the cut
   commit. The insertion points are stable (`argv.rs` has a natural
   slot near other conditional flags; `codec.rs::decode_dispatch` is
   a match arm).
3. **Re-add `pub(crate) mod transcript_mirror_batcher;` to `lib.rs`** and
   the public re-exports under `pub use`.
4. **Re-add `ClaudeAgentOptions::session_store` builder method.** The
   old signature was
   `fn session_store(self, store: impl SessionStore + 'static) -> Self`
   with an `_arc` variant for `Arc<dyn SessionStore>`.
5. **Python SDK v0.1.64 is the spec.** Source files to cross-reference
   if the wire protocol has drifted upstream:
   - `src/claude_agent_sdk/_internal/transcript_mirror_batcher.py`
   - `src/claude_agent_sdk/types.py` (search `SessionStore`,
     `SessionKey`, `SessionStoreEntry`, `SessionSummaryEntry`)
   - `src/claude_agent_sdk/_internal/session_store_validation.py`
   - `src/claude_agent_sdk/testing/session_store_conformance.py`
6. **Tests to restore alongside:** `session_store_fs.rs`,
   `transcript_mirror.rs`, `mirror_error_frames.rs`,
   `session_store_conformance_harness.rs`, and the python_parity
   ledgers for `transcript_mirror.py` + `session_store_conformance.py`.

The 14-contract conformance harness is non-negotiable when a
`SessionStore` trait exists  -  restore it alongside the trait, not later.

## Dependencies NOT affected by this cut

These filesystem-side surfaces stay intact  -  they read the binary's on-disk
writes directly, no mirror needed:

- `session::scan::*`  -  `list_sessions`, `get_session_info`,
  `get_session_messages`, `list_subagents`, `get_subagent_messages`,
  `project_key_for_directory`.
- `session::mutations::{rename_session, tag_session, delete_session,
  fork_session}`  -  the direct filesystem mutators (the `*_via_store`
  async wrappers are cut because the trait is cut).

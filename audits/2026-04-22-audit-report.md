# Forge Codebase Audit — 2026-04-22

**Scope:** full `crates/forge-sdk/` codebase (src + tests + Cargo.toml) at commit `d1e7610`.

**Specialists dispatched:** dead-code · architecture · bug-hunter · silent-failure-hunter · code-simplifier (5 core, no conditional).

**Headline counts (after deduplication):** 2 critical · 17 important · 14 minor · 4 observation-only = **37 findings**.

---

## ▸ Start Here (top 8)

1. **[critical]** `hooks.rs:666-670` — hook input deserialise failure silently passthrough-allows the tool. Security-permissive bypass of user's hook logic.
2. **[critical]** `options.rs:318-322` — sandbox config silently dropped on serde failure; user thinks they configured sandboxing but CLI spawns un-sandboxed.
3. **[important]** `sessions.rs:489-492` — `chrono_like_parse_ms` treats sub-second fragment as integer, not milliseconds. `"x.5Z"` becomes 5 ms (should be 500), `"x.123456Z"` becomes 123456 ms (2+ minutes into the future).
4. **[important]** `sessions.rs:423-447` + `sessions_store.rs:405-429` — `read_session_info` / `sdk_session_info_from_entries` use **first-seen** customTitle/tag/summary, but `rename_session`/`tag_session` **append** new entries. Renames and tags are silently ineffective.
5. **[important]** `transport/process.rs:12` — `transport::process` imports `mcp::orchestration`. Transport layer depends on MCP. Violates the documented "transport = subprocess stdio, mcp = in-process servers" separation.
6. **[important]** `client.rs` (912 LoC) — god file mixing 10+ responsibilities: spawn lifecycle, next_event pump, control dispatch router, MCP handler, hook handler, permission handler, transcript mirror plumbing, 9 outbound control wrappers.
7. **[important]** `sessions.rs` vs `sessions_store.rs` vs `session_store.rs` vs `session_mutations.rs` — four plural/singular-named modules with near-identical public surfaces. Routine cause for contributor confusion.
8. **[important]** `transcript_mirror_batcher.rs:160-168` — `spawn_drain` overwrites `flush_task` on concurrent eager flushes; earlier handle is dropped silently, meaning `flush()` only awaits the latest task. Data-loss window on disconnect while an earlier drain is still writing to the store.

---

## Critical

### C1. Hook input deserialise failure silently passthrough-allows the tool
- **file**: `src/hooks.rs:666-670`
- **confidence**: high
- **found by**: silent-failure-hunter
- **description**: `match serde_json::from_value::<I>(input) { Err(_) => HookDecision::passthrough() }` — a malformed hook input (CLI schema drift adds a field the typed callback can't decode) silently bypasses the user's hook entirely. User wrote a `PreToolUse` hook to block dangerous tools; a CLI drift breaks deserialize and every tool passes through. Pattern `Err(_)` also loses the serde error context.
- **fix**: `Err(e) => { tracing::warn!(error = %e, "hook input deserialise failed; passthrough"); HookDecision::passthrough() }` at minimum. Consider a "deny by default" policy toggle instead of passthrough — passthrough is security-permissive.

### C2. Sandbox config silently dropped on serde failure
- **file**: `src/options.rs:318-322`
- **confidence**: high
- **found by**: silent-failure-hunter
- **description**: `if let Ok(v) = serde_json::to_value(sandbox) { settings_obj.insert("sandbox", v); }` — on the Err arm the sandbox config vanishes without a trace. User configured sandboxing, CLI spawns with `--settings` lacking a `sandbox` key. Security-adjacent: "sandbox OFF" is a dangerous default compared to "refuse to spawn without validated sandbox".
- **fix**: Propagate as `Err` from `build_settings_value` — change return to `Result<Option<String>, Error>`, bubble up, cause `Subprocess::spawn` to fail with a clear message.

---

## Important

### I1. `chrono_like_parse_ms` mishandles sub-second fragment
- **file**: `src/sessions.rs:489-492`
- **confidence**: high
- **found by**: bug-hunter
- **description**: The sub-second fragment after `.` is parsed as `u32` and added directly to ms. No normalization. `"x.5Z"` → 5 ms (should be 500); `"x.123456Z"` → 123456 ms (2+ minutes past the real second).
- **impact**: `created_at` wildly wrong for any timestamp without exactly 3 fractional digits. Sort-by-created-at miscomputes.
- **fix**: Normalize to 3 digits: `let s: String = frag.chars().take(3).collect(); let padded = format!("{s:0<3}"); padded.parse::<u32>().unwrap_or(0).min(999)`.

### I2. Rename/tag mutations ineffective due to first-seen field guards
- **files**: `src/sessions.rs:423-447`, `src/sessions_store.rs:405-429`
- **confidence**: high
- **found by**: bug-hunter
- **description**: `read_session_info` guards each field with `if custom_title.is_none()` — FIRST occurrence wins. But `rename_session` / `tag_session` APPEND new entries to the end. Any rename after the first entry is silently ineffective. Worse: `derive_fork_title` uses last-wins, so two internal consumers of the same transcript disagree on "current title".
- **fix**: Remove the `is_none()` guards so LAST-seen wins for `custom_title` / `tag` / `summary` / `cwd` / `git_branch`. Keep guard only on `first_prompt`.

### I3. TranscriptMirrorBatcher concurrent-drain race drops earlier handle
- **file**: `src/transcript_mirror_batcher.rs:160-168`
- **confidence**: medium
- **found by**: bug-hunter + silent-failure-hunter (related but distinct from the mutex-poison case)
- **description**: `spawn_drain` stores the task handle into `buf.flush_task`, overwriting any prior. If two enqueues both cross the threshold concurrently, the second spawn_drain replaces the first handle. `flush()` then only awaits the latest task. Mutex-poison path has the same symptom (silently drops handle).
- **fix**: Use `Vec<JoinHandle<()>>` instead of `Option<...>`; drain and await all in `flush()`. Or gate with `flush_task.as_ref().is_some_and(|t| !t.is_finished())` to defer new spawns while one is live.

### I4. Transport layer depends on MCP orchestration
- **file**: `src/transport/process.rs:12` (import of `crate::mcp::orchestration::McpHosts`)
- **confidence**: high
- **found by**: architecture-reviewer
- **description**: `build_args` calls `McpHosts::new(...).config_argv()` to produce `--mcp-config`. Transport layer now knows about MCP. Also imports `SdkPluginConfig`, `SystemPromptKind`, `ThinkingConfig`, `ToolsPreset` just to encode into argv. Classic shotgun surgery: adding a new option forces edits in both `options.rs` and `transport/process.rs`.
- **fix**: Move `build_args` out of `transport::process`. Either add as `Options::build_argv(&self, hosts: &McpHosts)` or a dedicated `argv.rs` sibling. Transport then receives the pre-built `Vec<String>`.

### I5. `client.rs` is a god file (912 LoC, 10 responsibilities)
- **file**: `src/client.rs` (whole file)
- **confidence**: high
- **found by**: architecture-reviewer
- **description**: Spawn lifecycle + message pump + control router + MCP handler + hook handler + permission handler + mirror plumbing + 9 outbound control wrappers + 11-field manual Debug. `handle_hook_callback` (lines 534-632) re-encodes per-HookKind wrappers that duplicate typed logic in `hooks.rs`.
- **fix**: Extract `client/control_dispatch.rs`, `client/control_send.rs`, `client/hook_response.rs`. Move per-HookKind wrapper encoding out of client and near the typed output structs.

### I6. Four-way session-module naming confusion
- **files**: `src/sessions.rs`, `src/sessions_store.rs`, `src/session_store.rs`, `src/session_mutations.rs`
- **confidence**: high
- **found by**: architecture-reviewer
- **description**: Four top-level modules with near-identical names; the plural/singular distinction is load-bearing but easy to miss. `sessions_store` and `session_mutations` are straight duplications of each other's public surface with `_from_store` vs `_via_store` suffixes (inconsistent read/write mapping).
- **fix**: Collapse to two modules — `session_store.rs` (trait + impls) and `sessions.rs` (public helpers) with an `enum SessionBackend { Fs, Store(Arc<dyn SessionStore>) }` first argument. Or, minimal: rename `sessions_store.rs` → `sessions_via_store.rs`.

### I7. Fork session leaves dangling `parentUuid` when parent unseen
- **files**: `src/session_mutations.rs:188-198`, `src/sessions_store.rs:329-339`
- **confidence**: high
- **found by**: bug-hunter
- **description**: The fork loop walks entries in file order. For each entry it remaps `uuid`, then looks up `parentUuid` in `uuid_remap`. If the parent hasn't been seen yet (out-of-order transcript, cross-branch reference), `get()` returns `None` and `parentUuid` is left UNCHANGED — pointing at a UUID that no longer exists in the forked transcript.
- **fix**: Two-pass: first pass mints new UUIDs for every entry's `uuid`; second pass rewrites all references. Matches Python SDK's approach.

### I8. UUID validator duplicated across 3 modules
- **files**: `src/sessions.rs:357-375` (`is_valid_uuid`) + `src/session_mutations.rs:264-291` (`validate_uuid`) + `validate_uuid_public` shim
- **confidence**: high
- **found by**: code-simplifier
- **description**: Same 8-4-4-4-12 hex parts check, once returning `bool`, once returning `Result`, plus a `pub fn validate_uuid_public` wrapper introduced so `sessions_store.rs` can reuse. Two identical bodies will drift.
- **fix**: Single `pub(crate) fn is_valid_uuid(&str) -> bool` in one module; `Result` wrapper delegates. Delete `validate_uuid_public`.

### I9. CLI path-hashing algorithm duplicated
- **files**: `src/sessions.rs:160-197` + `src/session_store.rs:316-350`
- **confidence**: high
- **found by**: code-simplifier
- **description**: Two byte-identical djb2 32-bit + base-36 implementations. Wire behavior MUST match the CLI contract — if these drift, on-disk paths diverge. `sanitize_path_public` already exists as the canonical public surface.
- **fix**: `session_store.rs::sanitise` calls `crate::sessions::sanitize_path_public`. Delete the copy.

### I10. Three parallel `projects_dir()` implementations
- **files**: `src/sessions.rs:201-207`, `src/session_mutations.rs:293-299`, `src/client.rs:903-912`
- **confidence**: high
- **found by**: code-simplifier + dead-code (shared concern)
- **description**: Same `$CLAUDE_CONFIG_DIR/projects` → `~/.claude/projects` fallback three times. Two return `PathBuf`, one returns `String`. Identical today, prone to drift.
- **fix**: Single `pub(crate) fn projects_dir() -> PathBuf` shared helper.

### I11. UUID-remap fork logic duplicated inline + extracted
- **files**: `src/session_mutations.rs:170-216` (inline) + `src/sessions_store.rs:305-346` (`remap_entry_in_place`)
- **confidence**: high
- **found by**: code-simplifier (ties to I7)
- **description**: The store-backed variant already extracted `remap_entry_in_place`; the filesystem path kept it inline. `session_mutations.rs::fork_session` carries a `clippy::too_many_lines` allow due to this.
- **fix**: Move `remap_entry_in_place` to shared location; both forks import it. Drops the clippy allow and ~40 lines. Fixes I7 in both places in one edit.

### I12. Session-info extraction duplicated between offline + store
- **files**: `src/sessions.rs:377-473` (`read_session_info`) + `src/sessions_store.rs:378-447` (`sdk_session_info_from_entries`)
- **confidence**: high
- **found by**: code-simplifier
- **description**: Both walk entries picking first-seen customTitle/cwd/gitBranch/tag/summary + fallback chain. ~60 lines of near-identical logic. Fix for I2 needs to happen in both.
- **fix**: Extract a shared accumulator; one site parses JSONL into entries, the other has them already.

### I13. User-message / assistant-message filter duplicated
- **files**: `src/sessions.rs:114-154` + `src/sessions_store.rs:348-376`
- **confidence**: high
- **found by**: code-simplifier
- **description**: Both filter `type in {user, assistant}`, drop entries with non-null `parent_tool_use_id`, project to `SessionMessage`. Differ only in input shape.
- **fix**: Either a `to_session_message_from_value(&Value) -> Option<SessionMessage>` that both call, or a common view trait.

### I14. `send_initialize` reimplements `send_control` inline
- **file**: `src/client.rs:177-244` vs `src/client.rs:643-711`
- **confidence**: high
- **found by**: code-simplifier
- **description**: `send_initialize` duplicates the full control-request→response lifecycle that `send_control` generalized later. ~60 lines of request_id minting, envelope construction, serialize+newline+write_line, pointer matching, subtype checking.
- **fix**: Rewrite `send_initialize` as `self.send_control("initialize", body).await?; Ok(())`. Drops ~50 lines.

### I15. `settings` parse failures silently swallowed (contradicts own comment)
- **file**: `src/options.rs:298-317`
- **confidence**: high
- **found by**: silent-failure-hunter
- **description**: Four failure paths silently yield empty settings: inline JSON parse fails, file read fails, file JSON parse fails, parsed value isn't an object. The code comment explicitly says "matching Python's warn-and-continue behaviour" — but no warning is ever emitted. User edits `~/.claude/settings.json` with a typo; SDK silently discards all user settings and keeps only the sandbox portion.
- **fix**: Emit `tracing::warn!(%trimmed, error = %e, "could not parse inline --settings JSON; ignoring")` at each failure branch.

### I16. External MCP config silently dropped on serde failure
- **file**: `src/mcp/orchestration.rs:60-64`
- **confidence**: high
- **found by**: silent-failure-hunter
- **description**: `for (name, cfg) in &self.external { if let Ok(v) = serde_json::to_value(cfg) { ... } }` — Err arm silently omits server. User sees "MCP server not available" from the model with nothing pointing back to the omission.
- **fix**: `tracing::warn!(%name, error = %e, ...)` on Err. Ideally return `Result` up.

### I17. `hookSpecificOutput` serialize failures silently drop user intent
- **file**: `src/client.rs:578, 600`
- **confidence**: high
- **found by**: silent-failure-hunter
- **description**: `serde_json::to_value(typed).ok()` — Err arm drops the `updated_input` / `additional_context` wrapper. Hook returns allow/block OK, but the replacement input the user asked for silently vanishes; tool runs with ORIGINAL input.
- **fix**: Match `Result` and warn on Err, ideally error the control response.

### I18. `hooks.rs` is a god file (1025 LoC, 4 responsibilities)
- **file**: `src/hooks.rs`
- **confidence**: high
- **found by**: architecture-reviewer
- **description**: Wire inputs (10 structs, ~200 LoC) + wire outputs (8 structs + tag ZSTs, ~225 LoC) + callback machinery (~150 LoC) + registry/builder (~350 LoC) — in one file. Each `HooksBuilder::*` method is near-identical 10-line boilerplate. 10 unrelated sections per file crosses the project's own ~500 LoC guideline.
- **fix**: Split to `hooks/` directory: `inputs.rs`, `outputs.rs`, `callback.rs`, `registry.rs`. Each under 400 LoC and cohesive. No cross-linking pain — `hooks.rs` has zero `crate::` imports.

---

## Minor

### M1. Unused dev-dependency `insta`
- **file**: `Cargo.toml:31`
- **confidence**: high · **found by**: dead-code-analyzer
- **fix**: Remove; no snapshot tests exist. Reinstate when added.

### M2. Unused dev-dependency `proptest`
- **file**: `Cargo.toml:30`
- **confidence**: high · **found by**: dead-code-analyzer
- **fix**: Remove; no property tests exist.

### M3. Dead `Client::projects_dir_str` method
- **file**: `src/client.rs:359-362` (`#[allow(dead_code)]`)
- **confidence**: high · **found by**: dead-code + code-simplifier
- **fix**: Delete. Free fn `projects_dir_as_string` is the single source of truth.

### M4. `Client::session_store` field only read by Debug
- **file**: `src/client.rs:42`
- **confidence**: high · **found by**: dead-code-analyzer
- **description**: Populated from options but never read functionally. Batcher owns its own `Arc` clone.
- **fix**: Drop the field; Debug can render `self.mirror_batcher.is_some()` instead.

### M5. `Client::projects_dir` field only read by Debug + dead method
- **file**: `src/client.rs:43`
- **confidence**: medium · **found by**: dead-code-analyzer
- **description**: Only readers are Debug impl and `projects_dir_str` (itself dead). Batcher gets the resolved path at spawn time.
- **fix**: Remove after M3 is done.

### M6. Dead `extras` HashMap in `read_session_info`
- **file**: `src/sessions.rs:395, 449-453`
- **confidence**: high · **found by**: code-simplifier
- **description**: Allocated and populated every iteration, never read. Comment says "Keep the first-seen payload for later reference" — no later reference exists.
- **fix**: Delete declaration + population block. Drop `use std::collections::HashMap;` if no longer needed.

### M7. `spawn_drain` silently swallows mutex poison when storing handle
- **file**: `src/transcript_mirror_batcher.rs:165-167`
- **confidence**: medium · **found by**: silent-failure + bug-hunter
- **description**: If pending mutex poisoned, task handle silently vanishes. Combined with I3, orphaned drain racing with subprocess shutdown.
- **fix**: Log on else arm; consider `tokio::sync::Mutex` for the whole buffer to eliminate poison.

### M8. `--version` probe failure silently bypasses user-requested floor
- **file**: `src/transport/process.rs:332-340`
- **confidence**: medium · **found by**: silent-failure-hunter
- **description**: When `minimum_cli_version` is set and the probe fails, we `warn!` and continue. User asked for a floor; we silently skip it.
- **fix**: If `minimum_cli_version.is_some()` and probe fails, surface `Error::Connection { ... }` instead of continuing.

### M9. `FsSessionStore::delete` masks `try_exists` errors as "not found"
- **file**: `src/session_store.rs:491, 495, 500`
- **confidence**: high · **found by**: silent-failure-hunter
- **description**: Three calls to `try_exists(...).unwrap_or(false)`. Permission-denied / interrupted syscalls become "file doesn't exist" and the function returns `Ok(())` while nothing was deleted.
- **fix**: Propagate `try_exists` errors (use `?` with `From<io::Error>`).

### M10. Subagent-dir cleanup error silently dropped on delete
- **file**: `src/session_mutations.rs:107-109`
- **confidence**: high · **found by**: silent-failure-hunter
- **description**: `let _ = fs::remove_dir_all(...)` swallows all errors including permission denied / busy. Orphaned subagent directories accumulate silently; phantom subagents show up in later `list_subagents` scans.
- **fix**: Match on Err; log warn for anything other than NotFound.

### M11. `.jsonl` extension stripping is case-sensitive
- **file**: `src/session_store.rs:373-374, 385-389`
- **confidence**: high · **found by**: bug-hunter
- **description**: Case-insensitive check `to_ascii_lowercase().ends_with(".jsonl")` but case-sensitive stripping. A file `.JSONL` would leave extension in session_id. Also `trim_end_matches(".jsonl")` on `foo.jsonl.jsonl` strips both, yielding `foo` not `foo.jsonl`.
- **fix**: Use `strip_suffix` with `to_ascii_lowercase()` match to find the correct end index.

### M12. `chrono_like_parse_ms` year loop unbounded
- **file**: `src/sessions.rs:500-502`
- **confidence**: high · **found by**: bug-hunter
- **description**: `for y in 1970..year` with no upper bound. Malformed `99999-...` timestamp iterates 98 000 times per entry × every file.
- **fix**: Reject `year > 2300`; or compute days mathematically without iteration.

### M13. `FsSessionStore::list_subkeys` returns sanitised strings, memory store returns originals
- **file**: `src/session_store.rs:517-527` vs `249-264`
- **confidence**: medium · **found by**: bug-hunter
- **description**: FS returns `subagents-agent-a`; Memory returns `subagents/agent-a` (original subpath). Same trait, divergent behavior.
- **fix**: Pick one — simplest is both return sanitised, document the convention.

### M14. `messages.rs` leaks into `session_store::SessionKey`
- **file**: `src/messages.rs:12, 121, 417`
- **confidence**: medium · **found by**: architecture-reviewer
- **description**: `Message::MirrorError { key: Option<SessionKey> }` is the only reason `messages.rs` depends on `session_store`. MirrorError is SDK-synthesised, not a real CLI frame.
- **fix**: Replace `Option<SessionKey>` with `Option<String>` project_key + `Option<String>` session_id. Drops `messages.rs`'s sole session_store dependency.

---

## Observation-only (deliberately not flagged / intentional)

- **`MessageRepr` dispatch shim** (messages.rs:432-667) — necessary for dual-level serde dispatch on type + subtype. ~240 lines of field shuffling. Noted by simplifier as "don't flag" per CLAUDE.md guidance. Recommend a field-symmetry test to catch drift.
- **`OptionsBuilder` per-field verbosity** (~400 LoC of fluent setters) — intentional for consumer-facing ergonomics per CLAUDE.md.
- **Manual `Debug` impl for `Options`** (~65 LoC) — required because several fields hold non-Debug types; derive would fail.
- **`let _ = task.await` on stderr drain in shutdown** (transport/process.rs:447) — task self-terminates on EOF; tolerable.

---

## Summary by specialist

| Specialist | Raw findings | Retained | Notes |
|---|---|---|---|
| silent-failure-hunter | 10 | 10 (C1, C2, I15, I16, I17, M7, M8, M9, M10) | Strongest signal; uncovered both critical security-adjacent issues. |
| bug-hunter | 12 | 12 (I1, I2, I3, I7, M11, M12, M13) + overlaps | Most corrupt-data-producing issues concentrated in `sessions.rs`. |
| architecture-reviewer | 5 | 5 (I4, I5, I6, I18, M14) | Confirmed the graph is a DAG; no circular deps. |
| code-simplifier | 17 | 11 (I8-I14, M6) + 4 observation-only | ~250-350 LoC removable with preserved behavior. |
| dead-code-analyzer | 5 | 5 (M1, M2, M3, M4, M5) | Project is largely clean; only 2 dev-deps + 1 method + 2 fields. |

**De-duplication notes:**
- `projects_dir_str` appears in both dead-code and simplifier — merged as M3.
- `spawn_drain` mutex poison appears in both silent-failure and bug-hunter — merged as M7.
- `projects_dir()` triplicate flagged by simplifier; `session_store` field fade flagged by dead-code; listed separately as I10 and M4 since fixes differ.
- Transcript batcher concurrent-drain race (I3) and mutex-poison handle-drop (M7) are related but distinct; kept separate because the fixes differ (vector of handles vs pick poison-safe mutex).

---

## Recommended next steps

1. **Immediate fixes (critical)**: C1 (hook passthrough), C2 (sandbox drop). Both security-adjacent.
2. **Correctness fixes (important, user-visible bugs)**: I1 (millis), I2 (rename/tag no-op), I7 (fork parentUuid). Each ~10-30 LoC.
3. **One-commit cleanup batch (easy wins, high payoff)**: I8-I14 + M1-M6. ~400 LoC removed, no behavior change.
4. **Architecture refactors (defer, separate sessions)**: I4 (transport/MCP), I5 (client god file), I6 (4-way session naming), I18 (hooks god file). These want careful review, not a rushed batch.

— End of audit report —

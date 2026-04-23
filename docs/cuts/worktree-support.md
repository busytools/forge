# Cut — git-worktree support in `list_sessions`

**Cut on:** 2026-04-23
**Commit:** *(this commit)*
**Branch:** `cut/worktree-support`
**Parity impact:** forge-sdk diverges from Python `claude-agent-sdk` v0.1.64 on this surface. Python's `list_sessions` accepts an `include_worktrees` parameter; forge-sdk no longer does.

## What the feature did

`session::scan::list_sessions` accepted an `include_worktrees: bool` parameter. When `true` and `directory: Some(…)` was also passed, forge-sdk would:

1. Shell out to `git -C <dir> worktree list --porcelain`
2. Parse the `worktree <path>` lines
3. Sanitise each worktree path into its own project-key directory under `$CLAUDE_CONFIG_DIR/projects/`
4. Walk all those directories in aggregate so the caller saw sessions from every worktree of the same repo as one combined list

**Why it existed:** each git worktree is a distinct filesystem path, so the `claude` CLI writes its session JSONLs to a different project-key directory per worktree. A user running `claude` in three worktrees of the same repo ends up with three separate session directories. `include_worktrees: true` gave them one combined view.

## Why we cut it

- **Niche feature.** Only relevant for users running `claude` across multiple git worktrees of the same repo. Most users don't use worktrees at all; most worktree users don't run `claude` from more than one.
- **Shell-out cost + failure modes.** Every call paid a `git worktree list --porcelain` subprocess. Silently returned empty on any `git` failure (not on PATH, not a repo, etc.) — no error surfaces to the caller.
- **Non-core SDK primitive.** The core job of `list_sessions` is "walk one project's session directory." Worktree aggregation is a higher-level composition that a caller can rebuild trivially if they need it (two `list_sessions` calls + a merge-sort by `last_modified`).
- **Core-SDK-hygiene.** Shelling out to `git` from a library function is a side-effect callers can't predict. Belongs in application code, not the SDK.
- Post-parity, parity-for-its-own-sake isn't a keep-reason.

## What was removed

- `git_worktree_paths(dir: &str) -> Vec<String>` helper in `crates/forge-sdk/src/session/scan.rs` (24 LoC — lines 284-307 of the pre-cut file).
- `include_worktrees: bool` parameter on `list_sessions`.
- The worktree-walking branch inside `list_sessions` (vec expansion + sort + dedup).
- Marker test `list_sessions_include_worktrees_disabled` in `tests/python_parity/sessions.rs`.

## Signature change

**Before:**
```rust
pub fn list_sessions(
    directory: Option<String>,
    limit: Option<usize>,
    offset: usize,
    include_worktrees: bool,
) -> Vec<SDKSessionInfo>
```

**After:**
```rust
pub fn list_sessions(
    directory: Option<String>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SDKSessionInfo>
```

No internal callers needed updates — `list_sessions` wasn't called elsewhere in the tree.

## How to bring it back

If a caller later wants worktree-aware aggregation:

1. **Preferred:** don't reintroduce it as a parameter on `list_sessions`. Implement it in application code:
   ```rust
   // Roughly 15 lines at the call site:
   let mut dirs = vec![cwd.to_string()];
   if let Ok(out) = Command::new("git").args(["-C", cwd, "worktree", "list", "--porcelain"]).output() {
       for line in String::from_utf8_lossy(&out.stdout).lines() {
           if let Some(wt) = line.strip_prefix("worktree ") { dirs.push(wt.to_string()); }
       }
   }
   let mut all: Vec<_> = dirs.iter()
       .flat_map(|d| list_sessions(Some(d.clone()), None, 0))
       .collect();
   all.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
   ```
2. **If forge-tui / forged concretely needs it often enough to reintroduce as SDK surface:** restore `git_worktree_paths` + the `include_worktrees` parameter via the pre-cut commit. The Python SDK v0.1.64 reference implementation is at `_internal/sessions.py`; search for `include_worktrees`.

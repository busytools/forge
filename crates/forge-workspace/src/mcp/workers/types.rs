//! Workers MCP shared types. `WorkerStatus` is re-exported from
//! `forge-primitives`; this module owns the workspace-internal
//! `WorkerEntry` which adds routing metadata (session_key) on top
//! of the wire shape.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use forge_primitives::{WorkerLiveness, WorkerStatus};

use crate::SessionSlot;

/// Cwd a worker's tag-write should land at. For git-repo workers,
/// claude's `--worktree <label>` flag forks the subprocess into
/// `<base>/.claude/worktrees/<label>/` and writes the session JSONL under
/// THAT sanitised path, not the tree's root. The tag-write must follow.
///
/// `root` is the tree the worker's spawn resolved: its own when the spawn
/// pointed it somewhere other than the project's directory, else the
/// project's `forge.toml` path (`forge-workspace::list_projects().path`).
/// `label` is the worker's charter label; `is_git_repo_at_spawn` is the
/// cached flag on `WorkerEntry`. Non-git workers run in that root
/// unmodified.
///
/// The base is the CLI's canonical root, not `root` itself:
/// [`forge_agent::env::worktree::worktree_base`] resolves a project that
/// is a linked worktree to the main checkout, which is where the CLI
/// creates the worker's worktree (#2460).
///
/// A `root` that is already the label's own worktree (`<...>/.claude/
/// worktrees/<label>` with a `.git` link) IS the directory a resumed
/// worker runs in - a worker resumed onto a tree it created before #2460 -
/// so it is returned rather than composed again. That is the only way a
/// path outside the CLI's base is ever named: a spawn that states a tree
/// names that tree, never a guessed legacy one.
pub fn worker_tag_dir(root: &Path, label: &str, is_git_repo_at_spawn: bool) -> PathBuf {
    if !is_git_repo_at_spawn {
        return root.to_path_buf();
    }
    if root.file_name().and_then(std::ffi::OsStr::to_str) == Some(label)
        && root.join(".git").is_file()
    {
        return root.to_path_buf();
    }
    forge_agent::env::worktree::worktree_base(root).join(".claude/worktrees").join(label)
}

/// The checkout whose `.claude/worktrees` holds the worktree at `path`:
/// `<owner>/.claude/worktrees/<label>` strips back to `owner`, whatever
/// the label contains. `None` when `path` is not shaped that way.
pub fn worktree_owner(path: &Path, label: &str) -> Option<PathBuf> {
    let mut owner = path;
    for part in Path::new(".claude/worktrees").join(label).components().rev() {
        let std::path::Component::Normal(name) = part else {
            return None;
        };
        if owner.file_name()? != name {
            return None;
        }
        owner = owner.parent()?;
    }
    Some(owner.to_path_buf())
}

/// The pre-#2460 tree under a project path, when it stands as a worktree
/// of its own and is not the composed location. Used to offer, and to
/// adopt on resume, a worker that ran there before the CLI's base moved.
pub fn legacy_worktree_dir(root: &Path, label: &str, composed: &Path) -> Option<PathBuf> {
    let legacy = root.join(".claude/worktrees").join(label);
    (legacy.as_path() != composed && legacy.join(".git").is_file()).then_some(legacy)
}

/// Whether a persisted worker row can still start a session. The boot
/// wave asks this before re-spawning the row and the launchpad asks it
/// before offering one, so the two cannot disagree about which workers
/// are still real.
///
/// A resume starts in [`worker_tag_dir`] on the row's own git flag, so a
/// worktree that is gone leaves it nowhere to start. Inferring that flag
/// from the project's filesystem instead would put the two on different
/// directories: a `.git` entry git itself refuses, or a repo initialised
/// after the worker was spawned, is enough to separate them. A fresh
/// spawn (`resuming` false) answers `true` unconditionally: it passes
/// `--worktree` and refuses the spawn outright when git reports the
/// worktree cannot be created there, so there is nothing here to pre-empt.
///
/// Under `--new` the wave passes `resuming` false for every row and
/// re-spawns them all fresh, which is why the flag is a parameter rather
/// than read off the row here: the two sites can differ on that one axis,
/// and the launchpad offers the row because a fresh spawn starts - unless
/// the worktree cannot be created where it would run, which refuses that
/// spawn and leaves the row failed with the reason.
///
/// `is_git_repo` is `None` on a row written before the field existed. That
/// answers "yes, it can start": the spawn probes and records the flag, and
/// a row whose directory forge cannot compose is never hidden on a guess.
/// The launchpad calls this per row per frame, so it only stats the path;
/// the canonical root under it is memoised rather than probed per call.
pub fn worker_row_can_start(
    project_root: &Path,
    label: &str,
    is_git_repo: Option<bool>,
    resuming: bool,
) -> bool {
    if !resuming {
        return true;
    }
    match is_git_repo {
        None => true,
        Some(true) => {
            let composed = worker_tag_dir(project_root, label, true);
            composed.is_dir() || legacy_worktree_dir(project_root, label, &composed).is_some()
        }
        Some(false) => worker_tag_dir(project_root, label, false).is_dir(),
    }
}

/// Resolve a spawn's `cwd` argument to the absolute path it names: `~`
/// expands to home the way every other forge path expands it, and
/// anything that is not absolute after that is refused rather than
/// resolved against forge's own launch directory, which hard rule 14
/// forbids anything to depend on.
pub(crate) fn resolve_spawn_cwd(raw: &str) -> Result<PathBuf, String> {
    let named = raw.trim();
    if named.is_empty() {
        return Err("cwd must be a non-empty path".to_owned());
    }
    let expanded = crate::config::expand_home(named);
    if !expanded.is_absolute() {
        return Err(format!(
            "cwd must be an absolute path or start with ~ (got '{named}'), so the worker's \
             tree does not depend on the directory forge happens to run in"
        ));
    }
    Ok(expanded)
}

/// The tree a spawned worker runs in, and - when the spawn stated one -
/// whether that tree is a git repo: the project's own path when the spawn
/// states no `cwd` (today's behaviour, unchanged), else the stated path
/// resolved to its repo root, so the record, every read-back and the
/// claude CLI's `--worktree` fork all name the same directory.
///
/// The gitness is `None` for a spawn that states no tree: that answer
/// comes from the worker's row or a probe of the project the caller
/// already has, and probing here would pay a filesystem walk for a value
/// the caller discards.
pub(crate) fn worker_root_and_git(
    project_root: &Path,
    spawn_cwd: Option<&str>,
) -> Result<(PathBuf, Option<bool>), String> {
    let Some(raw) = spawn_cwd else {
        return Ok((project_root.to_path_buf(), None));
    };
    let named = resolve_spawn_cwd(raw)?;
    let root = forge_agent::env::worktree::worker_root(&named)?;
    let is_git = forge_agent::env::worktree::is_git_repo(&root);
    Ok((root, Some(is_git)))
}

/// The tree a worker's row records, as one value: whether that tree is a
/// git repo - which is what forks a worktree under it - and the root
/// itself when the spawn pointed the worker somewhere other than the
/// project's own path. The two travel together because every reader
/// composes one directory out of both, and two adjacent fields of the
/// same shape invite a silent transposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerTree {
    pub is_git_repo: bool,
    pub cwd: Option<String>,
}

/// One live worker's liveness, without the spawn args. A render path
/// reads only these three fields, and cloning a `WorkerEntry` to get
/// them copies the worker's whole charter.
#[derive(Debug, Clone)]
pub struct LiveWorkerState {
    pub label: String,
    pub status: WorkerLiveness,
    pub slot: SessionSlot,
    /// Why a `Failed` spawn died, carried through so a row can say it
    /// without cloning the whole entry.
    pub diagnostic: Option<String>,
}

/// In-memory entry stored in `Workspace.live_workers[project_key]`.
/// `WorkerStatus` is the wire shape `agents__list` carries for a worker;
/// `session_key` is the workspace-internal routing handle.
///
/// `needs_tag` is workspace-internal scratch state (not part of the
/// wire shape) that tracks whether the on-disk `forge:worker:<label>`
/// tag has been appended to the session's JSONL yet. claude CLI
/// writes the JSONL lazily on the first user turn, so an idle-spawned
/// worker has no JSONL at `Connected`. The tag-write is retried
/// opportunistically when the first `DeliverWorkerPrompt` arrives.
#[derive(Debug, Clone)]
pub struct WorkerEntry {
    pub label: String,
    pub charter: String,
    /// The slot this worker fills, which is what addresses it.
    pub slot: SessionSlot,
    /// The session id the worker runs under: the one minted and recorded
    /// at spawn (or the one resumed), refreshed if the CLI ever adopts
    /// another. `None` before the spawn's id is known.
    pub session_id: Option<forge_primitives::SessionId>,
    pub status: WorkerLiveness,
    pub spawned_at: SystemTime,
    /// The slot of the session that spawned this worker - its project's
    /// lead.
    pub spawned_by: SessionSlot,
    pub needs_tag: bool,
    /// Cached at spawn time: was the project's path a git repo?
    /// Drives the Inspector pane's WORKTREE section render and
    /// the "in worktree" status-string suffix. `true` means the
    /// worker was spawned with `--worktree=<label>` and is operating
    /// in claude's auto-created `<project>/.claude/worktrees/<label>/`;
    /// `false` means non-git-repo project (no worktree, plain cwd).
    pub is_git_repo_at_spawn: bool,
    /// Cached at spawn time: the tree this worker runs in when the spawn
    /// pointed it somewhere other than the project's own path. Every
    /// read-back composes the worker's directory from it - the entry
    /// holds it so a seat read does not go to the store per frame, the
    /// way the gitness above is held.
    pub cwd: Option<std::path::PathBuf>,
    /// Human-readable failure reason when `status == Failed` (set by
    /// `transition_worker_to_failed` on `Connected`-never-arrived
    /// outcomes). `None` otherwise. The Projects pane renders this
    /// as a DIM sub-row beneath the worker label so the failure
    /// cause is visible without having to switch into the worker's
    /// chat view.
    pub diagnostic: Option<String>,
    /// First-turn message, delivered as the worker's first user turn on
    /// Connected (see `maybe_kick_worker_on_connected`). `None` for a
    /// kick-less spawn, where the worker idles until the lead sends an
    /// `agents__send_message`.
    pub kick: Option<String>,
}

impl WorkerEntry {
    /// Live in the label-gate and cap-count sense: not `Failed`. Both
    /// rules must agree on this, or a respawnable label still eats a
    /// cap slot.
    pub(crate) fn is_live(&self) -> bool {
        !matches!(self.status, WorkerLiveness::Failed)
    }

    /// Project the workspace-internal entry to the wire shape.
    /// `session_id` is the worker's claude-issued session UUID.
    ///
    /// `activity` is left `None`: deriving it needs the entry's
    /// `DomainSession`, which this method has no handle on. Every
    /// caller is a `SessionUpdate::WorkerStatusChanged` emission whose
    /// consumers read `status` alone, with one exception -
    /// `Workspace::worker_status_snapshot` builds on this via
    /// struct-update syntax and overwrites `activity` with the derived
    /// value, which is what `agents__list` returns.
    pub fn to_status(&self) -> WorkerStatus {
        WorkerStatus {
            label: self.label.clone(),
            charter: self.charter.clone(),
            status: self.status,
            session_id: self
                .session_id
                .as_ref()
                .map_or_else(String::new, |id| id.as_str().to_owned()),
            slot: self.slot.clone(),
            spawned_at: self.spawned_at,
            spawned_by: self.spawned_by.clone(),
            diagnostic: self.diagnostic.clone(),
            activity: None,
        }
    }
}

/// First non-`Failed` worker carrying `label`, if any. The one-live-
/// worker-per-label guard: a `Spawning`/`Running` worker already holds
/// the label so another spawn for it is a duplicate, while a `Failed`
/// entry is ignored (its label may be re-spawned). Enforced at the
/// shared `handle_spawn_worker` core so no dispatch source can
/// double-insert a label.
pub(crate) fn live_worker_with_label<'a>(
    entries: &'a [WorkerEntry],
    label: &str,
) -> Option<&'a WorkerEntry> {
    entries.iter().find(|w| w.label == label && w.is_live())
}

/// Workers that count against the cap. The cap check and the
/// `agents__capacity` read both go through here, so the reported
/// number cannot drift from the enforced one.
pub(crate) fn live_worker_count(entries: &[WorkerEntry]) -> usize {
    entries.iter().filter(|w| w.is_live()).count()
}

#[cfg(test)]
mod is_git_repo_at_spawn_tests {
    use super::*;
    use crate::SessionSlot;
    use std::time::SystemTime;

    fn fake_entry(is_git: bool) -> WorkerEntry {
        WorkerEntry {
            label: "reviewer".into(),
            charter: "review the diff".into(),
            slot: SessionSlot::from_str_for_test("uuid-1"),
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::from_str_for_test("lead-uuid"),
            needs_tag: false,
            is_git_repo_at_spawn: is_git,
            cwd: None,
            diagnostic: None,
            kick: None,
        }
    }

    #[test]
    fn worker_entry_carries_is_git_repo_at_spawn_true() {
        let entry = fake_entry(true);
        assert!(entry.is_git_repo_at_spawn);
    }

    #[test]
    fn worker_entry_carries_is_git_repo_at_spawn_false() {
        let entry = fake_entry(false);
        assert!(!entry.is_git_repo_at_spawn);
    }
}

#[cfg(test)]
mod worker_tag_dir_tests {
    use super::*;
    use tempfile::tempdir;

    fn run_git(dir: &Path, args: &[&str]) {
        let status =
            std::process::Command::new("git").arg("-C").arg(dir).args(args).status().expect("git");
        assert!(status.success(), "git {args:?} in {}", dir.display());
    }

    /// A project path composes the CLI's location even when a pre-#2460
    /// tree of that label stands under it - a fresh spawn must not adopt
    /// a tree it did not create. A root that IS the label's worktree (the
    /// entry a resume onto such a tree records) is that worktree.
    #[test]
    fn worker_tag_dir_composes_the_cli_location_unless_the_root_is_the_worktree() {
        let holder = tempdir().expect("holder");
        let main = holder.path().join("main");
        std::fs::create_dir_all(&main).expect("main dir");
        run_git(&main, &["init", "-q"]);
        run_git(&main, &["config", "user.email", "t@example.com"]);
        run_git(&main, &["config", "user.name", "Test"]);
        run_git(&main, &["config", "commit.gpgsign", "false"]);
        std::fs::write(main.join("README.md"), "seed").expect("write seed");
        run_git(&main, &["add", "."]);
        run_git(&main, &["commit", "-q", "-m", "init"]);
        let main = std::fs::canonicalize(&main).expect("canonical main");
        let linked = holder.path().join("linked");
        run_git(&main, &["worktree", "add", "-q", "-b", "linked", linked.to_str().expect("utf8")]);

        let legacy = linked.join(".claude").join("worktrees").join("old");
        std::fs::create_dir_all(legacy.parent().expect("worktrees root")).expect("mkdir");
        run_git(
            &main,
            &["worktree", "add", "-q", "-b", "worktree-old", legacy.to_str().expect("utf8")],
        );
        assert_eq!(
            worker_tag_dir(&linked, "old", true),
            main.join(".claude").join("worktrees").join("old"),
            "a project path composes the CLI's location, not a stale legacy tree",
        );

        assert_eq!(
            worker_tag_dir(&legacy, "old", true),
            legacy,
            "a root that is the label's own worktree is where a resumed worker runs",
        );

        assert_eq!(
            worker_tag_dir(&linked, "old", false),
            linked,
            "a non-git worker runs in the tree itself",
        );
    }

    /// The owner of a composed path survives a label that itself spans
    /// path components.
    #[test]
    fn worktree_owner_strips_a_multi_component_label() {
        let path = Path::new("/main/.claude/worktrees/team/reviewer");
        assert_eq!(
            worktree_owner(path, "team/reviewer"),
            Some(Path::new("/main").to_path_buf()),
            "the owner is the checkout above `.claude/worktrees`",
        );
        assert_eq!(
            worktree_owner(Path::new("/main/somewhere/else"), "reviewer"),
            None,
            "a path not shaped as a worktree has no owner",
        );
    }
}

#[cfg(test)]
mod spawn_cwd_tests {
    use super::*;
    use tempfile::tempdir;

    /// `git init` is the whole fixture: the two questions these helpers
    /// ask (`--git-dir`, `--show-toplevel`) both answer in an empty repo.
    fn init_repo(dir: &Path) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q"])
            .status()
            .expect("git runs");
        assert!(status.success(), "git init succeeds");
    }

    #[test]
    fn resolve_spawn_cwd_expands_a_tilde_path() {
        let home = dirs::home_dir().expect("home dir");

        let resolved = resolve_spawn_cwd("~/Projects/nazar").expect("a tilde path resolves");

        assert_eq!(
            resolved,
            home.join("Projects/nazar"),
            "~ expands to home, the way every \
            other forge path expands it"
        );
    }

    #[test]
    fn resolve_spawn_cwd_takes_an_absolute_path_unchanged() {
        let resolved = resolve_spawn_cwd("/tmp/some-tree").expect("an absolute path resolves");

        assert_eq!(resolved, PathBuf::from("/tmp/some-tree"));
    }

    /// A bare relative path would resolve against forge's own launch
    /// directory, which hard rule 14 forbids anything to depend on - so it
    /// is refused rather than placed. The refusal also keeps the word
    /// "worktree" out of it: the spawn failure classifier routes any
    /// message containing that word to a worktree-creation failure, which
    /// a bad argument is not.
    #[test]
    fn resolve_spawn_cwd_refuses_a_bare_relative_path() {
        let error = resolve_spawn_cwd("Projects/nazar").expect_err("relative is refused");

        assert!(error.contains("absolute"), "the refusal says what form is wanted: {error}");
        assert!(!error.contains("worktree"), "and never the classifier's word: {error}");
    }

    #[test]
    fn resolve_spawn_cwd_refuses_an_empty_or_blank_argument() {
        for raw in ["", "   "] {
            let error = resolve_spawn_cwd(raw).expect_err("blank is refused");
            assert!(
                error.contains("non-empty"),
                "the refusal says what is wrong with a blank path: {error}",
            );
        }
    }

    /// D9: a spawn that states no tree keeps today's behaviour exactly -
    /// the project's own path, and no gitness answer: that one comes from
    /// the worker's row or the caller's own probe.
    #[test]
    fn worker_root_and_git_without_a_cwd_keeps_the_project_root() {
        let dir = tempdir().expect("tempdir");

        let (root, is_git) = worker_root_and_git(dir.path(), None).expect("resolves");

        assert_eq!(root, dir.path(), "no cwd leaves the project root where it was");
        assert_eq!(is_git, None, "and this helper answers no gitness for it");
    }

    /// D5: a subdirectory of a repo resolves to the repo root at spawn, so
    /// the record and the worktree fork agree with where the claude CLI
    /// actually forks.
    #[test]
    fn worker_root_and_git_resolves_a_repo_subdirectory_to_the_root() {
        let repo = tempdir().expect("repo dir");
        init_repo(repo.path());
        let nested = repo.path().join("crates/deep");
        std::fs::create_dir_all(&nested).expect("nested dir");

        let (root, is_git) =
            worker_root_and_git(Path::new("/nonexistent-project"), Some(&nested.to_string_lossy()))
                .expect("resolves");

        assert_eq!(
            root,
            std::fs::canonicalize(repo.path()).expect("canonical repo"),
            "the stated subdirectory resolves to the repo root, not itself",
        );
        assert_eq!(
            is_git,
            Some(true),
            "and the tree is read as a git repo, which is what forks a worktree",
        );
    }

    #[test]
    fn worker_root_and_git_refuses_a_stated_path_that_is_not_there() {
        let error = worker_root_and_git(Path::new("/tmp"), Some("/nonexistent/nope"))
            .expect_err("a path that is not there is refused");

        assert!(error.contains("/nonexistent/nope"), "the refusal names the path: {error}");
        assert!(!error.contains("worktree"), "and never the classifier's word: {error}");
    }
}

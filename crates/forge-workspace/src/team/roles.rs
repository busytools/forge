//! File-driven engineering-team role definitions. Each role's charter
//! and initial kick live in `~/.claude/forge-team/<label>/charter.md`
//! and `~/.claude/forge-team/<label>/kick.md`. Labels may contain `/`
//! for namespace subdirectories (e.g. `backend/researcher`).
//!
//! A role may also carry `resume-kick.md`, read by [`load_resume_kick`]
//! when a worker resumes rather than starts fresh.
//!
//! Starting content for some roles ships in the repo under
//! `docs/forge-team-defaults/<label>/` (currently `implementer`, with
//! `resume-kick.md`, and `lead`). Users copy from there on first setup
//! and author the rest. No runtime bootstrap: a label whose files are
//! missing is skipped with a warning, `lead` included when it is named
//! in `static_workers`. The one fallback is on the lead-spawn path,
//! where an absent lead CHARTER resolves to [`DEFAULT_LEAD_CHARTER`];
//! nothing supplies a missing `kick.md`.

use std::io;
use std::path::PathBuf;

/// Loaded role data: label + charter prose + initial-kick prose.
/// Constructed via [`Role::load`] (production) or by hand in tests.
#[derive(Debug, PartialEq, Eq)]
pub struct Role {
    pub label: String,
    pub charter: String,
    pub initial_kick: String,
}

impl Role {
    /// Load the charter + initial-kick for `label` from
    /// `~/.claude/forge-team/<label>/{charter,kick}.md`.
    ///
    /// Validates the label (rejects empty, leading `/`, any `..` or
    /// `.` segment) before any disk read so a malicious config can't
    /// traverse outside the forge-team directory.
    ///
    /// # Errors
    ///
    /// Returns [`CharterError`] when the label is invalid, the home
    /// directory can't be resolved, either file is missing, or the
    /// read fails for any reason other than NotFound.
    pub fn load(label: &str) -> Result<Self, CharterError> {
        Ok(Self {
            label: label.to_owned(),
            charter: load_charter(label)?,
            initial_kick: load_initial_kick(label)?,
        })
    }

    /// Resolve `label` project-first-then-global within `namespace`,
    /// then load the resolved charter + kick. The Role's `label` stays
    /// BARE; only the files come from the resolved dir.
    ///
    /// # Errors
    ///
    /// Returns [`CharterError::CharterNotFound`] when neither a project
    /// nor a global charter resolves for `label`; otherwise the
    /// underlying [`load_charter`] / [`load_initial_kick`] error.
    pub fn load_for(label: &str, namespace: &str) -> Result<Self, CharterError> {
        let resolved =
            resolve_role(label, namespace).ok_or_else(|| CharterError::CharterNotFound {
                label: label.to_owned(),
                path: role_dir(label).map(|d| d.join("charter.md")).unwrap_or_default(),
            })?;
        Ok(Self {
            label: label.to_owned(),
            charter: load_charter(&resolved)?,
            initial_kick: load_initial_kick(&resolved)?,
        })
    }
}

/// Reserved label addressing the caller's own lead - see the workers
/// MCP server. Kept here so charter validation in
/// `forge-workspace::config` can sanity-check `static_workers = [...]`
/// entries against it without pulling in `mcp::workers`.
pub const LEAD_LABEL: &str = "lead";

/// Bundled lead charter, compiled in as the fallback when
/// `~/.claude/forge-team/lead/charter.md` is absent.
pub const DEFAULT_LEAD_CHARTER: &str =
    include_str!("../../../../docs/forge-team-defaults/lead/charter.md");

/// Errors loading a role's charter or kick file from disk.
#[derive(Debug)]
pub enum CharterError {
    /// Label is empty, starts with `/`, or contains a path segment
    /// equal to `..`, `.`, or empty (consecutive slashes).
    InvalidLabel(String),
    /// `dirs::home_dir()` returned None - typically only in restricted
    /// CI environments without a HOME.
    NoHomeDir,
    /// Charter file (`<label>/charter.md`) doesn't exist at the
    /// resolved path.
    CharterNotFound { label: String, path: PathBuf },
    /// Initial-kick file (`<label>/kick.md`) doesn't exist at the
    /// resolved path.
    KickNotFound { label: String, path: PathBuf },
    /// Read failed for any reason other than NotFound (permission
    /// denied, IO error, etc.).
    ReadFailed { label: String, path: PathBuf, source: io::Error },
}

impl std::fmt::Display for CharterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidLabel(s) => write!(
                f,
                "invalid role label '{s}': labels must be non-empty, must not start with '/', and must not contain '..' or '.' segments"
            ),
            Self::NoHomeDir => {
                write!(f, "home directory not available; cannot locate ~/.claude/forge-team/")
            }
            Self::CharterNotFound { label, path } => {
                write!(f, "no charter found for role '{label}' at {}", path.display())
            }
            Self::KickNotFound { label, path } => {
                write!(f, "no initial-kick found for role '{label}' at {}", path.display())
            }
            Self::ReadFailed { label, path, source } => {
                write!(f, "failed to read role '{label}' at {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for CharterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReadFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Validate a role label. Rejects empty, leading `/`, and any segment
/// equal to `..`, `.`, or empty (which would create a `//` in the
/// path). Slashes ARE allowed as namespace separators.
pub fn validate_label(label: &str) -> Result<(), CharterError> {
    if label.is_empty() || label.starts_with('/') {
        return Err(CharterError::InvalidLabel(label.to_owned()));
    }
    for seg in label.split('/') {
        if seg.is_empty() || seg == ".." || seg == "." {
            return Err(CharterError::InvalidLabel(label.to_owned()));
        }
    }
    Ok(())
}

/// Resolve `~/.claude/forge-team/`. Returns None when home dir is
/// unavailable.
///
/// Under `cfg(any(test, feature = "testing"))`, an in-process override
/// is consulted first (see [`set_forge_team_root_for_test`]) so
/// integration tests can point at a tempdir fixture without touching
/// the user's real home directory.
pub fn forge_team_root() -> Option<PathBuf> {
    #[cfg(any(test, feature = "testing"))]
    {
        if let Some(p) = test_forge_team_root::get() {
            return Some(p);
        }
    }
    dirs::home_dir().map(|home| home.join(".claude").join("forge-team"))
}

/// Override `forge_team_root` for tests. The previous override (if
/// any) is replaced. Passing `None` clears the override so subsequent
/// calls fall back to `dirs::home_dir()`. Test-only.
///
/// Returns the prior override so the test can restore it on teardown.
///
/// This does NOT serialise: the override is a process-global, so two
/// lib tests racing their own set/restore under a single-process runner
/// (`cargo test`) can clobber each other. Lib tests must go through
/// [`override_forge_team_root_for_test`], which holds a process-wide
/// lock. This raw setter is kept for the integration harness, which
/// runs in its own test binary (no cross-test race).
#[cfg(any(test, feature = "testing"))]
pub fn set_forge_team_root_for_test(root: Option<PathBuf>) -> Option<PathBuf> {
    test_forge_team_root::set(root)
}

/// RAII guard from [`override_forge_team_root_for_test`]: holds the
/// process-wide team-root test lock for its lifetime and restores the
/// prior override on drop.
#[cfg(any(test, feature = "testing"))]
pub struct ForgeTeamRootTestGuard {
    prior: Option<PathBuf>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(any(test, feature = "testing"))]
impl Drop for ForgeTeamRootTestGuard {
    fn drop(&mut self) {
        set_forge_team_root_for_test(self.prior.take());
    }
}

/// Install `root` as the `forge_team_root` override under a process-wide
/// lock, restoring the prior override + releasing the lock on drop. Every
/// lib test that overrides the team root goes through this so the
/// set -> use -> restore window is atomic across parallel `cargo test`
/// threads (nextest's per-process isolation hid the race). Recovers a
/// poisoned lock (a prior test panicked while holding it) rather than
/// cascading the panic into every later team-root test.
#[cfg(any(test, feature = "testing"))]
pub fn override_forge_team_root_for_test(root: PathBuf) -> ForgeTeamRootTestGuard {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let prior = set_forge_team_root_for_test(Some(root));
    ForgeTeamRootTestGuard { prior, _lock: lock }
}

#[cfg(any(test, feature = "testing"))]
mod test_forge_team_root {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::OnceLock;

    fn cell() -> &'static Mutex<Option<PathBuf>> {
        static C: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
        C.get_or_init(|| Mutex::new(None))
    }

    pub(crate) fn get() -> Option<PathBuf> {
        cell().lock().ok().and_then(|g| g.clone())
    }

    pub(crate) fn set(root: Option<PathBuf>) -> Option<PathBuf> {
        let Ok(mut g) = cell().lock() else {
            return None;
        };
        std::mem::replace(&mut *g, root)
    }
}

/// Resolve `~/.claude/forge-team/<label>/`.
pub fn role_dir(label: &str) -> Result<PathBuf, CharterError> {
    validate_label(label)?;
    let root = forge_team_root().ok_or(CharterError::NoHomeDir)?;
    Ok(root.join(label))
}

/// True when `<label>/charter.md` exists under the forge-team root.
/// `label` may be bare or namespaced.
fn role_charter_exists(label: &str) -> bool {
    role_dir(label).is_ok_and(|d| d.join("charter.md").is_file())
}

/// Resolve a BARE role `label` for a session in `project_namespace` to
/// the on-disk dir-label whose charter exists: project-local
/// (`<namespace>/<label>`) first, then global (`<label>`). `None` when
/// neither exists. The returned value is for loading files only - the
/// worker keeps the bare `label` for display + addressing.
pub fn resolve_role(label: &str, project_namespace: &str) -> Option<String> {
    let scoped = format!("{project_namespace}/{label}");
    if role_charter_exists(&scoped) {
        return Some(scoped);
    }
    if role_charter_exists(label) {
        return Some(label.to_owned());
    }
    None
}

/// Load `<label>/charter.md` from the forge-team root.
///
/// # Errors
///
/// See [`CharterError`].
pub fn load_charter(label: &str) -> Result<String, CharterError> {
    let path = role_dir(label)?.join("charter.md");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(CharterError::CharterNotFound { label: label.to_owned(), path })
        }
        Err(source) => Err(CharterError::ReadFailed { label: label.to_owned(), path, source }),
    }
}

/// Load the lead charter, preferring the user override and falling
/// back to [`DEFAULT_LEAD_CHARTER`] so a lead is always charter-backed.
/// A missing override falls back silently; any other load failure is
/// logged first so the cause stays diagnosable.
pub fn load_lead_charter_or_default() -> String {
    match load_charter(LEAD_LABEL) {
        Ok(charter) => charter,
        Err(CharterError::CharterNotFound { .. }) => DEFAULT_LEAD_CHARTER.to_owned(),
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::team",
                error = %e,
                "could not load lead charter; using bundled default"
            );
            DEFAULT_LEAD_CHARTER.to_owned()
        }
    }
}

/// Load `<label>/kick.md` from the forge-team root.
///
/// # Errors
///
/// See [`CharterError`].
pub fn load_initial_kick(label: &str) -> Result<String, CharterError> {
    let path = role_dir(label)?.join("kick.md");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(CharterError::KickNotFound { label: label.to_owned(), path })
        }
        Err(source) => Err(CharterError::ReadFailed { label: label.to_owned(), path, source }),
    }
}

/// Optional resume-specific kick. When a worker session is resumed
/// rather than freshly spawned, the kick-on-Connected hook uses this
/// content (if present) to re-orient the worker - explicit "you're
/// picking up where you left off" framing instead of the
/// fresh-start framing in `kick.md`.
///
/// Returns:
/// - `Ok(Some(text))` when `<label>/resume-kick.md` exists.
/// - `Ok(None)` when the file is absent - caller falls back to the
///   default kick (or skips entirely per the
///   `worker_has_progress_past_kick` gate). Absent is the common case
///   for roles whose default behaviour is fine post-resume.
/// - `Err(CharterError)` only on label-validation failure or IO
///   error other than NotFound. NotFound is intentionally folded
///   into `Ok(None)` so callers don't need to pattern-match on the
///   missing-file case.
///
/// # Errors
///
/// See [`CharterError`]; NotFound on the file specifically is NOT an
/// error - it returns `Ok(None)`.
pub fn load_resume_kick(label: &str) -> Result<Option<String>, CharterError> {
    let path = role_dir(label)?.join("resume-kick.md");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CharterError::ReadFailed { label: label.to_owned(), path, source }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_label_accepts_simple_names() {
        assert!(validate_label("planner").is_ok());
        assert!(validate_label("researcher").is_ok());
        assert!(validate_label("custom-name_123").is_ok());
    }

    #[test]
    fn validate_label_accepts_namespaced_labels() {
        assert!(validate_label("backend/researcher").is_ok());
        assert!(validate_label("team/sub-team/role").is_ok());
    }

    #[test]
    fn validate_label_rejects_empty_and_dot_segments() {
        assert!(matches!(validate_label(""), Err(CharterError::InvalidLabel(_))));
        assert!(matches!(validate_label("/researcher"), Err(CharterError::InvalidLabel(_))));
        assert!(matches!(validate_label("foo/.."), Err(CharterError::InvalidLabel(_))));
        assert!(matches!(validate_label("../escape"), Err(CharterError::InvalidLabel(_))));
        assert!(matches!(validate_label("./relative"), Err(CharterError::InvalidLabel(_))));
        assert!(matches!(validate_label("foo//bar"), Err(CharterError::InvalidLabel(_))));
        assert!(matches!(validate_label("foo/./bar"), Err(CharterError::InvalidLabel(_))));
        assert!(matches!(validate_label("foo/../bar"), Err(CharterError::InvalidLabel(_))));
    }

    /// `Role::load` reads from a real `dirs::home_dir()`; this test
    /// exercises the labels-and-paths plumbing rather than the
    /// production filesystem (tests don't sandbox $HOME). The loader
    /// is integration-covered via the e2e tests in
    /// `tests/engineering_team_e2e.rs`.
    #[test]
    fn role_dir_joins_label_under_forge_team_root() {
        let Some(root) = forge_team_root() else {
            // Skip if HOME is unavailable in CI; we don't fabricate.
            return;
        };
        let dir = role_dir("planner").expect("simple label resolves");
        assert_eq!(dir, root.join("planner"));
        let nested = role_dir("backend/researcher").expect("namespaced label resolves");
        assert_eq!(nested, root.join("backend").join("researcher"));
    }

    #[test]
    fn role_dir_rejects_invalid_label() {
        assert!(matches!(role_dir(""), Err(CharterError::InvalidLabel(_))));
        assert!(matches!(role_dir("../escape"), Err(CharterError::InvalidLabel(_))));
    }

    /// CharterError implements Display + Error correctly so callers
    /// can surface the missing-file path to users.
    #[test]
    fn charter_error_displays_path_and_label() {
        let err = CharterError::CharterNotFound {
            label: "researcher".to_owned(),
            path: PathBuf::from("/tmp/forge-team/researcher/charter.md"),
        };
        let s = err.to_string();
        assert!(s.contains("researcher"));
        assert!(s.contains("/tmp/forge-team/researcher/charter.md"));
    }

    fn mk(root: &std::path::Path, label: &str) {
        let d = root.join(label);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("charter.md"), "description: x\n").unwrap();
        std::fs::write(d.join("kick.md"), "go\n").unwrap();
    }

    #[test]
    fn resolve_prefers_project_then_global() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path();
        mk(root, "implementer"); // global
        mk(root, "data-modules/steward"); // project
        mk(root, "forge/implementer"); // project override of a global
        let _guard = override_forge_team_root_for_test(root.to_path_buf());

        // bare project role resolves to <ns>/<label> from its own project
        assert_eq!(
            resolve_role("steward", "data-modules").as_deref(),
            Some("data-modules/steward")
        );
        // from another project, the project role does NOT resolve (scope)
        assert_eq!(resolve_role("steward", "forge"), None);
        // global resolves anywhere when no project role shadows it
        assert_eq!(resolve_role("implementer", "data-modules").as_deref(), Some("implementer"));
        // project role shadows a global of the same bare name
        assert_eq!(resolve_role("implementer", "forge").as_deref(), Some("forge/implementer"));
    }
}

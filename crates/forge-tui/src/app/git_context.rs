//! TUI-side cache for git context snapshots emitted by the agent.
//!
//! Filesystem reads, the `.git/HEAD` walker, the `notify::Watcher`,
//! and the 75ms debounce all live in `forge_workspace::env::git` (a
//! re-export of `forge_agent::env::git`). The TUI starts a watcher
//! per session via `AgentHandle::start_git_context_watch` and consumes
//! `AgentEvent::GitContextSnapshot` events; this module is the
//! App-side cache those events feed.

use forge_primitives::git::{GitBranch, GitContext};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum BranchDisplayState {
    Named(String),
    Detached,
    #[default]
    NoRepo,
    Unknown,
}

/// A git-context summary suitable for rendering in the footer chip
/// or status panel. Distinguishes named branches from detached HEAD
/// so the renderer can style them differently. `NoRepo` and
/// `Unknown` collapse to `None` at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchChip<'a> {
    Named(&'a str),
    Detached,
}

impl BranchDisplayState {
    #[must_use]
    pub(crate) fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Named(branch) => Some(branch.as_str()),
            Self::Detached | Self::NoRepo | Self::Unknown => None,
        }
    }

    #[must_use]
    pub(crate) fn as_chip(&self) -> Option<BranchChip<'_>> {
        match self {
            Self::Named(branch) => Some(BranchChip::Named(branch.as_str())),
            Self::Detached => Some(BranchChip::Detached),
            Self::NoRepo | Self::Unknown => None,
        }
    }
}

impl From<GitBranch> for BranchDisplayState {
    fn from(branch: GitBranch) -> Self {
        match branch {
            GitBranch::Named(name) => Self::Named(name),
            GitBranch::Detached => Self::Detached,
            GitBranch::NoRepo => Self::NoRepo,
            // `forge_primitives::git::GitBranch` is `#[non_exhaustive]`; explicit
            // `Unknown` plus the wildcard for any future variants both
            // surface as `Unknown` (TUI hides the chip).
            GitBranch::Unknown | _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct GitContextState {
    branch: BranchDisplayState,
}

impl GitContextState {
    #[must_use]
    pub(crate) fn branch_name(&self) -> Option<&str> {
        self.branch.as_deref()
    }

    #[must_use]
    pub(crate) fn branch_chip(&self) -> Option<BranchChip<'_>> {
        self.branch.as_chip()
    }

    /// Apply a bridge-pushed snapshot. Returns `true` when the
    /// resolved branch state changed (i.e. the caller should mark
    /// `needs_redraw`).
    pub(crate) fn apply_snapshot(&mut self, context: GitContext) -> bool {
        let next = BranchDisplayState::from(context.branch);
        if next != self.branch {
            self.branch = next;
            return true;
        }
        false
    }

    #[cfg(test)]
    pub(crate) fn set_branch_for_test(&mut self, branch: Option<&str>) {
        self.branch = match branch {
            Some(branch) => BranchDisplayState::Named(branch.to_owned()),
            None => BranchDisplayState::NoRepo,
        };
    }

    #[cfg(test)]
    pub(crate) fn set_detached_for_test(&mut self) {
        self.branch = BranchDisplayState::Detached;
    }
}

#[cfg(test)]
mod tests {
    use super::{BranchChip, BranchDisplayState, GitContextState};
    use forge_primitives::git::{GitBranch, GitContext};

    fn ctx(branch: GitBranch) -> GitContext {
        // forge_primitives::git::GitContext is #[non_exhaustive] but
        // GitContext::default() yields `branch: GitBranch::NoRepo`;
        // overwrite the field for test setup.
        let mut c = GitContext::default();
        c.branch = branch;
        c
    }

    #[test]
    fn default_is_no_repo() {
        let state = GitContextState::default();
        assert_eq!(state.branch_name(), None);
    }

    #[test]
    fn apply_snapshot_named_returns_true_on_change() {
        let mut state = GitContextState::default();
        let changed = state.apply_snapshot(ctx(GitBranch::Named("main".to_owned())));
        assert!(changed);
        assert_eq!(state.branch_name(), Some("main"));
    }

    #[test]
    fn apply_snapshot_returns_false_on_same() {
        let mut state = GitContextState::default();
        state.apply_snapshot(ctx(GitBranch::Named("main".to_owned())));
        let changed = state.apply_snapshot(ctx(GitBranch::Named("main".to_owned())));
        assert!(!changed);
    }

    #[test]
    fn apply_snapshot_detached_yields_no_branch_name() {
        let mut state = GitContextState::default();
        state.apply_snapshot(ctx(GitBranch::Detached));
        assert_eq!(state.branch_name(), None);
        assert_eq!(state.branch, BranchDisplayState::Detached);
    }

    #[test]
    fn set_branch_for_test_updates_state() {
        let mut state = GitContextState::default();
        state.set_branch_for_test(Some("feature/x"));
        assert_eq!(state.branch_name(), Some("feature/x"));
        state.set_branch_for_test(None);
        assert_eq!(state.branch_name(), None);
    }

    #[test]
    fn branch_chip_named_returns_chip() {
        let mut state = GitContextState::default();
        state.apply_snapshot(ctx(GitBranch::Named("main".to_owned())));
        assert_eq!(state.branch_chip(), Some(BranchChip::Named("main")));
    }

    #[test]
    fn branch_chip_detached_returns_chip() {
        let mut state = GitContextState::default();
        state.apply_snapshot(ctx(GitBranch::Detached));
        assert_eq!(state.branch_chip(), Some(BranchChip::Detached));
        // branch_name still collapses Detached to None for legacy callers.
        assert_eq!(state.branch_name(), None);
    }

    #[test]
    fn branch_chip_no_repo_returns_none() {
        let state = GitContextState::default();
        assert_eq!(state.branch_chip(), None);
    }

    #[test]
    fn branch_chip_unknown_returns_none() {
        let mut state = GitContextState::default();
        state.apply_snapshot(ctx(GitBranch::Unknown));
        assert_eq!(state.branch_chip(), None);
    }
}

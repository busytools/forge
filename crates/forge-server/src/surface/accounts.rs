//! `account_pool()`: the account pool as a view draws it.

use forge_workspace::AccountPoolView;

use super::ViewSurface;

impl ViewSurface {
    /// The pool as the home snapshot and `AccountsChanged` both state it.
    ///
    /// **Delegated rather than rebuilt.** The frame's payload and the
    /// snapshot's field must be one builder's answer, or a client folding the
    /// frame and a client reading the snapshot can disagree (rule 25).
    pub fn accounts(&self) -> AccountPoolView {
        self.workspace.account_pool_view()
    }
}

//! The account pool: the account state map and the assignment plan
//! behind one handle.
//!
//! Owns the two locks the caller used to hold as struct fields. The
//! guards keep the caller's lock discipline: a site that took one lock
//! still takes one, never holds it across an await, and sees the same
//! guard type it saw before.

use parking_lot::{Mutex, MutexGuard};

use crate::account::AccountStateMap;
use crate::assignment_plan::AssignmentPlan;

/// The account state map and the assignment plan, behind one handle.
pub struct AccountPool {
    accounts: Mutex<AccountStateMap>,
    plan: Mutex<Option<AssignmentPlan>>,
}

impl AccountPool {
    pub fn new(specs: &[forge_primitives::account::LoadedAccount]) -> Self {
        Self { accounts: Mutex::new(AccountStateMap::new(specs)), plan: Mutex::new(None) }
    }

    /// Empty map for the `testing` feature's `Workspace::testing_stub`.
    #[cfg(any(test, feature = "testing"))]
    pub fn empty_for_test() -> Self {
        Self { accounts: Mutex::new(AccountStateMap::empty_for_test()), plan: Mutex::new(None) }
    }

    /// TRANSITIONAL: lent so compound call sites keep their critical
    /// section while they live in the caller; 2c absorbs those sites
    /// into the gateway and this guard stops being public.
    pub fn state(&self) -> MutexGuard<'_, AccountStateMap> {
        self.accounts.lock()
    }

    /// TRANSITIONAL: same terms as [`Self::state`], for the plan; 2c
    /// absorbs its compound sites too.
    pub fn plan(&self) -> MutexGuard<'_, Option<AssignmentPlan>> {
        self.plan.lock()
    }
}

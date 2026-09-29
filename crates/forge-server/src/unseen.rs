//! The green diamond: a turn that finished on a session this page has not
//! shown since.

use std::collections::HashSet;

use forge_primitives::SessionSlot;

/// Which sessions have finished a turn since this view last showed them.
///
/// Viewer-relative by nature - whether a completion is unseen depends on
/// who is looking, and two views can disagree about the same slot and both
/// be right - so it is held here rather than on the core.
#[derive(Default, Clone)]
pub struct Unseen {
    unseen: HashSet<SessionSlot>,
}

impl Unseen {
    pub fn new() -> Self {
        Self::default()
    }

    /// A turn finished on `slot`. Marking an already-unseen session is the
    /// same as marking it once.
    pub fn mark_completed(&mut self, slot: &SessionSlot) {
        self.unseen.insert(slot.clone());
    }

    /// This view has shown `slot`, so nothing about it is unseen.
    pub fn clear(&mut self, slot: &SessionSlot) {
        self.unseen.remove(slot);
    }

    pub fn is_unseen(&self, slot: &SessionSlot) -> bool {
        self.unseen.contains(slot)
    }
}

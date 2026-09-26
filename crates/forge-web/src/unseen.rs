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

#[cfg(test)]
mod tests {
    use super::Unseen;
    use forge_primitives::SessionSlot;

    /// Catches a marker that clears everything at once, one that never
    /// clears, and one that misses the second completion on a slot.
    #[test]
    fn a_completion_is_unseen_until_that_session_is_shown() {
        let mut unseen = Unseen::new();
        let done = SessionSlot::lead("Org", "forge");
        let other = SessionSlot::lead("Org", "other");

        assert!(!unseen.is_unseen(&done), "nothing is unseen before a turn finishes");

        unseen.mark_completed(&done);
        assert!(unseen.is_unseen(&done), "a completed turn is unseen until it is shown");

        unseen.mark_completed(&other);
        assert!(unseen.is_unseen(&done), "a second slot's completion must not clear the first");
        assert!(unseen.is_unseen(&other));

        unseen.clear(&done);
        assert!(!unseen.is_unseen(&done), "showing the session clears its diamond");
        assert!(unseen.is_unseen(&other), "and leaves every other session alone");
    }
}

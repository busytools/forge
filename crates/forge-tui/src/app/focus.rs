/// Logical focus target that can claim directional key navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    Mention,
    Emoji,
    Help,
}

impl FocusTarget {
    const fn bit(self) -> u8 {
        match self {
            Self::Mention => 1 << 0,
            Self::Emoji => 1 << 1,
            Self::Help => 1 << 2,
        }
    }
}

/// Effective owner of directional/navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusOwner {
    Input,
    Mention,
    Emoji,
    Help,
}

/// Set of currently available focus targets, packed into a u8
/// bitset. Copy + Default; tests construct via `.with(target)`
/// chain or `FocusContext::default()`.
#[derive(Debug, Clone, Copy)]
pub struct FocusContext {
    available: u8,
}

impl FocusContext {
    pub const fn empty() -> Self {
        Self { available: 0 }
    }

    pub const fn with(mut self, target: FocusTarget) -> Self {
        self.available |= target.bit();
        self
    }

    pub const fn supports(self, target: FocusTarget) -> bool {
        (self.available & target.bit()) != 0
    }
}

impl From<FocusTarget> for FocusOwner {
    fn from(value: FocusTarget) -> Self {
        match value {
            FocusTarget::Mention => Self::Mention,
            FocusTarget::Emoji => Self::Emoji,
            FocusTarget::Help => Self::Help,
        }
    }
}

/// Focus claim manager:
/// latest valid claim wins; invalid claims are dropped during normalization.
#[derive(Debug, Clone, Default)]
pub struct FocusManager {
    stack: Vec<FocusTarget>,
}

impl FocusManager {
    /// Resolve the current focus owner for key routing.
    pub fn owner(&self, context: FocusContext) -> FocusOwner {
        for target in self.stack.iter().rev().copied() {
            if context.supports(target) {
                return target.into();
            }
        }
        FocusOwner::Input
    }

    /// Claim focus for the target. Latest valid claim wins.
    pub fn claim(&mut self, target: FocusTarget, context: FocusContext) {
        self.stack.retain(|t| *t != target);
        self.stack.push(target);
        self.normalize(context);
    }

    /// Release focus claim for the target.
    pub fn release(&mut self, target: FocusTarget, context: FocusContext) {
        if let Some(idx) = self.stack.iter().rposition(|t| *t == target) {
            self.stack.remove(idx);
        }
        self.normalize(context);
    }

    /// Remove claims no longer valid in the current context.
    pub fn normalize(&mut self, context: FocusContext) {
        self.stack.retain(|target| context.supports(*target));
    }
}

#[cfg(test)]
mod tests {
    use super::{FocusContext, FocusManager, FocusOwner, FocusTarget};

    #[test]
    fn owner_defaults_to_input_without_claims() {
        let mgr = FocusManager::default();
        let ctx = FocusContext::empty();
        assert_eq!(mgr.owner(ctx), FocusOwner::Input);
    }

    #[test]
    fn latest_valid_claim_wins() {
        let mut mgr = FocusManager::default();
        let ctx = FocusContext::empty().with(FocusTarget::Help).with(FocusTarget::Mention);
        mgr.claim(FocusTarget::Help, ctx);
        mgr.claim(FocusTarget::Mention, ctx);
        assert_eq!(mgr.owner(ctx), FocusOwner::Mention);
    }

    #[test]
    fn invalid_claims_are_normalized_out() {
        let mut mgr = FocusManager::default();
        let valid_ctx = FocusContext::empty().with(FocusTarget::Mention);
        let invalid_ctx = FocusContext::empty();
        mgr.claim(FocusTarget::Mention, valid_ctx);
        assert_eq!(mgr.owner(valid_ctx), FocusOwner::Mention);
        mgr.normalize(invalid_ctx);
        assert_eq!(mgr.owner(invalid_ctx), FocusOwner::Input);
    }

    #[test]
    fn help_focus_target_works_when_enabled() {
        let mut mgr = FocusManager::default();
        let ctx = FocusContext::empty().with(FocusTarget::Help);
        mgr.claim(FocusTarget::Help, ctx);
        assert_eq!(mgr.owner(ctx), FocusOwner::Help);
    }
}

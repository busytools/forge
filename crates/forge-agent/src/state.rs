//! Permission-mode enum used across the agent stack.
//!
//! The canonical type lives at `forge_primitives::permission::PermissionMode`
//! (Phase 0 of the MVVM refactor unified two previously-divergent
//! definitions). This module re-exports it so existing
//! `forge_agent::state::PermissionMode` imports keep resolving.

pub use forge_primitives::permission::PermissionMode;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_mode_reexport_resolves() {
        assert_eq!(PermissionMode::from_wire("auto"), Some(PermissionMode::Auto));
    }
}

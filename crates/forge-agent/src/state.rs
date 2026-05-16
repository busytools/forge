//! Permission-mode enum used across the agent stack. The canonical
//! type lives at `forge_primitives::permission::PermissionMode`;
//! this module re-exports it so the
//! `forge_agent::state::PermissionMode` import path resolves.

pub use forge_primitives::permission::PermissionMode;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_mode_reexport_resolves() {
        assert_eq!(PermissionMode::from_wire("auto"), Some(PermissionMode::Auto));
    }
}

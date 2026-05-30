//! Mode-state helpers - supported-mode list filtering + `ModeState`
//! builder. Used by App-side `events::sdk_message` (when
//! `System(init)` arrives + on `/mode` slash submit) and by the
//! worker (to assemble the Connected event payload at spawn time).

use std::collections::HashSet;

use forge_primitives::permission::PermissionMode;
use forge_primitives::{ModeInfo, ModeState};

/// Canonical MODE_OPTIONS order mirrored from upstream.
const CANONICAL_ORDER: [PermissionMode; 6] = [
    PermissionMode::Ask,
    PermissionMode::AcceptEdits,
    PermissionMode::Plan,
    PermissionMode::DontAsk,
    PermissionMode::Auto,
    PermissionMode::BypassPermissions,
];

/// Returns the supported-mode list filtered by the runtime-unavailable
/// list (but keeping the current mode if it's still set). Mirrors the
/// upstream rules: BASE + Auto (if model supports it) + BypassPermissions
/// (if session allows) + the current mode itself (so the active mode
/// never disappears mid-session).
pub fn supported_mode_ids_filtered(
    current_model_supports_auto_mode: bool,
    supports_bypass_permissions_mode: bool,
    current_mode: Option<PermissionMode>,
    runtime_unavailable_mode_ids: &[PermissionMode],
) -> Vec<PermissionMode> {
    let mut seen: HashSet<PermissionMode> = CANONICAL_ORDER[..4].iter().copied().collect();
    if current_model_supports_auto_mode {
        seen.insert(PermissionMode::Auto);
    }
    if supports_bypass_permissions_mode {
        seen.insert(PermissionMode::BypassPermissions);
    }
    if let Some(mode) = current_mode {
        seen.insert(mode);
    }
    CANONICAL_ORDER
        .into_iter()
        .filter(|m| seen.contains(m))
        .filter(|m| current_mode == Some(*m) || !runtime_unavailable_mode_ids.contains(m))
        .collect()
}

/// Composes a `ModeState` from the active mode + the resolved
/// supported-mode list.
pub fn build_mode_state_from_supported(
    mode: PermissionMode,
    supported_mode_ids: &[PermissionMode],
) -> ModeState {
    ModeState {
        current_mode_id: mode.as_wire().to_owned(),
        current_mode_name: mode.display_name().to_owned(),
        available_modes: supported_mode_ids
            .iter()
            .map(|m| ModeInfo {
                id: m.as_wire().to_owned(),
                name: m.display_name().to_owned(),
                description: None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_supported_modes_default_to_four() {
        let supported = supported_mode_ids_filtered(false, false, None, &[]);
        assert_eq!(
            supported,
            vec![
                PermissionMode::Ask,
                PermissionMode::AcceptEdits,
                PermissionMode::Plan,
                PermissionMode::DontAsk
            ]
        );
    }

    #[test]
    fn auto_mode_appears_when_current_model_supports_it() {
        let supported = supported_mode_ids_filtered(true, false, None, &[]);
        assert!(supported.contains(&PermissionMode::Auto));
    }

    #[test]
    fn bypass_appears_when_session_supports_it() {
        let supported = supported_mode_ids_filtered(false, true, None, &[]);
        assert!(supported.contains(&PermissionMode::BypassPermissions));
    }

    #[test]
    fn current_mode_survives_runtime_unavailable_filter() {
        let supported = supported_mode_ids_filtered(
            false,
            false,
            Some(PermissionMode::Plan),
            &[PermissionMode::Plan],
        );
        assert!(supported.contains(&PermissionMode::Plan));
    }

    #[test]
    fn build_mode_state_from_supported_uses_supported_list() {
        let supported = vec![
            PermissionMode::Ask,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::DontAsk,
        ];
        let state = build_mode_state_from_supported(PermissionMode::AcceptEdits, &supported);
        assert_eq!(state.current_mode_id, "acceptEdits");
        assert_eq!(state.current_mode_name, "Accept Edits");
        assert_eq!(state.available_modes.len(), 4);
        assert_eq!(state.available_modes[0].id, "default");
    }
}

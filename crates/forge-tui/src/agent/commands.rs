//! Mode-state helpers — supported-mode list filtering + `ModeState`
//! builder. Used by App-side `events::sdk_message` (when
//! `System(init)` arrives + on `/mode` slash submit) and by the
//! worker (to assemble the Connected event payload at spawn time).

use crate::agent::types::{ModeInfo, ModeState};

use super::state::PermissionMode;

const BASE_SUPPORTED_MODE_IDS: [PermissionMode; 4] = [
    PermissionMode::Default,
    PermissionMode::AcceptEdits,
    PermissionMode::Plan,
    PermissionMode::DontAsk,
];

fn unique_mode_ids(modes: Vec<PermissionMode>) -> Vec<PermissionMode> {
    // Mirror upstream's MODE_OPTIONS ordering: default, acceptEdits,
    // plan, dontAsk, auto, bypassPermissions. Filter to only present
    // ids while preserving canonical order.
    const CANONICAL_ORDER: [PermissionMode; 6] = [
        PermissionMode::Default,
        PermissionMode::AcceptEdits,
        PermissionMode::Plan,
        PermissionMode::DontAsk,
        PermissionMode::Auto,
        PermissionMode::BypassPermissions,
    ];
    let mut seen: std::collections::HashSet<PermissionMode> =
        std::collections::HashSet::with_capacity(modes.len());
    for m in modes {
        seen.insert(m);
    }
    CANONICAL_ORDER.into_iter().filter(|m| seen.contains(m)).collect()
}

/// Computes the supported-mode list from primitive inputs.
/// Mirrors the upstream rules: BASE + Auto (if model supports it) +
/// `BypassPermissions` (if session allows) + the current mode itself
/// (so the active mode never disappears mid-session).
#[must_use]
fn computed_supported_mode_ids_from_inputs(
    current_model_supports_auto_mode: bool,
    supports_bypass_permissions_mode: bool,
    current_mode: Option<PermissionMode>,
) -> Vec<PermissionMode> {
    let mut supported: Vec<PermissionMode> = BASE_SUPPORTED_MODE_IDS.to_vec();
    if current_model_supports_auto_mode {
        supported.push(PermissionMode::Auto);
    }
    if supports_bypass_permissions_mode {
        supported.push(PermissionMode::BypassPermissions);
    }
    if let Some(mode) = current_mode {
        supported.push(mode);
    }
    unique_mode_ids(supported)
}

/// Returns the supported-mode list filtered by the runtime-unavailable
/// list (but keeping the current mode if it's still set).
#[must_use]
pub fn supported_mode_ids_filtered(
    current_model_supports_auto_mode: bool,
    supports_bypass_permissions_mode: bool,
    current_mode: Option<PermissionMode>,
    runtime_unavailable_mode_ids: &[PermissionMode],
) -> Vec<PermissionMode> {
    let computed = computed_supported_mode_ids_from_inputs(
        current_model_supports_auto_mode,
        supports_bypass_permissions_mode,
        current_mode,
    );
    computed
        .into_iter()
        .filter(|m| current_mode == Some(*m) || !runtime_unavailable_mode_ids.contains(m))
        .collect()
}

fn mode_info_for_id(mode: PermissionMode) -> ModeInfo {
    ModeInfo {
        id: mode.as_wire().to_owned(),
        name: mode.display_name().to_owned(),
        description: None,
    }
}

/// Maps a supported-mode list into `ModeInfo` records ready for
/// `ModeState.available_modes`.
#[must_use]
fn available_modes_from_supported(supported_mode_ids: &[PermissionMode]) -> Vec<ModeInfo> {
    supported_mode_ids.iter().copied().map(mode_info_for_id).collect()
}

/// Composes a `ModeState` from the active mode + the resolved
/// supported-mode list. Used by App-side `apply_mode_state_from_init`
/// + `apply_optimistic_mode_change` + `apply_optimistic_model_change`.
#[must_use]
pub fn build_mode_state_from_supported(
    mode: PermissionMode,
    supported_mode_ids: &[PermissionMode],
) -> ModeState {
    ModeState {
        current_mode_id: mode.as_wire().to_owned(),
        current_mode_name: mode.display_name().to_owned(),
        available_modes: available_modes_from_supported(supported_mode_ids),
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
                PermissionMode::Default,
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
            PermissionMode::Default,
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

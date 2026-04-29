#![allow(
    dead_code,
    missing_docs,
    clippy::pedantic,
    clippy::disallowed_methods,
    clippy::while_let_loop,
    clippy::collapsible_if,
    reason = "lifted upstream from claude-code-rust"
)]

use crate::state::app::App;
use crate::state::inline_interactions::{
    focus_next_inline_interaction, focused_interaction, focused_interaction_dirty_idx,
    get_focused_interaction_tc, invalidate_if_changed, pop_next_valid_interaction_id,
};
use crate::state::keys::is_ctrl_char_shortcut;
use crate::state::messages::MessageBlock;
use crate::state::model;
use crate::state::model::PermissionOptionKind;
use crate::state::tool_call_info::InlinePermission;
use crate::state::viewport::LayoutInvalidation as InvalidationLevel;
use crossterm::event::{KeyCode, KeyEvent};

fn focused_permission(app: &App) -> Option<&InlinePermission> {
    focused_interaction(app)?.pending_permission.as_ref()
}

fn focused_option_index_by_kind(app: &App, kind: PermissionOptionKind) -> Option<usize> {
    focused_option_index_where(app, |opt| opt.kind == kind)
}

fn focused_option_index_where<F>(app: &App, mut predicate: F) -> Option<usize>
where
    F: FnMut(&model::PermissionOption) -> bool,
{
    focused_permission(app)?.options.iter().position(&mut predicate)
}

fn normalized_option_tokens(option: &model::PermissionOption) -> String {
    let mut out = String::new();
    for ch in option.name.chars().chain(option.option_id.chars()) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

fn option_tokens(option: &model::PermissionOption) -> (bool, bool, bool, bool) {
    let tokens = normalized_option_tokens(option);
    let allow_like =
        tokens.contains("allow") || tokens.contains("accept") || tokens.contains("approve");
    let reject_like =
        tokens.contains("reject") || tokens.contains("deny") || tokens.contains("disallow");
    let persistent_like = tokens.contains("always")
        || tokens.contains("dontask")
        || tokens.contains("remember")
        || tokens.contains("persist")
        || tokens.contains("bypasspermissions");
    let session_like = tokens.contains("session") || tokens.contains("onesession");
    (allow_like, reject_like, persistent_like, session_like)
}

fn option_is_allow_once_fallback(option: &model::PermissionOption) -> bool {
    let (allow_like, reject_like, persistent_like, session_like) = option_tokens(option);
    allow_like && !reject_like && !persistent_like && !session_like
}

fn option_is_allow_always_fallback(option: &model::PermissionOption) -> bool {
    let (allow_like, reject_like, persistent_like, _) = option_tokens(option);
    allow_like && !reject_like && persistent_like
}

fn option_is_allow_non_once_fallback(option: &model::PermissionOption) -> bool {
    let (allow_like, reject_like, persistent_like, session_like) = option_tokens(option);
    allow_like && !reject_like && (persistent_like || session_like)
}

fn option_is_reject_once_fallback(option: &model::PermissionOption) -> bool {
    let (allow_like, reject_like, persistent_like, _) = option_tokens(option);
    reject_like && !allow_like && !persistent_like
}

fn option_is_reject_fallback(option: &model::PermissionOption) -> bool {
    let (allow_like, reject_like, _, _) = option_tokens(option);
    reject_like && !allow_like
}

pub(super) fn focused_permission_is_plan_approval(app: &App) -> bool {
    focused_permission(app).is_some_and(|pending| {
        pending.options.iter().any(|opt| {
            matches!(opt.kind, PermissionOptionKind::PlanApprove | PermissionOptionKind::PlanReject)
        })
    })
}

fn move_permission_option_left(app: &mut App) {
    let dirty_idx = focused_interaction_dirty_idx(app);
    let mut changed = false;
    if let Some(tc) = get_focused_interaction_tc(app)
        && let Some(ref mut permission) = tc.pending_permission
    {
        let next = permission.selected_index.saturating_sub(1);
        if next != permission.selected_index {
            permission.selected_index = next;
            tc.mark_tool_call_layout_dirty();
            changed = true;
        }
    }
    invalidate_if_changed(app, dirty_idx, changed);
}

fn move_permission_option_right(app: &mut App, option_count: usize) {
    let dirty_idx = focused_interaction_dirty_idx(app);
    let mut changed = false;
    if let Some(tc) = get_focused_interaction_tc(app)
        && let Some(ref mut permission) = tc.pending_permission
        && permission.selected_index + 1 < option_count
    {
        permission.selected_index += 1;
        tc.mark_tool_call_layout_dirty();
        changed = true;
    }
    invalidate_if_changed(app, dirty_idx, changed);
}

fn handle_permission_option_keys(
    app: &mut App,
    key: KeyEvent,
    interaction_has_focus: bool,
    option_count: usize,
    plan_approval: bool,
) -> Option<bool> {
    if !interaction_has_focus {
        return None;
    }
    match key.code {
        KeyCode::Left if option_count > 0 => {
            move_permission_option_left(app);
            Some(true)
        }
        KeyCode::Right if option_count > 0 => {
            move_permission_option_right(app, option_count);
            Some(true)
        }
        KeyCode::Up if plan_approval && option_count > 0 => {
            move_permission_option_left(app);
            Some(true)
        }
        KeyCode::Down if plan_approval && option_count > 0 => {
            move_permission_option_right(app, option_count);
            Some(true)
        }
        KeyCode::Enter if option_count > 0 => {
            respond_permission(app, None);
            Some(true)
        }
        KeyCode::Esc => {
            if let Some(idx) = focused_option_index_by_kind(app, PermissionOptionKind::RejectOnce)
                .or_else(|| focused_option_index_by_kind(app, PermissionOptionKind::RejectAlways))
                .or_else(|| focused_option_index_where(app, option_is_reject_fallback))
            {
                respond_permission(app, Some(idx));
                Some(true)
            } else if option_count > 0 {
                respond_permission(app, Some(option_count - 1));
                Some(true)
            } else {
                Some(false)
            }
        }
        _ => None,
    }
}

fn handle_permission_quick_shortcuts(app: &mut App, key: KeyEvent) -> Option<bool> {
    if !matches!(key.code, KeyCode::Char(_)) {
        return None;
    }
    if focused_permission_is_plan_approval(app) {
        if is_ctrl_char_shortcut(key, 'y') {
            if let Some(idx) = focused_option_index_by_kind(app, PermissionOptionKind::PlanApprove)
            {
                respond_permission(app, Some(idx));
                return Some(true);
            }
            return Some(false);
        }
        if is_ctrl_char_shortcut(key, 'n') {
            if let Some(idx) = focused_option_index_by_kind(app, PermissionOptionKind::PlanReject) {
                respond_permission(app, Some(idx));
                return Some(true);
            }
            return Some(false);
        }
        if is_ctrl_char_shortcut(key, 'a') {
            return Some(false);
        }
        return None;
    }
    if is_ctrl_char_shortcut(key, 'y') {
        if let Some(idx) = focused_option_index_by_kind(app, PermissionOptionKind::AllowOnce)
            .or_else(|| focused_option_index_where(app, option_is_allow_once_fallback))
            .or_else(|| focused_option_index_by_kind(app, PermissionOptionKind::AllowSession))
            .or_else(|| focused_option_index_by_kind(app, PermissionOptionKind::AllowAlways))
            .or_else(|| focused_option_index_where(app, option_is_allow_always_fallback))
            .or_else(|| focused_option_index_where(app, option_is_allow_non_once_fallback))
        {
            respond_permission(app, Some(idx));
            return Some(true);
        }
        return Some(false);
    }
    if is_ctrl_char_shortcut(key, 'a') {
        if let Some(idx) = focused_option_index_by_kind(app, PermissionOptionKind::AllowSession)
            .or_else(|| focused_option_index_by_kind(app, PermissionOptionKind::AllowAlways))
            .or_else(|| focused_option_index_where(app, option_is_allow_non_once_fallback))
        {
            respond_permission(app, Some(idx));
            return Some(true);
        }
        return Some(false);
    }
    if is_ctrl_char_shortcut(key, 'n') {
        if let Some(idx) = focused_option_index_by_kind(app, PermissionOptionKind::RejectOnce)
            .or_else(|| focused_option_index_where(app, option_is_reject_once_fallback))
        {
            respond_permission(app, Some(idx));
            return Some(true);
        }
        return Some(false);
    }
    None
}

pub(super) fn handle_permission_key(
    app: &mut App,
    key: KeyEvent,
    interaction_has_focus: bool,
) -> bool {
    let option_count = focused_permission(app).map_or(0, |permission| permission.options.len());
    let plan_approval = focused_permission_is_plan_approval(app);

    if let Some(consumed) =
        handle_permission_option_keys(app, key, interaction_has_focus, option_count, plan_approval)
    {
        return consumed;
    }
    if let Some(consumed) = handle_permission_quick_shortcuts(app, key) {
        return consumed;
    }
    false
}

fn respond_permission(app: &mut App, override_index: Option<usize>) {
    let Some(tool_id) = pop_next_valid_interaction_id(app) else {
        return;
    };

    let Some((mi, bi)) = app.tool_call_index.get(&tool_id).copied() else {
        return;
    };
    let Some(MessageBlock::ToolCall(tc)) =
        app.messages.get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
    else {
        return;
    };
    let tc = tc.as_mut();
    let mut invalidated = false;
    if let Some(pending) = tc.pending_permission.take() {
        let idx = override_index.unwrap_or(pending.selected_index);
        if let Some(opt) = pending.options.get(idx) {
            tracing::debug!(
                target: crate::logging::targets::APP_PERMISSION,
                event_name = "permission_response_applied",
                message = "permission response applied",
                outcome = "success",
                tool_call_id = %tool_id,
                selected_index = idx,
                option_id = %opt.option_id,
                option_name = %opt.name,
                option_kind = ?opt.kind,
            );
            let _ = pending.response_tx.send(model::RequestPermissionResponse::new(
                model::RequestPermissionOutcome::Selected(model::SelectedPermissionOutcome::new(
                    opt.option_id.clone(),
                )),
            ));
        } else {
            tracing::warn!(
                target: crate::logging::targets::APP_PERMISSION,
                event_name = "permission_response_rejected",
                message = "permission response index was out of bounds",
                outcome = "failure",
                tool_call_id = %tool_id,
                selected_index = idx,
                option_count = pending.options.len(),
            );
        }
        tc.mark_tool_call_layout_dirty();
        invalidated = true;
    }
    if invalidated {
        app.sync_render_cache_slot(mi, bi);
        app.recompute_message_retained_bytes(mi);
        app.invalidate_layout(InvalidationLevel::MessageChanged(mi));
    }

    focus_next_inline_interaction(app);
}


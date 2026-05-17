use super::*;
use crate::app::config::{AddMarketplaceOverlayState, ConfigOverlayState};
use crate::app::dialog::DialogState;
use crate::app::slash::{SlashContext, SlashState};
use crate::app::state::types::ScrollbarDragState;
use crate::app::subagent::SubagentState;
use crate::app::{
    FocusTarget, PasteSessionState, SelectionKind, SelectionPoint, SelectionState, TodoItem,
    TodoStatus,
};

fn busy_view_test_app() -> App {
    let mut app = App::test_default();
    app.input_mut().set_text("draft");
    *app.selection_mut() = Some(SelectionState {
        kind: SelectionKind::Chat,
        start: SelectionPoint { row: 0, col: 0 },
        end: SelectionPoint { row: 0, col: 4 },
        dragging: true,
    });
    app.scrollbar_drag =
        Some(ScrollbarDragState { thumb_grab_offset: 1, track_space: 4, max_scroll: 12 });
    *app.pending_submit_mut() = Some(app.input().snapshot());
    *app.pending_paste_text_mut() = "blocked".to_owned();
    *app.pending_paste_session_mut() = Some(PasteSessionState {
        id: 1,
        start: SelectionPoint { row: 0, col: 0 },
        placeholder_index: Some(0),
    });
    *app.active_paste_session_mut() = Some(PasteSessionState {
        id: 2,
        start: SelectionPoint { row: 0, col: 0 },
        placeholder_index: Some(1),
    });
    *app.mention_mut() =
        Some(crate::app::mention::MentionState::new(0, 0, "rs".to_owned(), vec![]));
    *app.slash_mut() = Some(SlashState {
        trigger_row: 0,
        trigger_col: 0,
        query: "/co".to_owned(),
        context: SlashContext::CommandName,
        candidates: vec![],
        dialog: DialogState::default(),
    });
    *app.subagent_mut() = Some(SubagentState {
        trigger_row: 0,
        trigger_col: 0,
        query: "plan".to_owned(),
        candidates: vec![],
        dialog: DialogState::default(),
    });
    *app.todos_mut() = vec![TodoItem {
        content: "todo".to_owned(),
        status: TodoStatus::Pending,
        active_form: "todo".to_owned(),
    }];
    app.set_todo_verification_nudge(true);
    app.pending_interaction_ids_mut().push("perm-1".to_owned());
    app.claim_focus_target(FocusTarget::Permission);
    app
}

#[test]
fn set_active_view_clears_transient_chat_state_but_keeps_draft() {
    let mut app = busy_view_test_app();

    set_active_view(&mut app, ActiveView::Plugins);

    assert_eq!(app.active_view, ActiveView::Plugins);
    assert_eq!(app.input().text(), "draft");
    assert!(app.selection().is_none());
    assert!(app.scrollbar_drag.is_none());
    assert!(app.mention().is_none());
    assert!(app.slash().is_none());
    assert!(app.subagent().is_none());
    assert!(app.pending_paste_text().is_empty());
    assert!(app.pending_paste_session().is_none());
    assert!(app.active_paste_session().is_none());
    assert!(app.pending_submit().is_none());
}

#[test]
fn set_active_view_same_view_is_noop() {
    let mut app = busy_view_test_app();
    app.needs_redraw = false;

    set_active_view(&mut app, ActiveView::Chat);

    assert_eq!(app.active_view, ActiveView::Chat);
    assert!(app.selection().is_some());
    assert!(app.mention().is_some());
    assert!(!app.pending_paste_text().is_empty());
    assert!(app.pending_submit().is_some());
    assert!(!app.needs_redraw);
}

#[test]
fn set_active_view_keeps_permission_unfocused_when_returning_to_chat_with_draft() {
    let mut app = busy_view_test_app();

    set_active_view(&mut app, ActiveView::Plugins);
    assert_eq!(app.active_view, ActiveView::Plugins);

    set_active_view(&mut app, ActiveView::Chat);

    assert_eq!(app.active_view, ActiveView::Chat);
    // The test's invariant is "Permission isn't auto-claimed on
    // view-return"; previously the focus dropped to TodoList. With
    // the TodoList target retired (moved into the Inspector pane,
    // mouse-only), the fallback is Input — the surviving claim is
    // released across the view transition.
    assert_eq!(app.focus_owner(), crate::app::FocusOwner::Input);
}

#[test]
fn set_active_view_closes_help_without_clearing_question_mark_draft() {
    let mut app = App::test_default();
    app.input_mut().set_text("?");
    app.help_open = true;
    app.help_view = crate::app::HelpView::Subagents;
    app.help_visible_count = 7;

    set_active_view(&mut app, ActiveView::Plugins);
    assert_eq!(app.input().text(), "?");
    assert!(!app.is_help_active());
    assert_eq!(app.help_view, crate::app::HelpView::Keys);
    assert_eq!(app.help_visible_count, 0);

    set_active_view(&mut app, ActiveView::Chat);
    assert_eq!(app.input().text(), "?");
    assert!(!app.is_help_active());
}

#[test]
fn leaving_config_clears_config_overlay() {
    let mut app = App::test_default();
    app.active_view = ActiveView::Plugins;
    app.config.overlay = Some(ConfigOverlayState::AddMarketplace(AddMarketplaceOverlayState {
        draft: String::new(),
        cursor: 0,
    }));

    set_active_view(&mut app, ActiveView::Plugins);

    assert!(app.config.overlay.is_none());
}

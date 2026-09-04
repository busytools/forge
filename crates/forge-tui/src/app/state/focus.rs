//! Focus routing on `App`: what the autocomplete / emoji / help
//! overlays contribute to the focus context, and how directional-key
//! routing claims are claimed, released and normalized.

use super::{ActiveView, AutocompleteKind};
use crate::app::focus::{FocusContext, FocusOwner, FocusTarget};
use crate::app::mention;

impl super::App {
    /// Resolve the effective focus owner for Up/Down and other directional keys.
    pub fn focus_owner(&self) -> FocusOwner {
        self.focus.owner(self.focus_context())
    }

    pub fn active_autocomplete_kind(&self) -> Option<AutocompleteKind> {
        if self.emoji.is_some() {
            Some(AutocompleteKind::Emoji)
        } else if self.mention().is_some() {
            Some(AutocompleteKind::Mention)
        } else if self.slash().is_some() {
            Some(AutocompleteKind::Slash)
        } else if self.subagent().is_some() {
            Some(AutocompleteKind::Subagent)
        } else {
            None
        }
    }

    pub fn is_help_active(&self) -> bool {
        self.help_open
    }

    pub fn sync_help_open_with_input(&mut self) {
        if self.help_open && self.input().text().trim() != "?" {
            self.help_open = false;
            self.release_focus_target(FocusTarget::Help);
        }
    }

    pub fn autocomplete_focus_available(&self) -> bool {
        self.mention().is_some_and(mention::MentionState::has_selectable_candidates)
            || self.slash().is_some()
            || self.subagent().is_some()
    }

    /// Whether the emoji picker has rows to navigate. Separate from
    /// [`Self::autocomplete_focus_available`] because the picker is
    /// app-level and lives in the /diff view too.
    pub fn emoji_focus_available(&self) -> bool {
        self.emoji.as_ref().is_some_and(crate::app::emoji::EmojiState::has_selectable_candidates)
    }

    pub fn rebuild_chat_focus_from_state(&mut self) {
        if self.active_view != ActiveView::Chat {
            return;
        }

        self.normalize_focus_stack();

        if self.autocomplete_focus_available() {
            self.claim_focus_target(FocusTarget::Mention);
        } else {
            self.release_focus_target(FocusTarget::Mention);
        }

        if self.is_help_active() && !self.autocomplete_focus_available() {
            self.claim_focus_target(FocusTarget::Help);
        } else {
            self.release_focus_target(FocusTarget::Help);
        }

        self.normalize_focus_stack();
    }

    /// Claim key routing for a navigation target.
    /// The latest claimant wins.
    pub fn claim_focus_target(&mut self, target: FocusTarget) {
        let context = self.focus_context();
        self.focus.claim(target, context);
    }

    /// Release key routing claim for a navigation target.
    pub fn release_focus_target(&mut self, target: FocusTarget) {
        let context = self.focus_context();
        self.focus.release(target, context);
    }

    /// Drop claims that are no longer valid for current state.
    pub fn normalize_focus_stack(&mut self) {
        let context = self.focus_context();
        self.focus.normalize(context);
    }

    fn focus_context(&self) -> FocusContext {
        let mut ctx = FocusContext::empty();
        if self.autocomplete_focus_available() {
            ctx = ctx.with(FocusTarget::Mention);
        }
        if self.emoji_focus_available() {
            ctx = ctx.with(FocusTarget::Emoji);
        }
        if self.is_help_active() {
            ctx = ctx.with(FocusTarget::Help);
        }
        ctx
    }
}

#[cfg(test)]
mod tests {
    use super::super::App;
    use crate::app::focus::{FocusOwner, FocusTarget};
    use crate::app::slash::{SlashCandidate, SlashContext, SlashState};
    use crate::app::state::tests::make_test_app;
    use pretty_assertions::assert_eq;

    fn focus_test_app_with_available_targets() -> App {
        let mut app = make_test_app();
        *app.slash_mut() = Some(SlashState {
            trigger_row: 0,
            trigger_col: 0,
            query: String::new(),
            context: SlashContext::CommandName,
            candidates: vec![SlashCandidate {
                insert_value: "/config".into(),
                primary: "/config".into(),
                secondary: Some("Open settings".into()),
            }],
            dialog: crate::app::dialog::DialogState::default(),
        });
        app
    }

    #[test]
    fn focus_owner_respects_target_priority_and_release_order() {
        let mut app = focus_test_app_with_available_targets();

        assert_eq!(app.focus_owner(), FocusOwner::Input);

        app.claim_focus_target(FocusTarget::Mention);
        assert_eq!(app.focus_owner(), FocusOwner::Mention);

        app.release_focus_target(FocusTarget::Mention);
        assert_eq!(app.focus_owner(), FocusOwner::Input);
    }

    #[test]
    fn focus_owner_falls_back_to_input_when_claimed_target_is_unavailable() {
        let mut app = make_test_app();
        // Mention focus is only valid when slash/mention state is set;
        // claiming it without that state should fall back to Input.
        app.claim_focus_target(FocusTarget::Mention);
        assert_eq!(app.focus_owner(), FocusOwner::Input);
    }
}

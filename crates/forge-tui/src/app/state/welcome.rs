//! The welcome banner on `App`: building the welcome message from the
//! account / cwd / session-id snapshot and keeping the snapshot in
//! sync as those values land.

use super::{ChatMessage, MessageBlock, MessageRole};

impl super::App {
    /// Returns `(label, value)` for the welcome message's account
    /// line. The line's *layout slot* is reserved from the first
    /// frame in workspace mode - `Account: ...` shows immediately,
    /// then the value fills in once data lands. Avoids the
    /// alternative options (line pops in late, or flickers
    /// `Gateway` → `Gateway · team`) that surface as stale UI.
    ///
    /// Resolution table:
    /// - Workspace mode + both pieces → `"Account: name · tier"`.
    /// - Workspace mode + partial/no data → `"Account: ..."` skeleton.
    /// - Legacy mode (no workspace) + tier only → `"Subscription: tier"`.
    /// - Legacy mode + no data → empty (renderer hides line).
    fn welcome_account_display(&self) -> (String, String) {
        // Both accessors return owned values from the bucket; trim +
        // clone into owned form to avoid binding to temporaries.
        let display_name = self
            .active_account_display_name()
            .map(|n| n.trim().to_owned())
            .filter(|s| !s.is_empty());
        let subscription = self
            .account_info()
            .and_then(|a| a.subscription_type)
            .map(|t| t.trim().to_owned())
            .filter(|s| !s.is_empty());
        let workspace_mode = self.workspace.is_some();

        match (workspace_mode, display_name, subscription) {
            (_, Some(name), Some(tier)) => ("Account".to_owned(), format!("{name} · {tier}")),
            (true, _, _) => ("Account".to_owned(), "\u{2026}".to_owned()),
            (false, _, Some(tier)) => ("Subscription".to_owned(), tier),
            (false, _, None) => (String::new(), String::new()),
        }
    }

    fn welcome_cwd_display(&self) -> &str {
        let cwd = self.cwd().trim();
        if cwd.is_empty() { "-" } else { cwd }
    }

    fn welcome_session_id_display(&self) -> String {
        self.session_id()
            .map(|s| s.to_string())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "-".to_owned())
    }

    pub(crate) fn build_welcome_message(&self) -> ChatMessage {
        let (label, value) = self.welcome_account_display();
        let session_id = self.welcome_session_id_display();
        let mut message = ChatMessage::welcome(
            crate::FORGE_VERSION,
            &value,
            self.welcome_cwd_display(),
            &session_id,
        );
        // Override the constructor's default "Subscription" label
        // with the dynamic one chosen by `welcome_account_display`.
        if let Some(MessageBlock::Welcome(welcome)) = message.blocks.first_mut() {
            welcome.account_label = label;
        }
        message
    }

    pub(crate) fn current_welcome_tip_seed(&self) -> Option<u64> {
        let first = self.messages().first()?;
        let MessageBlock::Welcome(welcome) = first.blocks.first()? else {
            return None;
        };
        Some(welcome.tip_seed)
    }

    pub(crate) fn apply_welcome_tip_seed(message: &mut ChatMessage, tip_seed: u64) {
        let Some(MessageBlock::Welcome(welcome)) = message.blocks.first_mut() else {
            return;
        };
        welcome.tip_seed = tip_seed;
    }

    /// Update the welcome message with the latest session/account snapshot.
    pub fn sync_welcome_snapshot(&mut self) {
        // Carry the build-stamped version (with short SHA) through
        // every sync, not the bare `CARGO_PKG_VERSION`. Otherwise the
        // first sync after construction strips the SHA off the
        // welcome banner - the launchpad version line still shows
        // `+<sha>`, but the chat-view welcome reads as bare
        // `0.15.1`, which makes screenshots ambiguous about which
        // commit was running.
        let version = crate::FORGE_VERSION;
        let (label, value) = self.welcome_account_display();
        let cwd = self.welcome_cwd_display().to_owned();
        let session_id = self.welcome_session_id_display();
        let Some(first) = self.active_messages_mut().first_mut() else {
            return;
        };
        if !matches!(first.role, MessageRole::Welcome) {
            return;
        }
        let Some(MessageBlock::Welcome(welcome)) = first.blocks.first_mut() else {
            return;
        };
        if welcome.version != version
            || welcome.account_label != label
            || welcome.subscription != value
            || welcome.cwd != cwd
            || welcome.session_id != session_id
        {
            version.clone_into(&mut welcome.version);
            welcome.account_label = label;
            welcome.subscription = value;
            welcome.cwd = cwd;
            welcome.session_id = session_id;
            welcome.cache.invalidate();
            self.sync_render_cache_slot(0, 0);
            self.recompute_message_retained_bytes(0);
            self.invalidate_layout(super::LayoutInvalidation::MessagesFrom(0));
        }
    }
}

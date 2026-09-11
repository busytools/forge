//! The Slack seam on [`Workspace`]: one client per configured workspace,
//! the subscription records sessions create, and the boot verification
//! that proves each token.
//!
//! Everything here stays on `Workspace` as a second `impl` block, the
//! way [`crate::gotify`] does, so the boot path and the `mcp::slack`
//! facade keep their paths. The client itself lives in
//! `forge_connectors::slack`; this module holds the workspace state.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use forge_connectors::slack::{AuthTest, SlackApi, SlackClient, SlackHost};
use forge_primitives::slack::{SlackConfig, SlackDraft, SlackMessage, SlackSubscription};
use uuid::Uuid;

use crate::SessionKey;
use crate::workspace::Workspace;

/// One client per configured workspace, keyed by its label.
///
/// Held as [`SlackApi`] rather than the concrete client so a test can
/// substitute a double and drive a real facade against it. `Debug` is
/// hand-written because a trait object is not, and it prints only the
/// labels: a client is not a thing worth printing.
#[derive(Default)]
pub struct SlackWorkspaces {
    clients: BTreeMap<String, Arc<dyn SlackApi>>,
}

impl std::fmt::Debug for SlackWorkspaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackWorkspaces").field("labels", &self.labels()).finish()
    }
}

impl SlackWorkspaces {
    /// Labels address workspaces in the MCP tools and the Inspector, so
    /// they must be distinct and the token must be present.
    pub fn from_config(configs: &[SlackConfig], http: &reqwest::Client) -> Result<Self, String> {
        let mut clients: BTreeMap<String, Arc<dyn SlackApi>> = BTreeMap::new();
        for config in configs {
            let label = config.workspace.trim();
            if label.is_empty() {
                return Err("a [[slack]] entry has an empty workspace label".to_owned());
            }
            if config.token.trim().is_empty() {
                return Err(format!("slack workspace '{label}' has an empty token"));
            }
            if clients.contains_key(label) {
                return Err(format!("two [[slack]] entries share the label '{label}'"));
            }
            let client = Arc::new(SlackClient::new(http.clone(), config.token.clone()));
            clients.insert(label.to_owned(), client);
        }
        Ok(Self { clients })
    }

    /// A set built from caller-supplied APIs, so a test can drive a real
    /// facade against a double instead of a live workspace.
    #[cfg(test)]
    pub(crate) fn from_apis(apis: BTreeMap<String, Arc<dyn SlackApi>>) -> Self {
        Self { clients: apis }
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Every configured label, sorted (the map is ordered).
    pub fn labels(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    pub fn client(&self, label: &str) -> Option<Arc<dyn SlackApi>> {
        self.clients.get(label).cloned()
    }

    /// Prove every token at boot. A failure is reported, never fatal:
    /// one dead workspace must not stop forge from booting.
    pub async fn verify_all(&self) -> Vec<(String, Result<AuthTest, String>)> {
        let mut out = Vec::new();
        for (label, client) in &self.clients {
            let result = client.auth_test().await.map_err(|err| err.to_string());
            out.push((label.clone(), result));
        }
        out
    }
}

impl Workspace {
    /// Register a Slack subscription in the active set. Durable ones also
    /// persist to the redb store; ephemeral ad-hoc-worker ones stay in
    /// memory only and drop on restart.
    pub(crate) fn add_slack_subscription(
        &self,
        sub: forge_primitives::slack::SlackSubscription,
        durable: bool,
    ) {
        if durable
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::slack::insert(db, &sub)
        {
            tracing::warn!(
                target: "forge_workspace::slack",
                %error,
                "persisting a Slack subscription failed",
            );
        }
        self.slack_subs.lock().push(sub);
    }

    /// Hold a composed draft and hand back its id plus the receiver the
    /// caller awaits. The draft is addressed to `caller`, so its prompt
    /// surfaces in the session that will read the reply rather than in
    /// whichever session happens to be focused.
    pub(crate) fn register_slack_draft(
        &self,
        caller: &SessionKey,
        draft: SlackDraft,
    ) -> (Uuid, tokio::sync::oneshot::Receiver<bool>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let id = draft.id;
        self.slack_drafts.lock().insert(id, (caller.clone(), sender));
        let _ = self
            .update_sender()
            .send(crate::protocol::SessionUpdate::SlackPostPending { key: caller.clone(), draft });
        (id, receiver)
    }

    /// Answer a held draft, returning whether one was waiting for THIS
    /// caller. That return is also how a caller asks whether a draft is
    /// still outstanding: it is true exactly once, for the entry that was
    /// there. A draft held for another session is refused rather than
    /// answered, and a caller that has gone away is not an error.
    pub(crate) fn resolve_slack_draft(
        &self,
        id: Uuid,
        caller: &SessionKey,
        approved: bool,
    ) -> bool {
        let mut drafts = self.slack_drafts.lock();
        if !drafts.get(&id).is_some_and(|(owner, _)| owner == caller) {
            return false;
        }
        let Some((_, sender)) = drafts.remove(&id) else { return false };
        let _ = sender.send(approved);
        true
    }

    /// Watch `conversation` because a mention arrived in it, owned by
    /// whoever subscribes to mentions in that workspace - not the lead,
    /// and not whichever session happens to be active. `All` mode is
    /// deliberate: pulled into a conversation by a mention, the agent
    /// should see what is said next rather than only the next mention.
    /// Returns whether a record was added.
    pub(crate) fn auto_subscribe_slack_conversation(
        &self,
        workspace: &str,
        conversation: &str,
    ) -> bool {
        let owner = {
            let subs = self.slack_subs.lock();
            let owner = subs.iter().find(|sub| {
                sub.workspace == workspace
                    && matches!(
                        sub.target,
                        forge_primitives::slack::SlackSubscriptionTarget::Mentions
                    )
            });
            let Some(owner) = owner else { return false };
            let covered = subs.iter().any(|sub| {
                sub.workspace == workspace
                    && sub.project == owner.project
                    && sub.team_role == owner.team_role
                    && matches!(
                        &sub.target,
                        forge_primitives::slack::SlackSubscriptionTarget::Conversation { id, .. }
                            if id == conversation
                    )
            });
            if covered {
                return false;
            }
            (owner.project.clone(), owner.team_role.clone())
        };

        self.add_slack_subscription(
            forge_primitives::slack::SlackSubscription {
                id: Uuid::new_v4(),
                workspace: workspace.to_owned(),
                project: owner.0,
                team_role: owner.1,
                target: forge_primitives::slack::SlackSubscriptionTarget::Conversation {
                    id: conversation.to_owned(),
                    mode: forge_primitives::slack::SlackWatchMode::All,
                },
                created_at: std::time::SystemTime::now(),
            },
            true,
        );
        true
    }

    /// Per-workspace pump liveness, for the Inspector's SLACK section.
    /// One entry per workspace a pump has reported on.
    pub fn slack_connected_workspaces(&self) -> std::collections::BTreeMap<String, bool> {
        self.slack_connected.lock().clone()
    }

    /// Every Slack subscription owned in `project`, whichever session
    /// owns it. Backs the pump, which needs the whole set.
    pub fn slack_subscriptions_for_project(
        &self,
        project: &str,
    ) -> Vec<forge_primitives::slack::SlackSubscription> {
        self.slack_subs.lock().iter().filter(|s| s.project == project).cloned().collect()
    }

    /// Remove the subscription `id` in `project` only when its owner
    /// matches `owner` (`None` = a lead subscription, `Some(label)` =
    /// that worker's), from both the active set and the redb store.
    /// Returns whether an entry was removed. Backs the owner-scoped
    /// `slack__unsubscribe` so a caller removes only what it subscribed.
    pub(crate) fn remove_slack_subscription_owned_by(
        &self,
        project: &str,
        id: Uuid,
        owner: Option<&str>,
    ) -> bool {
        let removed = {
            let mut subs = self.slack_subs.lock();
            let before = subs.len();
            subs.retain(|s| {
                !(s.id == id && s.project == project && s.team_role.as_deref() == owner)
            });
            subs.len() != before
        };
        if removed
            && let Some(db) = self.db.lock().as_ref()
            && let Err(error) = crate::store::slack::remove(db, id)
        {
            tracing::warn!(
                target: "forge_workspace::slack",
                %error,
                "removing a persisted Slack subscription failed",
            );
        }
        removed
    }

    /// Prove each workspace's token once, logging the team it belongs to
    /// or the failure. Idempotent, and a no-op with no `[[slack]]` entry.
    pub fn start_slack_verification(self: &Arc<Self>) {
        if self.slack.is_empty() {
            return;
        }
        if self.slack_verification_started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let Some(ws) = weak.upgrade() else { return };
            for (label, result) in ws.slack.verify_all().await {
                match result {
                    Ok(auth) => {
                        // Kept because the pump needs the id to recognise
                        // `<@U...>` mentions, and `auth.test` is the only
                        // call that reports it.
                        ws.slack_user_ids.lock().insert(label.clone(), auth.user_id.clone());
                        tracing::info!(
                            target: "forge_workspace::slack",
                            workspace = %label,
                            team = %auth.team,
                            user = %auth.user,
                            "slack workspace verified",
                        );
                    }
                    Err(error) => tracing::warn!(
                        target: "forge_workspace::slack",
                        workspace = %label,
                        %error,
                        "slack auth.test failed; this workspace stays dormant",
                    ),
                }
            }
        });
    }

    /// Start one pump per configured workspace that has at least one
    /// subscription. Idempotent per workspace, and a no-op with no
    /// `[[slack]]` entry.
    pub fn start_slack_subsystem(self: &Arc<Self>) {
        if self.slack.is_empty() {
            return;
        }
        for label in self.slack.labels() {
            if !self.slack_subs.lock().iter().any(|sub| sub.workspace == label) {
                continue;
            }
            let mut guard = self.slack_subsystem.lock();
            if guard.contains_key(&label) {
                continue;
            }
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            guard.insert(label.clone(), shutdown_tx);
            drop(guard);

            let poll_seconds = self
                .config
                .slack
                .iter()
                .find(|config| config.workspace.trim() == label.as_str())
                .map_or(30, |config| config.poll_seconds);
            let host: Arc<dyn SlackHost> = Arc::new(SlackSubsystemHost::new(self));
            tokio::spawn(forge_connectors::slack::run_workspace_pump(
                host,
                label,
                poll_seconds,
                shutdown_rx,
            ));
        }
    }

    /// Stop every Slack pump once no subscription remains, and mark each
    /// workspace disconnected. No-op while any subscription is active.
    pub fn stop_slack_subsystem_if_idle(&self) {
        if !self.slack_subs.lock().is_empty() {
            return;
        }
        let handles: Vec<_> =
            std::mem::take(&mut *self.slack_subsystem.lock()).into_values().collect();
        for handle in handles {
            let _ = handle.send(());
        }
        for connected in self.slack_connected.lock().values_mut() {
            *connected = false;
        }
    }
}

/// The [`SlackHost`] one pump drives: a `Weak<Workspace>` behind the port,
/// so the workspace can drop while a pump runs and every port call then
/// degrades to a no-op.
pub(crate) struct SlackSubsystemHost(std::sync::Weak<Workspace>);

impl SlackSubsystemHost {
    pub(crate) fn new(workspace: &Arc<Workspace>) -> Self {
        Self(Arc::downgrade(workspace))
    }
}

impl SlackHost for SlackSubsystemHost {
    fn client(&self, workspace: &str, timeout: Duration) -> Result<SlackClient, String> {
        let ws = self.0.upgrade().ok_or("the workspace is gone")?;
        let config = ws
            .config
            .slack
            .iter()
            .find(|config| config.workspace.trim() == workspace)
            .ok_or_else(|| format!("no [[slack]] entry named '{workspace}'"))?;
        let http =
            forge_agent::http_trust::with_extra_roots(reqwest::Client::builder().timeout(timeout))
                .build()
                .map_err(|error| error.to_string())?;
        Ok(SlackClient::new(http, config.token.clone()))
    }

    fn user_id(&self, workspace: &str) -> Option<String> {
        self.0.upgrade()?.slack_user_ids.lock().get(workspace).cloned()
    }

    fn subscriptions(&self, workspace: &str) -> Vec<SlackSubscription> {
        let Some(ws) = self.0.upgrade() else { return Vec::new() };
        ws.slack_subs.lock().iter().filter(|sub| sub.workspace == workspace).cloned().collect()
    }

    fn watermark(&self, workspace: &str, conversation: &str) -> Option<String> {
        let ws = self.0.upgrade()?;
        let db = ws.db.lock();
        let db = db.as_ref()?;
        crate::store::slack::watermark(db, workspace, conversation).unwrap_or_else(|error| {
            tracing::warn!(
                target: "forge_workspace::slack",
                %error,
                "reading a Slack watermark failed; treating it as unset",
            );
            None
        })
    }

    fn set_watermark(&self, workspace: &str, conversation: &str, ts: &str) {
        let Some(ws) = self.0.upgrade() else { return };
        let db = ws.db.lock();
        let Some(db) = db.as_ref() else { return };
        if let Err(error) = crate::store::slack::set_watermark(db, workspace, conversation, ts) {
            tracing::warn!(
                target: "forge_workspace::slack",
                %error,
                "writing a Slack watermark failed",
            );
        }
    }

    fn set_connected(&self, workspace: &str, connected: bool) {
        let Some(ws) = self.0.upgrade() else { return };
        ws.slack_connected.lock().insert(workspace.to_owned(), connected);
    }

    fn auto_subscribe(&self, workspace: &str, message: &SlackMessage) -> bool {
        let Some(ws) = self.0.upgrade() else { return false };
        ws.auto_subscribe_slack_conversation(workspace, &message.conversation)
    }

    fn deliver(&self, subscription: &SlackSubscription, message: &SlackMessage) {
        let Some(ws) = self.0.upgrade() else { return };
        if let Err(err) = ws.dispatch(crate::protocol::Command::DeliverSlackMessage {
            project: subscription.project.clone(),
            team_role: subscription.team_role.clone(),
            message: message.clone(),
        }) {
            tracing::warn!(
                target: "forge_workspace::slack",
                project = %subscription.project,
                error = ?err,
                "slack DeliverSlackMessage dispatch failed",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::slack::{SlackSubscription, SlackSubscriptionTarget, SlackWatchMode};
    use uuid::Uuid;

    fn cfg(workspace: &str, token: &str) -> SlackConfig {
        SlackConfig { workspace: workspace.to_owned(), token: token.to_owned(), poll_seconds: 30 }
    }

    fn sub_for(project: &str, team_role: Option<&str>) -> SlackSubscription {
        SlackSubscription {
            id: Uuid::new_v4(),
            workspace: "acme".to_owned(),
            project: project.to_owned(),
            team_role: team_role.map(str::to_owned),
            target: SlackSubscriptionTarget::DirectMessages,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// A workspace whose `[[slack]]` holds exactly `label`. The tempdir
    /// is returned so it outlives the workspace it names.
    fn workspace_with_one_slack_workspace(
        label: &str,
    ) -> (
        Arc<Workspace>,
        tempfile::TempDir,
        tokio::sync::mpsc::UnboundedReceiver<crate::protocol::SessionUpdate>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = crate::config::LoadedConfig::empty_for_test();
        config.slack = vec![cfg(label, "xoxp-test")];
        let (ws, rx) = Workspace::testing_stub_with_config(dir.path().to_path_buf(), config);
        (ws, dir, rx)
    }

    fn draft(workspace: &str, conversation: &str) -> SlackDraft {
        SlackDraft {
            id: Uuid::new_v4(),
            workspace: workspace.to_owned(),
            conversation: conversation.to_owned(),
            thread_ts: None,
            text: "hello".to_owned(),
        }
    }

    /// A worker's draft must not surface in the lead's dock prompt: the
    /// approval is answered by whoever will read the reply, and the agent
    /// waiting on the oneshot is the one that asked.
    #[test]
    fn a_draft_is_addressed_to_the_session_that_asked() {
        let (ws, _dir, mut rx) = workspace_with_one_slack_workspace("acme");
        let caller = SessionKey::from_session_id("worker-uuid");
        let (_id, _decision) = ws.register_slack_draft(&caller, draft("acme", "C1"));

        let mut addressed = Vec::new();
        while let Ok(update) = rx.try_recv() {
            if let crate::protocol::SessionUpdate::SlackPostPending { key, .. } = update {
                addressed.push(key);
            }
        }
        assert_eq!(
            addressed,
            vec![caller],
            "the draft is addressed to the worker that asked, never the lead",
        );
    }

    #[test]
    fn answering_a_draft_that_is_not_pending_is_refused() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        let caller = SessionKey::from_session_id("caller-uuid");
        assert!(!ws.resolve_slack_draft(Uuid::new_v4(), &caller, true));
    }

    #[test]
    fn answering_a_draft_removes_it() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        let caller = SessionKey::from_session_id("caller-uuid");
        let (id, _decision) = ws.register_slack_draft(&caller, draft("acme", "C1"));

        assert!(ws.resolve_slack_draft(id, &caller, true), "the draft was waiting");
        assert!(!ws.resolve_slack_draft(id, &caller, true), "and is gone once answered");
    }

    /// An answer is only applied by the session the draft was addressed
    /// to; another session naming the id is refused, and the draft stays
    /// waiting for its owner.
    #[test]
    fn another_session_cannot_answer_a_draft_it_was_not_addressed() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        let asker = SessionKey::from_session_id("worker-uuid");
        let other = SessionKey::from_session_id("lead-uuid");
        let (id, _decision) = ws.register_slack_draft(&asker, draft("acme", "C1"));

        assert!(
            !ws.resolve_slack_draft(id, &other, true),
            "an answer from another session must be refused",
        );
        assert!(
            ws.resolve_slack_draft(id, &asker, true),
            "and the draft is still waiting for the session that asked",
        );
    }

    /// `tokio::test`: starting a subsystem spawns a pump, which needs a
    /// runtime.
    #[tokio::test]
    async fn the_subsystem_does_not_start_without_subscriptions() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        ws.start_slack_subsystem();
        assert!(
            ws.slack_subsystem.lock().is_empty(),
            "a pump with nothing to watch is a task that only burns a timer",
        );
    }

    #[tokio::test]
    async fn the_subsystem_starts_once_a_subscription_exists() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        ws.add_slack_subscription(sub_for("acme", None), true);

        ws.start_slack_subsystem();
        assert_eq!(ws.slack_subsystem.lock().len(), 1, "one pump for the one configured workspace");

        let id = ws.slack_subscriptions_for_project("acme")[0].id;
        ws.remove_slack_subscription_owned_by("acme", id, None);
        ws.stop_slack_subsystem_if_idle();
        assert!(ws.slack_subsystem.lock().is_empty(), "the pump stops with its last subscription");
    }

    fn sub_mentions_for(project: &str, team_role: Option<&str>) -> SlackSubscription {
        let mut sub = sub_for(project, team_role);
        sub.target = SlackSubscriptionTarget::Mentions;
        sub
    }

    #[test]
    fn the_auto_subscription_lands_on_the_mention_subscriptions_owner() {
        // Not on the lead, and not on whichever session happens to be
        // active: the conversation belongs to whoever asked to be told.
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        ws.add_slack_subscription(sub_mentions_for("forge", Some("tester")), true);

        ws.auto_subscribe_slack_conversation("acme", "C1");

        let added = ws.slack_subscriptions_for_project("forge");
        assert!(
            added.iter().any(|sub| sub.team_role.as_deref() == Some("tester")
                && sub.target
                    == SlackSubscriptionTarget::Conversation {
                        id: "C1".to_owned(),
                        mode: SlackWatchMode::All,
                    }),
            "the conversation lands on the mention subscription's owner: {added:?}",
        );
    }

    fn sub_for_conversation(project: &str, team_role: Option<&str>, id: &str) -> SlackSubscription {
        let mut sub = sub_for(project, team_role);
        sub.target =
            SlackSubscriptionTarget::Conversation { id: id.to_owned(), mode: SlackWatchMode::All };
        sub
    }

    #[test]
    fn a_conversation_already_watched_is_not_auto_subscribed_again() {
        let (ws, _dir, _rx) = workspace_with_one_slack_workspace("acme");
        ws.add_slack_subscription(sub_mentions_for("forge", None), true);
        ws.add_slack_subscription(sub_for_conversation("forge", None, "C1"), true);

        assert!(
            !ws.auto_subscribe_slack_conversation("acme", "C1"),
            "a conversation that owner already watches adds nothing",
        );
        assert_eq!(ws.slack_subscriptions_for_project("forge").len(), 2, "and no third record");
    }

    #[test]
    fn a_subscription_is_scoped_to_its_project_and_owner() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.add_slack_subscription(sub_for("forge", None), true);
        ws.add_slack_subscription(sub_for("forge", Some("tester")), true);
        ws.add_slack_subscription(sub_for("other", None), true);

        let visible = ws.slack_subscriptions_for_project("forge");
        assert_eq!(visible.len(), 2, "another project's subscription must not leak in");
        assert!(visible.iter().any(|s| s.team_role.is_none()));
        assert!(visible.iter().any(|s| s.team_role.as_deref() == Some("tester")));
    }

    #[test]
    fn a_worker_cannot_remove_another_owners_subscription() {
        let (ws, _rx) = Workspace::testing_stub();
        let lead = sub_for("forge", None);
        ws.add_slack_subscription(lead.clone(), true);

        assert!(
            !ws.remove_slack_subscription_owned_by("forge", lead.id, Some("tester")),
            "a worker must not remove the lead's subscription",
        );
        assert_eq!(
            ws.slack_subscriptions_for_project("forge").len(),
            1,
            "a refused removal removes nothing",
        );
        assert!(ws.remove_slack_subscription_owned_by("forge", lead.id, None));
        assert!(ws.slack_subscriptions_for_project("forge").is_empty());
    }

    #[test]
    fn duplicate_workspace_labels_are_refused() {
        let configs = vec![cfg("acme", "xoxp-test"), cfg("acme", "xoxp-other")];
        let err = SlackWorkspaces::from_config(&configs, &reqwest::Client::new())
            .expect_err("two workspaces sharing a label cannot be addressed apart");
        assert!(err.contains("acme"), "the error has to name the shared label, got: {err}");
    }

    #[test]
    fn an_empty_token_is_refused() {
        let configs = vec![cfg("acme", "")];
        let err = SlackWorkspaces::from_config(&configs, &reqwest::Client::new())
            .expect_err("a blank token would fail on every call");
        assert!(
            err.contains("acme") && err.contains("empty token"),
            "the error has to name the workspace and the missing credential, got: {err}",
        );
    }

    #[test]
    fn an_empty_workspace_label_is_refused() {
        let configs = vec![cfg("", "xoxp-test")];
        let err = SlackWorkspaces::from_config(&configs, &reqwest::Client::new())
            .expect_err("a workspace with no label cannot be addressed at all");
        assert!(
            err.contains("empty workspace label"),
            "the error has to name the label, got: {err}",
        );
    }
}

//! `SettingsFacade` - the seam between the settings MCP tools and the
//! config machinery. The production impl ([`ProdSettingsFacade`]) reads the
//! document the settings page draws and routes every write through the
//! workspace's held-write registry; the mock returns preloaded results so the
//! tool tests can assert argument handling and error surfacing without a
//! config.

use std::sync::{Arc, Weak};

use forge_primitives::settings::{
    SettingsEditOp, SettingsWriteAsk, SettingsWriteChange, SettingsWriteEnding,
};
use forge_primitives::{EffectClass, SettingsEditRequest};
use serde_json::Value;
use uuid::Uuid;

use crate::SessionSlot;
use crate::workspace::Workspace;

/// Why a settings read or write did not happen, in the words the tool hands
/// the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettingsError {
    /// The path is no key this config resolves.
    UnknownKey(String),
    /// The key belongs to a project the caller is not in; reads refuse it by
    /// name rather than returning another project's setting.
    ForeignProject { key: String, org: String, project: String },
    /// The key is one of another org's own keys, refused on the same terms.
    ForeignOrg { key: String, org: String, caller_org: String },
    /// The catalogue refused the value, the key cannot be unset, or the
    /// write would change nothing.
    Wanted(String),
    /// No client is attached to answer the approval prompt.
    NoReader,
    /// The approval prompt closed without an answer.
    Abandoned,
    /// The reader approved but the write did not land.
    Refused(String),
}

impl SettingsError {
    /// The sentence a caller reads.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::UnknownKey(key) => format!(
                "`{key}` is not a key in this config; call settings__index to see the \
                 catalogue keys and the instances this config declares"
            ),
            Self::ForeignProject { key, org, project } => format!(
                "`{key}` belongs to project {org}/{project}. Ask that project's lead through \
                 the peer channel - agents__send_message to org \"{org}\", project \
                 \"{project}\", label \"lead\" - or, if you have no peer channel, report the \
                 need to your reader instead"
            ),
            Self::ForeignOrg { key, org, caller_org } => format!(
                "`{key}` belongs to org {org}; you are in {caller_org}. Ask one of that org's \
                 project leads through the peer channel - agents__send_message to org \"{org}\" \
                 and one of its projects, label \"lead\" - or, if you have no peer channel, \
                 report the need to your reader instead"
            ),
            Self::Wanted(message) => message.clone(),
            Self::NoReader => "no client is attached to approve a configuration write, so \
                               nothing was written; the write needs the reader at a client"
                .to_owned(),
            Self::Abandoned => {
                "the approval prompt closed without an answer; nothing was written".to_owned()
            }
            Self::Refused(message) => {
                format!("the write was approved but did not land: {message}")
            }
        }
    }
}

/// One key's effective value, with what a caller needs to act on it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SettingsValue {
    pub key: String,
    pub pattern: String,
    pub effect: EffectClass,
    pub value: Value,
}

/// One key and its new value, as `settings__set` hands them through.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub(crate) struct SettingsSetItem {
    pub key: String,
    pub value: Value,
}

/// What a write did, which is what the agent's result says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettingsWriteOutcome {
    /// The reader approved and the write landed. Each key's effect class is
    /// carried once, in the order the batch first names one.
    Applied { effects: Vec<EffectClass> },
    /// The reader declined; nothing was written.
    Declined,
}

/// The settings tools' view of the workspace.
#[async_trait::async_trait]
pub(crate) trait SettingsFacade: Send + Sync {
    /// The compact catalogue: keys, types, state and effect class, no values.
    async fn index(&self, caller: &SessionSlot) -> Result<String, SettingsError>;

    /// One key's effective value. The caller's own project's keys and the
    /// global keys read freely; another project's key is refused by name.
    async fn get(&self, caller: &SessionSlot, key: &str) -> Result<SettingsValue, SettingsError>;

    /// Set one or more keys, applied as one validated save and held for the
    /// reader's approval.
    async fn set(
        &self,
        caller: &SessionSlot,
        items: Vec<SettingsSetItem>,
    ) -> Result<SettingsWriteOutcome, SettingsError>;

    /// Clear one key, held for the reader's approval.
    async fn unset(
        &self,
        caller: &SessionSlot,
        key: &str,
    ) -> Result<SettingsWriteOutcome, SettingsError>;
}

/// Settles the held write as abandoned if the awaiting future is dropped
/// before the write is settled - the calling session died mid-wait, or its
/// connection went. The registry's own teardown sweep covers a session that
/// goes without running this drop; this covers a cancelled call on a live
/// session. Settling twice is a no-op in the registry.
struct ResolveOnDrop {
    workspace: Arc<Workspace>,
    id: Uuid,
    caller: SessionSlot,
}

impl Drop for ResolveOnDrop {
    fn drop(&mut self) {
        self.workspace.settle_settings_write(self.id, &self.caller, SettingsWriteEnding::Abandoned);
    }
}

pub(crate) struct ProdSettingsFacade {
    workspace: Weak<Workspace>,
}

impl ProdSettingsFacade {
    pub(crate) fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn SettingsFacade> {
        Arc::new(Self { workspace: Arc::downgrade(workspace) })
    }

    fn workspace(&self) -> Result<Arc<Workspace>, SettingsError> {
        self.workspace.upgrade().ok_or_else(|| {
            SettingsError::Refused("the forge server is no longer running".to_owned())
        })
    }

    /// One write, from the catalogue check through the reader's decision to
    /// the validated writer. **The whole batch is checked before anything is
    /// asked**: an unknown key, a wrong type, or a batch the loader would
    /// refuse - a section whose keys are mutually required, say - is refused
    /// by name with no prompt minted.
    async fn write(
        &self,
        caller: &SessionSlot,
        items: Vec<SettingsSetItem>,
        unset: bool,
    ) -> Result<SettingsWriteOutcome, SettingsError> {
        if items.is_empty() {
            return Err(SettingsError::Wanted(
                "a write needs at least one edit; nothing was written".to_owned(),
            ));
        }
        let ws = self.workspace()?;
        let config = ws.config_arc();
        let document = ws.settings_snapshot();

        let mut edits = Vec::with_capacity(items.len());
        let mut changes = Vec::with_capacity(items.len());
        let mut effects: Vec<EffectClass> = Vec::new();
        for item in &items {
            let target = crate::settings::resolve_target(&config, &document, &item.key)
                .ok_or_else(|| SettingsError::UnknownKey(item.key.clone()))?;
            let proposed = if unset { Value::Null } else { item.value.clone() };
            if unset {
                if target.edit.required {
                    return Err(SettingsError::Wanted(format!(
                        "`{}` is required and cannot be unset",
                        item.key
                    )));
                }
                if target.current.is_null() {
                    return Err(SettingsError::Wanted(format!(
                        "`{}` is already unset; nothing was written",
                        item.key
                    )));
                }
            } else {
                if proposed.is_null() {
                    return Err(SettingsError::Wanted(format!(
                        "`{}` is cleared with settings__unset, not with a null value",
                        item.key
                    )));
                }
                crate::settings_write::validate(&proposed, &target.edit, &item.key)
                    .map_err(|error| SettingsError::Wanted(error.message))?;
            }
            edits.push(SettingsEditRequest {
                area: target.area.clone(),
                key_path: item.key.clone(),
                pattern: target.pattern.clone(),
                op: SettingsEditOp::Set,
                value: (!unset).then(|| item.value.clone()),
            });
            changes.push(SettingsWriteChange {
                key: item.key.clone(),
                current: target.current.clone(),
                proposed,
                unset,
            });
            if !effects.contains(&target.class) {
                effects.push(target.class);
            }
        }

        // A save that changes nothing is not worth a prompt.
        if changes.iter().all(|change| change.current == change.proposed) {
            let message = if changes.len() == 1 {
                format!("`{}` already holds that value; nothing was written", changes[0].key)
            } else {
                "every key in this write already holds its value; nothing was written".to_owned()
            };
            return Err(SettingsError::Wanted(message));
        }

        // **The loader validates the batch, not each key.** It is the only
        // authority on cross-key rules, and its refusal is owed before the
        // reader is shown a prompt for a save that could not land.
        let fingerprint = document.fingerprint.clone();
        ws.validate_settings(&fingerprint, &edits).map_err(|error| match error {
            crate::workspace::SettingsSaveError::Stale => SettingsError::Wanted(
                "the configuration changed since it was read; call again to write against \
                 the current config"
                    .to_owned(),
            ),
            crate::workspace::SettingsSaveError::Invalid { message, .. }
            | crate::workspace::SettingsSaveError::Refused { message, .. } => {
                SettingsError::Wanted(message)
            }
            crate::workspace::SettingsSaveError::Io(message) => SettingsError::Refused(message),
        })?;

        let ask = SettingsWriteAsk { id: Uuid::new_v4(), edits: changes };
        let (id, decision, answerable) = ws.register_settings_write(caller, ask);
        if !answerable {
            return Err(SettingsError::NoReader);
        }
        // The guard outlives the await: dropping the future (a cancelled
        // tool call, a session that died) settles the registry as abandoned
        // rather than leaving the prompt to be answered after its asker is
        // gone.
        let _guard = ResolveOnDrop { workspace: Arc::clone(&ws), id, caller: caller.clone() };
        let Ok(approved) = decision.await else {
            ws.settle_settings_write(id, caller, SettingsWriteEnding::Abandoned);
            return Err(SettingsError::Abandoned);
        };
        if !approved {
            ws.settle_settings_write(id, caller, SettingsWriteEnding::Declined);
            return Ok(SettingsWriteOutcome::Declined);
        }
        // **The ending is emitted after the write, not after the click.** A
        // view that did not answer must not read "applied" for an edit the
        // writer then refused, so the outcome crosses with the ending.
        match ws.save_settings(&fingerprint, &edits) {
            Ok(()) => {
                ws.settle_settings_write(id, caller, SettingsWriteEnding::Applied);
                Ok(SettingsWriteOutcome::Applied { effects })
            }
            Err(error) => {
                let message = match error {
                    crate::workspace::SettingsSaveError::Stale => {
                        "the configuration changed since this write was shown; nothing was \
                         written. Call again to write against the current config"
                            .to_owned()
                    }
                    other => other.to_string(),
                };
                ws.settle_settings_write(
                    id,
                    caller,
                    SettingsWriteEnding::Refused { message: message.clone() },
                );
                Err(SettingsError::Refused(message))
            }
        }
    }
}

#[async_trait::async_trait]
impl SettingsFacade for ProdSettingsFacade {
    async fn index(&self, caller: &SessionSlot) -> Result<String, SettingsError> {
        let ws = self.workspace()?;
        let document = ws.settings_snapshot();
        Ok(crate::settings::index_text(&ws.config_arc(), &document, caller.org(), caller.project()))
    }

    async fn get(&self, caller: &SessionSlot, key: &str) -> Result<SettingsValue, SettingsError> {
        let ws = self.workspace()?;
        let config = ws.config_arc();
        if let Some(owner) = crate::settings::owning_project(&config, key)
            && (owner.org != caller.org() || owner.name != caller.project())
        {
            return Err(SettingsError::ForeignProject {
                key: key.to_owned(),
                org: owner.org.clone(),
                project: owner.name.clone(),
            });
        }
        if let Some(owner_org) = crate::settings::owning_org(&config, key)
            && owner_org != caller.org()
        {
            return Err(SettingsError::ForeignOrg {
                key: key.to_owned(),
                org: owner_org.to_owned(),
                caller_org: caller.org().to_owned(),
            });
        }
        let document = ws.settings_snapshot();
        let target = crate::settings::resolve_target(&config, &document, key)
            .ok_or_else(|| SettingsError::UnknownKey(key.to_owned()))?;
        Ok(SettingsValue {
            key: target.path,
            pattern: target.pattern,
            effect: target.class,
            value: target.current,
        })
    }

    async fn set(
        &self,
        caller: &SessionSlot,
        items: Vec<SettingsSetItem>,
    ) -> Result<SettingsWriteOutcome, SettingsError> {
        self.write(caller, items, false).await
    }

    async fn unset(
        &self,
        caller: &SessionSlot,
        key: &str,
    ) -> Result<SettingsWriteOutcome, SettingsError> {
        self.write(caller, vec![SettingsSetItem { key: key.to_owned(), value: Value::Null }], true)
            .await
    }
}

/// Preloaded answers for the tool tests; nothing here touches a config.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockSettingsFacade {
    pub index_text: parking_lot::Mutex<String>,
    pub values: parking_lot::Mutex<std::collections::HashMap<String, Value>>,
    pub missing: parking_lot::Mutex<Option<SettingsError>>,
    pub set_calls: parking_lot::Mutex<Vec<(SessionSlot, Vec<SettingsSetItem>)>>,
    pub unset_calls: parking_lot::Mutex<Vec<(SessionSlot, String)>>,
    pub write_result: parking_lot::Mutex<Option<Result<SettingsWriteOutcome, SettingsError>>>,
}

#[cfg(test)]
impl MockSettingsFacade {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn into_arc(self) -> Arc<dyn SettingsFacade> {
        Arc::new(self)
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl SettingsFacade for MockSettingsFacade {
    async fn index(&self, _caller: &SessionSlot) -> Result<String, SettingsError> {
        Ok(self.index_text.lock().clone())
    }

    async fn get(&self, _caller: &SessionSlot, key: &str) -> Result<SettingsValue, SettingsError> {
        if let Some(error) = self.missing.lock().clone() {
            return Err(error);
        }
        let value = self
            .values
            .lock()
            .get(key)
            .cloned()
            .ok_or_else(|| SettingsError::UnknownKey(key.to_owned()))?;
        Ok(SettingsValue {
            key: key.to_owned(),
            pattern: key.to_owned(),
            effect: EffectClass::Live,
            value,
        })
    }

    async fn set(
        &self,
        caller: &SessionSlot,
        items: Vec<SettingsSetItem>,
    ) -> Result<SettingsWriteOutcome, SettingsError> {
        self.set_calls.lock().push((caller.clone(), items));
        self.write_result.lock().clone().unwrap_or(Ok(SettingsWriteOutcome::Declined))
    }

    async fn unset(
        &self,
        caller: &SessionSlot,
        key: &str,
    ) -> Result<SettingsWriteOutcome, SettingsError> {
        self.unset_calls.lock().push((caller.clone(), key.to_owned()));
        self.write_result.lock().clone().unwrap_or(Ok(SettingsWriteOutcome::Declined))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::SessionSlot;

    const CONFIG: &str = r#"
[[orgs]]
name = "Default"
accounts = ["Stargate"]

[[orgs.projects]]
name = "forge"
path = "/tmp/forge"
max_workers = 2

[[orgs.projects]]
name = "web"
path = "/tmp/web"

[[orgs]]
name = "Other"
accounts = ["Stargate"]

[[orgs.projects]]
name = "elsewhere"
path = "/tmp/elsewhere"

[[accounts]]
display_name = "Stargate"
token = "t"
models = ["claude-sonnet-5"]
provider = "anthropic"
"#;

    fn fixture() -> (Arc<Workspace>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_dir = crate::config::ensure_forge_data_dir(dir.path()).expect("forge/");
        fs::write(config_dir.join("forge.toml"), CONFIG).expect("write forge.toml");
        let workspace = Arc::new(Workspace::new_for_test(dir.path().to_path_buf()).expect("new"));
        (workspace, dir)
    }

    fn caller() -> SessionSlot {
        SessionSlot::lead("Default", "forge")
    }

    fn item(key: &str, value: serde_json::Value) -> SettingsSetItem {
        SettingsSetItem { key: key.to_owned(), value }
    }

    fn facade(ws: &Arc<Workspace>) -> Arc<dyn SettingsFacade> {
        ProdSettingsFacade::from_arc(ws)
    }

    async fn wait_for_write(ws: &Arc<Workspace>) -> Uuid {
        for _ in 0..400 {
            if let Some(id) = ws.settings_writes.lock().keys().next().copied() {
                return id;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("no configuration write was parked");
    }

    fn read_config(ws: &Workspace) -> String {
        fs::read_to_string(ws.config_path()).expect("read forge.toml")
    }

    /// The index carries the catalogue and no value, read through the tool's
    /// own facade.
    #[tokio::test]
    async fn the_index_carries_the_catalogue_and_no_value() {
        let (ws, _dir) = fixture();

        let text = facade(&ws).index(&caller()).await.expect("the index reads");

        assert!(text.contains("server.port"), "{text}");
        assert!(text.contains("Default/forge"), "{text}");
        assert!(!text.contains("Stargate\", \"t\""), "a value leaked: {text}");
    }

    /// The caller's own project's keys and the global keys read freely, a
    /// worker sees the same own-project view its lead does, and another
    /// project's key is refused by name with the owning project and the
    /// request path.
    #[tokio::test]
    async fn get_reads_the_callers_own_project_and_the_global_keys() {
        let (ws, _dir) = fixture();
        let facade = facade(&ws);

        let global = facade.get(&caller(), "server.port").await.expect("a global key reads");
        assert_eq!(global.pattern, "server.port");

        let own = facade
            .get(&caller(), "orgs.Default.projects.forge.max_workers")
            .await
            .expect("the caller's own project key reads");
        assert_eq!(own.value, json!(2));

        let worker = SessionSlot::worker("Default", "forge", "settings-worker");
        let same = facade
            .get(&worker, "orgs.Default.projects.forge.max_workers")
            .await
            .expect("a worker reads its own project");
        assert_eq!(same, own, "a lead and a worker share the own-project view");

        let error = facade
            .get(&caller(), "orgs.Default.projects.web.max_workers")
            .await
            .expect_err("another project's key is refused");
        let message = error.message();
        assert!(message.contains("orgs.Default.projects.web.max_workers"), "{message}");
        assert!(message.contains("Default/web"), "the owner is named: {message}");
        assert!(message.contains("agents__send_message"), "the request path is named: {message}");
        assert!(message.contains("reader"), "the fallback is named: {message}");
    }

    /// A set parks as a pending ask, the file is unchanged until the reader
    /// approves, and the approval writes through the validated rename writer.
    #[tokio::test]
    async fn an_approved_set_applies_and_a_declined_one_writes_nothing() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);
        let key = "orgs.Default.projects.forge.max_workers";

        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move { facade.set(&caller(), vec![item(key, json!(4))]).await }
        });
        let id = wait_for_write(&ws).await;

        assert_eq!(read_config(&ws), CONFIG, "nothing is written while the write waits");
        let held = ws.pending_asks(&caller());
        assert!(
            held.iter().any(|ask| matches!(
                ask,
                crate::protocol::PendingAsk::SettingsWrite(held)
                    if held.id == id
                        && held.edits.len() == 1
                        && held.edits[0].current == json!(2)
                        && held.edits[0].proposed == json!(4)
            )),
            "the record read carries the key and both values",
        );

        assert!(ws.resolve_settings_write(id, &caller(), true));
        let outcome = task.await.expect("no panic").expect("the write lands");
        assert_eq!(
            outcome,
            SettingsWriteOutcome::Applied {
                effects: vec![forge_primitives::EffectClass::NextSession],
            },
        );
        assert!(read_config(&ws).contains("max_workers = 4"), "the value landed");
        ws.reload_config().expect("the written file loads");
        assert_eq!(ws.config_arc().projects[0].max_workers, Some(4));

        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move { facade.set(&caller(), vec![item(key, json!(7))]).await }
        });
        let id = wait_for_write(&ws).await;
        assert!(ws.resolve_settings_write(id, &caller(), false));
        let outcome = task.await.expect("no panic").expect("a decline is an outcome");
        assert_eq!(outcome, SettingsWriteOutcome::Declined);
        assert!(read_config(&ws).contains("max_workers = 4"), "a decline writes nothing");
    }

    /// A section the config does not have emits no row either: the global
    /// keys `settings__index` lists are readable before the section exists.
    #[tokio::test]
    async fn an_unset_global_section_key_reads_null() {
        let (ws, _dir) = fixture();
        let facade = facade(&ws);

        let gotify = facade.get(&caller(), "gotify.url").await.expect("gotify.url reads");
        assert_eq!(gotify.value, json!(null));
        assert_eq!(gotify.pattern, "gotify.url");

        let systemone =
            facade.get(&caller(), "systemone.enabled").await.expect("systemone.enabled reads");
        assert_eq!(systemone.value, json!(null));
    }

    /// An optional key the config does not set reads as unset and can be
    /// enabled through the same held write, which is what the index promises
    /// by listing it.
    #[tokio::test]
    async fn an_unset_optional_key_reads_null_and_can_be_set() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);
        let key = "orgs.Default.projects.forge.handoff.enabled";

        let read = facade.get(&caller(), key).await.expect("an unset optional key reads");
        assert_eq!(read.value, json!(null), "the key is addressable before it is set");
        assert_eq!(read.pattern, "orgs[].projects[].handoff.enabled");

        // The real watcher, not a hand-called reload: the save is one the
        // server's watch has to see and apply, exactly as the page's save is.
        let (changed, mut edits) = tokio::sync::mpsc::unbounded_channel();
        let _watch = crate::env::config_watch::watch(ws.config_path(), changed).expect("the watch");
        let watching = tokio::spawn({
            let ws = Arc::clone(&ws);
            async move {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    tokio::select! {
                        Some(()) = edits.recv() => {
                            let _ = ws.reload_config();
                        }
                        () = tokio::time::sleep_until(deadline) => return false,
                    }
                    if ws.config_arc().projects[0].handoff.as_ref().and_then(|h| h.enabled)
                        == Some(true)
                    {
                        return true;
                    }
                }
            }
        });

        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move { facade.set(&caller(), vec![item(key, json!(true))]).await }
        });
        let id = wait_for_write(&ws).await;
        assert!(ws.resolve_settings_write(id, &caller(), true));
        let outcome = task.await.expect("no panic").expect("the write lands");
        assert!(matches!(outcome, SettingsWriteOutcome::Applied { .. }));
        assert!(read_config(&ws).contains("enabled = true"), "the key was created");
        assert!(watching.await.expect("no panic"), "the watcher applied the written config");
    }

    /// **Every write is held, another project's included.** Reads refuse a
    /// foreign project's key, but a write is not the read: it parks with the
    /// key the reader sees, and only their approval lands it.
    #[tokio::test]
    async fn a_write_to_another_projects_key_is_held_too() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);
        let key = "orgs.Default.projects.web.max_workers";

        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move { facade.set(&caller(), vec![item(key, json!(5))]).await }
        });
        let id = wait_for_write(&ws).await;
        let held = ws.pending_asks(&caller());
        assert!(
            held.iter().any(|ask| matches!(
                ask,
                crate::protocol::PendingAsk::SettingsWrite(held)
                    if held.id == id
                        && held.edits.iter().any(|edit| edit.key == key && edit.proposed == json!(5))
            )),
            "the write parks with the foreign key the reader has to see",
        );

        assert!(ws.resolve_settings_write(id, &caller(), true));
        let outcome = task.await.expect("no panic").expect("the write lands");
        assert!(matches!(outcome, SettingsWriteOutcome::Applied { .. }));
        assert!(read_config(&ws).contains("max_workers = 5"));
    }

    /// **An org's own keys belong to that org.** They are not a project's,
    /// but they shape every project in it, so another org's are refused by
    /// name with the same request path a project's key gets.
    #[tokio::test]
    async fn another_orgs_org_level_key_is_refused_by_name() {
        let (ws, _dir) = fixture();

        let error = facade(&ws)
            .get(&caller(), "orgs.Other.browser_profile")
            .await
            .expect_err("another org's key is refused");

        let message = error.message();
        assert!(message.contains("orgs.Other.browser_profile"), "{message}");
        assert!(message.contains("belongs to org Other"), "{message}");
        assert!(message.contains("agents__send_message"), "{message}");
        assert!(message.contains("reader"), "{message}");
    }

    /// A cancelled call settles the registry as abandoned and says so, so a
    /// dock elsewhere stands down rather than waiting on a caller that is
    /// gone. A killed session whose control task outlives its disconnect is
    /// the teardown sweep's, tested in `settings_ask`.
    #[tokio::test]
    async fn a_cancelled_call_settles_the_write_as_abandoned() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let mut updates = ws.subscribe_mirror();
        let facade = facade(&ws);
        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move {
                facade
                    .set(&caller(), vec![item("orgs.Default.projects.forge.max_workers", json!(5))])
                    .await
            }
        });
        let _id = wait_for_write(&ws).await;

        task.abort();
        let _ = task.await;
        for _ in 0..200 {
            if ws.settings_writes.lock().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(ws.settings_writes.lock().is_empty(), "the cancelled call left nothing parked");
        let mut endings = Vec::new();
        while let Ok(frame) = updates.try_recv() {
            if let crate::protocol::SessionUpdate::SettingsWriteResolved { ending, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                endings.push(ending);
            }
        }
        assert_eq!(endings, vec![SettingsWriteEnding::Abandoned]);
    }

    /// An unknown key and a wrong type are refused naming the key before any
    /// ask is minted.
    #[tokio::test]
    async fn an_unknown_key_or_a_wrong_type_is_refused_before_the_ask() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);

        let unknown = facade
            .set(&caller(), vec![item("orgs.Default.projects.forge.nope", json!(1))])
            .await
            .expect_err("an unknown key is refused");
        assert!(unknown.message().contains("orgs.Default.projects.forge.nope"));
        assert!(ws.settings_writes.lock().is_empty(), "no ask was minted");

        let wrong = facade
            .set(&caller(), vec![item("orgs.Default.projects.forge.max_workers", json!("four"))])
            .await
            .expect_err("a wrong type is refused");
        assert!(wrong.message().contains("max_workers"), "{}", wrong.message());
        assert!(wrong.message().contains("whole number"), "{}", wrong.message());
        assert!(ws.settings_writes.lock().is_empty(), "no ask was minted");
    }

    /// A required key cannot be unset, and an optional one can: the clear
    /// goes through the same held writer as a set.
    #[tokio::test]
    async fn unset_refuses_a_required_key_and_clears_an_optional_one() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);

        let required = facade
            .unset(&caller(), "orgs.Default.projects.forge.path")
            .await
            .expect_err("a required key cannot be unset");
        assert!(required.message().contains("required"), "{}", required.message());

        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move { facade.unset(&caller(), "orgs.Default.projects.forge.max_workers").await }
        });
        let id = wait_for_write(&ws).await;
        assert!(ws.resolve_settings_write(id, &caller(), true));
        let outcome = task.await.expect("no panic").expect("the clear lands");
        assert!(matches!(outcome, SettingsWriteOutcome::Applied { .. }));
        assert!(
            !read_config(&ws).contains("max_workers"),
            "the key left the file: {}",
            read_config(&ws),
        );
    }

    /// No client to answer: the write fails closed and nothing is written.
    #[tokio::test]
    async fn a_write_with_no_reader_fails_closed() {
        let (ws, _dir) = fixture();

        let error = facade(&ws)
            .set(&caller(), vec![item("orgs.Default.projects.forge.max_workers", json!(4))])
            .await
            .expect_err("no reader means no write");

        assert_eq!(error, SettingsError::NoReader);
        assert!(ws.settings_writes.lock().is_empty());
        assert_eq!(read_config(&ws), CONFIG, "nothing was written");
    }

    /// A write shown against one file does not land on a file that moved
    /// underneath it: the approval is refused whole rather than applied to a
    /// config the reader never saw.
    #[tokio::test]
    async fn a_write_shown_against_a_moved_file_is_refused() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);

        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move {
                facade
                    .set(&caller(), vec![item("orgs.Default.projects.forge.max_workers", json!(4))])
                    .await
            }
        });
        let id = wait_for_write(&ws).await;
        fs::write(ws.config_path(), format!("{CONFIG}\n# moved\n")).expect("move the file");
        assert!(ws.resolve_settings_write(id, &caller(), true));

        let error = task.await.expect("no panic").expect_err("a stale write is refused");
        assert!(matches!(error, SettingsError::Refused(_)), "{error:?}");
        assert!(!read_config(&ws).contains("max_workers = 4"), "nothing was written");
    }

    /// **The acceptance case: a section whose keys are mutually required is
    /// created in one held, validated write.** `[vault]` needs both its keys,
    /// so neither can land alone - the pair rides one save, one ask and one
    /// approval.
    #[tokio::test]
    async fn a_vault_pair_creates_the_section_in_one_write() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);

        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move {
                facade
                    .set(
                        &caller(),
                        vec![
                            item("vault.url", json!("https://vault.test")),
                            item("vault.email", json!("me@vault.test")),
                        ],
                    )
                    .await
            }
        });
        let id = wait_for_write(&ws).await;

        assert_eq!(read_config(&ws), CONFIG, "nothing is written while the pair waits");
        let held = ws.pending_asks(&caller());
        let ask = held
            .iter()
            .find_map(|ask| match ask {
                crate::protocol::PendingAsk::SettingsWrite(held) if held.id == id => {
                    Some(held.clone())
                }
                _ => None,
            })
            .expect("the pair parked one write");
        assert_eq!(ask.edits.len(), 2, "one card carries both keys");
        assert_eq!(ask.edits[0].key, "vault.url");
        assert_eq!(ask.edits[0].current, json!(null));
        assert_eq!(ask.edits[0].proposed, json!("https://vault.test"));
        assert_eq!(ask.edits[1].key, "vault.email");
        assert_eq!(ask.edits[1].proposed, json!("me@vault.test"));

        assert!(ws.resolve_settings_write(id, &caller(), true));
        let outcome = task.await.expect("no panic").expect("the pair lands");
        assert!(
            matches!(
                outcome,
                SettingsWriteOutcome::Applied { ref effects }
                    if effects.as_slice() == [forge_primitives::EffectClass::Live]
            ),
            "{outcome:?}",
        );

        let written = read_config(&ws);
        assert!(written.contains("[vault]"), "the section was created: {written}");
        assert!(written.contains("url = \"https://vault.test\""), "{written}");
        assert!(written.contains("email = \"me@vault.test\""), "{written}");
        ws.reload_config().expect("the written file loads");
        let vault = ws.config_arc().vault.clone().expect("the section is live");
        assert_eq!(vault.url, "https://vault.test");
        assert_eq!(vault.email, "me@vault.test");
    }

    /// A half pair is refused before the reader is asked: the loader's own
    /// words name the missing key, and no dock prompt is minted for a write
    /// that could never land.
    #[tokio::test]
    async fn a_partial_pair_is_refused_before_the_ask() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            facade.set(&caller(), vec![item("vault.url", json!("https://vault.test"))]),
        )
        .await
        .expect("the refusal precedes the ask, so nothing waits on an approval")
        .expect_err("a half [vault] is refused");

        let message = error.message();
        assert!(message.contains("vault"), "the section is named: {message}");
        assert!(message.contains("email"), "the missing key is named: {message}");
        assert!(ws.settings_writes.lock().is_empty(), "no ask was minted");
        assert_eq!(read_config(&ws), CONFIG, "nothing was written");
    }

    /// **One bad key refuses the whole batch**: the good pair beside it is
    /// not written, and nothing is asked.
    #[tokio::test]
    async fn one_bad_key_refuses_the_whole_batch() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            facade.set(
                &caller(),
                vec![
                    item("vault.url", json!("https://vault.test")),
                    item("vault.email", json!("me@vault.test")),
                    item("orgs.Default.projects.forge.max_workers", json!("four")),
                ],
            ),
        )
        .await
        .expect("the refusal precedes the ask, so nothing waits on an approval")
        .expect_err("a wrong-typed member refuses the batch");

        assert!(error.message().contains("max_workers"), "{}", error.message());
        assert!(ws.settings_writes.lock().is_empty(), "no ask was minted");
        assert_eq!(read_config(&ws), CONFIG, "the good pair was not written");
    }

    /// **A batch naming one key twice is refused before the ask.** The pair
    /// that would net back to the value already in the file must not slip
    /// past the "changes nothing" guard as a write the reader approves.
    #[tokio::test]
    async fn a_batch_naming_one_key_twice_is_refused_before_the_ask() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);
        let key = "orgs.Default.projects.forge.max_workers";

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            facade.set(&caller(), vec![item(key, json!(4)), item(key, json!(2))]),
        )
        .await
        .expect("the refusal precedes the ask, so nothing waits on an approval")
        .expect_err("a duplicate key is refused");

        assert!(error.message().contains("max_workers"), "{}", error.message());
        assert!(error.message().contains("twice"), "{}", error.message());
        assert!(ws.settings_writes.lock().is_empty(), "no ask was minted");
        assert_eq!(read_config(&ws), CONFIG, "nothing was written");
    }

    /// The hold is the whole batch's: a decline writes neither key.
    #[tokio::test]
    async fn a_declined_batch_writes_nothing() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);

        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move {
                facade
                    .set(
                        &caller(),
                        vec![
                            item("vault.url", json!("https://vault.test")),
                            item("vault.email", json!("me@vault.test")),
                        ],
                    )
                    .await
            }
        });
        let id = wait_for_write(&ws).await;
        assert!(ws.resolve_settings_write(id, &caller(), false));

        let outcome = task.await.expect("no panic").expect("a decline is an outcome");
        assert_eq!(outcome, SettingsWriteOutcome::Declined);
        assert_eq!(read_config(&ws), CONFIG, "a decline writes nothing");
        assert!(ws.config_arc().vault.is_none(), "the section stayed absent");
    }
}

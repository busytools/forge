//! `SettingsFacade` - the seam between the settings MCP tools and the
//! config machinery. The production impl ([`ProdSettingsFacade`]) reads the
//! document the settings page draws and routes every write through the
//! workspace's held-write registry; the mock returns preloaded results so the
//! tool tests can assert argument handling and error surfacing without a
//! config.

use std::sync::{Arc, Weak};

use forge_primitives::settings::{SettingsEditOp, SettingsWriteAsk, SettingsWriteEnding};
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

/// What a write did, which is what the agent's result says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsWriteOutcome {
    /// The reader approved and the write landed.
    Applied { effect: EffectClass },
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

    /// Set one key, held for the reader's approval.
    async fn set(
        &self,
        caller: &SessionSlot,
        key: &str,
        value: Value,
    ) -> Result<SettingsWriteOutcome, SettingsError>;

    /// Clear one key, held for the reader's approval.
    async fn unset(
        &self,
        caller: &SessionSlot,
        key: &str,
    ) -> Result<SettingsWriteOutcome, SettingsError>;
}

/// Resolves the held write if the awaiting future is dropped before the
/// reader answers - the calling session died mid-wait, or its connection
/// went. Without this the registry entry and its oneshot sender outlive
/// everyone who could answer them.
struct ResolveOnDrop {
    workspace: Arc<Workspace>,
    id: Uuid,
    caller: SessionSlot,
}

impl Drop for ResolveOnDrop {
    fn drop(&mut self) {
        self.workspace.resolve_settings_write(
            self.id,
            &self.caller,
            SettingsWriteEnding::Abandoned,
        );
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
    /// the validated writer. Every step that can refuse without a decision
    /// refuses before the prompt is minted.
    async fn write(
        &self,
        caller: &SessionSlot,
        key: &str,
        value: Option<Value>,
    ) -> Result<SettingsWriteOutcome, SettingsError> {
        let ws = self.workspace()?;
        let document = ws.settings_snapshot();
        let target = crate::settings::target(&document, key)
            .ok_or_else(|| SettingsError::UnknownKey(key.to_owned()))?;
        let unset = value.is_none();
        let proposed = value.clone().unwrap_or(Value::Null);
        if unset {
            if target.edit.required {
                return Err(SettingsError::Wanted(format!(
                    "`{key}` is required and cannot be unset"
                )));
            }
            if target.current.is_null() {
                return Err(SettingsError::Wanted(format!(
                    "`{key}` is already unset; nothing was written"
                )));
            }
        } else {
            if proposed.is_null() {
                return Err(SettingsError::Wanted(format!(
                    "`{key}` is cleared with settings__unset, not with a null value"
                )));
            }
            crate::settings_write::validate(&proposed, &target.edit, key)
                .map_err(|error| SettingsError::Wanted(error.message))?;
            if target.current == proposed {
                return Err(SettingsError::Wanted(format!(
                    "`{key}` already holds that value; nothing was written"
                )));
            }
        }

        let edit = SettingsEditRequest {
            area: target.area.clone(),
            key_path: key.to_owned(),
            pattern: target.pattern.clone(),
            op: SettingsEditOp::Set,
            value: value.clone(),
        };
        let ask = SettingsWriteAsk {
            id: Uuid::new_v4(),
            key: key.to_owned(),
            current: target.current.clone(),
            proposed: proposed.clone(),
            unset,
        };
        let fingerprint = document.fingerprint.clone();
        let (id, decision, answerable) = ws.register_settings_write(caller, ask);
        if !answerable {
            return Err(SettingsError::NoReader);
        }
        // The guard outlives the await: dropping the future (a cancelled
        // tool call, a session that died) clears the registry and says the
        // write was abandoned rather than leaving the prompt to be answered
        // after its asker is gone.
        let _guard = ResolveOnDrop { workspace: Arc::clone(&ws), id, caller: caller.clone() };
        match decision.await {
            Ok(true) => {}
            Ok(false) => return Ok(SettingsWriteOutcome::Declined),
            Err(_) => return Err(SettingsError::Abandoned),
        }
        ws.save_settings(&fingerprint, std::slice::from_ref(&edit))
            .map_err(|error| SettingsError::Refused(error.to_string()))?;
        Ok(SettingsWriteOutcome::Applied { effect: target.class })
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
        let document = ws.settings_snapshot();
        let target = crate::settings::target(&document, key)
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
        key: &str,
        value: Value,
    ) -> Result<SettingsWriteOutcome, SettingsError> {
        self.write(caller, key, Some(value)).await
    }

    async fn unset(
        &self,
        caller: &SessionSlot,
        key: &str,
    ) -> Result<SettingsWriteOutcome, SettingsError> {
        self.write(caller, key, None).await
    }
}

/// Preloaded answers for the tool tests; nothing here touches a config.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockSettingsFacade {
    pub index_text: parking_lot::Mutex<String>,
    pub values: parking_lot::Mutex<std::collections::HashMap<String, Value>>,
    pub missing: parking_lot::Mutex<Option<SettingsError>>,
    pub set_calls: parking_lot::Mutex<Vec<(SessionSlot, String, Value)>>,
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
        key: &str,
        value: Value,
    ) -> Result<SettingsWriteOutcome, SettingsError> {
        self.set_calls.lock().push((caller.clone(), key.to_owned(), value));
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
            async move { facade.set(&caller(), key, json!(4)).await }
        });
        let id = wait_for_write(&ws).await;

        assert_eq!(read_config(&ws), CONFIG, "nothing is written while the write waits");
        let held = ws.pending_asks(&caller());
        assert!(
            held.iter().any(|ask| matches!(
                ask,
                crate::protocol::PendingAsk::SettingsWrite(held)
                    if held.id == id && held.current == json!(2) && held.proposed == json!(4)
            )),
            "the record read carries the key and both values",
        );

        assert!(ws.resolve_settings_write(
            id,
            &caller(),
            SettingsWriteEnding::Answered { approved: true },
        ));
        let outcome = task.await.expect("no panic").expect("the write lands");
        assert_eq!(
            outcome,
            SettingsWriteOutcome::Applied { effect: forge_primitives::EffectClass::NextSession },
        );
        assert!(read_config(&ws).contains("max_workers = 4"), "the value landed");
        ws.reload_config().expect("the written file loads");
        assert_eq!(ws.config_arc().projects[0].max_workers, Some(4));

        let task = tokio::spawn({
            let facade = Arc::clone(&facade);
            async move { facade.set(&caller(), key, json!(7)).await }
        });
        let id = wait_for_write(&ws).await;
        assert!(ws.resolve_settings_write(
            id,
            &caller(),
            SettingsWriteEnding::Answered { approved: false },
        ));
        let outcome = task.await.expect("no panic").expect("a decline is an outcome");
        assert_eq!(outcome, SettingsWriteOutcome::Declined);
        assert!(read_config(&ws).contains("max_workers = 4"), "a decline writes nothing");
    }

    /// An unknown key and a wrong type are refused naming the key before any
    /// ask is minted.
    #[tokio::test]
    async fn an_unknown_key_or_a_wrong_type_is_refused_before_the_ask() {
        let (ws, _dir) = fixture();
        let _answerer = ws.subscribe_answerer();
        let facade = facade(&ws);

        let unknown = facade
            .set(&caller(), "orgs.Default.projects.forge.nope", json!(1))
            .await
            .expect_err("an unknown key is refused");
        assert!(unknown.message().contains("orgs.Default.projects.forge.nope"));
        assert!(ws.settings_writes.lock().is_empty(), "no ask was minted");

        let wrong = facade
            .set(&caller(), "orgs.Default.projects.forge.max_workers", json!("four"))
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
        assert!(ws.resolve_settings_write(
            id,
            &caller(),
            SettingsWriteEnding::Answered { approved: true },
        ));
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
            .set(&caller(), "orgs.Default.projects.forge.max_workers", json!(4))
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
                facade.set(&caller(), "orgs.Default.projects.forge.max_workers", json!(4)).await
            }
        });
        let id = wait_for_write(&ws).await;
        fs::write(ws.config_path(), format!("{CONFIG}\n# moved\n")).expect("move the file");
        assert!(ws.resolve_settings_write(
            id,
            &caller(),
            SettingsWriteEnding::Answered { approved: true },
        ));

        let error = task.await.expect("no panic").expect_err("a stale write is refused");
        assert!(matches!(error, SettingsError::Refused(_)), "{error:?}");
        assert!(!read_config(&ws).contains("max_workers = 4"), "nothing was written");
    }
}

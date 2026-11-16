use std::collections::HashMap;

use forge_primitives as types;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLaunchSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_progress_summaries: Option<bool>,
    /// Charter appended to the spawned `claude` subprocess's system
    /// prompt via `--append-system-prompt`. Set for worker spawns (the
    /// LLM-supplied inline persona, via `handle_spawn_worker`) and for
    /// lead spawns (the lead charter, via `apply_lead_charter`); `None`
    /// only when the caller supplies none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charter: Option<String>,
    /// Lead-session delegation preamble: when `Some`, appended to the
    /// system prompt after the forge MCP trust block so a Lead session
    /// knows how to spawn and drive workers. Built by the workspace for
    /// Lead spawns only; `None` for workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_preamble: Option<String>,
    /// Spawn-path-specific extra CLI args to thread through to the
    /// `claude` subprocess via `OptionsBuilder::extra_arg`. Each pair
    /// becomes `--<flag> <value>` (or `--<flag>` when the value is
    /// `None`). Workers in git-repo projects populate
    /// `("worktree", Some(label))` so claude forks a worktree at
    /// `<repo>/.claude/worktrees/<label>/`; lead/resume paths leave
    /// this empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<(String, Option<String>)>,
    /// Boot-wave fresh-start routing flag (`--new`). When `true`, the
    /// project-target spawn arms skip resuming the catalog lead session
    /// and start a fresh one, and the lead cascades it to its workers.
    /// Pure workspace-side routing - `#[serde(skip)]` keeps it out of
    /// every serialized form, so it never reaches the `settings` JSON
    /// sent to `claude`. Defaults `false`; only the boot dispatch sets it.
    #[serde(skip)]
    pub force_new: bool,
    /// The first turn a lead receives when this spawn carries a stored
    /// id to resume. The boot wave sets it so a restarted lead continues
    /// without a human prompt; the spawn delivers it only then, so a
    /// first boot or `--new` sends nothing. A resume the CLI refuses
    /// still takes the note, whose wording does not depend on history.
    /// Pure workspace-side routing - `#[serde(skip)]` keeps it out of
    /// every serialized form.
    #[serde(skip)]
    pub resume_kick: Option<String>,
    /// Per-spawn env overrides layered over the account env this
    /// session spawns under. A present key replaces, an absent key
    /// keeps, and a key mapped to an empty string removes the variable.
    /// Empty by default, which is exactly the previous behaviour: this
    /// is how per-spawn env reaches a respawned child without mutating
    /// the previous generation's Agent.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_overrides: HashMap<String, String>,
    /// The engine this launch runs on, when a `forge.toml` key asked for
    /// one. Absent means `default` - the `claude` CLI - so a launch
    /// settings that predates the key spawns exactly as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<EngineSetting>,
}

impl SessionLaunchSettings {
    /// The `permissions.defaultMode` keys inside `settings`, shared by
    /// the workspace's account stamp and the readers here so a rename
    /// cannot split the seam. The `settings` JSON is forge-internal -
    /// the CLI is driven by argv - so one const per key is the whole
    /// contract.
    pub const PERMISSIONS_KEY: &str = "permissions";
    pub const PERMISSIONS_DEFAULT_MODE_KEY: &str = "defaultMode";
}

/// The binary the `forge` engine names on PATH.
///
/// Portable name, not a path: the certified artifact is installed on each
/// machine like the `claude` CLI, and a synced `forge.toml` can name it
/// without knowing where that machine keeps it.
pub const FORGE_ENGINE_BINARY: &str = "claude-headless";

/// The engine a session's subprocess runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// The `claude` CLI on PATH - forge's own behavior.
    #[default]
    Default,
    /// The certified `claude-headless` port.
    Forge,
}

impl Engine {
    /// Every value the `engine` keys accept, quoted for load errors.
    pub const ACCEPTED: &'static str = "\"default\", \"forge\"";

    /// Every engine, so the settings page's choices and this type cannot
    /// drift apart.
    pub const ALL: &'static [Self] = &[Self::Default, Self::Forge];

    /// The stored spelling, which is also the settings page's choice.
    pub const fn as_stored(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Forge => "forge",
        }
    }

    /// Parse a stored value. A typo is a load error at the key, never a
    /// quiet fallback to `claude`.
    pub fn from_stored(value: &str) -> Option<Self> {
        match value {
            "default" => Some(Self::Default),
            "forge" => Some(Self::Forge),
            _ => None,
        }
    }

    /// The binary this engine launches; `None` keeps the SDK's own
    /// default, `claude`.
    pub fn binary(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::Forge => Some(FORGE_ENGINE_BINARY),
        }
    }
}

/// The `forge.toml` scope whose `engine` key chose an engine, kept so a
/// missing binary can name the key a person has to fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EngineScope {
    /// No key chose it: the `claude` CLI default.
    Default,
    /// The org's own `engine` key.
    Org { org: String },
    /// The project's own `engine` key.
    Project { org: String, project: String },
}

impl EngineScope {
    /// The key that carries the setting.
    pub fn setting(&self) -> String {
        match self {
            Self::Default => "engine".to_owned(),
            Self::Org { org } => format!("orgs.{org}.engine"),
            Self::Project { org, project } => format!("orgs.{org}.projects.{project}.engine"),
        }
    }

    /// The scope in the reader's words.
    pub fn label(&self) -> String {
        match self {
            Self::Default => "the default".to_owned(),
            Self::Org { org } => format!("org '{org}'"),
            Self::Project { org, project } => format!("project '{project}' of org '{org}'"),
        }
    }
}

/// The engine a session launches on, with the scope that chose it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSetting {
    pub engine: Engine,
    pub scope: EngineScope,
}

impl EngineSetting {
    /// The `claude` CLI: what an unset - or explicitly `default` -
    /// config resolves to.
    pub fn cli() -> Self {
        Self { engine: Engine::Default, scope: EngineScope::Default }
    }

    /// The project's own key's value, when the project set one; `None`
    /// when the value came from the org or the default, which is what the
    /// settings page's project row shows.
    pub fn project_own(&self) -> Option<Engine> {
        match self.scope {
            EngineScope::Project { .. } => Some(self.engine),
            EngineScope::Default | EngineScope::Org { .. } => None,
        }
    }

    /// The failure sentence a missing `forge` binary owes the reader: the
    /// binary, the setting that asked for it, and no fallback.
    pub fn missing_binary(&self) -> String {
        format!(
            "engine '{}' needs the '{}' binary on PATH, and this machine has none; it was \
             chosen by {} ({}). Install the certified port or set that key back to 'default'.",
            self.engine.as_stored(),
            FORGE_ENGINE_BINARY,
            self.scope.label(),
            self.scope.setting(),
        )
    }
}

/// The class of a spawn failure, when the SDK could name one.
///
/// A consumer that would otherwise search the rendered message for a
/// word can read this instead: `Error::CwdNotFound` renders the path it
/// could not enter, and for a worker that path runs through
/// `.claude/worktrees/<label>`, so the word "worktree" appears in a
/// failure that has nothing to do with creating one.
// No `Default`: `Unclassified` is the value that hands classification
// back to the message heuristics, so a site that omits the kind by
// accident should not compile into the behaviour this exists to replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnFailureKind {
    /// The bridge could not attribute the failure - a child that exited,
    /// a closed stream, a refusal. Message heuristics stay in charge.
    Unclassified,
    /// `forge_sdk::Error::CwdNotFound`: the subprocess had no working
    /// directory to start in.
    WorkingDirMissing,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Connected {
        session_id: String,
        cwd: String,
        current_model: types::CurrentModel,
        available_models: Vec<types::AvailableModel>,
        mode: Option<types::ModeState>,
        history_updates: Option<Vec<types::Message>>,
        /// Compactions the resumed transcript records, seeding the
        /// per-session count. `0` for a fresh session.
        compaction_count: u32,
    },
    /// A spawn failure the bridge could name, carried beside the message
    /// so a consumer reads the variant rather than searching rendered
    /// prose for a word that a path can also contain.
    ConnectionFailed {
        message: String,
        kind: SpawnFailureKind,
    },
    /// A user-facing turn never reached the CLI (or was never
    /// acknowledged): a view committed to `Thinking` before the
    /// dispatch, so without this event the spinner never tears down.
    TurnError {
        session_id: String,
        message: String,
        /// The class of the failure, derived from `message` where the
        /// event is built so no reader has to search the prose again.
        class: forge_primitives::TurnErrorClass,
    },
    PermissionRequest {
        session_id: String,
        request: types::PermissionRequest,
    },
    QuestionRequest {
        session_id: String,
        request: types::QuestionRequest,
    },
    /// The CLI answered a `cancel_async_message`. `cancelled: false` means the
    /// prompt was already dequeued for execution - it is on its way to the
    /// model, and a reader has to say so rather than pretend it was dropped.
    PromptCancelResolved {
        uuid: String,
        cancelled: bool,
    },
    McpOperationError {
        session_id: String,
        error: types::McpOperationError,
    },
    /// The CLI refused (or failed) a `set_permission_mode` control
    /// request. `message` carries the underlying error text so the
    /// consumer can roll back the optimistic mode change and surface
    /// the reason.
    SetModeFailed {
        session_id: String,
        mode: forge_primitives::PermissionMode,
        message: String,
    },
    /// The CLI refused (or failed) a `set_model` control request.
    /// `message` carries the underlying error text so the consumer can
    /// roll back the optimistic model change and surface the reason.
    SetModelFailed {
        session_id: String,
        model: String,
        message: String,
    },
    ContextUsage {
        session_id: String,
        /// The share the CLI itself reports, against its effective window.
        percentage: Option<u8>,
        /// Raw model context-window size in tokens (e.g. 200_000 for
        /// Sonnet's default cap, 1_000_000 for the 1M-context variant).
        /// `None` when the upstream probe hasn't reported it yet.
        /// Sourced from `ContextUsageResponse.raw_max_tokens`.
        max_tokens: Option<u64>,
        /// The tokens the model actually read on its last request.
        total_tokens: Option<u64>,
        /// The effective window after any lowered auto-compact window,
        /// which is the denominator a fill trigger has to use.
        effective_max_tokens: Option<u64>,
    },
    McpSnapshot {
        session_id: String,
        servers: Vec<forge_primitives::McpServerStatus>,
        error: Option<String>,
    },
    /// Raw `forge_primitives::Message` envelope from the underlying
    /// SDK Client, forwarded to the consumer (e.g. a view's app)
    /// for per-variant dispatch and state mutation.
    SdkMessage {
        session_id: String,
        msg: forge_primitives::Message,
    },
    /// Observation of CLI runtime state captured from a hook input
    /// payload as it passes through the SDK's hook-callback dispatch.
    /// Hooks fire on every tool use, prompt submit, etc., so this is
    /// a high-fidelity signal compared to the lower-frequency
    /// `system/status` events. Fields are `Option` because the CLI
    /// only populates the relevant subset on each hook event (e.g.
    /// `agent_id` / `agent_type` are absent for main-agent tool calls).
    HookObservation {
        /// Session id the hook fired in.
        session_id: String,
        /// `tool_use_id` when the hook event is tool-lifecycle scoped.
        /// `None` for events that aren't bound to a specific tool call
        /// (`UserPromptSubmit`, `Stop`, etc.).
        tool_use_id: Option<String>,
        /// Permission mode active at the moment the hook fired. Wire
        /// value as a string (e.g. `"acceptEdits"`, `"plan"`); the
        /// consumer types it with `forge_primitives::PermissionMode`.
        permission_mode: Option<String>,
        /// Effort level active at the moment the hook fired (CLI
        /// 2.1.133+, absent for older CLIs). Wire value as a string
        /// (`"low"` / `"medium"` / `"high"` / `"xhigh"` / `"max"`);
        /// the consumer maps to `forge_primitives::EffortLevel`.
        effort: Option<String>,
        /// Sub-agent identifier when the hook fired inside a
        /// `Task`-spawned worker. Matches the `agent_id` from the
        /// subagent's `SubagentStart` / `SubagentStop` hooks.
        agent_id: Option<String>,
        /// Sub-agent type name (e.g. `"general-purpose"`,
        /// `"code-reviewer"`).
        agent_type: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::SessionLaunchSettings;

    /// Every engine round-trips through its stored spelling, so a variant
    /// added without a value fails here rather than at a config load.
    #[test]
    fn every_engine_round_trips_through_its_stored_spelling() {
        for engine in super::Engine::ALL {
            assert_eq!(
                super::Engine::from_stored(engine.as_stored()),
                Some(*engine),
                "{} must parse back to itself",
                engine.as_stored(),
            );
        }
        assert_eq!(super::Engine::from_stored("claude"), None, "an unknown value is a load error");
    }

    /// The engine rides the launch settings only when a key asked for one,
    /// and its scope survives the JSON the agent channel carries.
    #[test]
    fn an_engine_rides_the_launch_settings_with_its_scope() {
        let settings = SessionLaunchSettings {
            engine: Some(super::EngineSetting {
                engine: super::Engine::Forge,
                scope: super::EngineScope::Project {
                    org: "Busytools".to_owned(),
                    project: "forge".to_owned(),
                },
            }),
            ..SessionLaunchSettings::default()
        };
        let json = serde_json::to_value(&settings).expect("serialize");
        assert_eq!(
            json.get("engine"),
            Some(&serde_json::json!({
                "engine": "forge",
                "scope": { "kind": "project", "org": "Busytools", "project": "forge" },
            })),
            "the engine and the key that chose it cross together",
        );
        let back: SessionLaunchSettings = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, settings, "a round trip keeps the setting");

        let unset = serde_json::to_value(SessionLaunchSettings::default()).expect("serialize");
        assert_eq!(unset.get("engine"), None, "an unset engine stays out of the wire");
    }

    #[test]
    fn session_launch_settings_serializes_agent_progress_summaries() {
        let settings = SessionLaunchSettings {
            settings: Some(serde_json::json!({ "model": "haiku" })),
            agent_progress_summaries: Some(true),
            ..SessionLaunchSettings::default()
        };

        let json = serde_json::to_value(&settings).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "settings": { "model": "haiku" },
                "agent_progress_summaries": true
            })
        );
    }
}

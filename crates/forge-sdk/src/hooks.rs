//! Hook callbacks — 10 hook kinds dispatched by opaque `callback_id`.
//!
//! Mirrors Python SDK's `HookMatcher` / `HookContext` machinery. Callbacks
//! are registered at initialize time; the CLI emits `hook_callback`
//! `control_request`s with an opaque `callback_id` (minted by the SDK) plus
//! an `input` payload whose `hook_event_name` discriminates concrete types.

use std::marker::PhantomData;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifies which hook point a callback is registered for. Ten event
/// kinds mirrored from Python SDK v0.1.64 (`types.py:216-227`), plus
/// `Unknown` as a fallback for forward-compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookKind {
    /// Before a tool is invoked.
    PreToolUse,
    /// After a tool completes successfully.
    PostToolUse,
    /// After a tool errors.
    PostToolUseFailure,
    /// Every user prompt (can rewrite or cancel).
    UserPromptSubmit,
    /// End of an assistant turn.
    Stop,
    /// End of a sub-agent turn.
    SubagentStop,
    /// Start of a sub-agent turn.
    SubagentStart,
    /// Before session compaction.
    PreCompact,
    /// Out-of-band notification to the caller.
    Notification,
    /// Permission request observation (distinct from `can_use_tool`).
    PermissionRequest,
    /// Fallback for hook events forge-sdk doesn't yet recognise.
    Unknown,
}

impl HookKind {
    /// Wire-name used by the `claude` binary.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::SubagentStart => "SubagentStart",
            Self::PreCompact => "PreCompact",
            Self::Notification => "Notification",
            Self::PermissionRequest => "PermissionRequest",
            Self::Unknown => "Unknown",
        }
    }

    /// Parse a wire-name back into the enum. Unknown strings fall through
    /// to `HookKind::Unknown` — forge-sdk is forward-compatible with new
    /// hook types Anthropic introduces between our parity checks.
    #[must_use]
    pub fn from_wire(s: &str) -> Self {
        match s {
            "PreToolUse" => Self::PreToolUse,
            "PostToolUse" => Self::PostToolUse,
            "PostToolUseFailure" => Self::PostToolUseFailure,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "Stop" => Self::Stop,
            "SubagentStop" => Self::SubagentStop,
            "SubagentStart" => Self::SubagentStart,
            "PreCompact" => Self::PreCompact,
            "Notification" => Self::Notification,
            "PermissionRequest" => Self::PermissionRequest,
            _ => Self::Unknown,
        }
    }
}

/// Context carried alongside every hook invocation.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// Hook point being invoked.
    pub kind: HookKind,
    /// Tool name when applicable (`PreToolUse` / `PostToolUse`).
    pub tool_name: Option<String>,
    /// Session id.
    pub session_id: String,
    /// Tool-use id when the hook fired in a tool-use context
    /// (`PreToolUse`, `PostToolUse`). `None` for other hook kinds.
    pub tool_use_id: Option<String>,
}

/// Input payload for `PreToolUse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreToolUseInput {
    /// Tool the model wants to invoke.
    pub tool_name: String,
    /// The model's proposed input.
    pub tool_input: Value,
}

/// Input payload for `PostToolUse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostToolUseInput {
    /// Tool that was invoked.
    pub tool_name: String,
    /// The input the tool actually ran with.
    pub tool_input: Value,
    /// The tool's response.
    pub tool_response: Value,
    /// Whether the tool errored.
    #[serde(default)]
    pub is_error: bool,
}

/// Input payload for `UserPromptSubmit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPromptSubmitInput {
    /// Raw prompt text.
    pub prompt: String,
}

/// Input payload for `Stop`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopInput {
    /// Number of turns in the session.
    pub num_turns: u64,
}

/// Input payload for `SubagentStop`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStopInput {
    /// Parent tool-use id that spawned the subagent.
    pub parent_tool_use_id: String,
    /// Number of turns the subagent ran.
    pub num_turns: u64,
}

/// Input payload for `PreCompact`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreCompactInput {
    /// Estimated tokens before compaction.
    pub before_tokens: u64,
    /// Target tokens after compaction.
    pub target_tokens: u64,
}

/// A hook decision.
#[derive(Debug, Clone)]
pub struct HookDecision {
    inner: HookDecisionKind,
}

#[derive(Debug, Clone)]
enum HookDecisionKind {
    Allow {
        updated_input: Option<Value>,
    },
    Deny {
        reason: String,
    },
    /// No-op — purely observational; continue unchanged.
    Passthrough,
}

impl HookDecision {
    /// Allow the action unchanged.
    #[must_use]
    pub fn allow() -> Self {
        Self {
            inner: HookDecisionKind::Allow {
                updated_input: None,
            },
        }
    }

    /// Allow but substitute a new input payload (`PreToolUse` /
    /// `UserPromptSubmit`).
    #[must_use]
    pub fn replace_input(new_input: Value) -> Self {
        Self {
            inner: HookDecisionKind::Allow {
                updated_input: Some(new_input),
            },
        }
    }

    /// Deny the action with a reason string.
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            inner: HookDecisionKind::Deny {
                reason: reason.into(),
            },
        }
    }

    /// Observational only — continue unchanged (typical `PostToolUse` /
    /// `Stop`).
    #[must_use]
    pub fn passthrough() -> Self {
        Self {
            inner: HookDecisionKind::Passthrough,
        }
    }

    /// True if the decision allows the action.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        !matches!(self.inner, HookDecisionKind::Deny { .. })
    }

    /// Optional modified input.
    #[must_use]
    pub fn updated_input(&self) -> Option<&Value> {
        match &self.inner {
            HookDecisionKind::Allow { updated_input } => updated_input.as_ref(),
            _ => None,
        }
    }

    /// Optional deny reason.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match &self.inner {
            HookDecisionKind::Deny { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Trait for hook callbacks. Each hook kind has its own concrete callback;
/// the trait is parameterised over the input type.
pub trait HookCallback<I>: Send + Sync
where
    I: Send + 'static,
{
    /// Called when the matching hook fires.
    fn call<'a>(
        &'a self,
        input: I,
        context: HookContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookDecision> + Send + 'a>>;
}

impl<F, Fut, I> HookCallback<I> for F
where
    F: Fn(I, HookContext) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = HookDecision> + Send + 'static,
    I: Send + 'static,
{
    fn call<'a>(
        &'a self,
        input: I,
        context: HookContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HookDecision> + Send + 'a>> {
        Box::pin(self(input, context))
    }
}

/// Type-erased hook callback. The concrete input type is rebuilt at
/// dispatch time from `input.hook_event_name`.
#[async_trait::async_trait]
pub trait ErasedHookCallback: Send + Sync {
    /// Deserialise `input` into the concrete input type and invoke the
    /// wrapped callback.
    async fn call_erased(&self, input: Value, context: HookContext) -> HookDecision;
}

/// Adapter so any typed [`HookCallback<I>`] implements
/// [`ErasedHookCallback`] when `I: DeserializeOwned`.
pub(crate) struct ErasedCallback<I, C>
where
    I: serde::de::DeserializeOwned + Send + 'static,
    C: HookCallback<I>,
{
    pub(crate) inner: C,
    pub(crate) _marker: PhantomData<fn() -> I>,
}

#[async_trait::async_trait]
impl<I, C> ErasedHookCallback for ErasedCallback<I, C>
where
    I: serde::de::DeserializeOwned + Send + 'static,
    C: HookCallback<I>,
{
    async fn call_erased(&self, input: Value, context: HookContext) -> HookDecision {
        match serde_json::from_value::<I>(input) {
            Ok(typed) => self.inner.call(typed, context).await,
            Err(_) => HookDecision::passthrough(),
        }
    }
}

/// Registry of hook callbacks. Construct with [`HooksBuilder`], attach to
/// `OptionsBuilder` via `.hooks(...)`.
#[derive(Default, Clone)]
pub struct Hooks {
    pub(crate) pre_tool_use: Vec<(String, Arc<dyn ErasedHookCallback>)>,
    pub(crate) post_tool_use: Vec<(String, Arc<dyn ErasedHookCallback>)>,
    pub(crate) user_prompt_submit: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) stop: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) subagent_stop: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) pre_compact: Vec<Arc<dyn ErasedHookCallback>>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("pre_tool_use_count", &self.pre_tool_use.len())
            .field("post_tool_use_count", &self.post_tool_use.len())
            .field("user_prompt_submit_count", &self.user_prompt_submit.len())
            .field("stop_count", &self.stop.len())
            .field("subagent_stop_count", &self.subagent_stop.len())
            .field("pre_compact_count", &self.pre_compact.len())
            .finish()
    }
}

impl Hooks {
    /// Mint opaque `callback_id`s (e.g. `hook_0`, `hook_1`, …) for every
    /// registered callback and return them bundled for dispatch-time
    /// lookup. Returns a map `id → callback` plus a parallel metadata
    /// vector describing which event/matcher each id belongs to (used to
    /// populate the initialize `control_request` payload).
    pub(crate) fn mint_registry(&self) -> HookRegistry {
        let mut registry = HookRegistry::default();
        let mut counter: u64 = 0;

        let mut mint =
            |kind: HookKind, matcher: Option<String>, cb: Arc<dyn ErasedHookCallback>| {
                let id = format!("hook_{counter}");
                counter += 1;
                registry.metadata.push(HookRegistryEntry {
                    id: id.clone(),
                    kind,
                    matcher,
                });
                registry.by_id.insert(id, cb);
            };

        for (matcher, cb) in &self.pre_tool_use {
            mint(HookKind::PreToolUse, Some(matcher.clone()), cb.clone());
        }
        for (matcher, cb) in &self.post_tool_use {
            mint(HookKind::PostToolUse, Some(matcher.clone()), cb.clone());
        }
        for cb in &self.user_prompt_submit {
            mint(HookKind::UserPromptSubmit, None, cb.clone());
        }
        for cb in &self.stop {
            mint(HookKind::Stop, None, cb.clone());
        }
        for cb in &self.subagent_stop {
            mint(HookKind::SubagentStop, None, cb.clone());
        }
        for cb in &self.pre_compact {
            mint(HookKind::PreCompact, None, cb.clone());
        }

        registry
    }
}

/// Internal bundle mapping opaque ids to erased callbacks, with parallel
/// metadata for the initialize payload.
#[derive(Default)]
pub(crate) struct HookRegistry {
    pub(crate) by_id: std::collections::HashMap<String, Arc<dyn ErasedHookCallback>>,
    pub(crate) metadata: Vec<HookRegistryEntry>,
}

/// One entry describing a minted hook id.
///
/// Fields are currently held for future wiring into the `initialize`
/// `control_request` payload (Plan 2 Task 6.5 per corrections C2.9).
/// Tagged `#[allow(dead_code)]` until that task lands.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct HookRegistryEntry {
    pub(crate) id: String,
    pub(crate) kind: HookKind,
    pub(crate) matcher: Option<String>,
}

/// Builder for [`Hooks`].
#[derive(Default)]
pub struct HooksBuilder {
    inner: Hooks,
}

impl std::fmt::Debug for HooksBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HooksBuilder")
            .field("inner", &self.inner)
            .finish()
    }
}

impl HooksBuilder {
    /// Start empty.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a `PreToolUse` hook. `matcher` is a glob against tool names
    /// (`"*"` matches all); pass `"Bash"` to match only the Bash tool.
    #[must_use]
    pub fn pre_tool_use<C>(mut self, matcher: impl Into<String>, callback: C) -> Self
    where
        C: HookCallback<PreToolUseInput> + 'static,
    {
        self.inner.pre_tool_use.push((
            matcher.into(),
            Arc::new(ErasedCallback::<PreToolUseInput, C> {
                inner: callback,
                _marker: PhantomData,
            }),
        ));
        self
    }

    /// Register a `PostToolUse` hook.
    #[must_use]
    pub fn post_tool_use<C>(mut self, matcher: impl Into<String>, callback: C) -> Self
    where
        C: HookCallback<PostToolUseInput> + 'static,
    {
        self.inner.post_tool_use.push((
            matcher.into(),
            Arc::new(ErasedCallback::<PostToolUseInput, C> {
                inner: callback,
                _marker: PhantomData,
            }),
        ));
        self
    }

    /// Register a `UserPromptSubmit` hook.
    #[must_use]
    pub fn user_prompt_submit<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<UserPromptSubmitInput> + 'static,
    {
        self.inner
            .user_prompt_submit
            .push(Arc::new(ErasedCallback::<UserPromptSubmitInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `Stop` hook.
    #[must_use]
    pub fn stop<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<StopInput> + 'static,
    {
        self.inner
            .stop
            .push(Arc::new(ErasedCallback::<StopInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `SubagentStop` hook.
    #[must_use]
    pub fn subagent_stop<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<SubagentStopInput> + 'static,
    {
        self.inner
            .subagent_stop
            .push(Arc::new(ErasedCallback::<SubagentStopInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `PreCompact` hook.
    #[must_use]
    pub fn pre_compact<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<PreCompactInput> + 'static,
    {
        self.inner
            .pre_compact
            .push(Arc::new(ErasedCallback::<PreCompactInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Finalise.
    #[must_use]
    pub fn build(self) -> Hooks {
        self.inner
    }
}

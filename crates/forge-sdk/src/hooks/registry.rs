//! Hook registry — stores callbacks, mints opaque `callback_id`s, and
//! renders the `hooks` field of the `initialize` `control_request` payload.

use std::marker::PhantomData;
use std::sync::Arc;

use super::HookKind;
use super::callback::{ErasedCallback, ErasedHookCallback, HookCallback};
use super::inputs::{
    NotificationInput, PermissionRequestInput, PostToolUseFailureInput, PostToolUseInput,
    PreCompactInput, PreToolUseInput, StopInput, SubagentStartInput, SubagentStopInput,
    UserPromptSubmitInput,
};

/// Registry of hook callbacks. Construct with [`HooksBuilder`], attach to
/// `OptionsBuilder` via `.hooks(...)`.
#[derive(Default, Clone)]
pub struct Hooks {
    pub(crate) pre_tool_use: Vec<(String, Arc<dyn ErasedHookCallback>)>,
    pub(crate) post_tool_use: Vec<(String, Arc<dyn ErasedHookCallback>)>,
    pub(crate) post_tool_use_failure: Vec<(String, Arc<dyn ErasedHookCallback>)>,
    pub(crate) user_prompt_submit: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) stop: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) subagent_stop: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) subagent_start: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) pre_compact: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) notification: Vec<Arc<dyn ErasedHookCallback>>,
    pub(crate) permission_request: Vec<(String, Arc<dyn ErasedHookCallback>)>,
    /// Timeout (seconds) the CLI should apply to every hook callback.
    /// `None` means "use the default forge-sdk emits" (30 — matches
    /// Python's per-matcher default). Overridable via
    /// [`HooksBuilder::default_timeout_secs`] for scenarios that need
    /// to provoke `control_cancel_request` on slow callbacks.
    pub(crate) default_timeout_secs: Option<u64>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks")
            .field("pre_tool_use_count", &self.pre_tool_use.len())
            .field("post_tool_use_count", &self.post_tool_use.len())
            .field(
                "post_tool_use_failure_count",
                &self.post_tool_use_failure.len(),
            )
            .field("user_prompt_submit_count", &self.user_prompt_submit.len())
            .field("stop_count", &self.stop.len())
            .field("subagent_stop_count", &self.subagent_stop.len())
            .field("subagent_start_count", &self.subagent_start.len())
            .field("pre_compact_count", &self.pre_compact.len())
            .field("notification_count", &self.notification.len())
            .field("permission_request_count", &self.permission_request.len())
            .field("default_timeout_secs", &self.default_timeout_secs)
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
        let mut registry = HookRegistry {
            default_timeout_secs: self.default_timeout_secs,
            ..HookRegistry::default()
        };
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
        for (matcher, cb) in &self.post_tool_use_failure {
            mint(
                HookKind::PostToolUseFailure,
                Some(matcher.clone()),
                cb.clone(),
            );
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
        for cb in &self.subagent_start {
            mint(HookKind::SubagentStart, None, cb.clone());
        }
        for cb in &self.pre_compact {
            mint(HookKind::PreCompact, None, cb.clone());
        }
        for cb in &self.notification {
            mint(HookKind::Notification, None, cb.clone());
        }
        for (matcher, cb) in &self.permission_request {
            mint(
                HookKind::PermissionRequest,
                Some(matcher.clone()),
                cb.clone(),
            );
        }

        registry
    }

    /// Render the `hooks` key of the `initialize` `control_request` payload
    /// exactly as the Client will send it. Test-only surface — production
    /// code uses this indirectly through the Client's initialize path.
    #[doc(hidden)]
    #[must_use]
    pub fn to_initialize_payload_for_test(&self) -> serde_json::Value {
        self.mint_registry().to_initialize_payload()
    }
}

/// Internal bundle mapping opaque ids to erased callbacks, with parallel
/// metadata for the initialize payload.
#[derive(Default)]
pub(crate) struct HookRegistry {
    pub(crate) by_id: std::collections::HashMap<String, Arc<dyn ErasedHookCallback>>,
    pub(crate) metadata: Vec<HookRegistryEntry>,
    pub(crate) default_timeout_secs: Option<u64>,
}

impl HookRegistry {
    /// Render the `hooks` field of the `initialize` `control_request`:
    /// `{"PreToolUse": [{"matcher": "...", "hookCallbackIds": ["hook_0"], "timeout": 30}, ...], ...}`.
    pub(crate) fn to_initialize_payload(&self) -> serde_json::Value {
        use std::collections::BTreeMap;

        // Group ids by (kind, matcher). BTreeMap for deterministic output.
        let mut by_kind: BTreeMap<&'static str, BTreeMap<String, Vec<String>>> = BTreeMap::new();
        for entry in &self.metadata {
            let kind_name = entry.kind.as_str();
            let matcher_key = entry.matcher.clone().unwrap_or_default();
            by_kind
                .entry(kind_name)
                .or_default()
                .entry(matcher_key)
                .or_default()
                .push(entry.id.clone());
        }

        let mut map = serde_json::Map::new();
        for (kind_name, matcher_group) in by_kind {
            let specs: Vec<serde_json::Value> = matcher_group
                .into_iter()
                .map(|(matcher, ids)| {
                    let mut spec = serde_json::Map::new();
                    if !matcher.is_empty() {
                        spec.insert("matcher".into(), serde_json::Value::String(matcher));
                    }
                    spec.insert(
                        "hookCallbackIds".into(),
                        serde_json::Value::Array(ids.into_iter().map(Into::into).collect()),
                    );
                    spec.insert(
                        "timeout".into(),
                        serde_json::json!(self.default_timeout_secs.unwrap_or(30)),
                    );
                    serde_json::Value::Object(spec)
                })
                .collect();
            map.insert(kind_name.into(), serde_json::Value::Array(specs));
        }
        serde_json::Value::Object(map)
    }
}

/// One entry describing a minted hook id.
#[derive(Debug, Clone)]
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

    /// Override the per-hook timeout (seconds) emitted in the
    /// initialize `control_request`. The CLI uses this to decide how
    /// long to wait on each `hook_callback` reply before giving up and
    /// emitting a `control_cancel_request`. Default when unset is
    /// 30 seconds — matches Python SDK's per-matcher default
    /// (`types.py` `HookMatcher.timeout`). Lower it for scenarios
    /// that deliberately provoke cancellation; raise it for
    /// long-running callbacks (e.g. LLM sub-calls inside a hook).
    #[must_use]
    pub fn default_timeout_secs(mut self, secs: u64) -> Self {
        self.inner.default_timeout_secs = Some(secs);
        self
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

    /// Register a `PostToolUseFailure` hook. `matcher` follows the same
    /// tool-name glob semantics as [`Self::pre_tool_use`] / [`Self::post_tool_use`].
    #[must_use]
    pub fn post_tool_use_failure<C>(mut self, matcher: impl Into<String>, callback: C) -> Self
    where
        C: HookCallback<PostToolUseFailureInput> + 'static,
    {
        self.inner.post_tool_use_failure.push((
            matcher.into(),
            Arc::new(ErasedCallback::<PostToolUseFailureInput, C> {
                inner: callback,
                _marker: PhantomData,
            }),
        ));
        self
    }

    /// Register a `Notification` hook.
    #[must_use]
    pub fn notification<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<NotificationInput> + 'static,
    {
        self.inner
            .notification
            .push(Arc::new(ErasedCallback::<NotificationInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `SubagentStart` hook.
    #[must_use]
    pub fn subagent_start<C>(mut self, callback: C) -> Self
    where
        C: HookCallback<SubagentStartInput> + 'static,
    {
        self.inner
            .subagent_start
            .push(Arc::new(ErasedCallback::<SubagentStartInput, C> {
                inner: callback,
                _marker: PhantomData,
            }));
        self
    }

    /// Register a `PermissionRequest` hook (observational; `matcher` globs
    /// against tool names the same way as [`Self::pre_tool_use`]).
    #[must_use]
    pub fn permission_request<C>(mut self, matcher: impl Into<String>, callback: C) -> Self
    where
        C: HookCallback<PermissionRequestInput> + 'static,
    {
        self.inner.permission_request.push((
            matcher.into(),
            Arc::new(ErasedCallback::<PermissionRequestInput, C> {
                inner: callback,
                _marker: PhantomData,
            }),
        ));
        self
    }

    /// Finalise.
    #[must_use]
    pub fn build(self) -> Hooks {
        self.inner
    }
}

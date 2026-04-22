//! Hook callback trait + decision type + type-erasure plumbing.

use std::marker::PhantomData;

use serde_json::Value;

use super::HookContext;

/// A hook decision.
///
/// The primary shape (allow / deny / replace-input / passthrough) mirrors
/// Python's `SyncHookJSONOutput.decision` field; the `with_*` builders
/// attach the optional control fields Python documents alongside it —
/// `continue_` / `suppressOutput` / `stopReason` / `systemMessage`. See
/// `types.py:463-505` + `_internal/query.py:40-55` for the full contract.
#[derive(Debug, Clone)]
pub struct HookDecision {
    inner: HookDecisionKind,
    continue_execution: Option<bool>,
    suppress_output: Option<bool>,
    stop_reason: Option<String>,
    system_message: Option<String>,
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
    /// Deferred-execution ACK — ships Python's `AsyncHookJSONOutput`
    /// shape (`{"async": true, "asyncTimeout": <ms>}`) so the CLI
    /// knows not to wait on the hook's in-line response. The caller
    /// is expected to deliver the real result out-of-band. Wiring
    /// the out-of-band return path is follow-up work; forge-sdk
    /// currently only emits the ACK.
    Defer {
        timeout_ms: Option<u64>,
    },
}

impl HookDecision {
    fn with_inner(inner: HookDecisionKind) -> Self {
        Self {
            inner,
            continue_execution: None,
            suppress_output: None,
            stop_reason: None,
            system_message: None,
        }
    }

    /// Allow the action unchanged.
    #[must_use]
    pub fn allow() -> Self {
        Self::with_inner(HookDecisionKind::Allow {
            updated_input: None,
        })
    }

    /// Allow but substitute a new input payload (`PreToolUse` /
    /// `UserPromptSubmit`).
    #[must_use]
    pub fn replace_input(new_input: Value) -> Self {
        Self::with_inner(HookDecisionKind::Allow {
            updated_input: Some(new_input),
        })
    }

    /// Deny the action with a reason string.
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::with_inner(HookDecisionKind::Deny {
            reason: reason.into(),
        })
    }

    /// Observational only — continue unchanged (typical `PostToolUse` /
    /// `Stop`).
    #[must_use]
    pub fn passthrough() -> Self {
        Self::with_inner(HookDecisionKind::Passthrough)
    }

    /// Defer the hook response. Emits Python's `AsyncHookJSONOutput`
    /// shape (`types.py:448-460`): `{"async": true, "asyncTimeout":
    /// <ms>?}`. Pass `None` for no explicit timeout. The CLI will
    /// proceed without waiting for the hook's final verdict; the
    /// caller is expected to deliver the real decision out-of-band
    /// (wiring that channel is follow-up work — forge-sdk currently
    /// emits only the ACK shape).
    #[must_use]
    pub fn defer(timeout_ms: Option<u64>) -> Self {
        Self::with_inner(HookDecisionKind::Defer { timeout_ms })
    }

    /// True iff the decision was constructed via [`defer`](Self::defer).
    #[must_use]
    pub fn is_deferred(&self) -> bool {
        matches!(self.inner, HookDecisionKind::Defer { .. })
    }

    /// Optional timeout in milliseconds attached to a
    /// [`defer`](Self::defer) decision.
    #[must_use]
    pub fn defer_timeout_ms(&self) -> Option<u64> {
        match &self.inner {
            HookDecisionKind::Defer { timeout_ms } => *timeout_ms,
            _ => None,
        }
    }

    /// Attach the Python SDK's `continue_` control field (CLI wire name:
    /// `continue`). Pass `false` to signal that Claude should not proceed
    /// after the hook — typically combined with
    /// [`with_stop_reason`](Self::with_stop_reason).
    #[must_use]
    pub fn with_continue(mut self, should_continue: bool) -> Self {
        self.continue_execution = Some(should_continue);
        self
    }

    /// Attach the Python SDK's `suppressOutput` control field. When
    /// `true`, the CLI hides stdout from transcript mode.
    #[must_use]
    pub fn with_suppress_output(mut self, suppress: bool) -> Self {
        self.suppress_output = Some(suppress);
        self
    }

    /// Attach the Python SDK's `stopReason` control field — the message
    /// the CLI shows when `continue` is set to `false`.
    #[must_use]
    pub fn with_stop_reason(mut self, reason: impl Into<String>) -> Self {
        self.stop_reason = Some(reason.into());
        self
    }

    /// Attach the Python SDK's `systemMessage` control field — a warning
    /// displayed to the user alongside the hook's decision.
    #[must_use]
    pub fn with_system_message(mut self, msg: impl Into<String>) -> Self {
        self.system_message = Some(msg.into());
        self
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

    /// `continue` control field, if the callback set one. Wire name is
    /// `continue` (Python stores as `continue_` to dodge the keyword).
    #[must_use]
    pub fn continue_execution(&self) -> Option<bool> {
        self.continue_execution
    }

    /// `suppressOutput` control field, if the callback set one.
    #[must_use]
    pub fn suppress_output(&self) -> Option<bool> {
        self.suppress_output
    }

    /// `stopReason` control field, if the callback set one.
    #[must_use]
    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }

    /// `systemMessage` control field, if the callback set one.
    #[must_use]
    pub fn system_message(&self) -> Option<&str> {
        self.system_message.as_deref()
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
            Err(e) => {
                // Security-permissive passthrough would silently skip the
                // caller's hook logic. Log prominently so a CLI schema drift
                // doesn't invisibly bypass the user's policy.
                tracing::warn!(
                    error = %e,
                    hook_kind = ?context.kind,
                    "hook input deserialise failed; passthrough (hook not consulted). CLI schema drift?"
                );
                HookDecision::passthrough()
            }
        }
    }
}

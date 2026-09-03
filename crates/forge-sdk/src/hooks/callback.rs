//! Hook callback trait + decision type + type-erasure plumbing.

use std::marker::PhantomData;

use serde_json::Value;

use forge_primitives::HookContext;

/// A hook decision.
///
/// The shape (allow / deny / replace-input / passthrough) mirrors
/// the CLI's `SyncHookJSONOutput.decision` field. See the hook-callback
/// contract docs for the full wire story.
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
    /// No-op - purely observational; continue unchanged.
    Passthrough,
    /// Deferred-execution ACK - ships the CLI's `AsyncHookJSONOutput`
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
        Self { inner }
    }

    /// Allow the action unchanged.
    pub fn allow() -> Self {
        Self::with_inner(HookDecisionKind::Allow { updated_input: None })
    }

    /// Allow but substitute a new input payload (`PreToolUse` /
    /// `UserPromptSubmit`).
    pub fn replace_input(new_input: Value) -> Self {
        Self::with_inner(HookDecisionKind::Allow { updated_input: Some(new_input) })
    }

    /// Deny the action with a reason string.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::with_inner(HookDecisionKind::Deny { reason: reason.into() })
    }

    /// Observational only - continue unchanged (typical `PostToolUse` /
    /// `Stop`).
    pub fn passthrough() -> Self {
        Self::with_inner(HookDecisionKind::Passthrough)
    }

    /// Defer the hook response. Emits the CLI's `AsyncHookJSONOutput`
    /// shape: `{"async": true, "asyncTimeout": <ms>?}`. Pass `None` for
    /// no explicit timeout. The CLI will proceed without waiting for the
    /// hook's final verdict; the caller is expected to deliver the real
    /// decision out-of-band (wiring that channel is follow-up work -
    /// forge-sdk currently emits only the ACK shape).
    pub fn defer(timeout_ms: Option<u64>) -> Self {
        Self::with_inner(HookDecisionKind::Defer { timeout_ms })
    }

    /// True iff the decision was constructed via [`defer`](Self::defer).
    pub fn is_deferred(&self) -> bool {
        matches!(self.inner, HookDecisionKind::Defer { .. })
    }

    /// Optional timeout in milliseconds attached to a
    /// [`defer`](Self::defer) decision.
    pub fn defer_timeout_ms(&self) -> Option<u64> {
        match &self.inner {
            HookDecisionKind::Defer { timeout_ms } => *timeout_ms,
            _ => None,
        }
    }

    /// True if the decision allows the action.
    pub fn is_allow(&self) -> bool {
        !matches!(self.inner, HookDecisionKind::Deny { .. })
    }

    /// Optional modified input.
    pub fn updated_input(&self) -> Option<&Value> {
        match &self.inner {
            HookDecisionKind::Allow { updated_input } => updated_input.as_ref(),
            _ => None,
        }
    }

    /// Optional deny reason.
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

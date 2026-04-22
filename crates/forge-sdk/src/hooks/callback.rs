//! Hook callback trait + decision type + type-erasure plumbing.

use std::marker::PhantomData;

use serde_json::Value;

use super::HookContext;

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

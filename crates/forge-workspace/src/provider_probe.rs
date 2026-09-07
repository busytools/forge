//! Workspace-side routing of account probes through the
//! forge-providers backends, shared by the boot loader and the 60 s
//! usage poller.

use std::collections::HashMap;
use std::path::Path;

use forge_agent::cloud::AgentHost;
use forge_primitives::account::Provider;
use forge_providers::{AccountEnv, ProbeError, ProviderBackend, UsageSnapshot};

pub(crate) fn backend_for(provider: Provider) -> Result<&'static dyn ProviderBackend, ProbeError> {
    if let Some(backend) = forge_providers::backend(provider) {
        return Ok(backend);
    }
    debug_assert!(
        false,
        "every provider the probe arms reach must have a backend registered; {provider:?}"
    );
    Err(ProbeError::Unmappable(format!("no backend registered for {provider:?}")))
}

/// One probe round-trip through the account's backend and the shared
/// host.
pub(crate) async fn probe_via_backend(
    provider: Provider,
    config_dir: &Path,
    env: &HashMap<String, String>,
) -> Result<UsageSnapshot, ProbeError> {
    let backend = backend_for(provider)?;
    let account_env = AccountEnv { config_dir, env };
    backend.probe(&account_env, &AgentHost).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry holds every token the anthropic-shaped arms can
    /// reach; a missing registration is the one error this module
    /// invents, and it must never surface for Anthropic.
    #[test]
    fn anthropic_backend_always_resolves() {
        assert!(backend_for(Provider::Anthropic).is_ok());
    }
}

//! The System One decision tools' workspace leg: the boot-time client
//! construction, shared by the real boot and the testing stubs so both
//! agree on trust handling.

use std::path::Path;

use forge_system_one::{SystemOneClient, SystemOneConfig};

use crate::error::WorkspaceError;

/// Build the decision client for a loaded `[systemone]` section; `None`
/// keeps the tools unregistered for every session. The client goes
/// through the same TLS-trust helper the Slack and Gotify legs use, so
/// a NODE_EXTRA_CA_CERTS bundle behaves identically for decisions.
pub(crate) fn client_for(
    config: Option<&SystemOneConfig>,
    forge_toml: &Path,
) -> Result<Option<SystemOneClient>, WorkspaceError> {
    let Some(config) = config else {
        return Ok(None);
    };
    let http = forge_agent::http_trust::with_extra_roots(reqwest::Client::builder())
        .build()
        .map_err(|error| WorkspaceError::ConfigInvalid {
            path: forge_toml.to_path_buf(),
            message: format!("systemone http client: {error}"),
        })?;
    Ok(Some(SystemOneClient::new(config, http)))
}

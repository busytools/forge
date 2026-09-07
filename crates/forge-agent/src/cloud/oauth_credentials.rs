//! The session OAuth snapshot.
//!
//! A token-mode account's credential is the setup token in its env -
//! the only credential an Anthropic account has. forge-agent's SDK
//! bridge reads it through [`session_oauth_credentials`] when a
//! session reports its OAuth state.

use std::collections::HashMap;

pub use forge_primitives::cloud::oauth_credentials::OauthCredentials;

/// The credential a session's OAuth snapshot reports: the setup token
/// in the account's env, with no locally-known expiry. `None` when the
/// env carries no token.
pub fn session_oauth_credentials<S: std::hash::BuildHasher>(
    env: &HashMap<String, String, S>,
) -> Option<OauthCredentials> {
    forge_providers::token_bearer(env)
        .map(|token| OauthCredentials { access_token: token.to_owned(), expires_at: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The env arrives trimmed (the config load normalizes it), so the
    /// snapshot's read is the plain token.
    #[test]
    fn a_token_session_snapshots_its_env_token_not_the_shared_keychain() {
        let mut env = HashMap::new();
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "setup-token".to_owned());
        let credentials = session_oauth_credentials(&env);
        assert_eq!(
            credentials,
            Some(OauthCredentials { access_token: "setup-token".to_owned(), expires_at: None }),
            "the env token IS the session's credential, with no locally-known expiry",
        );
    }

    #[test]
    fn an_env_without_a_token_snapshots_nothing() {
        let credentials = session_oauth_credentials(&HashMap::new());
        assert_eq!(credentials, None, "no token in env, no credential");
    }
}

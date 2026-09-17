//! Session registration: the binding of `(org, project, session)` to
//! an account, and the env set a child is stamped with.
//!
//! The child never holds a real credential. Its `ANTHROPIC_BASE_URL`
//! points at this listener with the three routing segments in the
//! path, and the credential it does hold is a dummy delivered through
//! the SAME variable the account's real credential uses - moving a
//! credential between variables changes which entitlements reach the
//! backend, not only which header carries it.

use std::collections::HashMap;

use forge_primitives::account::Provider;
use parking_lot::Mutex;

use crate::account::AccountKey;

/// What the CLI sends when it has no real credential. Authorises
/// nothing upstream; the gateway replaces it before forwarding.
pub const DUMMY_CREDENTIAL: &str = "forge-gateway-unused";

pub(crate) const OAUTH_VARIABLE: &str = "CLAUDE_CODE_OAUTH_TOKEN";
pub(crate) const AUTH_TOKEN_VARIABLE: &str = "ANTHROPIC_AUTH_TOKEN";
const API_KEY_VARIABLE: &str = "ANTHROPIC_API_KEY";
const ALT_BASE_URL_VARIABLE: &str = "CLAUDE_CODE_API_BASE_URL";
const BASE_URL_VARIABLE: &str = "ANTHROPIC_BASE_URL";

/// One spawn's registration: who owns the session and where it sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub org: String,
    pub project: String,
    pub session: String,
    pub account: AccountKey,
    pub provider: Provider,
}

/// The env a child is stamped with, in insertion order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvSet(pub Vec<(String, String)>);

impl EnvSet {
    pub fn into_map(self) -> HashMap<String, String> {
        self.0.into_iter().collect()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// The session bindings, keyed by the three routing segments.
#[derive(Default)]
pub struct Bindings {
    by_session: Mutex<HashMap<(String, String, String), AccountKey>>,
}

impl Bindings {
    /// Register `registration` and derive the env set its child is
    /// stamped with, from the account's real env. Re-registering the
    /// same three segments replaces the prior binding, so a respawned
    /// CLI answers to this generation of the session, not the last.
    pub fn register(
        &self,
        registration: &Registration,
        listener_base: &str,
        account_env: &HashMap<String, String>,
    ) -> EnvSet {
        let segments =
            (registration.org.clone(), registration.project.clone(), registration.session.clone());
        self.by_session.lock().insert(segments, registration.account.clone());

        let mut env: Vec<(String, String)> = account_env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .filter(|(k, _)| {
                k != BASE_URL_VARIABLE
                    && k != ALT_BASE_URL_VARIABLE
                    && k != OAUTH_VARIABLE
                    && k != AUTH_TOKEN_VARIABLE
                    && k != API_KEY_VARIABLE
            })
            .collect();
        for (key, value) in stamped_gateway_keys(registration, listener_base) {
            if let Some(slot) = env.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = value;
            } else {
                env.push((key, value));
            }
        }
        EnvSet(env)
    }

    /// Re-register `registration` and return ONLY the four
    /// gateway-owned keys for a respawn's launch-settings overrides:
    /// the base URL naming this listener, the alt base-url slot, the
    /// dummy in the account's credential variable, and the forced-empty
    /// API key. The account and project env already live in the bridge
    /// from the original spawn, so carrying the whole env here would
    /// let a key declared in both layers revert to its account value.
    ///
    /// `registration.session` is the segment the respawned child runs
    /// under; `replaced_session` is the one the child it replaces ran
    /// under, dropped under the same lock as the insert, so no lookup
    /// lands between the two. Without that drop a session keeps one
    /// binding for every id it has ever held.
    pub fn respawn_env_overrides(
        &self,
        registration: &Registration,
        replaced_session: &str,
        listener_base: &str,
    ) -> HashMap<String, String> {
        {
            let mut map = self.by_session.lock();
            map.remove(&(
                registration.org.clone(),
                registration.project.clone(),
                replaced_session.to_owned(),
            ));
            map.insert(
                (
                    registration.org.clone(),
                    registration.project.clone(),
                    registration.session.clone(),
                ),
                registration.account.clone(),
            );
        }
        stamped_gateway_keys(registration, listener_base).into_iter().collect()
    }

    /// The account bound to the three routing segments, if any.
    pub fn binding_for(&self, org: &str, project: &str, session: &str) -> Option<AccountKey> {
        self.by_session
            .lock()
            .get(&(org.to_owned(), project.to_owned(), session.to_owned()))
            .cloned()
    }

    /// Remove and return the binding for the three routing segments,
    /// read and removed under one acquisition. A caller that acts on
    /// what it read - cooling the account it has proved exhausted - must
    /// not race a re-bind of the same triple between the two, or it
    /// cools one account and drops another's binding.
    pub fn take_binding(&self, org: &str, project: &str, session: &str) -> Option<AccountKey> {
        self.by_session.lock().remove(&(org.to_owned(), project.to_owned(), session.to_owned()))
    }

    /// Bind a selected account to the three routing segments.
    pub fn bind(&self, org: &str, project: &str, session: &str, account: AccountKey) {
        self.by_session
            .lock()
            .insert((org.to_owned(), project.to_owned(), session.to_owned()), account);
    }

    /// Drop the binding, so the session's next request selects again.
    pub fn unbind(&self, org: &str, project: &str, session: &str) {
        self.by_session.lock().remove(&(org.to_owned(), project.to_owned(), session.to_owned()));
    }

    /// Drop every binding onto `account`, so those sessions select
    /// again. The usage probe knows only the account it proved
    /// exhausted.
    pub fn unbind_for_account(&self, account: &AccountKey) {
        self.by_session.lock().retain(|_, bound| bound != account);
    }
}

/// The four gateway-owned keys, stamped fresh for `registration`:
/// every other env key composes per the documented precedence, these
/// four are the gateway's alone.
fn stamped_gateway_keys(registration: &Registration, listener_base: &str) -> Vec<(String, String)> {
    let base_url = format!(
        "{}/{}/{}/{}",
        listener_base, registration.org, registration.project, registration.session
    );
    let credential_variable = credential_variable_for(registration.provider);
    vec![
        (BASE_URL_VARIABLE.to_owned(), base_url.clone()),
        (ALT_BASE_URL_VARIABLE.to_owned(), base_url),
        (credential_variable.to_owned(), DUMMY_CREDENTIAL.to_owned()),
        (API_KEY_VARIABLE.to_owned(), String::new()),
    ]
}

/// The variable the account's real credential lives in, which is the
/// variable its dummy has to travel in too. The one home for the rule:
/// the config load maps the flat token onto it, the stamp puts the
/// dummy in it, and the forward leg reads the real credential back out
/// of it.
pub fn credential_variable_for(provider: Provider) -> &'static str {
    if provider.uses_base_url() { AUTH_TOKEN_VARIABLE } else { OAUTH_VARIABLE }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_env() -> HashMap<String, String> {
        HashMap::from([
            ("CLAUDE_CODE_OAUTH_TOKEN".to_owned(), "real-oauth-token".to_owned()),
            ("CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_owned(), "1000000".to_owned()),
        ])
    }

    fn base_url_env() -> HashMap<String, String> {
        HashMap::from([
            ("ANTHROPIC_BASE_URL".to_owned(), "https://openrouter.ai/api".to_owned()),
            ("ANTHROPIC_AUTH_TOKEN".to_owned(), "real-openrouter-key".to_owned()),
            ("ANTHROPIC_API_KEY".to_owned(), String::new()),
        ])
    }

    fn registration(provider: Provider) -> Registration {
        Registration {
            org: "Busytools".to_owned(),
            project: "forge".to_owned(),
            session: "session-1".to_owned(),
            account: AccountKey("OpenRouter".to_owned()),
            provider,
        }
    }

    fn env_contains_secret(env: &EnvSet, secret: &str) -> bool {
        env.0.iter().any(|(_, v)| v.contains(secret))
    }

    #[test]
    fn the_base_url_is_always_the_listener_with_the_three_segments() {
        let bindings = Bindings::default();
        let env = bindings.register(
            &registration(Provider::Openrouter),
            "http://127.0.0.1:8787",
            &base_url_env(),
        );
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL"),
            Some("http://127.0.0.1:8787/Busytools/forge/session-1"),
            "the three routing segments ride the stamped base URL",
        );
    }

    #[test]
    fn a_base_url_account_gets_its_dummy_in_the_auth_token_variable() {
        let bindings = Bindings::default();
        let env = bindings.register(
            &registration(Provider::Openrouter),
            "http://127.0.0.1:8787",
            &base_url_env(),
        );
        assert_eq!(
            env.get("ANTHROPIC_AUTH_TOKEN"),
            Some(DUMMY_CREDENTIAL),
            "the dummy travels in the same variable the real credential uses",
        );
        assert!(
            !env_contains_secret(&env, "real-openrouter-key"),
            "the real credential never reaches the child",
        );
    }

    #[test]
    fn an_anthropic_account_gets_its_dummy_in_the_oauth_variable() {
        let bindings = Bindings::default();
        let env = bindings.register(
            &registration(Provider::Anthropic),
            "http://127.0.0.1:8787",
            &anthropic_env(),
        );
        assert_eq!(
            env.get("CLAUDE_CODE_OAUTH_TOKEN"),
            Some(DUMMY_CREDENTIAL),
            "the OAuth variable carries the dummy, not the real setup token",
        );
        assert!(
            !env_contains_secret(&env, "real-oauth-token"),
            "the real token never reaches the child"
        );
    }

    #[test]
    fn an_anthropic_account_never_passes_a_stray_auth_token_to_the_child() {
        let bindings = Bindings::default();
        let mut env = anthropic_env();
        env.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), "stray-base-url-key".to_owned());
        let stamped =
            bindings.register(&registration(Provider::Anthropic), "http://127.0.0.1:8787", &env);
        assert_eq!(
            stamped.get("ANTHROPIC_AUTH_TOKEN"),
            None,
            "the credential variable the account does not use is removed, not passed through",
        );
        assert!(
            !env_contains_secret(&stamped, "stray-base-url-key"),
            "a foreign credential never reaches the child",
        );
        assert_eq!(
            stamped.get("CLAUDE_CODE_OAUTH_TOKEN"),
            Some(DUMMY_CREDENTIAL),
            "the account's own variable still carries the dummy",
        );
    }

    #[test]
    fn the_api_key_is_forced_empty() {
        let bindings = Bindings::default();
        let env = bindings.register(
            &registration(Provider::Openrouter),
            "http://127.0.0.1:8787",
            &base_url_env(),
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY"),
            Some(""),
            "an empty x-api-key source is the same as unset"
        );
    }

    #[test]
    fn the_alt_base_url_is_never_left_pointing_elsewhere() {
        let bindings = Bindings::default();
        let mut env_with_alt = base_url_env();
        env_with_alt
            .insert("CLAUDE_CODE_API_BASE_URL".to_owned(), "http://localhost:18765".to_owned());
        let env = bindings.register(
            &registration(Provider::Openrouter),
            "http://127.0.0.1:8787",
            &env_with_alt,
        );
        assert_eq!(
            env.get("CLAUDE_CODE_API_BASE_URL"),
            Some("http://127.0.0.1:8787/Busytools/forge/session-1"),
            "the lower-precedence variable must not bypass the redirect",
        );
    }

    #[test]
    fn unrelated_env_passes_through_verbatim() {
        let bindings = Bindings::default();
        let env = bindings.register(
            &registration(Provider::Anthropic),
            "http://127.0.0.1:8787",
            &anthropic_env(),
        );
        assert_eq!(
            env.get("CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            Some("1000000"),
            "an unrelated env key rides through untouched",
        );
    }

    #[test]
    fn reregistration_replaces_the_prior_binding() {
        let bindings = Bindings::default();
        let mut first = registration(Provider::Openrouter);
        first.account = AccountKey("OldAccount".to_owned());
        bindings.register(&first, "http://127.0.0.1:8787", &base_url_env());
        bindings.register(
            &registration(Provider::Openrouter),
            "http://127.0.0.1:8787",
            &base_url_env(),
        );
        assert_eq!(
            bindings.binding_for("Busytools", "forge", "session-1"),
            Some(AccountKey("OpenRouter".to_owned())),
            "a respawn's registration replaces the previous generation's binding",
        );
    }

    #[test]
    fn a_respawn_binding_names_the_segment_the_child_runs_under() {
        let bindings = Bindings::default();
        bindings.register(
            &registration(Provider::Anthropic),
            "http://127.0.0.1:8787",
            &anthropic_env(),
        );
        let mut respawned = registration(Provider::Anthropic);
        respawned.session = "session-2".to_owned();

        let overrides =
            bindings.respawn_env_overrides(&respawned, "session-1", "http://127.0.0.1:8787");

        assert_eq!(
            overrides.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:8787/Busytools/forge/session-2"),
            "the respawned child's base URL names the id it runs under",
        );
        assert_eq!(
            bindings.binding_for("Busytools", "forge", "session-2"),
            Some(AccountKey("OpenRouter".to_owned())),
            "the binding moves to the new segment",
        );
        assert_eq!(
            bindings.binding_for("Busytools", "forge", "session-1"),
            None,
            "the id the session left behind keeps no binding",
        );
    }

    #[test]
    fn an_unregistered_session_has_no_binding() {
        let bindings = Bindings::default();
        assert_eq!(bindings.binding_for("Busytools", "forge", "ghost"), None);
    }
}

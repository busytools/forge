/// The decisions endpoint for a configured base. A base already carrying
/// `/v1` appends only `/systemone` (pydantic-ai's rule, so an SDK-style
/// base and a host-style base both land on the same route).
pub fn systemone_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") { format!("{base}/systemone") } else { format!("{base}/v1/systemone") }
}

/// The per-request timeout a section gets when it names none.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The raw `[systemone]` section as written in forge.toml.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemOneSection {
    pub enabled: Option<bool>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub timeout_ms: Option<u64>,
}

/// An enabled, validated `[systemone]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemOneConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub timeout_ms: u64,
}

impl SystemOneSection {
    /// `Ok(None)` is the dormant state (disabled, or nothing to start);
    /// `Err` is enabled-but-unusable, and the caller prefixes the
    /// section name into the message.
    pub fn into_config(self) -> Result<Option<SystemOneConfig>, String> {
        if !self.enabled.unwrap_or(true) {
            return Ok(None);
        }
        let base_url = self
            .base_url
            .filter(|b| !b.trim().is_empty())
            .ok_or("`base_url` is required when `[systemone]` is enabled")?;
        let model = self
            .model
            .filter(|m| !m.trim().is_empty())
            .ok_or("`model` is required when `[systemone]` is enabled")?;
        let timeout_ms = self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if timeout_ms == 0 {
            return Err("`timeout_ms` must be greater than zero".to_owned());
        }
        Ok(Some(SystemOneConfig {
            base_url: base_url.trim().to_owned(),
            api_key: self.api_key.filter(|k| !k.trim().is_empty()),
            model: model.trim().to_owned(),
            timeout_ms,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemone_url_handles_base_shapes() {
        assert_eq!(
            systemone_url("https://api.typesafe.ai"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            systemone_url("https://api.typesafe.ai/"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            systemone_url("https://api.typesafe.ai/v1"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            systemone_url("https://openrouter.ai/api"),
            "https://openrouter.ai/api/v1/systemone"
        );
        assert_eq!(systemone_url("http://localhost:11434/"), "http://localhost:11434/v1/systemone");
    }

    #[test]
    fn disabled_section_resolves_to_none() {
        let section = SystemOneSection {
            enabled: Some(false),
            base_url: None,
            api_key: None,
            model: None,
            timeout_ms: None,
        };
        assert_eq!(section.into_config(), Ok(None));
    }

    #[test]
    fn missing_base_url_is_refused_when_enabled() {
        let section = SystemOneSection {
            enabled: None,
            base_url: None,
            api_key: None,
            model: Some("jev-latest".to_owned()),
            timeout_ms: None,
        };
        assert_eq!(
            section.into_config(),
            Err("`base_url` is required when `[systemone]` is enabled".to_owned())
        );
    }

    #[test]
    fn whitespace_base_url_is_treated_as_missing() {
        let section = SystemOneSection {
            enabled: None,
            base_url: Some("  ".to_owned()),
            api_key: None,
            model: Some("jev-latest".to_owned()),
            timeout_ms: None,
        };
        assert_eq!(
            section.into_config(),
            Err("`base_url` is required when `[systemone]` is enabled".to_owned())
        );
    }

    #[test]
    fn missing_model_is_refused_when_enabled() {
        let section = SystemOneSection {
            enabled: None,
            base_url: Some("https://api.typesafe.ai".to_owned()),
            api_key: None,
            model: None,
            timeout_ms: None,
        };
        assert_eq!(
            section.into_config(),
            Err("`model` is required when `[systemone]` is enabled".to_owned())
        );
    }

    #[test]
    fn zero_timeout_is_refused() {
        let section = SystemOneSection {
            enabled: None,
            base_url: Some("https://api.typesafe.ai".to_owned()),
            api_key: None,
            model: Some("jev-latest".to_owned()),
            timeout_ms: Some(0),
        };
        assert_eq!(section.into_config(), Err("`timeout_ms` must be greater than zero".to_owned()));
    }

    #[test]
    fn full_section_resolves_with_defaults() {
        let section = SystemOneSection {
            enabled: None,
            base_url: Some("https://api.typesafe.ai/".to_owned()),
            api_key: Some("k".to_owned()),
            model: Some("jev-latest".to_owned()),
            timeout_ms: None,
        };
        assert_eq!(
            section.into_config(),
            Ok(Some(SystemOneConfig {
                base_url: "https://api.typesafe.ai/".to_owned(),
                api_key: Some("k".to_owned()),
                model: "jev-latest".to_owned(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
            }))
        );
    }

    #[test]
    fn blank_api_key_resolves_to_none() {
        let section = SystemOneSection {
            enabled: None,
            base_url: Some("https://api.typesafe.ai".to_owned()),
            api_key: Some("   ".to_owned()),
            model: Some("jev-latest".to_owned()),
            timeout_ms: None,
        };
        let cfg = section.into_config().expect("an enabled section with url and model resolves");
        assert_eq!(cfg.expect("resolved").api_key, None);
    }

    #[test]
    fn unknown_key_in_section_is_refused() {
        let result: Result<SystemOneSection, _> = toml::from_str(
            "enabled = true\nbase_uri = \"https://api.typesafe.ai\"\nmodel = \"jev-latest\"\n",
        );
        let err = result.expect_err("a mistyped key refuses the section");
        assert!(err.to_string().contains("base_uri"), "{err}");
    }

    #[test]
    fn section_deserializes_from_toml() {
        let section: SystemOneSection = toml::from_str(
            "enabled = false\nbase_url = \"https://api.typesafe.ai\"\nmodel = \"jev-latest\"\n",
        )
        .expect("the section parses");
        assert_eq!(section.enabled, Some(false));
        assert_eq!(section.base_url.as_deref(), Some("https://api.typesafe.ai"));
        assert_eq!(section.into_config(), Ok(None));
    }
}

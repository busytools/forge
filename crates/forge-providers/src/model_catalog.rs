//! OpenRouter's public model catalog: fetch, parse, and the curated
//! `/model` list built on top of it, behind the [`ModelCatalog`] trait
//! a backend exposes through
//! [`ProviderBackend::model_catalog`](crate::ProviderBackend::model_catalog).
//!
//! `GET {base}/v1/models` is public (no auth, free) and carries every
//! model the account can name. forge serves only the curated ten - a
//! maintained constant in this module - enriched with live price and
//! context figures from the fetch. Same URL-join lesson as the
//! openrouter backend's key url: `ANTHROPIC_BASE_URL` already ends
//! in `/api`, so only the `/v1/models` tail is appended.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use forge_primitives::AvailableModel;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ProviderHost;
use crate::helpers::truncated_body_suffix;

/// Timeout for one catalog round-trip. Matches the probe budget; the
/// endpoint is a static public list.
pub const CATALOG_TIMEOUT: Duration = Duration::from_secs(8);

/// How long a cached catalog is served without a refetch.
pub const CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a failed fetch is remembered, so an unreachable endpoint
/// costs its timeout at most once per window instead of on every
/// connect.
pub const CATALOG_FAILURE_TTL: Duration = Duration::from_secs(10 * 60);

/// The catalog half of a backend: fetch + parse + the curated merge.
/// The workspace glue owns the redb cache and the fresh/stale/miss
/// orchestration; these three are the provider-owned pieces.
#[async_trait]
pub trait ModelCatalog: Send + Sync {
    /// One round-trip against `{base}/v1/models`.
    async fn fetch(
        &self,
        base_url: &str,
        host: &dyn ProviderHost,
    ) -> Result<Vec<CatalogModel>, ModelCatalogError>;
    /// Curated picker rows from a fetched catalog; the caller falls
    /// back to the discovered list when the merge is empty.
    fn curated(&self, models: &[CatalogModel]) -> Vec<AvailableModel>;
    /// What the cache says for a base url, judged at `now`.
    fn decision(&self, cached: Option<CachedCatalog>, now: SystemTime) -> CatalogDecision;
}

#[derive(Debug, Error)]
pub enum ModelCatalogError {
    #[error("model catalog request failed with HTTP {0}{1}")]
    HttpStatus(u16, String),
    #[error("model catalog request was rate-limited")]
    RateLimited,
    #[error("model catalog network error: {0}")]
    Network(String),
    #[error("model catalog response did not decode: {0}")]
    Decode(String),
}

/// One catalog model, restricted to the fields the curated list needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    pub context_length: u64,
    pub pricing: CatalogPricing,
    pub supported_parameters: Vec<String>,
    pub architecture: CatalogArchitecture,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogPricing {
    pub prompt: String,
    pub completion: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogArchitecture {
    pub modality: String,
}

/// A cached catalog snapshot: the parsed models plus when they landed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedCatalog {
    pub fetched_at: SystemTime,
    pub models: Vec<CatalogModel>,
}

/// What the cache says for one base url.
#[derive(Debug, PartialEq)]
pub enum CatalogDecision {
    /// Within the TTL: serve it, no network.
    Fresh(Vec<CatalogModel>),
    /// Older than the TTL: serve it AND refresh in the background.
    Stale(Vec<CatalogModel>),
    /// Nothing cached (or the row failed to decode): fetch now.
    Miss,
}

/// `{base}/v1/models`. The base already ends in `/api`; appending the
/// documented `/api/v1/models` path would double it and 404.
fn models_url(base_url: &str) -> String {
    format!("{}/v1/models", base_url.trim_end_matches('/'))
}

/// The dynamic picker replaces the hand-maintained curated constant:
/// every fetched model that passes the mechanical bar is grouped into a
/// model family (vendor + base name before any variant suffix), and each
/// family contributes its top variants by completion price - the frontier
/// row plus the cheaper tiers below it (flash etc.). Families are ordered
/// by their frontier completion price, so the strongest models surface
/// first and a new release shows up on the next fetch without any
/// hand-maintained constant going stale.

/// Per-family variant cap: the frontier row plus its cheaper variants
/// (flash tiers etc.) under each family.
const VARIANTS_PER_FAMILY: usize = 3;

/// How many families the picker serves, strongest first.
const FAMILIES_SHOWN: usize = 8;

/// Family key for a model id: vendor plus the base name before the last
/// `-`-separated variant segment (`z-ai/glm-5.3-flash` -> `z-ai/glm-5.3`;
/// `anthropic/claude-fable-5.1` -> `anthropic/claude-fable`). Keeps each
/// family's frontier and its flash tiers together.
fn family_key(id: &str) -> String {
    /// Known variant suffixes: one trailing segment stripped when
    /// present, so a family's frontier and its cheaper tiers group
    /// together. Version segments (5.3, 5.1) are NOT variants and stay.
    const VARIANT_SUFFIXES: [&str; 6] = ["flash", "pro", "mini", "lite", "latest", "vision"];
    let (vendor, rest) = id.split_once('/').unwrap_or((id, ""));
    let lower = rest.to_lowercase();
    let base = VARIANT_SUFFIXES
        .iter()
        .find_map(|suffix| {
            lower.strip_suffix(suffix).and_then(|stripped| stripped.strip_suffix('-'))
        })
        .map_or(rest.to_owned(), |base| base.to_owned());
    format!("{vendor}/{base}")
}

/// Family display label: the vendor plus the base name, humanized.
fn family_label(key: &str) -> String {
    let (vendor, base) = key.split_once('/').unwrap_or(("", key));
    let vendor_label = match vendor {
        "z-ai" => "Z.ai",
        "x-ai" => "xAI",
        "moonshotai" => "Moonshot",
        other => other,
    };
    format!("{vendor_label} {base}")
}

/// Parse a catalog response body. Strict: a truncated or reshaped
/// payload errors rather than silently yielding an empty list.
pub fn parse_catalog(body: &[u8]) -> Result<Vec<CatalogModel>, ModelCatalogError> {
    #[derive(Deserialize)]
    struct Envelope {
        data: Vec<CatalogModel>,
    }
    serde_json::from_slice::<Envelope>(body)
        .map(|envelope| envelope.data)
        .map_err(|error| ModelCatalogError::Decode(error.to_string()))
}

/// The mechanical bar a model must pass to serve in the curated list:
/// 1M+ context, tool support, paid, text-out.
fn passes_mechanical_bar(model: &CatalogModel) -> bool {
    model.context_length >= 1_000_000
        && model.supported_parameters.iter().any(|parameter| parameter == "tools")
        && !model.id.ends_with(":free")
        && model.architecture.modality.rsplit("->").next() == Some("text")
}

/// Dollars per million tokens, from a decimal-dollars-per-token wire
/// string. `None` when the string does not parse.
fn per_million(price: &str) -> Option<f64> {
    price.trim().parse::<f64>().ok().map(|per_token| per_token * 1_000_000.0)
}

/// Compact context rendering: `1310720` -> `1.31M`, `1000000` -> `1M`.
fn compact_ctx(context_length: u64) -> String {
    // Context lengths are far below 2^52, so the cast cannot lose an
    // integer digit.
    #[allow(clippy::cast_precision_loss)]
    let millions = context_length as f64 / 1_000_000.0;
    let mut text = format!("{millions:.2}");
    if text.ends_with('0') {
        text.truncate(text.trim_end_matches('0').len());
    }
    if text.ends_with('.') {
        text.pop();
    }
    format!("{text}M")
}

/// Output price per million tokens: `$4.40`, `$0.25`, `$0.075`.
fn price_label(per_token_price: &str) -> Option<String> {
    let per_million = per_million(per_token_price)?;
    let text = if per_million >= 0.1 {
        format!("{per_million:.2}")
    } else {
        let mut text = format!("{per_million:.3}");
        while text.ends_with('0') {
            text.pop();
        }
        text
    };
    Some(format!("${text}"))
}

pub(crate) async fn fetch_catalog(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<Vec<CatalogModel>, ModelCatalogError> {
    let response = client
        .get(models_url(base_url))
        .send()
        .await
        .map_err(|error| ModelCatalogError::Network(error.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|error| ModelCatalogError::Network(format!("body read: {error}")))?;
    match status {
        200 => parse_catalog(&body).map_err(|error| {
            // A 200 that will not parse is the shape a wrong base url
            // takes: the bare host answers 200 with an HTML page.
            tracing::warn!(
                target: "forge_providers::model_catalog",
                url = %models_url(base_url),
                error = %error,
                body_suffix = %truncated_body_suffix(&body),
                "200 from the models endpoint did not decode; check the base url is the API root"
            );
            error
        }),
        429 => Err(ModelCatalogError::RateLimited),
        _ => Err(ModelCatalogError::HttpStatus(status, truncated_body_suffix(&body))),
    }
}

/// What the cache says for `base_url`, judged at `now`. An empty
/// models vec is the failure marker the caller writes when a fetch
/// fails with nothing cached: within [`CATALOG_FAILURE_TTL`] it reads
/// as fresh-empty (serve the discovered list, no network), after it as
/// stale-empty, which serves the same but retries the fetch in the
/// background - so the inline fetch happens only when nothing at all
/// is cached. A pathological `200` with zero models is stored the same
/// way and gets the same short retry cadence rather than a full-day
/// Fresh.
pub(crate) fn catalog_decision(cached: Option<CachedCatalog>, now: SystemTime) -> CatalogDecision {
    let Some(cached) = cached else {
        return CatalogDecision::Miss;
    };
    // A fetched_at in the future (clock moved back) reads as age zero,
    // i.e. fresh - serving beats refetching on skew.
    let age = now.duration_since(cached.fetched_at).unwrap_or_default();
    let ttl = if cached.models.is_empty() { CATALOG_FAILURE_TTL } else { CATALOG_TTL };
    if age < ttl {
        CatalogDecision::Fresh(cached.models)
    } else {
        CatalogDecision::Stale(cached.models)
    }
}

pub(crate) fn curated_available_models(catalog: &[CatalogModel]) -> Vec<AvailableModel> {
    // Mechanical bar first: 1M+ ctx, tools, paid, text-out.
    let eligible: Vec<&CatalogModel> =
        catalog.iter().filter(|m| passes_mechanical_bar(m)).collect();

    // Group into families, each kept sorted by completion price (the
    // capability proxy) so the frontier row is first.
    let mut families: Vec<(String, Vec<&CatalogModel>)> = Vec::new();
    for model in &eligible {
        let key = family_key(&model.id);
        match families.iter_mut().find(|(key_existing, _)| *key_existing == key) {
            Some((_, rows)) => {
                rows.push(model);
                rows.sort_by(|a, b| {
                    per_million(&b.pricing.completion)
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&per_million(&a.pricing.completion).unwrap_or(f64::INFINITY))
                });
            }
            None => families.push((key, vec![model])),
        }
    }

    // Strongest families first: frontier completion price.
    families.sort_by(|a, b| {
        let a_top = a.1.first().map_or(0.0, |m| per_million(&m.pricing.completion).unwrap_or(0.0));
        let b_top = b.1.first().map_or(0.0, |m| per_million(&m.pricing.completion).unwrap_or(0.0));
        b_top.partial_cmp(&a_top).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut rows = Vec::new();
    for (key, variants) in families.iter().take(FAMILIES_SHOWN) {
        let label = family_label(key);
        for (variant_idx, model) in variants.iter().take(VARIANTS_PER_FAMILY).enumerate() {
            let out_price = price_label(&model.pricing.completion).unwrap_or_default();
            let variant_tag = if variant_idx == 0 { "frontier" } else { "variant" };
            let description = format!(
                "{out_price}/M out - {} ctx - {}",
                compact_ctx(model.context_length),
                if entry_open(model) { "open weights" } else { "closed" },
            );
            let display = if variant_idx == 0 {
                format!("{} {} ({})", label, "frontier", entry_open_word(model))
            } else {
                model.name.clone()
            };
            let _ = variant_tag;
            rows.push(AvailableModel::new(&model.id, display).description(description));
        }
    }
    rows
}

/// Open-weights marker from the model id: DeepSeek, Qwen, Z.ai flash and
/// Meta models publish weights; the closed labs do not.
fn entry_open(model: &CatalogModel) -> bool {
    model.id.starts_with("z-ai/")
        || model.id.starts_with("deepseek/")
        || model.id.starts_with("qwen/")
        || model.id.starts_with("moonshotai/")
        || model.id.starts_with("minimax/")
        || model.id.starts_with("meta-llama/")
}

fn entry_open_word(model: &CatalogModel) -> &'static str {
    if entry_open(model) { "open weights" } else { "closed" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SPECIMEN: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/model_catalog.json"));

    fn specimen() -> Vec<CatalogModel> {
        parse_catalog(SPECIMEN.as_bytes()).expect("specimen parses")
    }

    /// A minimal-but-complete catalog row for hand-built payloads.
    fn row(id: &str, ctx: u64, tools: bool) -> CatalogModel {
        CatalogModel {
            id: id.to_owned(),
            name: "Test Model".to_owned(),
            context_length: ctx,
            pricing: CatalogPricing {
                prompt: "0.000001".to_owned(),
                completion: "0.000004".to_owned(),
            },
            supported_parameters: if tools { vec!["tools".to_owned()] } else { vec![] },
            architecture: CatalogArchitecture { modality: "text->text".to_owned() },
        }
    }

    // -- url join ----------------------------------------------------

    #[test]
    fn models_url_joins_one_v1_segment_onto_the_configured_base() {
        assert_eq!(models_url("https://openrouter.ai/api"), "https://openrouter.ai/api/v1/models");
    }

    #[test]
    fn models_url_tolerates_a_trailing_slash() {
        assert_eq!(models_url("https://openrouter.ai/api/"), "https://openrouter.ai/api/v1/models");
    }

    #[test]
    fn models_url_never_doubles_the_api_segment() {
        assert!(!models_url("https://openrouter.ai/api").contains("/api/api/"));
    }

    // -- parse -------------------------------------------------------

    #[test]
    fn parse_catalog_reads_the_live_capture_shape() {
        let models = specimen();
        assert_eq!(models.len(), 18, "the fixture carries the curated set + negatives");
        let glm = models.iter().find(|m| m.id == "z-ai/glm-5.3").expect("glm-5.3 present");
        assert_eq!(glm.name, "Z.ai: GLM 5.3");
        assert_eq!(glm.context_length, 1_310_720);
        assert_eq!(glm.pricing.completion, "0.0000044");
    }

    #[test]
    fn parse_catalog_rejects_a_truncated_payload() {
        let cut = &SPECIMEN.as_bytes()[..SPECIMEN.len() / 2];
        assert!(parse_catalog(cut).is_err(), "truncation errors, never an empty list");
    }

    #[test]
    fn parse_catalog_rejects_a_reshaped_payload() {
        let object_data = br#"{"data": {"id": "a/b"}}"#;
        assert!(parse_catalog(object_data).is_err(), "a non-array data member errors");
        let missing_pricing = br#"{"data": [{"id": "a/b", "name": "B", "context_length": 1,
            "supported_parameters": [], "architecture": {"modality": "text->text"}}]}"#;
        assert!(parse_catalog(missing_pricing).is_err(), "a row without pricing errors");
    }

    // -- mechanical bar ---------------------------------------------

    #[test]
    fn bar_passes_a_curated_shape() {
        let glm = specimen().into_iter().find(|m| m.id == "z-ai/glm-5.3").expect("present");
        assert!(passes_mechanical_bar(&glm));
    }

    #[test]
    fn bar_rejects_short_context_and_free_tiers() {
        let granite =
            specimen().into_iter().find(|m| m.id == "ibm-granite/granite-4.2-8b").expect("present");
        assert!(!passes_mechanical_bar(&granite), "131K context fails the bar");
        let free = specimen()
            .into_iter()
            .find(|m| m.id == "inclusionai/ling-3.0-flash-fin:free")
            .expect("present");
        assert!(!passes_mechanical_bar(&free), ":free fails the bar");
    }

    #[test]
    fn bar_rejects_models_without_tool_support() {
        let no_tools = row("vendor/model", 2_000_000, false);
        assert!(!passes_mechanical_bar(&no_tools));
    }

    #[test]
    fn bar_rejects_non_text_output_modalities() {
        let mut audio_out = row("vendor/model", 2_000_000, true);
        audio_out.architecture.modality = "text->text+audio".to_owned();
        assert!(!passes_mechanical_bar(&audio_out));
    }

    // -- dynamic picker ----------------------------------------------

    /// The strongest family leads the picker. The specimen's
    /// highest-completion-price eligible model is Fable 5.1
    /// ($50/M out), so the Anthropic family frontiers first.
    #[test]
    fn picker_serves_frontier_first_and_respects_the_variant_cap() {
        let rows = curated_available_models(&specimen());
        assert!(!rows.is_empty(), "the specimen yields rows");
        // GPT 5.5 Pro passes the bar at $180/M out - nothing beats it, so
        // the first row is the OpenAI frontier.
        assert_eq!(rows[0].id, "openai/gpt-5.5-pro", "frontier family first");
        // Per-family cap: no family contributes more than
        // VARIANTS_PER_FAMILY rows.
        let anthropic_rows = rows.iter().filter(|r| r.id.starts_with("anthropic/")).count();
        assert!(
            anthropic_rows <= VARIANTS_PER_FAMILY,
            "a family must not exceed the variant cap, got {anthropic_rows}"
        );
    }

    /// The dynamic picker serves rows for a fetched model with no
    /// hand-maintained entry: a new family release appears on the next
    /// fetch without any constant edit.
    #[test]
    fn picker_serves_models_without_a_curated_entry() {
        let rows = curated_available_models(&specimen());
        // The fixture carries openai/gpt-5.2 (400K ctx, tools) - below the
        // 1M bar, so it must NOT appear even though it is a known family.
        assert!(
            rows.iter().all(|r| r.id != "openai/gpt-5.2"),
            "models below the mechanical bar stay out of the picker"
        );
        // glm-5.3-flash IS in the fixture and passes: it must appear
        // even though no hand-written entry names it.
        assert!(
            rows.iter().any(|r| r.id == "z-ai/glm-5.3-flash"),
            "the flash variant rides its family's rows"
        );
    }

    // -- ttl decision ------------------------------------------------

    /// A snapshot fetched at a fixed epoch, judged `age` later.
    fn decision_for_age(age: Duration) -> CatalogDecision {
        let fetched_at = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let cached =
            CachedCatalog { fetched_at, models: vec![row("vendor/model", 2_000_000, true)] };
        catalog_decision(Some(cached), fetched_at + age)
    }

    #[test]
    fn fresh_cache_serves_without_a_fetch() {
        let age = CATALOG_TTL.checked_sub(Duration::from_secs(1)).expect("ttl exceeds 1s");
        assert!(matches!(decision_for_age(age), CatalogDecision::Fresh(_)));
    }

    #[test]
    fn stale_cache_serves_and_refreshes() {
        assert!(matches!(
            decision_for_age(CATALOG_TTL + Duration::from_secs(1)),
            CatalogDecision::Stale(_)
        ));
    }

    #[test]
    fn empty_cache_is_a_miss() {
        assert_eq!(catalog_decision(None, std::time::SystemTime::now()), CatalogDecision::Miss);
    }

    /// The failure marker (an empty models vec) lives on the short
    /// window: fresh within it, stale past it. `fresh-empty` serves the
    /// discovered list with no network; `stale-empty` does the same
    /// while a background retry is due.
    #[test]
    fn the_failure_marker_uses_the_short_window() {
        let fetched_at = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let marker = CachedCatalog { fetched_at, models: Vec::new() };
        let within = CATALOG_FAILURE_TTL.checked_sub(Duration::from_secs(1)).expect("ttl > 1s");
        assert!(
            matches!(catalog_decision(Some(marker.clone()), fetched_at + within), CatalogDecision::Fresh(models) if models.is_empty()),
            "an in-window marker is fresh-empty, not a fetch trigger"
        );
        let past = CATALOG_FAILURE_TTL + Duration::from_secs(1);
        assert!(
            matches!(catalog_decision(Some(marker), fetched_at + past), CatalogDecision::Stale(models) if models.is_empty()),
            "an expired marker is stale-empty, due for a background retry"
        );
    }

    // -- live fetch --------------------------------------------------

    /// One hermetic loopback round-trip: the url actually reached is
    /// `{base}/v1/models` and a 200 body parses.
    #[tokio::test]
    async fn fetch_catalog_reads_a_200_body() {
        let body = SPECIMEN.as_bytes().to_vec();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let request = read_request(&mut stream);
            assert!(
                request.contains("GET /v1/models HTTP/1.1"),
                "the request must hit /v1/models, got: {}",
                request.lines().next().unwrap_or_default()
            );
            reply_ok(&mut stream, &body);
        });
        let client = reqwest::Client::builder().build().expect("client");
        let models =
            fetch_catalog(&client, &format!("http://127.0.0.1:{port}")).await.expect("fetch");
        assert_eq!(models.len(), 18);
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let read = stream.read(&mut buf).unwrap_or(0);
        String::from_utf8_lossy(&buf[..read]).into_owned()
    }

    fn reply_ok(stream: &mut std::net::TcpStream, body: &[u8]) {
        use std::io::Write;
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(body);
    }
}

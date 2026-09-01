//! OpenRouter's public model catalog: fetch, parse, and the curated
//! `/model` list built on top of it.
//!
//! `GET {base}/v1/models` is public (no auth, free) and carries every
//! model the account can name. forge serves only the curated ten - a
//! maintained constant in this module - enriched with live price and
//! context figures from the fetch. Same URL-join lesson as
//! [`super::oauth_usage`]'s key url: `ANTHROPIC_BASE_URL` already ends
//! in `/api`, so only the `/v1/models` tail is appended.

use std::time::{Duration, SystemTime};

use forge_primitives::AvailableModel;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::http_trust;

/// Timeout for one catalog round-trip. Matches [`super::oauth_usage`]'s
/// probe budget; the endpoint is a static public list.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(8);

/// How long a cached catalog is served without a refetch.
pub const CATALOG_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a failed fetch is remembered, so an unreachable endpoint
/// costs its timeout at most once per window instead of on every
/// connect.
pub const CATALOG_FAILURE_TTL: Duration = Duration::from_secs(10 * 60);

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

/// Which quality band a curated entry sits in. Rendered in the picker
/// row's display name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// 93%+ on at least one independent harness.
    OpusClass,
    Strong,
    /// Reference models, kept marked and drop-eligible.
    ClosedReference,
}

impl Tier {
    fn label(self) -> &'static str {
        match self {
            Self::OpusClass => "Opus-class",
            Self::Strong => "Strong",
            Self::ClosedReference => "Closed reference",
        }
    }
}

/// One curated entry: what Ved locked on 2026-09-01, minus the price
/// and context figures, which come live from the fetch so the rows
/// never go stale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CuratedModel {
    pub slug: &'static str,
    pub tier: Tier,
    pub benchmark: Option<&'static str>,
    pub source: Option<&'static str>,
    /// Date the benchmark figures were researched, `YYYY-MM-DD`.
    pub researched: &'static str,
    pub open: bool,
    /// Row-level warning, e.g. a cost note.
    pub note: Option<&'static str>,
}

/// The curated ten, in picker order. Benchmarks are SWE-bench Verified
/// unless the string says otherwise. Maintained by hand; the
/// `curated_constant_entries_pass_the_mechanical_bar` test holds every
/// entry against the live capture.
pub const CURATED: &[CuratedModel] = &[
    CuratedModel {
        slug: "z-ai/glm-5.3",
        tier: Tier::OpusClass,
        benchmark: Some("SWE-bench V 97%"),
        source: Some("vals.ai"),
        researched: "2026-09-01",
        open: true,
        note: None,
    },
    CuratedModel {
        slug: "deepseek/deepseek-v4-pro-0813",
        tier: Tier::OpusClass,
        benchmark: Some("SWE-bench V 96.4% / 80.6%"),
        source: Some("anotherwrapper / benchlm"),
        researched: "2026-09-01",
        open: true,
        note: None,
    },
    CuratedModel {
        slug: "moonshotai/kimi-k3",
        tier: Tier::OpusClass,
        benchmark: Some("SWE-bench V 93.4%"),
        source: Some("anotherwrapper"),
        researched: "2026-09-01",
        open: true,
        note: Some("one heavy session can consume the account's monthly cap"),
    },
    CuratedModel {
        slug: "z-ai/glm-5.3-flash",
        tier: Tier::OpusClass,
        benchmark: Some("~93%"),
        source: Some("vals.ai, independent"),
        researched: "2026-09-01",
        open: true,
        note: Some("Z.ai launch: Terminal-Bench 2.1 84.3 (Opus 4.8 85.0); DeepSWE 63.4 (58.0)"),
    },
    CuratedModel {
        slug: "deepseek/deepseek-v4-flash",
        tier: Tier::Strong,
        benchmark: Some("SWE-bench V 91%"),
        source: Some("vals.ai"),
        researched: "2026-09-01",
        open: true,
        note: None,
    },
    CuratedModel {
        slug: "minimax/minimax-m3",
        tier: Tier::Strong,
        benchmark: Some("~81%"),
        source: None,
        researched: "2026-09-01",
        open: true,
        note: None,
    },
    CuratedModel {
        slug: "z-ai/glm-5.2",
        tier: Tier::Strong,
        benchmark: Some("78.7%"),
        source: None,
        researched: "2026-09-01",
        open: true,
        note: None,
    },
    CuratedModel {
        slug: "google/gemini-2.5-flash",
        tier: Tier::ClosedReference,
        benchmark: None,
        source: None,
        researched: "2026-09-01",
        open: false,
        note: None,
    },
    CuratedModel {
        slug: "x-ai/grok-4.3",
        tier: Tier::ClosedReference,
        benchmark: None,
        source: None,
        researched: "2026-09-01",
        open: false,
        note: None,
    },
    CuratedModel {
        slug: "deepseek/deepseek-v4-pro",
        tier: Tier::ClosedReference,
        benchmark: None,
        source: None,
        researched: "2026-09-01",
        open: true,
        note: None,
    },
];

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

pub async fn fetch_catalog(base_url: &str) -> Result<Vec<CatalogModel>, ModelCatalogError> {
    let client = http_trust::with_extra_roots(reqwest::Client::builder().timeout(CATALOG_TIMEOUT))
        .build()
        .map_err(|error| ModelCatalogError::Network(format!("client build: {error}")))?;
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
                target: "forge_agent::cloud::model_catalog",
                url = %models_url(base_url),
                error = %error,
                body_suffix = %super::oauth_usage::truncated_body_suffix(&body),
                "200 from the models endpoint did not decode; check the base url is the API root"
            );
            error
        }),
        429 => Err(ModelCatalogError::RateLimited),
        _ => Err(ModelCatalogError::HttpStatus(
            status,
            super::oauth_usage::truncated_body_suffix(&body),
        )),
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
pub fn catalog_decision(cached: Option<CachedCatalog>, now: SystemTime) -> CatalogDecision {
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

pub fn curated_available_models(catalog: &[CatalogModel]) -> Vec<AvailableModel> {
    CURATED
        .iter()
        .filter_map(|entry| {
            let model = catalog.iter().find(|model| model.id == entry.slug)?;
            if !passes_mechanical_bar(model) {
                return None;
            }
            let out_price = price_label(&model.pricing.completion)?;
            let mut description = format!(
                "{} - {out_price}/M out - {} ctx - {}",
                entry_benchmark(entry),
                compact_ctx(model.context_length),
                if entry.open { "open" } else { "closed" },
            );
            if let Some(note) = entry.note {
                description.push_str(" - ");
                description.push_str(note);
            }
            Some(
                AvailableModel::new(entry.slug, format!("{} ({})", model.name, entry.tier.label()))
                    .description(description),
            )
        })
        .collect()
}

fn entry_benchmark(entry: &CuratedModel) -> String {
    match (entry.benchmark, entry.source) {
        (Some(benchmark), Some(source)) => format!("{benchmark} ({source})"),
        (Some(benchmark), None) => benchmark.to_owned(),
        _ => String::new(),
    }
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
        assert_eq!(models.len(), 12, "the fixture carries ten curated + two negatives");
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

    // -- curated constant -------------------------------------------

    /// The mechanical bar gates what may enter the curated list. Every
    /// slug must exist in the live capture and pass there - a constant
    /// edited to include a 262K or free model fails here.
    #[test]
    fn curated_constant_entries_pass_the_mechanical_bar() {
        let models = specimen();
        for entry in CURATED {
            let model = models
                .iter()
                .find(|m| m.id == entry.slug)
                .unwrap_or_else(|| panic!("curated slug {} missing from the capture", entry.slug));
            assert!(passes_mechanical_bar(model), "{} must pass the mechanical bar", entry.slug);
        }
    }

    #[test]
    fn curated_rows_map_the_catalog_in_constant_order() {
        let rows = curated_available_models(&specimen());
        assert_eq!(rows.len(), 10, "every curated slug present in the capture maps to a row");
        for (row, entry) in rows.iter().zip(CURATED.iter()) {
            assert_eq!(row.id, entry.slug, "constant order is preserved");
        }
        let first = &rows[0];
        assert_eq!(first.display_name, "Z.ai: GLM 5.3 (Opus-class)");
        let description = first.description.as_deref().expect("curated rows carry a description");
        assert!(description.contains("97%"), "benchmark score shown");
        assert!(description.contains("vals.ai"), "benchmark source shown");
        assert!(description.contains("$4.40"), "output price shown");
        assert!(description.contains("1.31M"), "context shown");
        assert!(description.contains("open"), "openness marker shown");
    }

    #[test]
    fn curated_rows_carry_the_kimi_cost_note() {
        let rows = curated_available_models(&specimen());
        let kimi = rows.iter().find(|r| r.id == "moonshotai/kimi-k3").expect("present");
        let description = kimi.description.as_deref().expect("description");
        assert!(
            description.contains("one heavy session can consume the account's monthly cap"),
            "the cost note must be shown, got: {description}"
        );
    }

    #[test]
    fn curated_rows_show_harness_variance_for_deepseek_pro_0813() {
        let rows = curated_available_models(&specimen());
        let deepseek =
            rows.iter().find(|r| r.id == "deepseek/deepseek-v4-pro-0813").expect("present");
        let description = deepseek.description.as_deref().expect("description");
        assert!(description.contains("96.4%") && description.contains("80.6%"));
        assert!(description.contains("anotherwrapper") && description.contains("benchlm"));
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
        let models =
            fetch_catalog(&format!("http://127.0.0.1:{port}")).await.expect("fetch succeeds");
        assert_eq!(models.len(), 12);
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

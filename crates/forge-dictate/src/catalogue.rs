//! The transcribe.cpp catalogue: the per-variant feed the runtime
//! publishes.
//!
//! Each variant is one JSON document (`catalog/<variant>.json`) carrying
//! the model's identity, its downloads, and the measured accuracy and
//! speed rows. Parsing is tolerant by design: upstream grows the schema,
//! so unknown fields are ignored and the optional blocks default, while a
//! document from another schema is refused by name rather than
//! half-read.

use serde::Deserialize;

use crate::Error;

/// The schema string every entry this build reads carries.
pub const SCHEMA: &str = "transcribe-catalog-v1";

/// The quants a headline accuracy figure is read at, most preferred
/// first: the reference quants the catalogue measures its headline rows
/// at, so a comparison across variants reads the same axis.
const REFERENCE_QUANTS: [&str; 3] = ["Q8_0", "F16", "BF16"];

/// One variant's catalogue entry.
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogueEntry {
    pub variant: String,
    /// Missing falls back to the variant itself.
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub family: String,
    /// Parameter count of the upstream checkpoint.
    #[serde(default)]
    pub params: u64,
    #[serde(default)]
    pub license: Option<License>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub headline_benchmark: Option<HeadlineBenchmark>,
    #[serde(default)]
    pub downloads: Vec<Download>,
    #[serde(default)]
    pub speed_benchmarks: Vec<SpeedRow>,
    #[serde(default)]
    pub accuracy_benchmarks: Vec<AccuracyRow>,
}

impl CatalogueEntry {
    /// Whether the variant supports streaming recognition.
    pub fn streaming(&self) -> bool {
        self.capabilities.streaming.supported
    }

    /// The download carrying this exact file name, which is how a pinned
    /// model is joined to its entry.
    pub fn download_for(&self, filename: &str) -> Option<&Download> {
        self.downloads.iter().find(|download| download.filename == filename)
    }

    /// The best m4-max Metal wall-clock realtime factor across the
    /// quantised speed rows. `None` when the feed carries no such row.
    pub fn m4_metal_xrt_wall(&self) -> Option<f64> {
        self.speed_benchmarks
            .iter()
            .filter(|row| row.machine == "m4-max" && row.backend == "metal")
            .filter_map(|row| row.xrt_wall)
            .reduce(f64::max)
    }

    /// Word error rate on FLEURS English at the reference quant.
    pub fn fleurs_en_wer(&self) -> Option<f64> {
        self.wer_for("fleurs", "test", "en")
    }

    /// Word error rate on the entry's own headline benchmark.
    pub fn headline_wer(&self) -> Option<f64> {
        let headline = self.headline_benchmark.as_ref()?;
        self.wer_for(&headline.dataset, &headline.split, &headline.language)
    }

    fn wer_for(&self, dataset: &str, split: &str, language: &str) -> Option<f64> {
        REFERENCE_QUANTS.iter().find_map(|quant| {
            self.accuracy_benchmarks
                .iter()
                .find(|row| {
                    row.dataset == dataset
                        && row.split == split
                        && row.language == language
                        && &row.quant == quant
                })
                .and_then(|row| row.err_pct)
        })
    }
}

/// The licence block, in the two forms the feed carries.
#[derive(Debug, Clone, Deserialize)]
pub struct License {
    #[serde(default)]
    pub spdx: String,
    #[serde(default)]
    pub display: String,
}

/// What a variant can do, as far as the feed verifies it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub streaming: Support,
}

/// One capability's support flag.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Support {
    #[serde(default)]
    pub supported: bool,
}

/// The benchmark a variant's headline figure comes from.
#[derive(Debug, Clone, Deserialize)]
pub struct HeadlineBenchmark {
    #[serde(default)]
    pub dataset: String,
    #[serde(default)]
    pub split: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub metric: String,
}

/// One downloadable quantisation of the variant.
#[derive(Debug, Clone, Deserialize)]
pub struct Download {
    #[serde(default)]
    pub quant: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub size_bytes: u64,
}

/// One measured speed row. Many fields cross that nothing reads; the
/// wall-clock realtime factor is the one the page compares on.
#[derive(Debug, Clone, Deserialize)]
pub struct SpeedRow {
    #[serde(default)]
    pub machine: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub quant: String,
    #[serde(default)]
    pub xrt_wall: Option<f64>,
}

/// One measured accuracy row.
#[derive(Debug, Clone, Deserialize)]
pub struct AccuracyRow {
    #[serde(default)]
    pub dataset: String,
    #[serde(default)]
    pub split: String,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub quant: String,
    #[serde(default)]
    pub err_pct: Option<f64>,
}

/// Parse one catalogue document, refusing anything that is not an entry
/// of the schema this build reads.
pub fn parse_entry(raw: &str) -> Result<CatalogueEntry, Error> {
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|source| {
        Error::Catalogue { message: format!("the document is not JSON: {source}") }
    })?;
    match value.get("schema").and_then(serde_json::Value::as_str) {
        Some(SCHEMA) => {}
        Some(other) => {
            return Err(Error::Catalogue {
                message: format!(
                    "the document's schema is {other:?}, and this build reads {SCHEMA:?}"
                ),
            });
        }
        None => {
            return Err(Error::Catalogue {
                message: format!("the document carries no {SCHEMA:?} schema"),
            });
        }
    }
    let mut entry: CatalogueEntry = serde_json::from_value(value).map_err(|source| {
        Error::Catalogue { message: format!("the entry does not parse: {source}") }
    })?;
    if entry.display_name.is_empty() {
        entry.display_name.clone_from(&entry.variant);
    }
    Ok(entry)
}

#[cfg(test)]
mod tests_catalogue {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/fixtures/catalogue/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).expect("the fixture must be readable")
    }

    /// The identity facts the page's IN USE and FIND A MODEL rows draw.
    #[test]
    fn a_real_entry_parses_into_the_identity_the_page_draws() {
        let entry = parse_entry(&fixture("cohere-transcribe-03-2026.json"))
            .expect("the published entry must parse");

        assert_eq!(entry.variant, "cohere-transcribe-03-2026");
        assert_eq!(entry.params, 2_049_026_832, "the parameter count is the feed's own");
        assert_eq!(
            entry.license.as_ref().map(|l| l.display.as_str()),
            Some("Apache-2.0"),
            "the licence crosses in the form the feed displays"
        );
        assert_eq!(entry.languages.len(), 14, "every language the feed lists is carried");
        assert!(entry.languages.iter().any(|l| l == "en"));
        assert!(!entry.streaming(), "cohere-transcribe is not a streaming variant");
    }

    /// The pinned file is joined to the catalogue by name and size: this is
    /// what makes an update check possible at all.
    #[test]
    fn the_pinned_file_is_found_among_the_downloads() {
        let entry = parse_entry(&fixture("cohere-transcribe-03-2026.json")).expect("parse");

        let download = entry
            .download_for("cohere-transcribe-03-2026-Q4_K_M.gguf")
            .expect("the pinned quant must be carried");
        assert_eq!(download.quant, "Q4_K_M");
        assert_eq!(
            download.size_bytes, 1_558_162_944,
            "the size is the witness the pin is joined on"
        );

        assert!(
            entry.download_for("nothing-ships-this-name.gguf").is_none(),
            "a name the feed does not carry must answer None, not a guess"
        );
    }

    /// A document from a schema this build does not know is refused by
    /// name: half-reading a future entry is how a page draws wrong facts.
    #[test]
    fn an_entry_from_another_schema_is_refused_by_name() {
        let err = parse_entry(r#"{"schema": "transcribe-catalog-v2", "variant": "x"}"#)
            .expect_err("a foreign schema must be refused");
        assert!(
            err.to_string().contains("transcribe-catalog-v2"),
            "the refusal must name the schema it saw, got: {err}"
        );
    }

    /// Anything that is not an entry at all is a refusal, never a
    /// half-defaulted row on a page. Each refusal names what it saw.
    #[test]
    fn a_document_that_is_not_an_entry_is_refused_rather_than_half_read() {
        let err =
            parse_entry(r#"{"hello": 1}"#).expect_err("a document with no schema is not an entry");
        assert!(
            err.to_string().contains("schema"),
            "the refusal must say the document carries no catalogue schema, got: {err}"
        );

        let err = parse_entry(r#"{"schema": "transcribe-catalog-v1"}"#)
            .expect_err("a right-schema document with no variant is not usable");
        assert!(
            err.to_string().contains("variant"),
            "the refusal must say which field was missing, got: {err}"
        );
        assert!(parse_entry("not json").is_err(), "a non-document must be refused too");
    }

    /// The measured numbers the page compares on come off the rows
    /// themselves, and both fixtures pin the real figures.
    #[test]
    fn the_measured_facts_come_off_the_rows_the_page_compares_on() {
        let cohere = parse_entry(&fixture("cohere-transcribe-03-2026.json")).expect("parse");
        assert!(
            (cohere.m4_metal_xrt_wall().expect("a metal speed row") - 72.88).abs() < 1e-9,
            "cohere's m4-max metal xrt_wall, got {:?}",
            cohere.m4_metal_xrt_wall()
        );
        assert_eq!(cohere.fleurs_en_wer(), Some(5.08), "FLEURS-en at the reference quant");
        assert_eq!(
            cohere.headline_wer(),
            Some(1.27),
            "the headline row at the reference quant, not whichever row the file lists first"
        );

        let granite =
            parse_entry(&fixture("granite-speech-5.0-470m-turboctc.json")).expect("parse");
        assert!(
            (granite.m4_metal_xrt_wall().expect("a metal speed row") - 388.76).abs() < 1e-9,
            "granite's speed is the number the update line shows beside cohere's"
        );
        assert_eq!(granite.fleurs_en_wer(), Some(4.61));
        assert_eq!(granite.headline_wer(), Some(1.33));
    }

    /// Upstream grows the schema; an entry carrying only the required
    /// fields must still parse, with the optional facts empty.
    #[test]
    fn an_entry_with_only_the_required_fields_parses_with_empty_optionals() {
        let entry = parse_entry(r#"{"schema": "transcribe-catalog-v1", "variant": "tiny"}"#)
            .expect("the required fields are all it must carry");

        assert_eq!(entry.variant, "tiny");
        assert_eq!(entry.display_name, "tiny", "a missing display name falls back to the variant");
        assert!(entry.languages.is_empty());
        assert!(entry.downloads.is_empty());
        assert_eq!(entry.m4_metal_xrt_wall(), None, "no rows, no number");
        assert_eq!(entry.fleurs_en_wer(), None);
        assert!(!entry.streaming(), "absent capabilities are not a claim of support");
    }
}

//! The Hugging Face Hub calls the speaking model's install makes: the
//! repository's current revision, its blobs with their LFS digests, and the
//! download of one pinned file.
//!
//! The token is carried for a gated repository; the speaking model's own
//! repository is open, so an absent token changes nothing.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The Hub's connect and read bounds, a whole-request backstop sized so the
/// largest variant (325 MB at 0.4 Mbps) still finishes, and the page cap on
/// one listing before the install gives up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_TREE_PAGES: usize = 20;

/// One file the repository publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubBlob {
    pub path: String,
    pub size: u64,
    /// The LFS digest the repository publishes, when the file is stored by
    /// LFS.
    pub sha256: Option<String>,
}

/// The files one speaking variant is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakingFiles {
    pub model: HubBlob,
    pub voices: Vec<HubBlob>,
}

/// A Hub client with the optional token every request carries.
pub struct Hub {
    client: reqwest::Client,
    token: Option<String>,
}

/// The Hub calls one speaking install makes, as a seam a test can stand in
/// for. Production holds [`Hub`]; the install path holds a `dyn HubPort`.
#[async_trait::async_trait]
pub trait HubPort: Send + Sync {
    async fn revision(&self, repo: &str) -> Result<String, String>;
    async fn blobs(&self, repo: &str, revision: &str) -> Result<Vec<HubBlob>, String>;
    async fn download(
        &self,
        repo: &str,
        revision: &str,
        blob: &HubBlob,
        dest: &Path,
        on_bytes: &mut (dyn FnMut(u64) + Send),
    ) -> Result<(), String>;
}

#[async_trait::async_trait]
impl HubPort for Hub {
    async fn revision(&self, repo: &str) -> Result<String, String> {
        Hub::revision(self, repo).await
    }

    async fn blobs(&self, repo: &str, revision: &str) -> Result<Vec<HubBlob>, String> {
        Hub::blobs(self, repo, revision).await
    }

    async fn download(
        &self,
        repo: &str,
        revision: &str,
        blob: &HubBlob,
        dest: &Path,
        on_bytes: &mut (dyn FnMut(u64) + Send),
    ) -> Result<(), String> {
        Hub::download(self, repo, revision, blob, dest, on_bytes).await
    }
}

impl Hub {
    pub fn new(token: Option<String>) -> Result<Self, String> {
        // A stalled Hub must fail the install rather than hold it in
        // `Downloading` forever; the total is a backstop for one big file.
        // The trust roots are the machine's, so a corporate MITM CA in
        // `NODE_EXTRA_CA_CERTS` or `SSL_CERT_FILE` reaches the Hub too.
        let client = forge_agent::http_trust::with_extra_roots(
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .timeout(TOTAL_TIMEOUT),
        )
        .build()
        .map_err(|error| format!("the Hub client did not build: {error}"))?;
        Ok(Self { client, token })
    }

    /// One authorised GET, its status checked.
    async fn get(&self, url: &str) -> Result<reqwest::Response, String> {
        let request = self.client.get(url);
        let request = match self.token.as_deref() {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        let response = request
            .send()
            .await
            .map_err(|error| format!("the Hub did not answer for {url}: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("the Hub answered {} for {url}", response.status()));
        }
        Ok(response)
    }

    /// The repository's current revision sha. Every file URL is pinned to it,
    /// so one install fetches one coherent set.
    pub async fn revision(&self, repo: &str) -> Result<String, String> {
        let url = format!("https://huggingface.co/api/models/{repo}");
        let response = self.get(&url).await?;
        let info: ModelInfo = response
            .json()
            .await
            .map_err(|error| format!("the Hub's answer for {repo} did not parse: {error}"))?;
        Ok(info.sha)
    }

    /// Every file the revision publishes, with its LFS digest.
    ///
    /// **Recursive, because the model and its voices are not at the root.**
    /// The plain tree call answers the root's own entries - `onnx` and
    /// `voices` as directories - so a reader that dropped directories would
    /// find no model at all. A page that caps follows its `Link` header.
    pub async fn blobs(&self, repo: &str, revision: &str) -> Result<Vec<HubBlob>, String> {
        let mut url = tree_url(repo, revision);
        let mut blobs = Vec::new();
        for _ in 0..MAX_TREE_PAGES {
            let response = self.get(&url).await?;
            let next = response
                .headers()
                .get(reqwest::header::LINK)
                .and_then(|link| link.to_str().ok())
                .and_then(next_page);
            let raw = response
                .bytes()
                .await
                .map_err(|error| format!("the Hub's listing for {repo} did not read: {error}"))?;
            blobs.extend(parse_blobs(&raw)?);
            match next {
                Some(next) => url = next,
                None => return Ok(blobs),
            }
        }
        Err(format!("the Hub's listing for {repo} did not finish within {MAX_TREE_PAGES} pages"))
    }

    /// Fetch one pinned file, reporting the bytes written so far.
    ///
    /// The file lands at `dest` only once its byte length and, where the
    /// repository publishes one, its digest match; a mismatch leaves nothing
    /// behind.
    pub async fn download(
        &self,
        repo: &str,
        revision: &str,
        blob: &HubBlob,
        dest: &Path,
        mut on_bytes: impl FnMut(u64),
    ) -> Result<(), String> {
        let url = format!("https://huggingface.co/{repo}/resolve/{revision}/{}", blob.path);
        let request = self.client.get(&url);
        let request = match self.token.as_deref() {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        let mut response = request
            .send()
            .await
            .map_err(|error| format!("{} did not fetch: {error}", blob.path))?;
        if !response.status().is_success() {
            return Err(format!("{} answered {}", blob.path, response.status()));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let part = part_path(dest);
        let mut file =
            std::fs::File::create(&part).map_err(|error| format!("{}: {error}", part.display()))?;
        let mut hasher = Sha256::new();
        let mut written = 0_u64;
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => {
                    let _ = std::fs::remove_file(&part);
                    return Err(format!("{} did not finish: {error}", blob.path));
                }
            };
            if let Err(error) = file.write_all(&chunk) {
                let _ = std::fs::remove_file(&part);
                return Err(format!("{}: {error}", part.display()));
            }
            hasher.update(&chunk);
            written += chunk.len() as u64;
            on_bytes(written);
        }
        drop(file);
        finish_file(&part, dest, blob, written, &hex::encode(hasher.finalize()))
    }
}

/// Where a download is written while it is being checked.
fn part_path(dest: &Path) -> std::path::PathBuf {
    let name =
        dest.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    dest.with_file_name(format!("{name}.part"))
}

/// Check a finished download and move it into place, or remove it.
pub fn finish_file(
    part: &Path,
    dest: &Path,
    blob: &HubBlob,
    written: u64,
    digest: &str,
) -> Result<(), String> {
    if written != blob.size {
        let _ = std::fs::remove_file(part);
        return Err(format!(
            "{} is {written} bytes and the entry declares {}",
            blob.path, blob.size
        ));
    }
    if let Some(expected) = blob.sha256.as_deref()
        && !digest.eq_ignore_ascii_case(expected)
    {
        let _ = std::fs::remove_file(part);
        return Err(format!("{} declares sha256 {expected}, and the file is {digest}", blob.path));
    }
    std::fs::rename(part, dest).map_err(|error| format!("{}: {error}", dest.display()))
}

/// The listing URL for one revision: recursive, because the model and its
/// voices sit under `onnx/` and `voices/`.
pub fn tree_url(repo: &str, revision: &str) -> String {
    format!("https://huggingface.co/api/models/{repo}/tree/{revision}?recursive=true&limit=1000")
}

/// The next page a `Link` header names, when it names one.
pub fn next_page(link: &str) -> Option<String> {
    for part in link.split(',') {
        if !part.contains("rel=\"next\"") {
            continue;
        }
        let start = part.find('<')?;
        let end = start + 1 + part[start + 1..].find('>')?;
        return Some(part[start + 1..end].to_owned());
    }
    None
}

/// Refuse a listing whose model file disagrees with the compiled variant.
///
/// The digest the install checks a download against comes from this same
/// listing, so without this the compile-time pin would be display only: a
/// replaced upstream file would install under a new revision while the page
/// still drew the old one.
pub fn verify_release(
    variant: &crate::tts::SpeakingVariant,
    model: &HubBlob,
) -> Result<(), String> {
    if model.size != variant.size {
        return Err(format!(
            "{} is {} bytes at this revision and the compiled entry declares {}",
            variant.path, model.size, variant.size
        ));
    }
    match model.sha256.as_deref() {
        Some(digest) if digest.eq_ignore_ascii_case(variant.sha256) => Ok(()),
        Some(digest) => Err(format!(
            "{} declares sha256 {digest} at this revision and the compiled entry declares {}",
            variant.path, variant.sha256
        )),
        None => Err(format!(
            "{} publishes no digest at this revision, so the compiled one cannot be checked",
            variant.path
        )),
    }
}

#[derive(Deserialize)]
struct ModelInfo {
    sha: String,
}

#[derive(Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    size: Option<u64>,
    lfs: Option<Lfs>,
}

#[derive(Deserialize)]
struct Lfs {
    oid: String,
}

/// The files a tree listing carries, directories dropped.
pub fn parse_blobs(raw: &[u8]) -> Result<Vec<HubBlob>, String> {
    let entries: Vec<TreeEntry> = serde_json::from_slice(raw)
        .map_err(|error| format!("the Hub's listing did not parse: {error}"))?;
    Ok(entries
        .into_iter()
        .filter(|entry| entry.kind == "file")
        .map(|entry| HubBlob {
            size: entry.size.unwrap_or(0),
            sha256: entry.lfs.map(|lfs| lfs.oid),
            path: entry.path,
        })
        .collect())
}

/// The voice id a style file's path names. Every named voice carries a
/// `<language><gender>_<name>` stem; the repository's `voices/af.bin` blend
/// does not, and is not one of the model's named voices.
pub fn voice_id(path: &str) -> Option<&str> {
    let file = std::path::Path::new(path.strip_prefix("voices/")?);
    if !file.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("bin")) {
        return None;
    }
    let name = file.file_stem()?.to_str()?;
    name.contains('_').then_some(name)
}

/// The model file and the style files one variant is made of.
pub fn speaking_files(blobs: &[HubBlob], variant_path: &str) -> Result<SpeakingFiles, String> {
    let model = blobs
        .iter()
        .find(|blob| blob.path == variant_path)
        .cloned()
        .ok_or_else(|| format!("the repository publishes no {variant_path}"))?;
    let voices: Vec<HubBlob> =
        blobs.iter().filter(|blob| voice_id(&blob.path).is_some()).cloned().collect();
    if voices.is_empty() {
        return Err("the repository publishes no voice files".to_owned());
    }
    Ok(SpeakingFiles { model, voices })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{HubBlob, finish_file, parse_blobs, speaking_files, voice_id};

    fn listing() -> Vec<HubBlob> {
        parse_blobs(
            br#"[
                {"type":"file","path":"onnx/model_quantized.onnx","size":92,"lfs":{"oid":"aa"}},
                {"type":"directory","path":"voices","size":0},
                {"type":"file","path":"voices/af_heart.bin","size":5,"lfs":{"oid":"bb"}},
                {"type":"file","path":"voices/af.bin","size":9,"lfs":{"oid":"cc"}},
                {"type":"file","path":"README.md","size":10}
            ]"#,
        )
        .expect("the listing parses")
    }

    /// The plain tree call answers only the root's directories; a listing
    /// that is not recursive is an install that finds no model at all.
    #[test]
    fn the_tree_url_is_recursive() {
        let url = super::tree_url("onnx-community/Kokoro-82M-v1.0-ONNX", "abc123");
        assert!(url.contains("/tree/abc123?recursive=true"), "got: {url}");
    }

    #[test]
    fn a_link_header_names_the_next_page_or_none() {
        let next = super::next_page(
            "<https://huggingface.co/api/models/o/r/tree/abc?recursive=true&cursor=xyz>; rel=\"next\"",
        );
        assert_eq!(
            next.as_deref(),
            Some("https://huggingface.co/api/models/o/r/tree/abc?recursive=true&cursor=xyz")
        );
        assert_eq!(super::next_page("<https://x>; rel=\"prev\""), None);
    }

    /// A recursive listing is flat: the model and its voices arrive by path
    /// even though the root carries them as directories.
    #[test]
    fn a_recursive_listing_finds_the_nested_model_and_voices() {
        let blobs = parse_blobs(
            br#"[
                {"type":"directory","path":"onnx","size":0},
                {"type":"directory","path":"voices","size":0},
                {"type":"file","path":"onnx/model_quantized.onnx","size":92,"lfs":{"oid":"aa"}},
                {"type":"file","path":"voices/af_heart.bin","size":5,"lfs":{"oid":"bb"}},
                {"type":"file","path":"voices/bf_emma.bin","size":5,"lfs":{"oid":"cc"}}
            ]"#,
        )
        .expect("the listing parses");
        let files = speaking_files(&blobs, "onnx/model_quantized.onnx").expect("the set");
        assert_eq!(files.model.path, "onnx/model_quantized.onnx");
        assert_eq!(files.voices.len(), 2);
    }

    /// The compiled variant's size and digest are the pin: a release that
    /// disagrees is refused by the file it names.
    #[test]
    fn a_release_that_disagrees_with_the_compiled_pin_is_refused() {
        let variant = crate::tts::SpeakingVariant::named(crate::tts::PINNED_VARIANT)
            .expect("the pinned variant exists");
        let pinned = HubBlob {
            path: variant.path.to_owned(),
            size: variant.size,
            sha256: Some(variant.sha256.to_owned()),
        };
        super::verify_release(&variant, &pinned).expect("the compiled release is accepted");

        let resized = HubBlob { size: variant.size + 1, ..pinned.clone() };
        let error = super::verify_release(&variant, &resized).expect_err("a different size");
        assert!(error.contains(variant.path), "got: {error}");

        let rehashed = HubBlob { sha256: Some("00".repeat(32)), ..pinned.clone() };
        let error = super::verify_release(&variant, &rehashed).expect_err("a different digest");
        assert!(error.contains("sha256"), "got: {error}");

        let undigested = HubBlob { sha256: None, ..pinned };
        let error = super::verify_release(&variant, &undigested).expect_err("no digest");
        assert!(error.contains("no digest"), "got: {error}");
    }

    #[test]
    fn a_tree_listing_drops_directories_and_keeps_lfs_digests() {
        let blobs = listing();
        assert_eq!(blobs.len(), 4, "the directory is not a file");
        let model = blobs
            .iter()
            .find(|blob| {
                std::path::Path::new(&blob.path)
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("onnx"))
            })
            .expect("the model");
        assert_eq!(model.size, 92);
        assert_eq!(model.sha256.as_deref(), Some("aa"));
        let readme = blobs.iter().find(|blob| blob.path == "README.md").expect("the readme");
        assert_eq!(readme.sha256, None, "a file outside LFS has no digest");
    }

    #[test]
    fn a_named_voice_is_a_stem_with_a_language_gender_prefix() {
        assert_eq!(voice_id("voices/af_heart.bin"), Some("af_heart"));
        assert_eq!(voice_id("voices/af.bin"), None, "the blend is not a named voice");
        assert_eq!(voice_id("onnx/model.onnx"), None);
    }

    #[test]
    fn a_variant_is_its_model_file_and_every_named_voice() {
        let files = speaking_files(&listing(), "onnx/model_quantized.onnx").expect("the set");
        assert_eq!(files.model.path, "onnx/model_quantized.onnx");
        assert_eq!(files.voices.len(), 1, "only the named voice, not the blend");
        assert_eq!(files.voices[0].path, "voices/af_heart.bin");
    }

    #[test]
    fn a_variant_the_repository_does_not_publish_is_refused_by_name() {
        let error = speaking_files(&listing(), "onnx/model_q4.onnx").expect_err("refused");
        assert!(error.contains("onnx/model_q4.onnx"), "got: {error}");
    }

    #[test]
    fn a_download_whose_digest_disagrees_is_taken_back() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let part = dir.path().join("model.onnx.part");
        let dest = dir.path().join("model.onnx");
        std::fs::write(&part, b"weights").expect("the part is written");
        let blob =
            HubBlob { path: "onnx/model.onnx".to_owned(), size: 7, sha256: Some("00".repeat(32)) };
        let error =
            finish_file(&part, &dest, &blob, 7, "11".repeat(32).as_str()).expect_err("refused");
        assert!(error.contains("sha256"), "got: {error}");
        assert!(!part.exists(), "the partial file goes");
        assert!(!dest.exists(), "nothing lands under the model's name");
    }

    #[test]
    fn a_download_of_the_wrong_length_is_taken_back() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let part = dir.path().join("model.onnx.part");
        let dest = dir.path().join("model.onnx");
        std::fs::write(&part, b"weights").expect("the part is written");
        let blob = HubBlob { path: "onnx/model.onnx".to_owned(), size: 99, sha256: None };
        let error =
            finish_file(&part, &dest, &blob, 7, "11".repeat(32).as_str()).expect_err("refused");
        assert!(error.contains("99"), "got: {error}");
        assert!(!dest.exists());
    }

    #[test]
    fn a_verified_download_lands_under_its_name() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let part = dir.path().join("model.onnx.part");
        let dest = dir.path().join("model.onnx");
        std::fs::write(&part, b"weights").expect("the part is written");
        // The digest the Hub would publish for these bytes, so the matching
        // path is as pinned as the mismatch above.
        let digest = {
            use sha2::{Digest as _, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(b"weights");
            hex::encode(hasher.finalize())
        };
        let blob =
            HubBlob { path: "onnx/model.onnx".to_owned(), size: 7, sha256: Some(digest.clone()) };
        finish_file(&part, &dest, &blob, 7, digest.as_str()).expect("it lands");
        assert_eq!(std::fs::read(&dest).expect("reads"), b"weights");
        assert!(!Path::new(&part).exists());
    }
}

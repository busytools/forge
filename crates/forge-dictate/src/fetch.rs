//! Model fetch: download, resume, and verify before use.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{Config, Error, ModelSpec};

/// Bytes between progress reports during a transfer.
const PROGRESS_INTERVAL: u64 = 1 << 20;

/// What [`prepare`] reports as it works.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Checking a file already on disk. No bytes are transferred.
    Verifying { file: String },
    /// `downloaded` of `total` bytes are now on disk for `file`.
    Downloading { file: String, downloaded: u64, total: u64 },
    /// `file` is present and verified.
    Ready { file: String },
}

/// Make every model the config names present and verified.
///
/// Downloads what is missing, resumes what was interrupted, and checks
/// size then SHA-256 against the [`crate::ModelSpec`] before reporting
/// a file ready. Safe to call repeatedly; a verified file is left
/// alone.
///
/// A file that is present but does not match its spec is reported, not
/// repaired. Deciding to discard someone's model file belongs to
/// whoever put it there.
pub fn prepare(cfg: &Config, mut on_progress: impl FnMut(Progress)) -> Result<(), Error> {
    let dir = models_dir(cfg)?;
    fs::create_dir_all(&dir).map_err(|source| Error::Io { path: dir.clone(), source })?;

    for spec in std::iter::once(&cfg.asr_model).chain(cfg.normalizer.as_ref()) {
        ensure(&dir, spec, &mut on_progress)?;
    }
    Ok(())
}

/// The configured directory, else a subdirectory of the platform cache
/// directory. Never derived from the working directory: a caller with
/// no usable cache directory gets an error to act on.
fn models_dir(cfg: &Config) -> Result<PathBuf, Error> {
    match &cfg.models_dir {
        Some(dir) => Ok(dir.clone()),
        None => dirs::cache_dir().map(|d| d.join("forge-dictate")).ok_or(Error::NoCacheDir),
    }
}

fn ensure(
    dir: &Path,
    spec: &ModelSpec,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<(), Error> {
    let target = dir.join(&spec.file);
    if target.try_exists().map_err(|source| Error::Io { path: target.clone(), source })? {
        on_progress(Progress::Verifying { file: spec.file.clone() });
        verify(&target, spec)?;
        on_progress(Progress::Ready { file: spec.file.clone() });
        return Ok(());
    }

    let partial = dir.join(format!("{}.part", spec.file));
    download(spec, &partial, on_progress)?;

    // Appending to bytes that already hash wrong can never converge, so
    // a failed partial goes instead of poisoning every later resume.
    if let Err(e) = verify(&partial, spec) {
        tracing::warn!(file = %spec.file, "downloaded model failed verification; discarding partial");
        let _ = fs::remove_file(&partial);
        return Err(e);
    }
    fs::rename(&partial, &target).map_err(|source| Error::Io { path: target, source })?;
    on_progress(Progress::Ready { file: spec.file.clone() });
    Ok(())
}

/// Size first, then digest: a truncated file is the common case and
/// costs one `stat` to reject.
fn verify(path: &Path, spec: &ModelSpec) -> Result<(), Error> {
    let actual =
        fs::metadata(path).map_err(|source| Error::Io { path: path.into(), source })?.len();
    if actual != spec.size {
        return Err(Error::SizeMismatch { path: path.into(), expected: spec.size, actual });
    }

    let digest = sha256(path)?;
    if !digest.eq_ignore_ascii_case(&spec.sha256) {
        return Err(Error::HashMismatch {
            path: path.into(),
            expected: spec.sha256.clone(),
            actual: digest,
        });
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String, Error> {
    let mut file = File::open(path).map_err(|source| Error::Io { path: path.into(), source })?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|source| Error::Io { path: path.into(), source })?;
    Ok(hex::encode(hasher.finalize()))
}

fn download(
    spec: &ModelSpec,
    partial: &Path,
    on_progress: &mut dyn FnMut(Progress),
) -> Result<(), Error> {
    let mut have = match fs::metadata(partial) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(source) => return Err(Error::Io { path: partial.into(), source }),
    };
    if have >= spec.size {
        have = 0;
    }

    let http = |source| Error::Http { url: spec.url.clone(), source };
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(http)?;

    let mut request = client.get(&spec.url);
    if have > 0 {
        tracing::debug!(file = %spec.file, have, "resuming interrupted download");
        request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut response = request.send().map_err(http)?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::HttpStatus { url: spec.url.clone(), status: status.as_u16() });
    }
    // A server free to ignore `Range` answers 200 with the whole file,
    // and appending that to what we hold would interleave two copies.
    let resuming = have > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
    if !resuming {
        have = 0;
    }

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if resuming {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let file =
        options.open(partial).map_err(|source| Error::Io { path: partial.into(), source })?;

    let mut sink = ProgressWriter {
        file,
        name: &spec.file,
        written: have,
        reported: have,
        total: spec.size,
        on_progress,
    };
    std::io::copy(&mut response, &mut sink)
        .map_err(|source| Error::Io { path: partial.into(), source })?;
    sink.report();
    Ok(())
}

/// Counts bytes on their way to disk and forwards the running total,
/// rate-limited so a multi-gigabyte transfer does not call back once
/// per read.
struct ProgressWriter<'a> {
    file: File,
    name: &'a str,
    written: u64,
    reported: u64,
    total: u64,
    on_progress: &'a mut dyn FnMut(Progress),
}

impl ProgressWriter<'_> {
    fn report(&mut self) {
        self.reported = self.written;
        (self.on_progress)(Progress::Downloading {
            file: self.name.to_owned(),
            downloaded: self.written,
            total: self.total,
        });
    }
}

impl std::io::Write for ProgressWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.file.write(buf)?;
        self.written += n as u64;
        if self.written - self.reported >= PROGRESS_INTERVAL {
            self.report();
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests_cached_verification {
    use super::*;
    use crate::{ConfigBuilder, ModelSpec};
    use sha2::{Digest, Sha256};
    use std::fs;

    /// A spec whose URL is guaranteed unreachable, so any code path
    /// that reaches the network fails loudly as [`Error::Http`]
    /// instead of quietly repairing what the test corrupted.
    fn offline_spec(file: &str, body: &[u8]) -> ModelSpec {
        ModelSpec {
            file: file.into(),
            url: "http://127.0.0.1:1/unreachable".into(),
            size: body.len() as u64,
            sha256: hex::encode(Sha256::digest(body)),
        }
    }

    #[test]
    fn truncated_cached_model_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = offline_spec("asr.gguf", b"the complete model weights");
        fs::write(dir.path().join("asr.gguf"), b"the complete model weight").unwrap();

        let cfg =
            ConfigBuilder::new().models_dir(dir.path()).asr_model(spec).normalizer(None).build();

        let err = prepare(&cfg, |_| {}).expect_err("a truncated model must not be accepted");
        assert!(
            matches!(err, Error::SizeMismatch { expected: 26, actual: 25, .. }),
            "truncation must be rejected on size, got: {err:?}"
        );
    }

    #[test]
    fn cached_model_of_right_length_but_wrong_bytes_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let spec = offline_spec("asr.gguf", b"the complete model weights");
        fs::write(dir.path().join("asr.gguf"), b"the corrupted model weight").unwrap();

        let cfg =
            ConfigBuilder::new().models_dir(dir.path()).asr_model(spec).normalizer(None).build();

        let err = prepare(&cfg, |_| {}).expect_err("a same-length corruption must not be accepted");
        assert!(
            matches!(err, Error::HashMismatch { .. }),
            "a file of the right length with the wrong bytes must be rejected on hash, got: {err:?}"
        );
    }
}

#[cfg(test)]
mod tests_download {
    use super::*;
    use crate::{ConfigBuilder, ModelSpec};
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// Loopback HTTP/1.1 server serving a fixed set of paths and
    /// honouring `Range: bytes=N-`, so the resume path has a real 206
    /// to resume against. Every request head it saw is readable from
    /// `seen`, which is how a test proves the `Range` header was sent
    /// rather than inferring it from the resulting bytes.
    struct Server {
        base: String,
        seen: mpsc::Receiver<String>,
    }

    fn serve(files: Vec<(&'static str, Vec<u8>)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, seen) = mpsc::channel();

        // Detached: nextest gives each test its own process, so the
        // accept loop dies with it and needs no shutdown channel.
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    head.push_str(&line);
                }

                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                let start: usize = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|l| l.split("bytes=").nth(1))
                    .and_then(|r| r.trim().trim_end_matches('-').parse().ok())
                    .unwrap_or(0);
                tx.send(head).unwrap();

                let body = files.iter().find(|(p, _)| *p == path).map(|(_, b)| b);
                let response = match body {
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                    Some(full) => {
                        let slice = &full[start.min(full.len())..];
                        let mut head = if start == 0 {
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", slice.len())
                        } else {
                            format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\n",
                                slice.len(),
                                start,
                                full.len() - 1,
                                full.len()
                            )
                        };
                        head.push_str("Connection: close\r\n\r\n");
                        let mut out = head.into_bytes();
                        out.extend_from_slice(slice);
                        out
                    }
                };
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });

        Server { base, seen }
    }

    fn spec_for(server: &Server, path: &str, body: &[u8]) -> ModelSpec {
        ModelSpec {
            file: path.trim_start_matches('/').into(),
            url: format!("{}{path}", server.base),
            size: body.len() as u64,
            sha256: hex::encode(Sha256::digest(body)),
        }
    }

    #[test]
    fn every_configured_model_is_downloaded_and_verified() {
        let asr = b"pretend these are recognition weights".to_vec();
        let norm = b"and these rewrite the words".to_vec();
        let server = serve(vec![("/asr.gguf", asr.clone()), ("/norm.gguf", norm.clone())]);
        let dir = tempfile::tempdir().unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &asr))
            .normalizer(spec_for(&server, "/norm.gguf", &norm))
            .build();

        let mut reported = Vec::new();
        prepare(&cfg, |p| reported.push(p)).expect("both models must download and verify");

        assert_eq!(fs::read(dir.path().join("asr.gguf")).unwrap(), asr, "asr model must land");
        assert_eq!(
            fs::read(dir.path().join("norm.gguf")).unwrap(),
            norm,
            "normalizer must be fetched too, not only the asr model"
        );

        let ready: Vec<_> = reported
            .iter()
            .filter_map(|p| match p {
                Progress::Ready { file } => Some(file.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ready, ["asr.gguf", "norm.gguf"], "each model must be reported ready");
        assert!(
            reported.iter().any(
                |p| matches!(p, Progress::Downloading { downloaded, total, .. } if downloaded == total)
            ),
            "a transfer must report its final byte count, got: {reported:?}"
        );
    }

    #[test]
    fn interrupted_download_resumes_from_what_is_already_on_disk() {
        let body = b"pretend these are recognition weights".to_vec();
        let server = serve(vec![("/asr.gguf", body.clone())]);
        let dir = tempfile::tempdir().unwrap();
        // A previous run got this far and stopped.
        fs::write(dir.path().join("asr.gguf.part"), &body[..10]).unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &body))
            .normalizer(None)
            .build();

        prepare(&cfg, |_| {}).expect("a partial download must resume, not fail");

        let head = server.seen.recv().unwrap();
        assert!(
            head.to_ascii_lowercase().contains("range: bytes=10-"),
            "resume must ask for the bytes it lacks, request was: {head}"
        );
        assert_eq!(
            fs::read(dir.path().join("asr.gguf")).unwrap(),
            body,
            "the resumed file must be the whole model, not the tail alone"
        );
    }

    #[test]
    fn partial_that_fails_verification_is_discarded() {
        let body = b"pretend these are recognition weights".to_vec();
        let server = serve(vec![("/asr.gguf", body.clone())]);
        let dir = tempfile::tempdir().unwrap();
        let partial = dir.path().join("asr.gguf.part");
        // Right length, wrong bytes: resuming appends the correct tail
        // onto a corrupt head, so the result can never verify.
        fs::write(&partial, b"CORRUPTED!").unwrap();

        let cfg = ConfigBuilder::new()
            .models_dir(dir.path())
            .asr_model(spec_for(&server, "/asr.gguf", &body))
            .normalizer(None)
            .build();

        let err = prepare(&cfg, |_| {}).expect_err("a corrupt resume must not be accepted");
        assert!(
            matches!(err, Error::HashMismatch { .. }),
            "expected a hash rejection, got: {err:?}"
        );
        assert!(
            !partial.exists(),
            "a partial that cannot ever verify must be discarded, or every later resume inherits it"
        );
    }
}

//! Error taxonomy for the crate.

use std::path::PathBuf;

/// Everything this crate can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No models directory was configured and the platform cache
    /// directory could not be resolved.
    #[error("no cache directory available; set Config::models_dir explicitly")]
    NoCacheDir,

    /// A filesystem operation on `path` failed. A transfer that dies
    /// mid-body lands here too rather than in [`Error::Http`]: it
    /// surfaces while reading the response, and `path` labels the
    /// partial being written rather than naming what actually failed.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The request failed before any body arrived - connection, TLS, or
    /// a refused handshake.
    #[error("could not fetch {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// The server answered, but not with the file.
    #[error("{url} returned HTTP {status}")]
    HttpStatus { url: String, status: u16 },

    /// A file on disk is not the length the spec records. The usual
    /// cause is an interrupted download that was never resumed.
    #[error("{} is {actual} bytes, expected {expected}", path.display())]
    SizeMismatch { path: PathBuf, expected: u64, actual: u64 },

    /// The progress callback asked to stop. Any partial transfer is left
    /// where it is, so a later call resumes rather than starting over.
    #[error("cancelled by the progress callback")]
    Cancelled,

    /// A file on disk is the right length but the wrong bytes.
    #[error("{} hashes to {actual}, expected {expected}", path.display())]
    HashMismatch { path: PathBuf, expected: String, actual: String },
}

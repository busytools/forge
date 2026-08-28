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

    /// The progress callback asked to stop. Whatever already finished is
    /// kept - a model that reached [`crate::Progress::Ready`] is
    /// installed - and any partial transfer is left where it is, so a
    /// later call resumes rather than starting over.
    #[error("cancelled by the progress callback")]
    Cancelled,

    /// The source declares a rate the models cannot read. Rejected
    /// rather than resampled: a silent resample yields plausible text
    /// from the wrong signal, and nothing downstream can detect that.
    #[error("audio is {actual} Hz, expected {expected} Hz; resample before transcribing")]
    SampleRate { expected: u32, actual: u32 },

    /// The source declares more than one channel. Interleaved stereo
    /// read as mono is a plausible doubled-rate signal, not an error, so
    /// it is refused at the boundary.
    #[error("audio has {actual} channels, expected mono; downmix before transcribing")]
    Channels { actual: u16 },

    /// The weights could not be loaded.
    #[error("could not load the model at {}: {message}", path.display())]
    ModelLoad { path: PathBuf, message: String },

    /// Recognition itself failed.
    #[error("recognition failed: {message}")]
    Recognition { message: String },

    /// The worker is gone, so nothing can be transcribed and no queued
    /// result will ever arrive.
    #[error("the transcription worker has stopped")]
    EngineStopped,

    /// A partial does not match its spec and could not be removed, so
    /// every later call fails identically until someone clears it. The
    /// usual cause is a models directory that is not writable.
    #[error("{} does not match its spec and could not be removed ({source}); remove it to unblock", path.display())]
    StalePartial {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A file on disk is the right length but the wrong bytes.
    #[error("{} hashes to {actual}, expected {expected}", path.display())]
    HashMismatch { path: PathBuf, expected: String, actual: String },
}

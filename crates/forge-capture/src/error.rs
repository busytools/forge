//! Why a microphone could not be opened or used.

/// What a host could not do with the microphone.
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// The host has no default input device at all.
    #[error("no input device is available")]
    NoInputDevice,
    /// A named device is not present. The alternatives are listed because
    /// a person who picked an interface needs the answer to "what is there".
    #[error("no input device named \"{wanted}\" (available: {available})")]
    DeviceNotFound {
        /// The id the caller asked for.
        wanted: String,
        /// The devices that are present, as `id (name)` pairs.
        available: String,
    },
    /// The device offers no stream this crate can open.
    #[error("the input device cannot serve {wanted} Hz (it offers: {offered})")]
    UnsupportedInput {
        /// The rate the caller needed.
        wanted: u32,
        /// What the device offered, so the mismatch is legible.
        offered: String,
    },
    /// The stream could not be opened, configured or started.
    #[error("{message}")]
    Capture {
        /// The host's own words for the failure.
        message: String,
    },
}

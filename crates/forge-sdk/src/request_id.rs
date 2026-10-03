//! Request ID generator for outbound `control_request` frames.
//!
//! Shape: `forge_<counter>_<hex8>` (e.g. `forge_42_3a9f2b0e`). The
//! `forge_` prefix lets stream-json logs distinguish
//! forge-sdk-originated requests from CLI-originated ones at a
//! glance. The CLI treats request IDs as opaque - it only echoes
//! them back in the matching `control_response` - so the prefix
//! has no wire-protocol effect.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a new opaque request id in the shape `forge_<counter>_<hex8>`.
/// The 8-hex (4 bytes) suffix is random. Falls back to counter bytes
/// if `getrandom` fails - surfaces a one-shot `tracing::warn!` so a
/// sandboxed runtime where the entropy source is unreachable
/// (chroot without `/dev/urandom`, seccomp filter, etc.) is visible
/// to log readers without breaking ID generation.
pub fn next() -> String {
    mint("forge")
}

/// Generate a new prompt id in the shape `forge_prompt_<counter>_<hex8>`.
///
/// Stamped as the `uuid` on every user frame forge writes. The CLI echoes it
/// back on each of that prompt's `command_lifecycle` frames, which is what
/// lets a view key a queued card to its own send; the distinct prefix keeps
/// prompt ids apart from control request ids in a stream-json log.
///
/// Never reuse one: the CLI treats a repeated id as a duplicate and drops the
/// prompt silently (measured, no frames and no delivery).
pub fn next_prompt_id() -> String {
    mint("forge_prompt")
}

/// The mint itself: `<prefix>_<counter>_<hex8>`, with the counter-derived
/// suffix standing in when the entropy source is unreachable.
fn mint(prefix: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0_u8; 4];
    if let Err(e) = getrandom::fill(&mut bytes) {
        static LOGGED_ONCE: AtomicBool = AtomicBool::new(false);
        if !LOGGED_ONCE.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                error = %e,
                "getrandom failed; falling back to counter-derived id suffix"
            );
        }
        bytes.copy_from_slice(&n.to_le_bytes()[..4]);
    }
    format!("{prefix}_{n}_{}", hex::encode(bytes))
}

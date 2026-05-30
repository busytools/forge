//! Request ID generator for outbound `control_request` frames.
//!
//! Shape: `forge_<counter>_<hex8>` (e.g. `forge_42_3a9f2b0e`). The
//! `forge_` prefix lets stream-json logs distinguish
//! forge-sdk-originated requests from CLI-originated ones at a
//! glance. The CLI treats request IDs as opaque  -  it only echoes
//! them back in the matching `control_response`  -  so the prefix
//! has no wire-protocol effect.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a new opaque request id in the shape `forge_<counter>_<hex8>`.
/// The 8-hex (4 bytes) suffix is random. Falls back to counter bytes
/// if `getrandom` fails  -  surfaces a one-shot `tracing::warn!` so a
/// sandboxed runtime where the entropy source is unreachable
/// (chroot without `/dev/urandom`, seccomp filter, etc.) is visible
/// to log readers without breaking ID generation.
pub fn next() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0_u8; 4];
    if let Err(e) = getrandom::fill(&mut bytes) {
        static LOGGED_ONCE: AtomicBool = AtomicBool::new(false);
        if !LOGGED_ONCE.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                error = %e,
                "getrandom failed; falling back to counter-derived request_id suffix"
            );
        }
        bytes.copy_from_slice(&n.to_le_bytes()[..4]);
    }
    format!("forge_{n}_{}", hex::encode(bytes))
}

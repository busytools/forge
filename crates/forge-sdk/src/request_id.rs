//! Request ID generator matching Python SDK's `req_<counter>_<hex4>` format
//! (see `_internal/query.py` counter + `secrets.token_hex(4)`).

use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a new opaque request id in the Python-compatible shape
/// `req_<counter>_<hex8>`. The 8-hex (4 bytes) suffix is random. Falls
/// back to counter bytes if `getrandom` fails.
#[must_use]
pub fn next() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0_u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        bytes.copy_from_slice(&n.to_le_bytes()[..4]);
    }
    format!("req_{n}_{}", hex::encode(bytes))
}

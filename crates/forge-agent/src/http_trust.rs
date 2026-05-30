//! Reqwest builder helper that extends the TLS trust store with
//! certs from `NODE_EXTRA_CA_CERTS` (with `SSL_CERT_FILE` as fallback)
//! to match native `claude`'s TLS trust behaviour.
//!
//! Forge makes outbound HTTPS from several non-rewriter call sites
//! (oauth usage probe, service-status check, CLI-version probe).
//! These use reqwest, which by default trusts webpki-roots + system
//! roots only  -  neither covers a mitmproxy / Zscaler / Palo Alto /
//! corporate CA setup that the user has wired up via
//! `NODE_EXTRA_CA_CERTS`. The native `claude` binary (Node) honours
//! that env var unconditionally; forge needs the same behaviour for
//! interoperability with the corporate CA configurations users
//! already have running.
//!
//! Apply at every reqwest client construction site via
//! `with_extra_roots(reqwest::Client::builder().timeout(…))`.

use std::fs;

/// Extend a reqwest `ClientBuilder` with PEM certs from the
/// `NODE_EXTRA_CA_CERTS` env var (falls back to `SSL_CERT_FILE`).
///
/// Unset, empty, unreadable, or unparseable paths are logged as
/// `warn` and the builder is returned unchanged. This mirrors how
/// Node's TLS layer treats `NODE_EXTRA_CA_CERTS` as a best-effort
/// extension  -  a malformed bundle doesn't kill the program.
pub fn with_extra_roots(mut b: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    for var in ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE"] {
        let Ok(path) = std::env::var(var) else { continue };
        if path.trim().is_empty() {
            continue;
        }
        let pem = match fs::read(&path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %path, var, error = %e, "extra CA bundle unreadable");
                continue;
            }
        };
        match reqwest::Certificate::from_pem_bundle(&pem) {
            Ok(certs) => {
                let n = certs.len();
                for c in certs {
                    b = b.add_root_certificate(c);
                }
                tracing::info!(
                    path = %path,
                    var,
                    added = n,
                    "loaded extra CA roots into reqwest"
                );
                // First env var that yields certs wins; don't double-
                // load if both NODE_EXTRA_CA_CERTS and SSL_CERT_FILE
                // are set to the same bundle.
                break;
            }
            Err(e) => {
                tracing::warn!(path = %path, var, error = %e, "failed to parse CA bundle");
            }
        }
    }
    b
}

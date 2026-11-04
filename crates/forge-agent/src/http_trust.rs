//! Reqwest builder helper that extends the TLS trust store with
//! certs from `NODE_EXTRA_CA_CERTS` (with `SSL_CERT_FILE` as fallback)
//! to match native `claude`'s TLS trust behaviour.
//!
//! Forge makes outbound HTTPS from several call sites (oauth usage
//! probe, service-status check, CLI-version probe).
//! These use reqwest, which by default trusts webpki-roots + system
//! roots only - neither covers a mitmproxy / Zscaler / Palo Alto /
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
/// extension - a malformed bundle doesn't kill the program.
pub fn with_extra_roots(mut b: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    for var in ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE"] {
        let Ok(path) = std::env::var(var) else { continue };
        if path.trim().is_empty() {
            continue;
        }
        let (next, added) = with_roots_from(b, &path, var);
        b = next;
        // First env var that yields certs wins; don't double-load if both
        // NODE_EXTRA_CA_CERTS and SSL_CERT_FILE name the same bundle.
        if added > 0 {
            break;
        }
    }
    b
}

/// Load one bundle into `b`, returning the builder and how many certificates
/// it added. Unreadable or unparseable bundles add nothing.
fn with_roots_from(
    mut b: reqwest::ClientBuilder,
    path: &str,
    var: &str,
) -> (reqwest::ClientBuilder, usize) {
    let pem = match fs::read(path) {
        Ok(pem) => pem,
        Err(error) => {
            tracing::warn!(path = %path, var, %error, "extra CA bundle unreadable");
            return (b, 0);
        }
    };
    match reqwest::Certificate::from_pem_bundle(&pem) {
        Ok(certs) => {
            let added = certs.len();
            for cert in certs {
                b = b.add_root_certificate(cert);
            }
            tracing::info!(path = %path, var, added, "loaded extra CA roots into reqwest");
            (b, added)
        }
        Err(error) => {
            tracing::warn!(path = %path, var, %error, "failed to parse CA bundle");
            (b, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loader the production clients use adds the bundle's certificates.
    #[test]
    fn a_valid_bundle_adds_its_certificates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/extra-root.pem");

        let (_, added) = with_roots_from(reqwest::Client::builder(), path, "NODE_EXTRA_CA_CERTS");

        assert_eq!(added, 1);
    }

    /// An unreadable bundle is best-effort, like Node's own handling.
    #[test]
    fn an_unreadable_bundle_adds_nothing() {
        let (_, added) =
            with_roots_from(reqwest::Client::builder(), "/nonexistent/ca.pem", "SSL_CERT_FILE");

        assert_eq!(added, 0);
    }
}

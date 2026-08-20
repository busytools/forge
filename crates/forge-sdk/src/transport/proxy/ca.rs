//! CA cert + key persistence for the wire-classification rewriter.
//!
//! Generates a self-signed CA on first launch and persists it under
//! the forge data dir (`~/Library/Application Support/forge-tui/ca/`
//! on macOS, `$XDG_DATA_HOME/forge-tui/ca/` on Linux). Reused across
//! launches so the spawned child can pin `NODE_EXTRA_CA_CERTS=...` to
//! a stable path.
//!
//! The key file is chmod 0600 on Unix, best-effort. It is not protected
//! beyond that: this is a CA whose cert forge asks the user to trust
//! system-wide, so anyone who can read the file can sign for any host
//! that machine trusts. See the scope section in `CLAUDE.md`.

use std::path::{Path, PathBuf};

use hudsucker::certificate_authority::RcgenAuthority;
use hudsucker::rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
};

use crate::Error;

/// Resolve the directory where the CA cert + key live, under forge's
/// machine-local app-support base (`forge-tui/ca/`).
///
/// # Errors
///
/// Propagates [`crate::paths::app_support_dir`]'s error when no
/// data/cache/home dir resolves.
pub fn ca_dir() -> Result<PathBuf, Error> {
    Ok(crate::paths::app_support_dir()?.join("ca"))
}

/// Resolve the cert + key paths inside [`ca_dir`].
///
/// # Errors
///
/// Propagates [`ca_dir`]'s error.
pub fn ca_paths() -> Result<(PathBuf, PathBuf), Error> {
    let dir = ca_dir()?;
    Ok((dir.join("ca-cert.pem"), dir.join("ca-key.pem")))
}

/// Generate a new CA if one doesn't already exist on disk; return
/// the cert + key paths in either case.
///
/// The CA's CN/O identifies it as forge's so anyone inspecting the
/// installed cert understands its origin. Validity is 10 years from
/// "1 hour ago" (the backdating guards against system clock skew at
/// first run).
///
/// # Errors
///
/// [`Error::Connection`] for I/O errors creating the dir, generating
/// the key, writing the files, or setting Unix permissions.
pub fn ensure_ca() -> Result<(PathBuf, PathBuf), Error> {
    let (cert_path, key_path) = ca_paths()?;
    if cert_path.exists() && key_path.exists() {
        return Ok((cert_path, key_path));
    }

    let dir = cert_path.parent().ok_or_else(|| Error::Connection {
        reason: format!("ca_paths returned a path with no parent: {}", cert_path.display()),
    })?;
    std::fs::create_dir_all(dir).map_err(|e| Error::Connection {
        reason: format!("creating CA dir {}: {e}", dir.display()),
    })?;

    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "forge wire-classification rewriter");
    dn.push(DnType::OrganizationName, "forge-tui");
    params.distinguished_name = dn;
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(3650);

    let key_pair = KeyPair::generate()
        .map_err(|e| Error::Connection { reason: format!("generating CA key pair: {e}") })?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| Error::Connection { reason: format!("self-signing CA: {e}") })?;

    std::fs::write(&cert_path, cert.pem()).map_err(|e| Error::Connection {
        reason: format!("writing CA cert to {}: {e}", cert_path.display()),
    })?;
    std::fs::write(&key_path, key_pair.serialize_pem()).map_err(|e| Error::Connection {
        reason: format!("writing CA key to {}: {e}", key_path.display()),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort key file restriction; ignore failure on
        // filesystems that don't honour mode bits.
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }

    Ok((cert_path, key_path))
}

/// Load the persisted CA into a [`RcgenAuthority`] hudsucker can use.
///
/// # Errors
///
/// [`Error::Connection`] for I/O failures, PEM parse errors, or
/// authority construction failures.
pub fn load_authority(cert_path: &Path, key_path: &Path) -> Result<RcgenAuthority, Error> {
    let cert_pem = std::fs::read_to_string(cert_path).map_err(|e| Error::Connection {
        reason: format!("reading CA cert {}: {e}", cert_path.display()),
    })?;
    let key_pem = std::fs::read_to_string(key_path).map_err(|e| Error::Connection {
        reason: format!("reading CA key {}: {e}", key_path.display()),
    })?;

    let key_pair = KeyPair::from_pem(&key_pem).map_err(|e| Error::Connection {
        reason: format!("parsing CA key from {}: {e}", key_path.display()),
    })?;
    let cert = CertificateParams::from_ca_cert_pem(&cert_pem)
        .map_err(|e| Error::Connection {
            reason: format!("parsing CA cert params from {}: {e}", cert_path.display()),
        })?
        .self_signed(&key_pair)
        .map_err(|e| Error::Connection {
            reason: format!("rebuilding self-signed CA cert: {e}"),
        })?;

    Ok(RcgenAuthority::new(key_pair, cert, 1_000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_dir_returns_a_path_with_ca_leaf() {
        let dir = ca_dir().expect("ca_dir should resolve on a normal dev machine");
        assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some("ca"));
    }

    #[test]
    fn ca_paths_share_parent() {
        let (cert, key) = ca_paths().expect("ca_paths");
        assert_eq!(cert.parent(), key.parent());
        assert_eq!(cert.file_name().and_then(|s| s.to_str()), Some("ca-cert.pem"));
        assert_eq!(key.file_name().and_then(|s| s.to_str()), Some("ca-key.pem"));
    }
}

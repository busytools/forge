//! The socket's Origin gate.
//!
//! A browser page the user did not point at forge can open `ws://` to the
//! loopback socket from any tab, and the socket carries the whole effective
//! config - credentials included. Browsers always send an `Origin` on a
//! WebSocket handshake and pages cannot forge it, so the header is the line
//! between the clients forge ships and a drive-by page.

use std::net::IpAddr;

use forge_primitives::server::ServerConfig;

/// The origins a Tauri webview is served from: `tauri://localhost` on macOS,
/// `http://tauri.localhost` on Android and Windows, `https://tauri.localhost`
/// where the scheme is https.
const SHELL_ORIGINS: &[&str] =
    &["tauri://localhost", "http://tauri.localhost", "https://tauri.localhost"];

/// The vite dev server's origin, allowed only in a debug build.
const DEV_ORIGINS: &[&str] = &["http://localhost:1420", "http://127.0.0.1:1420"];

/// Whether a request's `Origin` may open the socket.
///
/// A request with none is a native client - the protocol client, a harness,
/// `wscat` - and browsers never omit the header, so absence is allowed and
/// a present origin is what the gate reads.
pub(crate) fn allows(origin: Option<&str>, server: &ServerConfig) -> bool {
    let Some(origin) = origin else { return true };
    let origin = origin.trim_end_matches('/');
    if SHELL_ORIGINS.contains(&origin) {
        return true;
    }
    if cfg!(debug_assertions) && DEV_ORIGINS.contains(&origin) {
        return true;
    }
    if server.allowed_origins.iter().any(|allowed| allowed.trim_end_matches('/') == origin) {
        return true;
    }
    own_origins(server).iter().any(|allowed| allowed == origin)
}

/// The origins the server itself is reached at, from `[server] bind` and
/// `[server] port`. No port is hardcoded: a web client served from the
/// server's own address is the one origin the config knows.
fn own_origins(server: &ServerConfig) -> Vec<String> {
    let port = server.port;
    match server.bind {
        IpAddr::V4(ip) if ip.is_loopback() || ip.is_unspecified() => {
            vec![format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")]
        }
        IpAddr::V6(ip) if ip.is_loopback() || ip.is_unspecified() => {
            vec![format!("http://[::1]:{port}"), format!("http://localhost:{port}")]
        }
        IpAddr::V4(ip) => vec![format!("http://{ip}:{port}")],
        IpAddr::V6(ip) => vec![format!("http://[{ip}]:{port}")],
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn config() -> ServerConfig {
        ServerConfig { port: 9001, ..ServerConfig::default() }
    }

    /// A page forge did not ship is refused; the tab in the user's browser is
    /// the drive-by this exists for.
    #[test]
    fn a_foreign_origin_is_refused() {
        assert!(!allows(Some("https://evil.example"), &config()));
    }

    /// The shell's own webview origin is allowed on both schemes and on the
    /// Android host.
    #[test]
    fn the_shells_origins_are_allowed() {
        for origin in SHELL_ORIGINS {
            assert!(allows(Some(origin), &config()), "{origin}");
        }
    }

    /// The server's own origin is derived from the config's port, and a
    /// different port is not the server.
    #[test]
    fn the_servers_own_origin_comes_from_its_port() {
        assert!(allows(Some("http://127.0.0.1:9001"), &config()));
        assert!(allows(Some("http://localhost:9001"), &config()));
        assert!(!allows(Some("http://127.0.0.1:9002"), &config()));
    }

    /// A non-loopback bind is reached at its own address.
    #[test]
    fn a_network_bind_is_reached_at_its_own_address() {
        let server = ServerConfig { bind: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), ..config() };
        assert!(allows(Some("http://10.0.0.5:9001"), &server));
        assert!(!allows(Some("http://127.0.0.1:9001"), &server));
        let server = ServerConfig { bind: IpAddr::V6(Ipv6Addr::LOCALHOST), ..config() };
        assert!(allows(Some("http://[::1]:9001"), &server));
    }

    /// An origin the operator listed is allowed, with a trailing slash
    /// tolerated.
    #[test]
    fn a_configured_origin_is_allowed() {
        let server =
            ServerConfig { allowed_origins: vec!["http://localhost:8080".to_owned()], ..config() };
        assert!(allows(Some("http://localhost:8080"), &server));
        assert!(allows(Some("http://localhost:8080/"), &server));
        assert!(!allows(Some("http://localhost:8081"), &server));
    }

    /// A client that sends no Origin is a native one, not a browser page.
    #[test]
    fn an_absent_origin_is_a_native_client() {
        assert!(allows(None, &config()));
    }

    /// The dev client is a debug-build allowance, and never the shipping
    /// release's.
    #[test]
    fn the_dev_origin_is_allowed_only_in_a_debug_build() {
        assert_eq!(allows(Some("http://localhost:1420"), &config()), cfg!(debug_assertions));
    }
}

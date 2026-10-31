//! The socket's Origin gate.
//!
//! A browser page the user did not point at forge can open `ws://` to the
//! loopback socket from any tab, and the socket carries the whole effective
//! config - credentials included. Browsers always send an `Origin` on a
//! WebSocket handshake and pages cannot forge it, so the header is the line
//! between the pages the user meant to reach forge and a drive-by page: the
//! clients forge ships, and whatever the user is serving on their own
//! machine.

use std::net::IpAddr;

use axum::http::Uri;
use forge_primitives::server::ServerConfig;

/// The origins a Tauri webview is served from: `tauri://localhost` on macOS,
/// `http://tauri.localhost` on Android and Windows, `https://tauri.localhost`
/// where the scheme is https.
const SHELL_ORIGINS: &[&str] =
    &["tauri://localhost", "http://tauri.localhost", "https://tauri.localhost"];

/// The hosts a page served on the user's own machine is reached at. Any port
/// and either web scheme, in a release build as well as a debug one, so a dev
/// client attaches to the running server instead of a scratch forge.
const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "[::1]"];

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
    if is_loopback_origin(origin) {
        return true;
    }
    if server.allowed_origins.iter().any(|allowed| allowed.trim_end_matches('/') == origin) {
        return true;
    }
    own_origins(server).iter().any(|allowed| allowed == origin)
}

/// Whether `origin` is a page served on the user's own machine: an http or
/// https origin whose host is loopback, at any port.
///
/// The host is read from a parsed URI, never matched as a substring: a host
/// that merely contains a loopback name, like `localhost.evil.com`, is its
/// own host and stays refused.
fn is_loopback_origin(origin: &str) -> bool {
    let Ok(uri) = origin.parse::<Uri>() else { return false };
    let web_scheme = uri.scheme_str().is_some_and(|scheme| {
        scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
    });
    web_scheme
        && uri.authority().is_some_and(|authority| {
            LOOPBACK_HOSTS.iter().any(|host| authority.host().eq_ignore_ascii_case(host))
        })
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

    /// Any page the user serves on their own machine may attach: loopback
    /// host, either web scheme, any port. Neither the port nor an allowed
    /// origin is in the config, so only the loopback rule can allow these.
    #[test]
    fn a_loopback_origin_on_any_port_is_allowed() {
        for origin in [
            "http://localhost:53219",
            "https://localhost:53219",
            "http://127.0.0.1:53219",
            "https://127.0.0.1:53219",
            "http://[::1]:53219",
            "https://[::1]:53219",
            "http://localhost",
            "http://LOCALHOST:53219",
        ] {
            assert!(allows(Some(origin), &config()), "{origin}");
        }
    }

    /// The check is on the parsed host, not a substring: a host that merely
    /// contains a loopback name is a page from somewhere else.
    #[test]
    fn a_lookalike_host_is_refused() {
        for origin in [
            "http://localhost.evil.com:53219",
            "https://127.0.0.1.nip.io:53219",
            "http://localhost@evil.com",
            "http://evil.com#localhost",
        ] {
            assert!(!allows(Some(origin), &config()), "{origin}");
        }
    }

    /// The shell's own webview origin is allowed on both schemes and on the
    /// Android host.
    #[test]
    fn the_shells_origins_are_allowed() {
        for origin in SHELL_ORIGINS {
            assert!(allows(Some(origin), &config()), "{origin}");
        }
    }

    /// The server's own origin is derived from the config's bind and port: a
    /// network bind is reached at its own address there, and neither a
    /// neighbouring address nor a different port is the server.
    #[test]
    fn the_servers_own_origin_comes_from_its_bind_and_port() {
        let server = ServerConfig { bind: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), ..config() };
        assert!(allows(Some("http://10.0.0.5:9001"), &server));
        assert!(!allows(Some("http://10.0.0.5:9002"), &server));
        assert!(!allows(Some("http://10.0.0.6:9001"), &server));
        let server = ServerConfig {
            bind: IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            ..config()
        };
        assert!(allows(Some("http://[fe80::1]:9001"), &server));
        assert!(!allows(Some("http://[fe80::2]:9001"), &server));
    }

    /// An origin the operator listed is allowed, with a trailing slash
    /// tolerated. The host is not loopback, so the list is what allows it.
    #[test]
    fn a_configured_origin_is_allowed() {
        let server = ServerConfig {
            allowed_origins: vec!["http://forge-web.example:8080".to_owned()],
            ..config()
        };
        assert!(allows(Some("http://forge-web.example:8080"), &server));
        assert!(allows(Some("http://forge-web.example:8080/"), &server));
        assert!(!allows(Some("http://forge-web.example:8081"), &server));
    }

    /// A client that sends no Origin is a native one, not a browser page.
    #[test]
    fn an_absent_origin_is_a_native_client() {
        assert!(allows(None, &config()));
    }
}

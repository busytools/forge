//! Heuristic loopback-bind detection.
//!
//! Used at daemon startup to decide whether the configured listen
//! address is loopback-only. Non-loopback binds without app-layer auth
//! are surfaced as a `WARN` log so operators see the trust boundary
//! before their daemon is reachable from the network.
//!
//! Strict CIDR semantics aren't used here — we work with the literal
//! string the operator typed (e.g. `127.0.0.1:7373`, `[::1]:7373`,
//! `localhost:7373`) and consult well-known prefixes. This keeps the
//! check independent of the `std::net` parser and keeps both `host:port`
//! and bracketed IPv6 forms covered.

/// Is `bind` loopback-only? Recognises:
///
/// - `127.0.0.0/8` (IPv4 loopback block) — any address starting with `127.`
/// - `[::1]:N` — IPv6 loopback in canonical bracketed form
/// - `localhost:N` — DNS name reserved for loopback (RFC 6761 §6.3)
///
/// Anything else (including `0.0.0.0`, `[::]`, private RFC 1918 ranges,
/// public IPs, and unbracketed IPv6 forms) returns `false`.
#[must_use]
pub fn is_loopback_bind(bind: &str) -> bool {
    bind.starts_with("127.") || bind.starts_with("[::1]") || bind.starts_with("localhost")
}

#[cfg(test)]
mod tests {
    use super::is_loopback_bind;

    #[test]
    fn ipv4_loopback_127_0_0_1_is_loopback() {
        assert!(is_loopback_bind("127.0.0.1:7373"));
    }

    #[test]
    fn ipv4_loopback_block_127_anything_is_loopback() {
        // 127/8 is the whole loopback block.
        assert!(is_loopback_bind("127.0.0.0:7373"));
        assert!(is_loopback_bind("127.255.255.254:7373"));
    }

    #[test]
    fn ipv4_unspecified_0_0_0_0_is_not_loopback() {
        assert!(!is_loopback_bind("0.0.0.0:7373"));
    }

    #[test]
    fn ipv6_loopback_bracketed_is_loopback() {
        assert!(is_loopback_bind("[::1]:7373"));
    }

    #[test]
    fn ipv6_unspecified_bracketed_is_not_loopback() {
        assert!(!is_loopback_bind("[::]:7373"));
    }

    #[test]
    fn localhost_dns_name_is_loopback() {
        assert!(is_loopback_bind("localhost:7373"));
    }

    #[test]
    fn rfc1918_private_192_168_is_not_loopback() {
        assert!(!is_loopback_bind("192.168.1.1:7373"));
    }

    #[test]
    fn rfc1918_private_10_0_0_1_is_not_loopback() {
        // Private but not loopback — only 127/8 counts as loopback in
        // IPv4. The contract here is exactly "loopback", not "private".
        assert!(!is_loopback_bind("10.0.0.1:7373"));
    }
}

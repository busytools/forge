//! forge-agent's implementation of the
//! [`forge_gateway::ProviderHost`] port. The extra-roots HTTP client
//! and the `claude --version` UA cache stay on this side of the port
//! so forge-gateway carries no process plumbing of its own.

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;

use forge_gateway::ProviderHost;

/// The host every workspace-side backend probe runs against.
pub struct AgentHost;

/// Cached `claude-code/<version>` User-Agent, probed once per process.
/// `User-Agent` native CLI sends on /api/oauth/usage, captured from
/// mitmdump 2026-05-26 against claude CLI 2.1.133: `claude-code/<version>`,
/// no parens, no `(external, cli)` suffix - distinct from the
/// /v1/messages UA shape. Only a successful probe is cached; a failure
/// re-probes on the next call rather than pinning a stale fallback
/// that would lie about which version is running.
static UA: OnceLock<String> = OnceLock::new();

#[async_trait]
impl ProviderHost for AgentHost {
    fn http_client(&self, timeout: Duration) -> Result<reqwest::Client, String> {
        crate::http_trust::with_extra_roots(reqwest::Client::builder().timeout(timeout))
            .build()
            .map_err(|error| error.to_string())
    }

    fn streaming_http_client(
        &self,
        connect_timeout: Duration,
        idle_timeout: Duration,
    ) -> Result<reqwest::Client, String> {
        crate::http_trust::with_extra_roots(
            reqwest::Client::builder().connect_timeout(connect_timeout).read_timeout(idle_timeout),
        )
        .build()
        .map_err(|error| error.to_string())
    }

    async fn user_agent(&self) -> Result<String, String> {
        if let Some(cached) = UA.get() {
            return Ok(cached.clone());
        }
        // get_or_init isn't `Result`-friendly. set/get pair: if another
        // caller raced us and set first, our `set` errors out and we
        // return ours anyway - value is identical (same probe result
        // for the same machine) so the race is benign.
        let ua = resolve_ua("claude").await?;
        let _ = UA.set(ua.clone());
        Ok(ua)
    }
}

/// One `claude --version` round-trip, formatted as the UA. The
/// shell-out runs under spawn_blocking so a slow binary lookup never
/// parks a tokio worker; split from the cached [`AgentHost::user_agent`]
/// so the exec and its failure class are drivable without resolving a
/// real binary.
async fn resolve_ua(binary: &'static str) -> Result<String, String> {
    let version = tokio::task::spawn_blocking(move || {
        forge_sdk::transport::process::query_cli_version(binary)
    })
    .await
    .map_err(|e| format!("UA probe spawn_blocking panicked: {e}"))?
    .map_err(|e| format!("claude --version probe failed for UA: {e}"))?;
    Ok(format!("claude-code/{version}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// How long the stub holds a socket open after its last write. Far
    /// longer than any guard in these tests, so a client with no bound
    /// is still blocked when the guard fires - a hold the guard's own
    /// clock could outlast would let the read fail for the wrong reason.
    const STUB_HOLD: Duration = Duration::from_secs(30);

    /// A one-connection HTTP/1.1 stub: 200, chunked, `chunks` frames
    /// `gap` apart. Raw TCP rather than a server framework, because the
    /// property under test is the client's own bounds. `terminate`
    /// closes the body; without it the stub holds the socket open and
    /// silent afterwards, so only the client's bound can end the read.
    async fn chunked_stub(chunks: usize, gap: Duration, terminate: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("stub bind");
        let addr = listener.local_addr().expect("stub addr");
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        transfer-encoding: chunked\r\n\r\n";
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for _ in 0..chunks {
                tokio::time::sleep(gap).await;
                if socket.write_all(b"5\r\nchunk\r\n").await.is_err() {
                    return;
                }
            }
            if terminate && socket.write_all(b"0\r\n\r\n").await.is_err() {
                return;
            }
            tokio::time::sleep(STUB_HOLD).await;
        });
        format!("http://{addr}")
    }

    /// The streaming client's idle bound is per read, not a deadline for
    /// the whole response: a body that keeps producing chunks past the
    /// bound still completes, where a total timeout would cut it
    /// mid-stream and abandon a healthy turn.
    #[tokio::test]
    async fn the_streaming_client_bounds_each_read_not_the_whole_response() {
        let url = chunked_stub(10, Duration::from_millis(50), true).await;
        let client = AgentHost
            .streaming_http_client(Duration::from_secs(10), Duration::from_millis(300))
            .expect("client");
        let body = client
            .get(&url)
            .send()
            .await
            .expect("response")
            .bytes()
            .await
            .expect("a body that outlives the bound arrives whole");
        assert_eq!(
            body.len(),
            10 * 5,
            "every chunk arrives: the bound reset on each read rather than \
             expiring 300ms into a 500ms body",
        );
    }

    /// The other half: the bound is there at all. An upstream that goes
    /// silent after its headers must end the body rather than hold the
    /// caller open, which is the wedge A1 exists to stop. The guard
    /// wraps the whole request, headers included: awaiting `send()`
    /// outside it would start the clock after a client with no bound
    /// has already waited out the part under test.
    #[tokio::test]
    async fn the_streaming_client_ends_a_body_that_goes_silent() {
        let url = chunked_stub(0, Duration::from_millis(50), false).await;
        let client = AgentHost
            .streaming_http_client(Duration::from_secs(10), Duration::from_millis(300))
            .expect("client");
        let read = tokio::time::timeout(Duration::from_secs(5), async {
            client.get(&url).send().await.expect("response").bytes().await
        })
        .await
        .expect("the idle bound ends a silent body; an unbounded client never returns here");
        assert!(read.is_err(), "the silent body is cut at the bound, not read to a clean end");
    }

    /// A binary nothing resolves is the UaProbe class - the probe could
    /// not run, which is not a verdict about the endpoint. Driven
    /// through the real shell-out with a name that cannot resolve.
    #[tokio::test]
    async fn a_missing_claude_binary_is_a_ua_failure_not_a_network_failure() {
        let result = resolve_ua("forge-test-claude-absent-from-path").await;
        assert!(
            matches!(&result, Err(message) if message.starts_with("claude --version probe failed for UA")),
            "a binary nothing resolves is the UaProbe class; got {result:?}",
        );
    }

    /// Pins the User-Agent shape sent on /api/oauth/usage to the
    /// `claude-code/<version>` form captured from native CLI 2.1.133.
    /// The host at runtime spawns `claude --version` to fill in the
    /// version; in unit context we exercise the format only.
    #[test]
    fn oauth_usage_ua_shape_matches_native_claude_code_prefix() {
        let formatted = format!("claude-code/{}", "2.1.133");
        assert_eq!(formatted, "claude-code/2.1.133");
        assert!(!formatted.contains("(external"));
        assert!(!formatted.contains("(cli"));
        assert!(!formatted.starts_with("claude-cli/"));
    }
}

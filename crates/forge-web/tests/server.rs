//! The server's wiring: it binds what the config says, reports what it
//! bound, and binds nothing at all when it is turned off.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_primitives::WebConfig;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};

/// A port to hand the server: bind one, read it, let it go. Something
/// else can take it in the gap before the server binds, which is why
/// `start_on_a_free_port` retries rather than trusting this.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("the ephemeral address").port();
    drop(listener);
    port
}

/// Start on a free port, retrying if the gap in `free_port` lost the race.
async fn start_on_a_free_port(bind: IpAddr) -> (SocketAddr, WebConfig) {
    start_on_a_free_port_with(bind, std::convert::identity).await
}

/// [`start_on_a_free_port`] with the config adjusted before it is handed
/// over, so a test can set a key without giving up the retry.
async fn start_on_a_free_port_with(
    bind: IpAddr,
    adjust: impl Fn(WebConfig) -> WebConfig,
) -> (SocketAddr, WebConfig) {
    for _ in 0..8 {
        let config = adjust(WebConfig { port: free_port(), bind, ..WebConfig::default() });
        match forge_web::start(config.clone()).await {
            Ok(Some(bound)) => return (bound, config),
            Ok(None) => panic!("an enabled config must not come back disabled"),
            // A stolen probe port: take another and try again.
            Err(_) => {}
        }
    }
    panic!("no free port after eight tries");
}

/// Every interface rather than loopback, so the address has to come
/// from the config: loopback is what a hardcoded one would look like.
#[tokio::test]
async fn serves_on_the_configured_address() {
    let (bound, config) = start_on_a_free_port(IpAddr::V4(Ipv4Addr::UNSPECIFIED)).await;
    assert_eq!(
        bound,
        SocketAddr::new(config.bind, config.port),
        "the listener bound the configured address, not a default",
    );

    let body = reqwest::get(format!("http://127.0.0.1:{}/", config.port))
        .await
        .expect("the page is served")
        .text()
        .await
        .expect("the body reads");
    assert!(
        body.contains(&format!("listening on {bound}")),
        "the page reports the address it bound, got: {body}",
    );
    assert!(
        body.contains(&format!(
            "enabled = {}, port = {}, bind = {}",
            config.enabled, config.port, config.bind
        )),
        "the page reports what the config said, got: {body}",
    );
}

/// The Klin path, verbatim from the sheet the marks were picked from. A
/// literal rather than a call into the crate, so a redrawn or mistyped
/// path fails here instead of agreeing with itself.
const KLIN_PATH: &str = "M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2Zm2.5 \
                         19v-8.5a4.5 4.5 0 0 1 9 0V22h-9Z";

const LANES_BARS: &str = "x=\"10\" y=\"3\" width=\"4\" height=\"18\"";

async fn get(config: &WebConfig, path: &str) -> (reqwest::StatusCode, String, String) {
    let response =
        reqwest::get(format!("http://127.0.0.1:{}{path}", config.port)).await.expect("served");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map_or(String::new(), |value| value.to_str().expect("a readable content type").to_owned());
    (status, content_type, response.text().await.expect("the body reads"))
}

/// The mark a browser tab carries comes from `[web] mark`, so a mark
/// chosen in `forge.toml` is the one on the tab. Catches a route that
/// hardcodes the built-in, and one that serves the mark without a type a
/// browser will draw.
#[tokio::test]
async fn the_favicon_serves_the_configured_mark_in_the_palette() {
    let (_bound, config) =
        start_on_a_free_port_with(IpAddr::V4(Ipv4Addr::LOCALHOST), |mut config| {
            config.mark = Some("lanes".to_owned());
            config
        })
        .await;

    let (status, content_type, body) = get(&config, "/favicon.svg").await;

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(content_type, "image/svg+xml", "a browser draws it as an image, not as text");
    assert!(
        body.contains(LANES_BARS),
        "the favicon carries the mark the config picked, got: {body}",
    );
    assert!(
        body.contains("color=\"#f47600\""),
        "and draws it in the palette's accent, got: {body}",
    );
}

/// Unset keys are the built-ins: Klin on the tab, and the dark palette
/// as the page's own root variables rather than a stylesheet's copy of
/// it.
#[tokio::test]
async fn unset_names_fall_back_to_the_built_in_mark_and_palette() {
    let (_bound, config) = start_on_a_free_port(IpAddr::V4(Ipv4Addr::LOCALHOST)).await;

    let (status, _content_type, favicon) = get(&config, "/favicon.svg").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(favicon.contains(KLIN_PATH), "the default mark is Klin, got: {favicon}");

    let (status, _content_type, page) = get(&config, "/").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        page.contains("--accent:#f47600"),
        "the page carries the palette as its root variables, got: {page}",
    );
}

/// A port something else holds is an error rather than a silent no-op:
/// the caller is what tells the user the view is not serving, and an
/// on-by-default listener that loses a port fight has to say so.
#[tokio::test]
async fn a_taken_port_is_an_error() {
    let holder = TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = holder.local_addr().expect("the held address").port();
    let config = WebConfig { port, bind: IpAddr::V4(Ipv4Addr::LOCALHOST), ..WebConfig::default() };

    let error = forge_web::start(config).await.expect_err("a taken port must not pass as bound");

    assert!(
        error.to_string().contains(&format!("127.0.0.1:{port}")),
        "the error names the address it could not bind, got: {error}",
    );
}

/// The port is held for the whole test, so a view that tried to bind it
/// would come back with an error: `Ok(None)` is what says it never tried.
/// No window for another process to take the port, and no listening check
/// to be right about for the wrong reason.
#[tokio::test]
async fn disabled_binds_nothing() {
    let holder = TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = holder.local_addr().expect("the held address").port();
    let config = WebConfig {
        enabled: false,
        port,
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        ..WebConfig::default()
    };

    let bound = forge_web::start(config).await.expect("turning it off is not an error");

    assert!(bound.is_none(), "a disabled server binds nothing");
}

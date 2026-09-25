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
    for _ in 0..8 {
        let config = WebConfig { enabled: true, port: free_port(), bind };
        match forge_web::start(config).await {
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

/// A port something else holds is an error rather than a silent no-op:
/// the caller is what tells the user the view is not serving, and an
/// on-by-default listener that loses a port fight has to say so.
#[tokio::test]
async fn a_taken_port_is_an_error() {
    let holder = TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = holder.local_addr().expect("the held address").port();
    let config = WebConfig { enabled: true, port, bind: IpAddr::V4(Ipv4Addr::LOCALHOST) };

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
    let config = WebConfig { enabled: false, port, bind: IpAddr::V4(Ipv4Addr::LOCALHOST) };

    let bound = forge_web::start(config).await.expect("turning it off is not an error");

    assert!(bound.is_none(), "a disabled server binds nothing");
}

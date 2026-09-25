//! The server's wiring: it binds what the config says, reports what it
//! bound, and binds nothing at all when it is turned off.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use forge_primitives::WebConfig;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};

/// A port nothing is listening on: bind one, read it, let it go. The
/// window between that drop and the server's own bind is ours, not the
/// server's.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("the ephemeral address").port();
    drop(listener);
    port
}

/// Every interface rather than loopback, so the address has to come
/// from the config: loopback is what a hardcoded one would look like.
#[tokio::test]
async fn serves_on_the_configured_address() {
    let port = free_port();
    let config = WebConfig { enabled: true, port, bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED) };

    let bound = forge_web::start(config).await.expect("the server starts").expect("it is enabled");
    assert_eq!(
        bound,
        SocketAddr::new(config.bind, port),
        "the listener bound the configured address, not a default",
    );

    let body = reqwest::get(format!("http://127.0.0.1:{port}/"))
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

#[tokio::test]
async fn disabled_binds_nothing() {
    let port = free_port();
    let config = WebConfig { enabled: false, port, bind: IpAddr::V4(Ipv4Addr::LOCALHOST) };

    let bound = forge_web::start(config).await.expect("turning it off is not an error");

    assert!(bound.is_none(), "a disabled server binds nothing");
    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "nothing is listening on the configured port",
    );
}

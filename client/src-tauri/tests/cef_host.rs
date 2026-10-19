#![cfg(target_os = "macos")]
//! The CEF host's spawn and lifecycle, with a stand-in binary in place of the
//! real engine.
//!
//! What a stub can pin is the client's own decisions: the record it writes,
//! the endpoint it waits for, the kill it escalates, and the boot sweep that
//! cleans a host whose client never ran its exit. What needs a real Chromium
//! - frames, input, the dialog - belongs to the probe and the look, not here.

use std::path::Path;
use std::time::Duration;

use forge_client::browser::cef::{osr, spawn};
use forge_client::browser::{BrowserHost, StackPaths};

fn paths(dir: &Path) -> StackPaths {
    StackPaths {
        stack: dir.join("stack"),
        user_data: dir.join("user-data"),
        output: dir.join("output"),
        profiles: dir.join("profiles"),
    }
}

/// The shared cache root is its own directory, and a named profile's hangs
/// under `cef/` so the Brave data an old launch left is never read as this
/// profile's cookies.
#[test]
fn the_cache_roots_are_their_own_directories() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = paths(dir.path());
    assert_eq!(paths.cef_shared(), dir.path().join("cef-profile"));
    assert_eq!(paths.cef_named("hunt"), dir.path().join("profiles/hunt/cef"));
    assert_ne!(paths.cef_shared(), paths.user_data, "the Brave data dir is not the CEF root");
}

/// A record round-trips, and a half-written one reads as no record rather
/// than a guess.
#[test]
fn a_record_round_trips() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("profile");
    std::fs::create_dir_all(&profile).expect("the dir");
    assert_eq!(spawn::read_record(&profile), None, "nothing written yet");
    let record = spawn::Record { pid: 4242, port: 9411, ring: "/forge-cef-9411".to_owned() };
    spawn::write_record(&profile, &record).expect("the write");
    assert_eq!(spawn::read_record(&profile), Some(record));
    std::fs::write(spawn::record_path(&profile), b"{not json").expect("a torn record");
    assert_eq!(spawn::read_record(&profile), None, "a torn record is no record");
}

/// One ring name per port, so two hosts never share pixels.
#[test]
fn the_ring_is_named_for_its_port() {
    assert_eq!(spawn::ring_name(9411), "/forge-cef-9411");
    assert_ne!(spawn::ring_name(9411), spawn::ring_name(9413));
}

/// The wait answers when a `/json/version` comes back, and names the port
/// when nothing does.
#[tokio::test]
async fn the_endpoint_wait_answers_or_names_the_port() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let mut buffer = [0_u8; 1024];
            let _ = socket.read(&mut buffer).await;
            let body = "{\"Browser\":\"Chrome/154.0\"}";
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    spawn::wait_ready_within(port, Duration::from_secs(5))
        .await
        .expect("the answering endpoint is found");

    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let quiet = silent.local_addr().expect("the address").port();
    let refused = spawn::wait_ready_within(quiet, Duration::from_millis(300)).await;
    assert!(
        refused.expect_err("nothing answers").contains(&quiet.to_string()),
        "the refusal names the port it waited on",
    );
}

/// The page a carry writes is read back from the endpoint's own target list.
#[tokio::test]
async fn the_page_url_reads_the_page_target() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let mut buffer = [0_u8; 1024];
            let _ = socket.read(&mut buffer).await;
            let body = r#"[{"type":"page","url":"https://example.com/job"}]"#;
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    assert_eq!(
        spawn::page_url(port).await.as_deref(),
        Some("https://example.com/job"),
        "the page target's url is what a carry writes"
    );
}

/// A host the client spawned is a process it can end: the kill is escalated,
/// and the pid reads dead afterwards.
#[tokio::test]
async fn a_spawned_host_is_ended_by_its_pid() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let profile = dir.path().join("cef-profile");
    let stub = dir.path().join("stub-host.sh");
    std::fs::write(&stub, "#!/bin/sh\nexec sleep 30\n").expect("the stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("the stub is executable");
    }
    let pid = spawn::spawn_with(&stub, &profile, 9411, "/forge-cef-9411", 800, 600)
        .expect("the stub starts");
    assert!(spawn::running(pid), "the spawned stub is alive");
    let record = spawn::Record { pid, port: 9411, ring: "/forge-cef-9411".to_owned() };
    spawn::write_record(&profile, &record).expect("the record");
    spawn::end(pid).await;
    assert!(!spawn::running(pid), "the end takes the process down");
}

/// **A live record is adopted, not replaced.** The endpoint answers and the
/// ring maps, so the client takes the host the record names as its own: no
/// second host on the profile, and the pid it answers with is the recorded
/// one.
#[tokio::test]
async fn a_live_record_is_adopted() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = paths(dir.path());
    let host = BrowserHost::new(paths.clone());
    let cache = paths.cef_shared();
    std::fs::create_dir_all(&cache).expect("the cache root");

    let stub = dir.path().join("stub-host.sh");
    std::fs::write(&stub, "#!/bin/sh\nexec sleep 30\n").expect("the stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("the stub is executable");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let mut buffer = [0_u8; 1024];
            let _ = socket.read(&mut buffer).await;
            let body = "{\"Browser\":\"Chrome/154.0\"}";
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    let ring_name = "/forge-cef-adopt-test";
    let ring = osr::Ring::create(ring_name, 64, 64).expect("a ring");
    std::mem::forget(ring);
    let pid = spawn::spawn_with(&stub, &cache, port, ring_name, 800, 600).expect("the stub");
    spawn::write_record(&cache, &spawn::Record { pid, port, ring: ring_name.to_owned() })
        .expect("the record");

    let adopted = host.start().await.expect("the live record is adopted");
    assert_eq!(adopted.pid, pid, "the recorded pid is the host");
    assert_eq!(adopted.port, port, "on the recorded port");
    spawn::end(pid).await;
    spawn::remove_record(&cache);
    osr::Ring::unlink(ring_name);
}

/// **A record whose host still answers is ended, not left running.** The
/// owning client is gone (the claim is free), so this client's sweep ends the
/// recorded host - a pid it spawned in this test, so the kill is its own.
#[tokio::test]
async fn the_boot_sweep_ends_a_recorded_host_that_still_answers() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = paths(dir.path());
    let host = BrowserHost::new(paths.clone());
    let cache = paths.cef_shared();
    std::fs::create_dir_all(&cache).expect("the cache root");

    let stub = dir.path().join("stub-host.sh");
    std::fs::write(&stub, "#!/bin/sh\nexec sleep 30\n").expect("the stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("the stub is executable");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the address").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let mut buffer = [0_u8; 1024];
            let _ = socket.read(&mut buffer).await;
            let body = "{\"Browser\":\"Chrome/154.0\"}";
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    let pid = spawn::spawn_with(&stub, &cache, port, "/forge-cef-sweep-test", 800, 600)
        .expect("the stub");
    spawn::write_record(
        &cache,
        &spawn::Record { pid, port, ring: "/forge-cef-sweep-test".to_owned() },
    )
    .expect("the record");

    let reaped = host.reap_orphans().await;
    assert!(reaped.iter().any(|name| name == "cef-profile"), "the root is named: {reaped:?}");
    assert!(!spawn::running(pid), "the recorded host is ended");
    assert_eq!(spawn::read_record(&cache), None, "and its record goes");
}

/// **Another live client's browser is not this sweep's.** A cache root whose
/// claim is held - by a client still running, here this test - keeps its host
/// and its record: the exit reap and the boot sweep both read the claim, and
/// killing through it would take a browser out from under someone driving it.
#[tokio::test]
async fn the_boot_sweep_leaves_a_claimed_host_alone() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = paths(dir.path());
    let host = BrowserHost::new(paths.clone());
    let cache = paths.cef_shared();
    std::fs::create_dir_all(&cache).expect("the cache root");
    let claim = forge_client::browser::profiles::claim(&cache)
        .expect("the claim reads")
        .expect("the cache root is free");
    spawn::write_record(
        &cache,
        &spawn::Record { pid: 999_999, port: 9411, ring: "/forge-cef-claimed-test".to_owned() },
    )
    .expect("the record");

    let reaped = host.reap_orphans().await;
    assert!(reaped.is_empty(), "a claimed root is not reaped: {reaped:?}");
    assert!(spawn::read_record(&cache).is_some(), "and its record stays");
    drop(claim);
}

/// **The boot sweep cleans what a dead client left.** A record naming a pid
/// that is gone, and the ring that outlives its host on macOS, are both
/// removed without launching anything.
#[tokio::test]
async fn the_boot_sweep_cleans_a_stale_record_and_its_ring() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = paths(dir.path());
    let host = BrowserHost::new(paths.clone());
    let cache = paths.cef_shared();
    std::fs::create_dir_all(&cache).expect("the cache root");
    let ring_name = "/forge-cef-stale-test";
    let ring = osr::Ring::create(ring_name, 64, 64).expect("a ring");
    std::mem::forget(ring);
    spawn::write_record(
        &cache,
        &spawn::Record { pid: 999_999, port: 9411, ring: ring_name.to_owned() },
    )
    .expect("the record");

    let reaped = host.reap_orphans().await;
    assert!(reaped.iter().any(|name| name == "cef-profile"), "the stale root is named: {reaped:?}");
    assert_eq!(spawn::read_record(&cache), None, "the record goes with the sweep");
    assert!(osr::Ring::open(ring_name).is_err(), "and the ring's name is gone");
}

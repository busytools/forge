//! The reap: a browser host does not outlive the client that launched it.
//!
//! **Deliberately ignored**, like the other live suites: it launches the
//! machine's own browser. It needs no vendored driver - nothing here drives a
//! page - so it runs wherever a browser exists:
//!
//! ```text
//! cargo nextest run --manifest-path client/src-tauri/Cargo.toml \
//!     --run-ignored ignored-only -E 'test(reap)'
//! ```
//!
//! **The boot sweep is the only cover for a client that never ran its exit**:
//! a crash, a SIGKILL, or the update restart, whose main-thread path skips
//! `RunEvent::Exit` by design (tauri 2.12). The first test below is that
//! path's proof - a browser whose owning client is gone, reaped by the next
//! boot - and the second direction is pinned beside it: a browser a live
//! client holds, this client's or another's, is left alone.

mod support;

use std::path::{Path, PathBuf};

use forge_client::browser::{BrowserHost, StackPaths, chromium};
use support::Launched;

fn stack_paths(dir: &Path) -> StackPaths {
    StackPaths {
        stack: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack"),
        user_data: dir.join("user-data"),
        output: dir.join("output"),
        profiles: dir.join("profiles"),
    }
}

/// A browser up on `profile` with no host behind it: the shape a client that
/// died leaves.
async fn a_browser_with_no_client(profile: &Path) -> Launched {
    let binary = chromium::browser_binary().expect("the machine's own browser");
    let active = chromium::launch(&binary, profile, &[]).await.expect("the browser comes up");
    Launched::new(active.pid, active.port, profile.to_path_buf())
}

/// **The boot sweep, both directions.** A browser whose owning client is gone
/// (nothing holds the profile's client lock) is reaped; one a live client
/// holds is left alone.
#[tokio::test]
#[ignore = "launches the machine's own browser; run deliberately"]
async fn the_boot_sweep_reaps_an_orphan_and_leaves_a_claimed_browser() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = stack_paths(dir.path());
    std::fs::create_dir_all(&paths.profiles).expect("the profiles root");

    let orphan = a_browser_with_no_client(&paths.user_data).await;
    let orphan_port = chromium::read_active_port(&paths.user_data).expect("the orphan's port").port;
    assert!(chromium::probe(orphan_port).await, "the orphan answers before the sweep");

    let held_dir = paths.profiles.join("held");
    let held = a_browser_with_no_client(&held_dir).await;
    let held_port = chromium::read_active_port(&held_dir).expect("the held port").port;
    let client = chromium::claim(&held_dir).expect("the claim").expect("a free profile");

    let host = BrowserHost::new(paths.clone());
    let reaped = host.reap_orphans().await;

    assert_eq!(reaped, vec!["user-data".to_owned()], "the orphan is what went");
    assert!(!chromium::probe(orphan_port).await, "and its browser is gone");
    assert!(chromium::probe(held_port).await, "a live client's browser is left alone");

    drop(client);
    orphan.reap();
    held.reap();
}

/// The sweep is not a threat to this client's own browsers: what `start()`
/// claimed is never read as an orphan.
#[tokio::test]
#[ignore = "launches the machine's own browser; run deliberately"]
async fn the_sweep_never_reaps_a_browser_this_client_claimed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = stack_paths(dir.path());
    let host = BrowserHost::new(paths.clone());

    let mine = host.start().await.expect("the browser comes up");
    let browser = Launched::new(mine.pid, mine.port, paths.user_data.clone());

    let reaped = host.reap_orphans().await;

    assert!(reaped.is_empty(), "nothing of this client's is an orphan: {reaped:?}");
    assert!(chromium::probe(mine.port).await, "and its browser is still answering");
    browser.reap();
}

/// **The exit door.** A browser this client claimed goes with it.
#[tokio::test]
#[ignore = "launches the machine's own browser; run deliberately"]
async fn the_exit_reap_closes_the_browser_this_client_claimed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = stack_paths(dir.path());
    let host = BrowserHost::new(paths.clone());

    let mine = host.start().await.expect("the browser comes up");
    let browser = Launched::new(mine.pid, mine.port, paths.user_data.clone());

    let reaped = host.reap_owned().await;

    assert_eq!(reaped, vec!["user-data".to_owned()], "the profile it claimed is what went");
    assert!(!chromium::probe(mine.port).await, "and its browser is gone");
    browser.reap();
}

/// **A browser another live client holds is not this client's to reap**, even
/// when this client drives it: the host adopts the answering browser but the
/// contended claim never becomes its own, so the exit reap leaves it running.
#[tokio::test]
#[ignore = "launches the machine's own browser; run deliberately"]
async fn the_exit_reap_leaves_another_clients_browser_alone() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = stack_paths(dir.path());
    let host = BrowserHost::new(paths.clone());

    // The other client, standing here as the test's own held claim.
    let other = a_browser_with_no_client(&paths.user_data).await;
    let other_port = chromium::read_active_port(&paths.user_data).expect("the other's port").port;
    let other_client =
        chromium::claim(&paths.user_data).expect("the claim").expect("a free profile");

    let adopted = host.start().await.expect("the host adopts the answering browser");
    assert_eq!(adopted.port, other_port, "the host drives the browser that was already up");
    assert_eq!(adopted.pid, None, "and launched nothing of its own");

    let reaped = host.reap_owned().await;
    assert!(reaped.is_empty(), "what this client does not own is not its to reap: {reaped:?}");
    assert!(chromium::probe(other_port).await, "the other client's browser keeps running");

    drop(other_client);
    other.reap();
}

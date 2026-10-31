//! The boot reaper against a live host.
//!
//! **A fresh host is not an orphan.** The reaper runs detached at boot while
//! an attach can start a host on the same profile; `is_our_host` proves the
//! pid is this profile's host, and the child registry still tells a host this
//! process started from one an unclean exit left. Before that guard, the
//! reaper signalled the host it had just been handed and wiped its trail,
//! failing the first browser action after a crash.
//!
//! **These tests need the INSTALLED bundle**: the reaper identifies a host by
//! the installed executable's own path, and a scratch staging is a stranger
//! to it by design. `scripts/stage-browser-host.sh` stages one where the
//! server looks; the macOS CI job runs them with `--run-ignored`, so a fresh
//! checkout's plain nextest stays green without it.

#![cfg(target_os = "macos")]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
#![allow(unsafe_code, reason = "the drop guard signals this test's own child")]

mod support;

use forge_browser::osr::Ring;
use forge_browser::service::BrowserService;
use forge_browser::spawn;
use std::path::Path;

/// End this test's host and drop its ring however the test ends.
struct Guard {
    pid: u32,
    ring: String,
}

impl Guard {
    fn new(pid: u32, ring: &str) -> Self {
        Self { pid, ring: ring.to_owned() }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: a plain signal to this test's own child, by the pid it
        // saved. A host already ended ignores it.
        unsafe { libc::kill(i32::try_from(self.pid).unwrap_or(i32::MAX), libc::SIGTERM) };
        Ring::unlink(&self.ring);
    }
}

/// The installed host executable at THIS crate's version, or a panic naming
/// how to stage it.
fn installed_host() -> std::path::PathBuf {
    forge_browser::host_bundle::checked_executable().expect(
        "the host bundle is installed at this crate's version; scripts/stage-browser-host.sh \
         stages it",
    )
}

/// A profile in the store's org-rooted shape, made and returned.
fn profile_in(root: &Path) -> std::path::PathBuf {
    let profile = root.join("profiles").join("shared").join("default").join("cef");
    std::fs::create_dir_all(&profile).expect("the profile dir");
    profile
}

/// **The walk's denominator, for a survivor test.** A second profile whose
/// record names a pid no host owns; the reaper clears that record, so a test
/// that only asserts its own host survived can also prove the sweep reached
/// the store instead of walking an empty tree.
fn stale_sibling(root: &Path) -> std::path::PathBuf {
    let profile = root.join("profiles").join("sibling").join("default").join("cef");
    std::fs::create_dir_all(&profile).expect("the sibling profile dir");
    spawn::write_record(
        &profile,
        &spawn::Record { pid: 1, port: 1, ring: "/forge-reap-sibling".to_owned() },
    )
    .expect("the sibling record");
    profile
}

/// **A host this process started survives the boot reap.** The record names a
/// live host of this profile, so the reaper leaves it and its trail alone.
#[ignore = "needs the installed Forge Browser.app; scripts/stage-browser-host.sh stages it"]
#[tokio::test(flavor = "multi_thread")]
async fn a_host_this_process_started_survives_the_boot_reap() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = installed_host();
    let root = scratch.path().join("browser");
    let profile = profile_in(&root);
    let port = support::free_port();
    let ring = format!("/forge-reap-{}", std::process::id());

    let (pid, _input, _events) = spawn::spawn_with(
        &exe,
        &profile,
        port,
        &ring,
        800,
        600,
        1,
        (15, 75),
        &forge_browser::channel::socket_path(&profile),
    )
    .expect("the host spawns");
    let _guard = Guard::new(pid, &ring);
    spawn::wait_ready_for(pid, port, &ring).await.unwrap_or_else(|why| {
        panic!("the host never came ready: {why} (log: {})", profile.join("cef-host.log").display())
    });
    spawn::write_record(&profile, &spawn::Record { pid, port, ring: ring.clone() })
        .expect("the record");

    let sibling = stale_sibling(&root);
    let service = BrowserService::at(root.clone(), scratch.path().join("stack"));
    service.reap_stale().await;

    assert!(
        spawn::read_record(&sibling).is_none(),
        "the sweep reached the store: the sibling's record is cleared"
    );
    assert!(spawn::alive(pid), "the reaper left a host this process started");
    assert_eq!(
        spawn::read_record(&profile).map(|record| record.pid),
        Some(pid),
        "and its trail still names it"
    );

    spawn::end_verified(pid, port, &profile).await;
}

/// **A host an unclean exit left behind is ended, and its trail goes with
/// it.** The reaper's positive branch is what releases a profile's singleton
/// lock: one that only cleared files would leave the host running, and one
/// that only signalled would wedge the next boot on the trail.
#[ignore = "needs the installed Forge Browser.app; scripts/stage-browser-host.sh stages it"]
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_host_is_ended_and_its_trail_cleared() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = installed_host();
    let root = scratch.path().join("browser");
    let profile = profile_in(&root);
    let port = support::free_port();
    let ring = format!("/forge-reap-stale-{}", std::process::id());
    let log = scratch.path().join("host.log");
    // **Started outside `spawn_with`**: this test's own child is not in the
    // server's registry, which is what a host from a crashed process looks
    // like to the reaper.
    let mut host = support::Host::start(&exe, &profile, port, &ring, "800x600", &[], &log);
    let _ring = RingOnly(ring.clone());
    spawn::write_record(&profile, &spawn::Record { pid: host.pid(), port, ring: ring.clone() })
        .expect("the record");
    spawn::wait_ready_for(host.pid(), port, &ring).await.unwrap_or_else(|why| {
        panic!("the host never came ready: {why} (log: {})", profile.join("cef-host.log").display())
    });
    // **A process's path and arguments can trail its exec for a moment under
    // a loaded runner**, while the reaper meets hosts that have been up for a
    // while. The wait is this test's timing, not a softer rule: an identity
    // that is genuinely wrong still fails, at the same message, ten seconds on.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !spawn::is_our_host(host.pid(), &profile, Some(port)) {
        let binary = spawn::is_our_binary(host.pid());
        assert!(
            std::time::Instant::now() < deadline,
            "the installed bundle's host is the identity the reaper signals (binary ours \
             {binary}, running {}, expected {:?}, pid {}, profile {}, port {port}, log: {})",
            running_path(host.pid()).unwrap_or_default(),
            forge_browser::host_bundle::executable(),
            host.pid(),
            profile.display(),
            host.log(),
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // **The positive control**: the lock the reap is meant to clear is there
    // before it runs, so a reap that never got that far cannot pass.
    assert!(
        std::fs::symlink_metadata(profile.join("SingletonLock")).is_ok(),
        "the host's singleton lock exists before the reap",
    );

    let service = BrowserService::at(root.clone(), scratch.path().join("stack"));
    service.reap_stale().await;

    // **CEF's own shutdown is the wait, not the reaper's signal**: the
    // reaper insists at three seconds, and a loaded machine's browser can
    // take tens more to drain its process tree.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if host.try_wait().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the reaper left the stale host running (log: {})",
            host.log(),
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(spawn::read_record(&profile).is_none(), "the record went with the host");
    assert!(
        std::fs::symlink_metadata(profile.join("SingletonLock")).is_err(),
        "the singleton lock is cleared so the next launch can take it",
    );
}

/// **A host whose server went away closes itself.** The server's own
/// teardown is the fast path; this is the backstop for a kill that runs no
/// exit path. The host is spawned through the server's own `spawn_with`, so
/// the watch pipe is the production one; releasing its write end is exactly
/// what that server process dying does to it.
#[cfg(feature = "testing")]
#[ignore = "needs the installed Forge Browser.app; scripts/stage-browser-host.sh stages it"]
#[tokio::test(flavor = "multi_thread")]
async fn a_host_whose_parent_went_away_closes_itself() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = installed_host();
    let root = scratch.path().join("browser");
    let profile = profile_in(&root);
    let port = support::free_port();
    let ring = format!("/forge-reap-parent-{}", std::process::id());
    let (pid, _input, _events) = spawn::spawn_with(
        &exe,
        &profile,
        port,
        &ring,
        800,
        600,
        1,
        (15, 75),
        &forge_browser::channel::socket_path(&profile),
    )
    .expect("the host spawns");
    let _guard = Guard::new(pid, &ring);
    spawn::wait_ready_for(pid, port, &ring).await.unwrap_or_else(|why| {
        panic!("the host never came ready: {why} (log: {})", profile.join("cef-host.log").display())
    });

    assert!(
        spawn::drop_parent_watch_for_test(pid),
        "the server's own watch is the pipe the host is reading",
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if spawn::exit(pid).is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the host outlived the server that spawned it (log: {})",
            profile.join("cef-host.log").display(),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// **A host named only by its singleton lock is not reaped while it is
/// ours.** Chromium's own lock is the fallback identity when the record was
/// lost; a live child of this process named there keeps it and its lock.
#[ignore = "needs the installed Forge Browser.app; scripts/stage-browser-host.sh stages it"]
#[tokio::test(flavor = "multi_thread")]
async fn a_host_named_only_by_its_singleton_survives_the_boot_reap() {
    let scratch = tempfile::tempdir().expect("a scratch dir");
    let exe = installed_host();
    let root = scratch.path().join("browser");
    let profile = profile_in(&root);
    let port = support::free_port();
    let ring = format!("/forge-reap-singleton-{}", std::process::id());

    let (pid, _input, _events) = spawn::spawn_with(
        &exe,
        &profile,
        port,
        &ring,
        800,
        600,
        1,
        (15, 75),
        &forge_browser::channel::socket_path(&profile),
    )
    .expect("the host spawns");
    let _guard = Guard::new(pid, &ring);
    spawn::wait_ready_for(pid, port, &ring).await.unwrap_or_else(|why| {
        panic!("the host never came ready: {why} (log: {})", profile.join("cef-host.log").display())
    });
    // **No record**: the singleton is the only trail this test leaves.
    assert!(spawn::read_record(&profile).is_none(), "no record is written");
    let lock = profile.join("SingletonLock");
    let target = std::fs::read_link(&lock).expect("the host's singleton lock");
    assert!(
        target.to_string_lossy().ends_with(&format!("-{pid}")),
        "the singleton names the host: {}",
        target.display(),
    );

    let sibling = stale_sibling(&root);
    let service = BrowserService::at(root.clone(), scratch.path().join("stack"));
    service.reap_stale().await;

    assert!(
        spawn::read_record(&sibling).is_none(),
        "the sweep reached the store: the sibling's record is cleared"
    );
    assert!(spawn::alive(pid), "the reaper left a host this process started");
    assert!(
        std::fs::symlink_metadata(&lock).is_ok(),
        "and the singleton that names it is left in place",
    );

    spawn::end_verified(pid, port, &profile).await;
}

/// The path the kernel reports for a running process, for a failed identity
/// assertion: the two paths are what tell a symlinked install from a wrong
/// binary.
fn running_path(pid: u32) -> Option<String> {
    let mut buffer = [0_i8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `proc_pidpath` writes at most the buffer's length.
    let wrote = unsafe {
        libc::proc_pidpath(
            i32::try_from(pid).unwrap_or(i32::MAX),
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
        )
    };
    if wrote <= 0 {
        return None;
    }
    // SAFETY: a successful call NUL-terminates the path in the buffer.
    Some(unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned())
}

/// Drop-time ring cleanup for a test whose host owns its own child.
struct RingOnly(String);

impl Drop for RingOnly {
    fn drop(&mut self) {
        Ring::unlink(&self.0);
    }
}

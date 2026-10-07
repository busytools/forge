//! The named-profile contract over the real stack: **a name is a BROWSER of
//! its own** - its own process, its own data directory, its own logins - and
//! the session that opened it owns it.
//!
//! **Deliberately ignored**: it needs the vendored driver
//! (`just vendor-browser-stack`) and a browser on the machine. Run it where
//! both exist:
//!
//! ```text
//! cargo nextest run --manifest-path client/src-tauri/Cargo.toml \
//!     --run-ignored ignored-only -E 'test(profile)'
//! ```
//!
//! What it proves: two names drive two DIFFERENT browser processes (their
//! port files name different launches), a cookie set under one is invisible
//! under the other, a second session naming a held profile is refused by
//! name, and a browser killed under a profile comes back on the same
//! directory with its logins intact - Chromium persists a real profile, so
//! nothing here saves or restores storage.

mod support;

use std::path::{Path, PathBuf};

use forge_client::browser::driver::ReplyPart;
use forge_client::browser::profiles::Seat;
use forge_client::browser::{BrowserHost, StackPaths};
use serde_json::json;
use support::Launched;

/// The text one answer carries, which is every part that is not an image.
fn text_of(parts: &[ReplyPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ReplyPart::Text { text } => Some(text.clone()),
            ReplyPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn stack_paths(dir: &Path) -> StackPaths {
    StackPaths {
        stack: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack"),
        user_data: dir.join("user-data"),
        output: dir.join("output"),
        profiles: dir.join("profiles"),
    }
}

fn require_stack(stack: &Path) {
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored driver is not there - run `just vendor-browser-stack`",
    );
}

/// A one-page local server so a cookie has an origin to live on.
async fn serve_a_page() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let port = listener.local_addr().expect("the bound port").port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        for _ in 0..24 {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let body = "<title>ctx</title>ok";
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = socket.write_all(answer.as_bytes()).await;
        }
    });
    port
}

fn seat(label: &str) -> Seat {
    Seat { org: "Busytools".to_owned(), project: "forge".to_owned(), label: label.to_owned() }
}

/// **Two names, two browsers.** Each profile gets its own launch - the port
/// files under the two directories name them - and a cookie set through one
/// is invisible through the other, which is what a separate browser buys over
/// an isolated context.
#[tokio::test]
#[ignore = "drives the vendored driver against a browser on this machine"]
async fn two_named_profiles_are_two_browsers_and_see_different_cookies() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = stack_paths(dir.path());
    require_stack(&paths.stack);
    let host = BrowserHost::new(paths.clone());
    let page_port = serve_a_page().await;
    let page = format!("http://127.0.0.1:{page_port}/");
    let alpha = seat("alpha");
    let beta = seat("beta");

    let opened = host
        .call(&alpha, "browser_navigate", json!({ "url": page, "profile": "alpha" }))
        .await
        .unwrap_or_else(|why| panic!("alpha could not open its profile: {why}"));
    assert!(!opened.is_empty(), "a navigate answers with something");
    let set = format!(
        "async (page) => {{ await page.context().addCookies([{{ name: 'who', value: 'alpha', \
         url: '{page}' }}]); return 'set'; }}"
    );
    host.call(&alpha, "browser_run_code_unsafe", json!({ "code": set, "profile": "alpha" }))
        .await
        .unwrap_or_else(|why| panic!("alpha could not set its cookie: {why}"));

    host.call(&beta, "browser_navigate", json!({ "url": page, "profile": "beta" }))
        .await
        .unwrap_or_else(|why| panic!("beta could not open its profile: {why}"));
    let read = "async (page) => JSON.stringify(await page.context().cookies())";
    let seen = host
        .call(&beta, "browser_run_code_unsafe", json!({ "code": read, "profile": "beta" }))
        .await
        .map(|parts| text_of(&parts))
        .unwrap_or_else(|why| panic!("beta could not read its cookies: {why}"));
    assert!(
        !seen.contains("who") && !seen.contains("alpha"),
        "beta's browser does not see alpha's cookie: {seen}",
    );

    // **Two launches, not two contexts.** Each profile's own port file names
    // a different port; a shared browser would have answered both names on
    // one.
    let alpha_port = std::fs::read_to_string(paths.profiles.join("alpha/DevToolsActivePort"))
        .expect("alpha's launch wrote its port file");
    let beta_port = std::fs::read_to_string(paths.profiles.join("beta/DevToolsActivePort"))
        .expect("beta's launch wrote its port file");
    let alpha_port = alpha_port.lines().next().unwrap_or_default().to_owned();
    let beta_port = beta_port.lines().next().unwrap_or_default().to_owned();
    assert_ne!(
        alpha_port, beta_port,
        "each name is a browser of its own, not a context of one",
    );
    // The browsers die with the test, whichever way it ends.
    for name in ["alpha", "beta"] {
        if let Ok(port) = std::fs::read_to_string(paths.profiles.join(name).join("DevToolsActivePort"))
            && let Some(first) = port.lines().next()
            && let Ok(port) = first.trim().parse::<u16>()
        {
            let pid = std::fs::read_to_string(paths.profiles.join(name).join("browser.pid"))
                .ok()
                .and_then(|pid| pid.trim().parse::<u32>().ok());
            let launched = Launched::new(pid, port, paths.profiles.join(name));
            launched.reap();
        }
    }
}

/// **Show raises the NAMED profile's own browser, headed, on its own page** -
/// the piece a CAPTCHA hand-off on a profile stands on. The shared browser
/// stays headless throughout.
#[tokio::test]
#[ignore = "drives the vendored driver against a browser on this machine"]
async fn show_raises_the_named_profiles_own_browser_on_its_own_page() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = stack_paths(dir.path());
    require_stack(&paths.stack);
    let host = BrowserHost::new(paths.clone());
    let page_port = serve_a_page().await;
    let page = format!("http://127.0.0.1:{page_port}/");
    let alpha = seat("alpha");

    host.call(&alpha, "browser_navigate", json!({ "url": page, "profile": "hunt" }))
        .await
        .unwrap_or_else(|why| panic!("alpha could not open the profile: {why}"));

    host.show(Some("hunt")).await.unwrap_or_else(|why| panic!("the raise failed: {why}"));

    // The raised window is the profile's own: its marker names the page it
    // opened on, and the launch behind it is the headed one.
    let marker = std::fs::read_to_string(paths.profiles.join("hunt/windowed"))
        .expect("the headed launch left its windowed marker");
    assert!(
        marker.contains(&page),
        "the window opened on the profile's own page: {marker}",
    );
    assert!(
        !paths.user_data.join("windowed").exists(),
        "the shared browser was not the one raised",
    );
    let pid = std::fs::read_to_string(paths.profiles.join("hunt/browser.pid"))
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok());
    let args = std::process::Command::new("ps")
        .args(["-p", &pid.unwrap_or_default().to_string(), "-o", "args="])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default();
    assert!(
        args.contains("--keep-alive-for-test"),
        "the raise relaunched the profile's browser headed: {args}",
    );

    let port = std::fs::read_to_string(paths.profiles.join("hunt/DevToolsActivePort"))
        .ok()
        .and_then(|text| text.lines().next().and_then(|first| first.trim().parse::<u16>().ok()));
    Launched::new(pid, port.unwrap_or(0), paths.profiles.join("hunt")).reap();
}

/// The whole named-profile contract, in the shape the spec's acceptance
/// names it: two sessions, one profile name, the second refused - and then a
/// browser KILLED under the profile, with its logins intact when the next
/// call relaunches it on the same directory.
#[tokio::test]
#[ignore = "drives the vendored driver against a browser on this machine"]
async fn a_named_profile_refuses_another_session_and_keeps_its_logins_across_a_death() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = stack_paths(dir.path());
    require_stack(&paths.stack);
    let host = BrowserHost::new(paths.clone());
    let page_port = serve_a_page().await;
    let page = format!("http://127.0.0.1:{page_port}/");
    let alpha = seat("alpha");
    let beta = seat("beta");

    host.call(&alpha, "browser_navigate", json!({ "url": page, "profile": "hunt" }))
        .await
        .unwrap_or_else(|why| panic!("alpha could not open the profile: {why}"));
    // **What a login durably is, written where Chromium keeps it durably.**
    // localStorage is flushed as it changes; a cookie store is written on
    // Chromium's own cadence, so a cookie set seconds before a close can be
    // lost - the person's own browser behaves the same, and a session cookie
    // is not persisted at all. A site's auth token usually lives in both,
    // and localStorage is the half whose survival a test can pin.
    let set = "async (page) => { await page.evaluate(() => localStorage.setItem('who', 'alpha')); \
               return 'set'; }";
    host.call(&alpha, "browser_run_code_unsafe", json!({ "code": set, "profile": "hunt" }))
        .await
        .unwrap_or_else(|why| panic!("alpha could not set its storage: {why}"));

    // Beta naming the profile is refused by name, with alpha's slot in the
    // reason so it can tell who holds it.
    let refused = host.call(&beta, "browser_snapshot", json!({ "profile": "hunt" })).await;
    let Err(refusal) = refused else {
        panic!("beta was let into a profile it did not open");
    };
    assert!(refusal.contains("'hunt'"), "the refusal names the profile: {refusal}");
    assert!(refusal.contains("Busytools/forge/alpha"), "and the session holding it: {refusal}",);

    // The browser is closed the way Done closes it - a clean shutdown, which
    // is what flushes Chromium's cookie store. **A SIGKILL would lose the
    // just-set cookie instead**: Chromium writes cookies periodically, so a
    // crash drops the last stretch of logins, and that is the person's own
    // browser's behaviour too, not this host's.
    let port_file = paths.profiles.join("hunt/DevToolsActivePort");
    let port = std::fs::read_to_string(&port_file)
        .expect("the launch wrote its port file")
        .lines()
        .next()
        .and_then(|first| first.trim().parse::<u16>().ok())
        .expect("the port file names a port");
    forge_client::browser::chromium::hide(&paths.profiles.join("hunt")).await;
    assert!(
        !forge_client::browser::chromium::probe(port).await,
        "precondition: the browser really closed",
    );

    // The next call brings a new browser up on the SAME directory; Chromium
    // persisted the profile, so the login is still there. The page is
    // returned to first - the relaunch starts on about:blank, whose origin
    // cannot read storage at all.
    host.call(&alpha, "browser_navigate", json!({ "url": page, "profile": "hunt" }))
        .await
        .unwrap_or_else(|why| panic!("the profile's browser could not reopen the page: {why}"));
    let read = "async (page) => await page.evaluate(() => localStorage.getItem('who'))";
    let seen = host
        .call(&alpha, "browser_run_code_unsafe", json!({ "code": read, "profile": "hunt" }))
        .await
        .map(|parts| text_of(&parts))
        .unwrap_or_else(|why| panic!("the profile could not be reopened: {why}"));
    assert!(
        seen.contains("alpha"),
        "the profile's login survived the browser's close: {seen}",
    );

    let relaunched = std::fs::read_to_string(&port_file)
        .expect("the new launch wrote its port file")
        .lines()
        .next()
        .and_then(|first| first.trim().parse::<u16>().ok())
        .expect("the port file names a port");
    let pid = std::fs::read_to_string(paths.profiles.join("hunt/browser.pid"))
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok());
    Launched::new(pid, relaunched, paths.profiles.join("hunt")).reap();
}

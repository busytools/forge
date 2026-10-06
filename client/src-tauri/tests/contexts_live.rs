//! The contexts spike, as a live test: can two named contexts share the one
//! browser?
//!
//! **Deliberately ignored** - it needs the vendored stack (`just
//! vendor-browser-stack`) like the other live tests. Run it where the stack
//! is:
//!
//! ```text
//! cargo nextest run --manifest-path client/src-tauri/Cargo.toml \
//!     --run-ignored ignored-only -E 'test(two_named_contexts)'
//! ```
//!
//! **The answer, read out of the pinned driver's own bundle first.** The
//! driver does NOT multiplex contexts: attached over `--cdp-endpoint` it takes
//! `browser.contexts()[0]` - the browser's own persistent context - unless
//! `config.browser.isolated` is set, in which case it creates a context of its
//! own. Both flags are upstream's: `--isolated` sets that boolean and
//! `--storage-state <path>` fills `browser.contextOptions.storageState`, with
//! CLI flags overriding any config file. So the client LAYERS contexts: one
//! driver per named context, all over the one browser, each isolated with its
//! own storage file. A driver's exit closes only its own CDP websocket, so the
//! browser the client launched outlives every driver.
//!
//! This test proves the layering rather than the reading: two drivers over one
//! browser, a cookie set through one, and the other unable to see it.

use std::path::PathBuf;
use std::process::Command;

use forge_client::browser::contexts::Seat;
use forge_client::browser::driver::{Driver, ReplyPart};
use forge_client::browser::{BrowserHost, StackPaths};
use serde_json::json;

/// Kill the browser this test launched, by the PORT it holds - never by a
/// pattern: this machine runs other browsers, and one of them belongs to the
/// person sitting at it.
fn kill_the_browser_on(port: u16) {
    let Ok(listed) =
        Command::new("lsof").args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"]).output()
    else {
        return;
    };
    let pids = String::from_utf8_lossy(&listed.stdout);
    for pid in pids.split_whitespace() {
        let _ = Command::new("kill").arg(pid).status();
    }
}

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

#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn two_named_contexts_share_one_browser_and_see_different_cookies() {
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = StackPaths {
        stack,
        profile: dir.path().join("profile"),
        output: dir.path().join("output"),
        contexts: dir.path().join("contexts"),
    };
    let host = BrowserHost::new(paths.clone());
    host.start().await.expect("the browser comes up");
    let browser_port = forge_client::browser::chromium::read_active_port(&paths.profile)
        .expect("the start wrote its port file")
        .port;
    /// Out through the browser's port, so a panic never leaves a browser
    /// behind, and diverging so a match arm reads as an arm.
    fn finish(browser_port: u16, why: String) -> ! {
        kill_the_browser_on(browser_port);
        panic!("{why}");
    }

    // A page both contexts can visit, so a cookie has an origin to live on.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let page_port = listener.local_addr().expect("the bound port").port();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        for _ in 0..8 {
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

    // One driver per named context, both over the one browser. `None` would be
    // the browser's own context; `Some` is a context of this driver's own.
    let endpoint = format!("http://127.0.0.1:{browser_port}");
    let states = dir.path().join("contexts");
    let (node, cli) = (
        forge_client::browser::driver::node_path(&paths.stack),
        forge_client::browser::driver::cli_path(&paths.stack),
    );
    let alpha = match Driver::start(
        &node,
        &cli,
        &endpoint,
        &paths.output,
        Some(&states.join("alpha.json")),
    )
    .await
    {
        Ok(driver) => driver,
        Err(why) => {
            finish(browser_port, format!("the first context's driver did not start: {why}"))
        }
    };
    let beta =
        match Driver::start(&node, &cli, &endpoint, &paths.output, Some(&states.join("beta.json")))
            .await
        {
            Ok(driver) => driver,
            Err(why) => {
                finish(browser_port, format!("the second context's driver did not start: {why}"))
            }
        };

    let page = format!("http://127.0.0.1:{page_port}/");
    for (driver, name) in [(&alpha, "alpha"), (&beta, "beta")] {
        if let Err(why) = driver.call("browser_navigate", json!({ "url": page })).await {
            finish(browser_port, format!("{name} could not navigate: {why}"));
        }
    }

    // A cookie in alpha's context, through a snippet on its own page.
    let set = format!(
        "async (page) => {{ await page.context().addCookies([{{ name: 'who', value: 'alpha', \
         url: '{page}' }}]); return 'set'; }}"
    );
    if let Err(why) = alpha.call("browser_run_code_unsafe", json!({ "code": set })).await {
        finish(browser_port, format!("alpha could not set its cookie: {why}"));
    }

    // Beta asks its OWN context, and the answer is that alpha's cookie is not
    // there - which is what "a separate context" means.
    let read = "async (page) => JSON.stringify(await page.context().cookies())";
    let seen = match beta.call("browser_run_code_unsafe", json!({ "code": read })).await {
        Ok(parts) => text_of(&parts),
        Err(why) => finish(browser_port, format!("beta could not read its cookies: {why}")),
    };
    kill_the_browser_on(browser_port);
    assert!(
        !seen.contains("who") && !seen.contains("alpha"),
        "beta's context does not see alpha's cookie: {seen}",
    );
    assert!(
        seen.contains("[]"),
        "and beta's read is an empty cookie list rather than an error: {seen}",
    );
}

/// The whole named-context contract over the real stack, in the shape the
/// spec's own acceptance names it: two sessions, one context name, the second
/// refused - and then the release, which saves the context, kills its driver
/// and frees the name, so the next session to open it finds the cookies and
/// the open tabs it left.
#[tokio::test]
#[ignore = "drives the vendored stack; needs `just vendor-browser-stack`"]
async fn a_named_context_refuses_another_session_and_reopens_from_its_save() {
    let stack = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("browser-stack");
    assert!(
        stack.join("node/bin/node").is_file(),
        "the vendored stack is not there - run `just vendor-browser-stack`",
    );
    let dir = tempfile::tempdir().expect("a temp dir");
    let paths = StackPaths {
        stack,
        profile: dir.path().join("profile"),
        output: dir.path().join("output"),
        contexts: dir.path().join("contexts"),
    };
    let host = BrowserHost::new(paths.clone());
    host.start().await.expect("the browser comes up");
    let browser_port = forge_client::browser::chromium::read_active_port(&paths.profile)
        .expect("the start wrote its port file")
        .port;
    /// Out through the browser's port, so a panic never leaves a browser
    /// behind, and diverging so a match arm reads as an arm.
    fn finish(browser_port: u16, why: String) -> ! {
        kill_the_browser_on(browser_port);
        panic!("{why}");
    }

    // A page with a path, so a reopened tab is recognisable by its URL.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let page_port = listener.local_addr().expect("the bound port").port();
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
    let page = format!("http://127.0.0.1:{page_port}/");
    let second = format!("http://127.0.0.1:{page_port}/second");
    let seat = |label: &str| Seat {
        org: "Busytools".to_owned(),
        project: "forge".to_owned(),
        label: label.to_owned(),
    };
    let (alpha, beta) = (seat("alpha"), seat("beta"));

    // Alpha opens the context on first use, and leaves it with a cookie and a
    // second tab someone will have to find again.
    if let Err(why) =
        host.call(&alpha, "browser_navigate", json!({ "url": page, "context": "hunt" })).await
    {
        finish(browser_port, format!("alpha could not open the context: {why}"));
    }
    let set = format!(
        "async (page) => {{ await page.context().addCookies([{{ name: 'who', value: 'alpha', \
         url: '{page}' }}]); return 'set'; }}"
    );
    if let Err(why) = host
        .call(&alpha, "browser_run_code_unsafe", json!({ "code": set, "context": "hunt" }))
        .await
    {
        finish(browser_port, format!("alpha could not set its cookie: {why}"));
    }
    if let Err(why) = host
        .call(&alpha, "browser_tabs", json!({ "action": "new", "url": second, "context": "hunt" }))
        .await
    {
        finish(browser_port, format!("alpha could not open its second tab: {why}"));
    }

    // Beta naming the context is refused by name, with alpha's slot in the
    // reason so it can tell who holds it.
    let refused = host.call(&beta, "browser_snapshot", json!({ "context": "hunt" })).await;
    let Err(refusal) = refused else {
        finish(browser_port, "beta was let into a context it did not open".to_owned());
    };
    assert!(refusal.contains("'hunt'"), "the refusal names the context: {refusal}");
    assert!(refusal.contains("Busytools/forge/alpha"), "and the session holding it: {refusal}",);

    // The release saves the context and frees the name; the driver goes with
    // it, so what beta opens next is a fresh driver over the saved files.
    if let Err(why) = host.release(&alpha, "hunt").await {
        finish(browser_port, format!("alpha could not release its context: {why}"));
    }
    let read = "async (page) => JSON.stringify(await page.context().cookies())";
    let seen = match host
        .call(&beta, "browser_run_code_unsafe", json!({ "code": read, "context": "hunt" }))
        .await
    {
        Ok(parts) => text_of(&parts),
        Err(why) => {
            finish(browser_port, format!("beta could not open the released context: {why}"))
        }
    };
    assert!(
        seen.contains("who") && seen.contains("alpha"),
        "the released context reopens with the cookies it saved: {seen}",
    );

    let tabs = match host
        .call(&beta, "browser_tabs", json!({ "action": "list", "context": "hunt" }))
        .await
    {
        Ok(parts) => text_of(&parts),
        Err(why) => finish(browser_port, format!("beta could not list the context's tabs: {why}")),
    };
    kill_the_browser_on(browser_port);
    assert!(
        tabs.contains(&page) && tabs.contains("/second"),
        "and with the tabs it had open, as URLs: {tabs}",
    );
}

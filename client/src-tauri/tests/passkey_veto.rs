#![cfg(target_os = "macos")]
//! The passkey veto: its CDP conversation against a stand-in endpoint, and a
//! live probe that drives a real scratch host.
//!
//! **The stub pins the client's own decisions** - the pause it arms and its
//! params, the session it patches, the order of the steps, and what happens
//! when one fails. What only a real Chromium can answer - whether the script
//! actually reaches a document before it runs - is the ignored probe below,
//! whose run and reading are recorded on #2483.

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use forge_client::browser::cef::veto;
use forge_client::browser::cef::veto::testing::{Wire, endpoint};
use tauri_plugin_log::log::{Log, Metadata, Record};

/// Every line the crate logged while this binary ran.
static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;

impl Log for Capture {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        CAPTURED.lock().unwrap_or_else(PoisonError::into_inner).push(format!(
            "{} {} {}",
            record.level(),
            record.target(),
            record.args()
        ));
    }

    fn flush(&self) {}
}

/// Take the logger once for this binary, and answer what it captured.
fn capture_logs() -> &'static Mutex<Vec<String>> {
    static CAPTURE: Capture = Capture;
    // A logger alone captures nothing: the crate's max level starts off and
    // is raised separately.
    if tauri_plugin_log::log::set_logger(&CAPTURE).is_ok() {
        tauri_plugin_log::log::set_max_level(tauri_plugin_log::log::LevelFilter::Trace);
    }
    &CAPTURED
}

/// The veto's own lines, without the transport's trace chatter.
fn captured() -> Vec<String> {
    CAPTURED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|line| line.contains("passkey_veto"))
        .cloned()
        .collect()
}

#[tokio::test]
async fn the_veto_patches_and_resumes_every_attached_target() {
    let logs = capture_logs();
    logs.lock().unwrap_or_else(PoisonError::into_inner).clear();
    let stub = endpoint().await.expect("the stub endpoint");
    // **A frozen prototype's verdict is read out of band** - the patch is
    // sent, and its reply watched from a task so the loop stays free.
    stub.fail_verdict_once.store(true, std::sync::atomic::Ordering::Relaxed);
    let profile = Path::new("/tmp/veto-stub");
    let carried = Some("http://localhost/carried".to_owned());
    let armed =
        tokio::time::timeout(Duration::from_secs(5), veto::install(stub.port, profile, carried))
            .await
            .expect("the install answers");
    assert_eq!(
        armed.expect("the pause is armed"),
        veto::Carried::Opened,
        "the carried page opened behind the veto",
    );

    // The stub reports a running page, a context and a fresh `other` target,
    // twice each, then a sentinel context as the burst's last word. Waiting
    // for the sentinel's own patch means every earlier frame - duplicates
    // included - has been handled, so the assertions never race a sleep.
    let deadline = Instant::now() + Duration::from_secs(5);
    while stub.count("Runtime.evaluate") < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let calls = stub.calls.lock().unwrap_or_else(PoisonError::into_inner).clone();

    // **The pause is the whole coverage**, so it is the first thing sent and
    // it carries both flags: a target created later attaches by itself and
    // stops before its document runs.
    assert_eq!(calls[0]["method"], "Target.setAutoAttach", "the pause is armed first");
    assert_eq!(calls[0]["params"]["autoAttach"], true, "new targets attach by themselves");
    assert_eq!(
        calls[0]["params"]["waitForDebuggerOnStart"], true,
        "a target stops before its document runs",
    );
    assert_eq!(
        calls[0]["params"]["flatten"], true,
        "the engine refuses browser-level auto-attach without flatten",
    );

    // **A page that was already running is armed in place**: the domains,
    // then the script, then its existing world - and nothing is released,
    // because nothing was paused.
    let on = |method: &str, session: &str| {
        calls
            .iter()
            .position(|call| call["method"] == method && call["session"] == session)
            .unwrap_or_else(|| panic!("{method} on {session} was not sent: {calls:?}"))
    };
    let page = on("Page.enable", "S1");
    let runtime = on("Runtime.enable", "S1");
    let script = on("Page.addScriptToEvaluateOnNewDocument", "S1");
    let evaluate = on("Runtime.evaluate", "S1");
    assert!(page < runtime, "the page domain before the runtime: {page} {runtime}");
    assert!(runtime < script, "the script after the runtime is enabled: {runtime} {script}");
    assert!(script < evaluate, "the existing context is patched after the script: {evaluate}");
    assert!(
        !calls
            .iter()
            .any(|call| call["method"] == "Runtime.runIfWaitingForDebugger"
                && call["session"] == "S1"),
        "a running page has nothing to release: {calls:?}",
    );

    let arming = &calls[script];
    let source = arming["params"]["source"].as_str().expect("a script");
    assert!(source.contains("CredentialsContainer.prototype"), "the prototype is patched");
    assert!(source.contains("NotAllowedError"), "the refusal a page handles");
    assert!(source.contains("configurable: false"), "the patch is not replaceable");
    assert!(source.contains("return 'failed"), "a frozen prototype reports itself");

    // **A fresh tab is reported as `other` and paused**: the release comes
    // first (this engine answers no page-domain command while a target
    // waits), then the page session is asked for and armed.
    let release = on("Runtime.runIfWaitingForDebugger", "S2");
    let attach = calls
        .iter()
        .position(|call| call["method"] == "Target.attachToTarget")
        .unwrap_or_else(|| panic!("the fresh tab's page session was never asked for: {calls:?}"));
    let late = on("Page.enable", "S3");
    assert_eq!(calls[attach]["params"]["targetId"], "T2", "the attached target is the fresh tab");
    assert_eq!(calls[attach]["params"]["flatten"], true, "the session is flattened");
    assert!(release < attach, "the release comes before the page session: {release} {attach}");
    assert!(attach < late, "the fresh tab is armed on its page session: {attach} {late}");
    assert!(
        !calls.iter().any(|call| call["method"] == "Page.enable" && call["session"] == "S2"),
        "the `other` session is not armed: {calls:?}",
    );
    assert!(on("Page.addScriptToEvaluateOnNewDocument", "S3") > attach, "armed after the attach");

    // **The carried page opens behind the veto**: the navigation follows the
    // script's registration on the first page target, so the document a
    // hand-off shows is one the script starts with.
    let carried = on("Page.navigate", "S1");
    assert!(script < carried, "the carried page navigates after the script: {script} {carried}");
    assert_eq!(
        calls[carried]["params"]["url"], "http://localhost/carried",
        "the page the profile carried",
    );
    assert_eq!(stub.count("Page.navigate"), 1, "the carried page opens once");
    assert!(veto::target_armed(stub.port, "T1"), "the running page is armed");
    assert!(veto::target_armed(stub.port, "T2"), "the fresh tab is armed by its own id");

    // **The arm is once each, whatever the stub reported twice.** The release
    // is per paused REPORT, not per target: a repeated attach is a fresh
    // session with its own wait, and the guard below the release only stops a
    // second arm (measured 2026-10-23: a skipped report left a page paused,
    // #2534).
    assert_eq!(
        stub.count("Runtime.runIfWaitingForDebugger"),
        3,
        "every paused report is released: the fresh tab twice and the worker once",
    );
    assert_eq!(stub.count("Target.attachToTarget"), 1, "one page session");
    assert_eq!(stub.count("Page.addScriptToEvaluateOnNewDocument"), 2, "each target armed once",);
    assert_eq!(stub.count("Runtime.evaluate"), 2, "two contexts, one patch each");
    let patched_ids: Vec<i64> = calls
        .iter()
        .filter(|call| call["method"] == "Runtime.evaluate")
        .filter_map(|call| call["params"]["contextId"].as_i64())
        .collect();
    assert_eq!(
        patched_ids,
        vec![7, 99],
        "each default context is patched once, the duplicate report is skipped, and the \
         non-default context 55 is left alone",
    );
    let patched = &calls[evaluate];
    assert_eq!(patched["params"]["contextId"], 7, "the reported context is the one patched");
    assert_eq!(
        patched["params"]["returnByValue"], true,
        "the verdict is asked for, watched out of band",
    );
    assert_eq!(
        patched["params"]["expression"].as_str(),
        Some(source),
        "the same veto script runs in the existing context",
    );

    // **The failures are named, with their stage**: a patch that failed must
    // not read as a veto that installed, and a failed resume is a page nobody
    // can use. Both replies are watched out of band, so the assertions wait
    // for the watchers rather than a sleep.
    let deadline = Instant::now() + Duration::from_secs(5);
    let logged = loop {
        let logged = captured();
        let resume_failed = logged.iter().any(|line| {
            line.contains("cef_passkey_veto_failed")
                && line.contains("Runtime.runIfWaitingForDebugger")
        });
        let verdict_failed = logged.iter().any(|line| {
            line.contains("cef_passkey_veto_failed") && line.contains("failed TypeError")
        });
        let page_failed = logged
            .iter()
            .any(|line| line.contains("cef_passkey_veto_failed") && line.contains("Page.enable"));
        if resume_failed && verdict_failed && page_failed {
            break logged;
        }
        assert!(Instant::now() < deadline, "the watched failures are logged: {logged:?}",);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        logged
            .iter()
            .any(|line| line.contains("cef_passkey_veto_failed") && line.contains("Page.enable")),
        "the refused Page.enable is logged: {logged:?}",
    );

    // **A worker is not a page: released, never armed.** Leaving its pause
    // in place is what wedged the carried page for good (#2534), and arming
    // one earns a refused Page.enable per domain.
    assert!(
        calls
            .iter()
            .any(|call| call["method"] == "Runtime.runIfWaitingForDebugger"
                && call["session"] == "SW1"),
        "the paused worker is released: {calls:?}",
    );
    assert!(
        !calls.iter().any(|call| call["session"] == "SW1"
            && matches!(
                call["method"].as_str(),
                Some("Page.enable" | "Runtime.enable" | "Page.addScriptToEvaluateOnNewDocument")
            )),
        "no arm command goes to a service worker's session: {calls:?}",
    );
    assert!(stub.index("Target.setDiscoverTargets").is_none(), "no discovery fallback");
    assert!(stub.index("Page.reload").is_none(), "no page is reloaded");
}

/// **A page whose script did not register is not navigated to the carried
/// page.** The stub refuses the first registration; the install answers a
/// failure at once and sends no navigation, rather than leaving a document
/// running ahead of a veto that never landed.
#[tokio::test]
async fn a_carried_page_waits_for_a_registered_script() {
    let stub = endpoint().await.expect("the stub endpoint");
    stub.refuse_script_once.store(true, std::sync::atomic::Ordering::Relaxed);
    let profile = Path::new("/tmp/veto-stub");
    let carried = Some("http://localhost/carried".to_owned());
    let armed =
        tokio::time::timeout(Duration::from_secs(5), veto::install(stub.port, profile, carried))
            .await
            .expect("the install answers");
    let outcome = armed.expect("the connect stage succeeded");
    assert!(
        matches!(outcome, veto::Carried::Failed(ref why) if why.contains("not registered")),
        "the unregistered page is named: {outcome:?}",
    );
    assert_eq!(stub.count("Page.navigate"), 0, "nothing is navigated behind an unregistered page");
}

/// **The live probe, against a scratch host.** Run with `FORGE_VETO_PORT` on
/// a running `--cef-host`. It serves the repo's parse fixture from an
/// in-process listener, writes the profile's carried marker to it, installs
/// the real veto, and reads the page's own verdict over CDP: the carried page
/// must be refused before its own script's call settles, and so must a tab
/// the probe opens blank and then navigates. Every claim here is a read of
/// the page, not a sleep.
#[tokio::test]
#[ignore = "needs a running scratch host; run with FORGE_VETO_PORT"]
async fn the_live_veto_holds_a_scratch_host() {
    let port: u16 = std::env::var("FORGE_VETO_PORT")
        .expect("FORGE_VETO_PORT names the host to veto")
        .parse()
        .expect("FORGE_VETO_PORT is a port");
    let profile = std::path::PathBuf::from(
        std::env::var("FORGE_VETO_PROFILE").unwrap_or_else(|_| "/tmp/veto-live".into()),
    );
    let page_port = serve_fixture().await.expect("the fixture server");
    let page = format!("http://127.0.0.1:{page_port}/passkey_parse.html");
    std::fs::create_dir_all(&profile).expect("the profile");
    std::fs::write(profile.join("page"), &page).expect("the carried marker");
    assert_eq!(
        forge_client::browser::cef::host::carried_page(&profile),
        page,
        "a launch reads the marker this probe wrote",
    );

    let installed = tokio::time::timeout(
        Duration::from_secs(20),
        veto::install(port, &profile, Some(page.clone())),
    )
    .await
    .expect("the install answers");
    assert_eq!(
        installed.expect("the install succeeds"),
        veto::Carried::Opened,
        "the carried page opened behind the veto",
    );

    let mut wire = Wire::connect(port).await.expect("the host's endpoint");
    // **`Opened` is the navigation accepted, not its URL landed.** The probe
    // waits for the target to report the carried page, as the client's hold
    // does.
    let deadline = Instant::now() + Duration::from_secs(10);
    let carried = loop {
        let targets = wire.targets().await.expect("the host's targets");
        if let Some((id, _)) = targets.into_iter().find(|(_, url)| url == &page) {
            break id;
        }
        assert!(Instant::now() < deadline, "the carried page never opened on the host");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(
        settled(&mut wire, &carried).await,
        "rejected:NotAllowedError",
        "the carried page's first document is refused by the veto",
    );

    // **A tab the browser tools open at a URL**: blank first, then navigated
    // once the installer has armed that very target.
    let blank = wire.create_target("about:blank").await.expect("a blank tab");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !veto::target_armed(port, &blank) {
        assert!(Instant::now() < deadline, "the blank tab is armed before its navigation");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wire.navigate(&blank, &page).await.expect("the tab's document");
    assert_eq!(
        settled(&mut wire, &blank).await,
        "rejected:NotAllowedError",
        "a tab opened at a URL is refused on its first real document",
    );
}

/// The page's own verdict, once its script has settled.
async fn settled(wire: &mut Wire, target: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let value =
            wire.evaluate(target, "String(window.__result)").await.expect("the page answers");
        let value = value.as_str().unwrap_or_default().to_owned();
        if value != "pending" && value != "undefined" {
            return value;
        }
        assert!(Instant::now() < deadline, "the page's own verdict never settled");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The repo's parse fixture, served on a port of its own.
async fn serve_fixture() -> Result<u16, String> {
    let body = include_str!("fixtures/passkey_parse.html");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|why| format!("the fixture server cannot listen: {why}"))?;
    let port = listener
        .local_addr()
        .map_err(|why| format!("the fixture server has no address: {why}"))?
        .port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { continue };
            let mut request = [0_u8; 1024];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request).await;
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, answer.as_bytes()).await;
        }
    });
    Ok(port)
}

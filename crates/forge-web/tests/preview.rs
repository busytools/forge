//! Scratch: dump the served page beside the mockup. Not committed.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::Path;
use std::sync::Arc;

use forge_primitives::WebConfig;
use forge_sessions::surface::ViewSurface;
use forge_sessions::testing::Fleet;
use forge_web::WebState;

const OUT: &str = "/tmp/forge-session-preview";

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("the ephemeral address").port();
    drop(listener);
    port
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("project dir");
    git(dir, &["init", "-q"]);
    git(dir, &["symbolic-ref", "HEAD", "refs/heads/web-session-layout"]);
    git(dir, &["config", "user.email", "t@e.com"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("tracked.txt"), "one\n").expect("write");
    git(dir, &["add", "tracked.txt"]);
    git(dir, &["commit", "-qm", "init"]);
    for (name, lines) in [("app.rs", 12), ("view.rs", 40), ("style.css", 4)] {
        std::fs::write(dir.join(name), "x\n".repeat(lines)).expect("write");
    }
    std::fs::write(dir.join("tracked.txt"), "one\ntwo\n").expect("modify");
}

async fn start(surface: Arc<ViewSurface>) -> WebConfig {
    let config = WebConfig {
        port: free_port(),
        bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
        ..WebConfig::default()
    };
    let state =
        WebState::new(Arc::clone(&surface), Arc::new(forge_web::WorkCache::new()), config.clone());
    forge_web::start(state).await.expect("the view binds").expect("enabled");
    config
}

async fn get(config: &WebConfig, path: &str) -> (reqwest::StatusCode, String) {
    let response =
        reqwest::get(format!("http://127.0.0.1:{}{path}", config.port)).await.expect("served");
    (response.status(), response.text().await.expect("the body reads"))
}

#[tokio::test]
async fn dump_the_pages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fleet = Fleet::in_dir(
        dir.path(),
        &[("Busytools", &["forge", "busymail"]), ("Personal", &["companies"])],
    )
    .expect("the fleet builds");
    for (org, project) in
        [("Busytools", "forge"), ("Busytools", "busymail"), ("Personal", "companies")]
    {
        fleet.start(org, project).expect("declared");
        repo(&dir.path().join(project));
    }
    fleet.install_agent("Busytools", "forge", "lead");
    fleet.add_worker("Busytools", "forge", "em-dash-sweep").expect("worker");
    fleet
        .seed_transcript(
            "Busytools",
            "forge",
            "lead",
            &[
                r#"{"type":"user","message":{"role":"user","content":"Make the call tree the default, let me open one call at a time, and cover every kind the CLI can produce."}}"#,
                r#"{"type":"assistant","message":{"id":"m1","role":"assistant","model":"claude-opus-5","content":[{"type":"text","text":"One family per block, its calls under it, and a call opens on its own. Edits are the exception: their diff is already there."}]}}"#,
                r#"{"type":"assistant","message":{"id":"m2","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/tmp/preview/family.rs"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"pub enum ToolFamily {\n    Read, Search, Bash, Web,\n}"}]}}"#,
                r#"{"type":"assistant","message":{"id":"m3","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/tmp/preview/home.css","old_string":"  flex: none;","new_string":"  flex: 0 1 auto; min-width: 0;"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#,
                r#"{"type":"assistant","message":{"id":"m4","role":"assistant","model":"claude-opus-5","content":[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"just check"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t3","content":"Summary 199 tests run: 199 passed"}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":"[Message id=t-9 from agent 'companies' (org 'Personal')]\n\nis the cron issue filed? the wake dies on the 2nd"}}"#,
                r#"{"type":"user","message":{"role":"user","content":"[Message id=t-10 from agent 'steward' (org 'Personal')]\n\nIT IMPORTED. The window is lost"}}"#,
                r#"{"type":"user","message":{"role":"user","content":"[Gotify - app 'watcher', priority 3]\n\ndeploy finished"}}"#,
            ],
        )
        .expect("the transcript is written");
    let config = start(fleet.surface()).await;

    std::fs::create_dir_all(OUT).expect("out dir");
    for (path, name) in [("/", "home.html"), ("/session/Busytools/forge/lead", "session.html")] {
        let (status, body) = get(&config, path).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{path}");
        std::fs::write(format!("{OUT}/{name}"), body).expect("write");
    }
    let (status, css) = get(&config, "/web.css").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    std::fs::write(format!("{OUT}/web.css"), css).expect("write");
}

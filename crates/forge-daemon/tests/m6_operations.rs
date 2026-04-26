//! M6 — operational hardening: forged.toml config + tracing-appender logging.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(unsafe_code)]

mod common {
    pub mod env_guard;
    pub mod env_lock;
}

use forge_daemon::config::{Config, load_from_str};

use crate::common::env_guard::EnvGuard;
use crate::common::env_lock::ENV_LOCK;

// Round 3 — fix M10. ENV_LOCK lives in `common::env_lock` so this
// crate's env-mutating tests in m3_listing.rs and m6_operations.rs
// share one process-wide lock — without it, the two files' static
// locks would let tests across files run concurrently and race the
// global env-var space.

#[test]
fn config_defaults_when_missing_file() {
    let c = Config::default();
    assert_eq!(c.bind, vec!["127.0.0.1:7373"]);
    assert_eq!(c.log_retention_days, 14);
    assert!(
        c.log_dir.contains("Library/Logs/forge-daemon"),
        "log_dir default should point under Library/Logs/forge-daemon, got {}",
        c.log_dir
    );
}

#[test]
fn config_parses_minimal_toml() {
    let toml = r#"
        bind = ["10.0.0.1:7373", "127.0.0.1:7373"]
    "#;
    let c = load_from_str(toml).unwrap();
    assert_eq!(c.bind.len(), 2);
    assert_eq!(c.bind[0], "10.0.0.1:7373");
    assert_eq!(c.bind[1], "127.0.0.1:7373");
    // Defaults still apply for unspecified keys.
    assert_eq!(c.log_retention_days, 14);
    assert!(c.log_dir.contains("Library/Logs/forge-daemon"));
}

#[test]
fn config_full_toml_round_trip() {
    let toml = r#"
        bind = ["10.0.0.1:7373"]
        log_dir = "/tmp/forge-daemon-logs"
        log_retention_days = 7
    "#;
    let c = load_from_str(toml).unwrap();
    assert_eq!(c.bind, vec!["10.0.0.1:7373"]);
    assert_eq!(c.log_dir, "/tmp/forge-daemon-logs");
    assert_eq!(c.log_retention_days, 7);
}

#[test]
fn config_rejects_unknown_keys() {
    let toml = r#"
        bind = ["127.0.0.1:7373"]
        unknown_field = "foo"
    "#;
    let err = load_from_str(toml).unwrap_err();
    let s = err.to_string();
    assert!(
        s.contains("unknown") || s.contains("unknown_field"),
        "expected unknown-field error, got: {s}"
    );
}

#[test]
fn config_rejects_invalid_toml() {
    let err = load_from_str("not = valid = toml = at all").unwrap_err();
    // Just assert the error message reaches us through Error::InvalidRequest.
    assert!(err.to_string().contains("forge-daemon.toml"));
}

// ============================================================================
// `Config::default_path` resolution (round 2 — fix I13)
// ============================================================================

#[test]
fn default_path_returns_etc_when_xdg_and_home_both_unset() {
    let _g = ENV_LOCK.lock();
    let _xdg = EnvGuard::unset("XDG_CONFIG_HOME");
    let _home = EnvGuard::unset("HOME");
    let p = forge_daemon::config::default_path();
    assert_eq!(
        p,
        std::path::PathBuf::from("/etc/forge-daemon/forge-daemon.toml")
    );
}

#[test]
fn default_path_uses_xdg_when_only_xdg_set() {
    let _g = ENV_LOCK.lock();
    let _home = EnvGuard::unset("HOME");
    let _xdg = EnvGuard::new("XDG_CONFIG_HOME", "/tmp/xdg-fixture");
    let p = forge_daemon::config::default_path();
    assert_eq!(
        p,
        std::path::PathBuf::from("/tmp/xdg-fixture/forge-daemon/forge-daemon.toml")
    );
}

#[test]
fn default_path_uses_home_when_only_home_set() {
    let _g = ENV_LOCK.lock();
    let _xdg = EnvGuard::unset("XDG_CONFIG_HOME");
    let _home = EnvGuard::new("HOME", "/tmp/home-fixture");
    let p = forge_daemon::config::default_path();
    assert_eq!(
        p,
        std::path::PathBuf::from("/tmp/home-fixture/.config/forge-daemon/forge-daemon.toml")
    );
}

#[test]
fn default_path_xdg_takes_precedence_when_both_set() {
    let _g = ENV_LOCK.lock();
    let _home = EnvGuard::new("HOME", "/tmp/home-fixture");
    let _xdg = EnvGuard::new("XDG_CONFIG_HOME", "/tmp/xdg-fixture");
    let p = forge_daemon::config::default_path();
    assert_eq!(
        p,
        std::path::PathBuf::from("/tmp/xdg-fixture/forge-daemon/forge-daemon.toml")
    );
}

#[test]
fn logging_init_creates_log_dir_if_missing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let log_dir = tmp.path().join("logs");
    assert!(!log_dir.exists());
    let config = forge_daemon::config::Config {
        bind: vec!["127.0.0.1:0".into()],
        log_dir: log_dir.to_string_lossy().into_owned(),
        log_retention_days: 14,
    };
    forge_daemon::logging::init(&config).unwrap();
    assert!(log_dir.exists(), "init must create log_dir if missing");
    // Idempotency: a second call must not error even though the global
    // subscriber is already installed.
    forge_daemon::logging::init(&config).unwrap();
}

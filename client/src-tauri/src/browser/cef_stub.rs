//! The embedded browser on a desktop that does not ship it.
//!
//! The CEF framework, its helpers and the bundling step are macOS-only, so
//! the Linux and Windows shells compile with this module in the engine's
//! place. Every door refuses with one sentence rather than pretending a
//! surface exists; the rest of the client - chat, dictation, files - runs
//! normally.

/// The one sentence a browser door answers on these platforms.
pub const REFUSAL: &str = "the embedded browser is packaged for macOS; this client cannot open it";

/// No framework is packaged on these platforms.
pub fn available() -> bool {
    false
}

/// The frame ring: no host ever writes one here, so the type exists only to
/// keep the client's own host bookkeeping compiling.
#[derive(Clone, Copy, Debug)]
pub struct Ring;

impl Ring {
    pub fn create(_name: &str, _width: u32, _height: u32) -> Result<Self, String> {
        Ok(Self)
    }

    pub fn open(_name: &str) -> Result<Self, String> {
        Ok(Self)
    }

    pub fn unlink(_name: &str) {}
}

pub mod osr {
    pub use super::Ring;
}

pub mod spawn {
    use super::REFUSAL;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    /// How long a host is given to answer; the same bound the macOS launch
    /// uses, so a caller's arithmetic does not change with the platform.
    pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

    /// A running host, as the client keeps it. No stub host is ever started,
    /// so the value only exists where the client's own code builds one.
    pub struct ActiveHost {
        pub pid: u32,
        pub port: u16,
        pub ring: Arc<super::Ring>,
        pub profile: PathBuf,
    }

    impl ActiveHost {
        pub fn identity(&self) -> String {
            format!("cef-{}", self.pid)
        }
    }

    impl std::fmt::Debug for ActiveHost {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("ActiveHost")
                .field("pid", &self.pid)
                .field("port", &self.port)
                .field("profile", &self.profile)
                .finish_non_exhaustive()
        }
    }

    /// What the client records beside a host's cache root.
    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct Record {
        pub pid: u32,
        pub port: u16,
        pub ring: String,
    }

    pub fn ring_name(port: u16) -> String {
        format!("/forge-cef-{port}")
    }

    pub fn record_path(profile: &Path) -> PathBuf {
        profile.join("host.json")
    }

    pub fn read_record(profile: &Path) -> Option<Record> {
        let body = std::fs::read(record_path(profile)).ok()?;
        serde_json::from_slice(&body).ok()
    }

    pub fn write_record(profile: &Path, record: &Record) -> Result<(), String> {
        let body = serde_json::to_vec(record)
            .map_err(|why| format!("the host record cannot be written: {why}"))?;
        std::fs::write(record_path(profile), body)
            .map_err(|why| format!("the host record cannot be written: {why}"))
    }

    pub fn remove_record(profile: &Path) {
        let _ = std::fs::remove_file(record_path(profile));
    }

    pub fn free_port() -> Option<u16> {
        None
    }

    pub fn spawn(
        _profile: &Path,
        _port: u16,
        _ring: &str,
        _width: u32,
        _height: u32,
    ) -> Result<u32, String> {
        Err(REFUSAL.to_owned())
    }

    pub fn spawn_with(
        _exe: &Path,
        _profile: &Path,
        _port: u16,
        _ring: &str,
        _width: u32,
        _height: u32,
    ) -> Result<u32, String> {
        Err(REFUSAL.to_owned())
    }

    pub async fn wait_ready(_port: u16) -> Result<(), String> {
        Err(REFUSAL.to_owned())
    }

    pub async fn wait_ready_within(_port: u16, _bound: Duration) -> Result<(), String> {
        Err(REFUSAL.to_owned())
    }

    pub async fn wait_ready_for(_pid: u32, _port: u16, _profile: &Path) -> Result<(), String> {
        Err(REFUSAL.to_owned())
    }

    pub async fn end_verified(_pid: u32, _port: u16) -> bool {
        true
    }

    pub fn is_our_host(_pid: u32) -> bool {
        false
    }

    pub fn singleton_pid(_profile: &Path) -> Option<u32> {
        None
    }

    pub fn is_our_binary(_pid: u32) -> bool {
        false
    }

    pub fn clear_singleton(_profile: &Path) {}

    pub async fn probe(_port: u16) -> bool {
        false
    }

    pub async fn end(_pid: u32) {}

    pub fn running(_pid: u32) -> bool {
        false
    }

    pub async fn page_websocket(_port: u16) -> Option<String> {
        None
    }

    pub async fn page_url(_port: u16) -> Option<String> {
        None
    }

    pub fn ws_prefix(port: u16) -> String {
        format!("ws://127.0.0.1:{port}")
    }
}

pub mod surface {
    use super::REFUSAL;
    use super::spawn::ActiveHost;
    use std::sync::Arc;

    /// Where the page says its takeover's page area is, in CSS pixels.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Rect {
        pub x: f64,
        pub y: f64,
        pub width: f64,
        pub height: f64,
        pub scale: f64,
    }

    pub async fn attach(
        _app: &tauri::AppHandle,
        _host: Arc<ActiveHost>,
        _rect: Rect,
    ) -> Result<(), String> {
        Err(REFUSAL.to_owned())
    }

    pub fn set_rect(_app: &tauri::AppHandle, _host: &ActiveHost, _rect: Rect) {}

    pub fn detach(_app: &tauri::AppHandle) {}
}

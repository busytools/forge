//! Where the server's own CEF bundle lives.
//!
//! The bundle is staged beside forge's machine-local state by
//! `scripts/stage-browser-host.sh`; it is fixed to that directory, never to
//! the launch directory (hard rule 14).

use std::path::{Path, PathBuf};

/// The bundle's directory name, as the staging script writes it.
pub const APP: &str = "Forge Browser.app";

/// The bundle's executable, as its Info.plist names it.
const EXECUTABLE: &str = "Contents/MacOS/Forge Browser";

/// The app-support base forge's machine-local state sits under.
pub fn app_support() -> Result<PathBuf, String> {
    forge_sdk::app_support_dir()
        .map_err(|why| format!("the app-support directory cannot be resolved: {why}"))
}

/// The installed `Forge Browser.app`.
pub fn path() -> Result<PathBuf, String> {
    app_support().map(|dir| dir.join("browser-host").join(APP))
}

/// The executable inside the bundle that runs as a host.
pub fn executable() -> Result<PathBuf, String> {
    path().map(|app| app.join(EXECUTABLE))
}

/// The host executable, checked against this server's own version.
///
/// **A bundle another version staged is refused rather than run**: the ring
/// layout and the launch arguments are a contract between the two, and an
/// update that replaced the server but not the bundle would hand it a shape
/// it does not know.
pub fn checked_executable() -> Result<PathBuf, String> {
    check(&path()?)
}

fn check(app: &Path) -> Result<PathBuf, String> {
    let exe = app.join(EXECUTABLE);
    if !exe.is_file() {
        return Err(format!(
            "the browser host bundle is not installed at {}; reinstall forge (`just install` from \
             a checkout, or the release installer)",
            app.display()
        ));
    }
    let ours = env!("CARGO_PKG_VERSION");
    let found = version(app).ok_or_else(|| {
        format!(
            "the browser host bundle at {} carries no CFBundleVersion, so it cannot be shown to be \
             this server's; reinstall forge",
            app.display()
        )
    })?;
    if found != ours {
        return Err(format!(
            "the browser host bundle at {} is version {found} and this server is {ours}; reinstall \
             forge to replace it",
            app.display()
        ));
    }
    Ok(exe)
}

/// The bundle's `CFBundleVersion`, read out of its `Info.plist`. The
/// staging script writes the key's value as the next `<string>`.
fn version(app: &Path) -> Option<String> {
    let plist = std::fs::read_to_string(app.join("Contents/Info.plist")).ok()?;
    let after = plist.split("<key>CFBundleVersion</key>").nth(1)?;
    let value = after.split("<string>").nth(1)?;
    Some(value.split("</string>").next()?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{APP, EXECUTABLE, check, path};

    /// **The install location is the app-support tree, not a launch-relative
    /// path.** The bundle and the profiles are machine-local state, and a
    /// checkout's cwd must not change where the server looks.
    #[test]
    fn the_bundle_sits_beside_forges_own_state() {
        let path = path().expect("resolves on a normal dev machine");
        assert!(path.ends_with(format!("browser-host/{APP}")), "{}", path.display());
        assert!(path.is_absolute(), "an absolute install path: {}", path.display());
        let exe = path.join(EXECUTABLE);
        assert_eq!(exe.file_name().and_then(|name| name.to_str()), Some("Forge Browser"));
    }

    fn staged(app: &Path, version: Option<&str>) {
        let exe = app.join(EXECUTABLE);
        std::fs::create_dir_all(exe.parent().expect("the executable's dir")).expect("the dirs");
        std::fs::write(&exe, b"host").expect("the executable");
        if let Some(version) = version {
            std::fs::write(
                app.join("Contents/Info.plist"),
                format!(
                    "<key>CFBundleVersion</key>\n\t<string>{version}</string>\n\
                     <key>CFBundleShortVersionString</key>\n\t<string>{version}</string>\n"
                ),
            )
            .expect("the plist");
        }
    }

    /// **A missing or stale bundle names what to do about it**, and a bundle
    /// of this server's own version is accepted. The negative controls are
    /// the point: each refusal must name its own remedy, not a bare path.
    #[test]
    fn a_bundle_is_checked_for_presence_and_version() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let absent = dir.path().join("absent");
        let why = check(&absent).expect_err("no bundle");
        assert!(why.contains("not installed") && why.contains("reinstall"), "{why}");

        let stale = dir.path().join("stale");
        staged(&stale, Some("0.0.1"));
        let why = check(&stale).expect_err("another version's bundle");
        assert!(why.contains("0.0.1") && why.contains(env!("CARGO_PKG_VERSION")), "{why}");
        assert!(why.contains("reinstall"), "the remedy is named: {why}");

        let unversioned = dir.path().join("unversioned");
        staged(&unversioned, None);
        let why = check(&unversioned).expect_err("no version to check");
        assert!(why.contains("CFBundleVersion") && why.contains("reinstall"), "{why}");

        let ours = dir.path().join("ours");
        staged(&ours, Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(
            check(&ours).expect("this server's own bundle"),
            ours.join(EXECUTABLE),
            "the executable inside the bundle is what runs"
        );
    }
}

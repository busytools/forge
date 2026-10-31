//! Where the server's own CEF bundle lives.
//!
//! The bundle is staged per server version beside forge's machine-local state
//! by `scripts/stage-browser-host.sh`; it is fixed to that directory, never
//! to the launch directory (hard rule 14).

use std::path::{Path, PathBuf};

/// The bundle's directory name, as the staging script writes it.
pub const APP: &str = "Forge Browser.app";

/// The bundle's executable name, as its Info.plist names it.
pub const EXECUTABLE_NAME: &str = "Forge Browser";

/// The bundle's executable, as its Info.plist names it.
const EXECUTABLE: &str = "Contents/MacOS/Forge Browser";

/// The app-support base forge's machine-local state sits under.
pub fn app_support() -> Result<PathBuf, String> {
    forge_sdk::app_support_dir()
        .map_err(|why| format!("the app-support directory cannot be resolved: {why}"))
}

/// The directory every staged version's bundle sits under, so a reaper can
/// match a host of ANY staged version rather than only this build's.
pub fn install_root() -> Result<PathBuf, String> {
    app_support().map(|dir| dir.join("browser-host"))
}

/// The installed `Forge Browser.app`, under this server's own version.
///
/// **One bundle per version, so an install cannot replace the one a running
/// server uses.** Staging writes the new version's directory beside the old
/// one, and a process always resolves its own - the ring layout and the
/// launch arguments are a contract between the two halves.
pub fn path() -> Result<PathBuf, String> {
    app_support().map(|dir| dir.join("browser-host").join(env!("CARGO_PKG_VERSION")).join(APP))
}

/// Delete every staged bundle that is not this server's own version.
///
/// Staging never removes an older directory - the server still running on it
/// needs it - so the boot of the new server is the one moment that knows the
/// old tree is finished with. Each is a CEF tree, so leaving them would grow
/// the app-support dir by hundreds of megabytes per release.
///
/// **`sole_instance` is asked again before every removal**, not once at the
/// boot: a peer on another config dir that takes its lock after the first
/// check makes the tree shared again, and deleting its bundle is the
/// mid-task loss this prune exists beside. A machine-wide lock every boot
/// holds would close the window fully; this narrows it to the gap between
/// the check and one `remove_dir_all`.
pub fn prune_other_versions(sole_instance: &(dyn Fn() -> bool + Sync)) {
    let Ok(dir) = app_support() else { return };
    let removed = prune_other_versions_in(
        &dir.join("browser-host"),
        env!("CARGO_PKG_VERSION"),
        sole_instance,
    );
    if !removed.is_empty() {
        tracing::info!(
            event_name = "browser_host_versions_pruned",
            "the browser host bundles of other versions are removed: {:?}",
            removed,
        );
    }
}

fn prune_other_versions_in(
    root: &Path,
    keep: &str,
    sole_instance: &(dyn Fn() -> bool + Sync),
) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || path.file_name().and_then(|name| name.to_str()) == Some(keep) {
            continue;
        }
        if !sole_instance() {
            break;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            removed.push(path);
        }
    }
    removed
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
            "the browser host bundle at {} is version {found} and this server is {ours}; restart \
             forge so both halves run the version they were installed as",
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

    use super::{APP, EXECUTABLE, check, path, prune_other_versions_in};

    /// **The install location is the app-support tree, not a launch-relative
    /// path, and it is keyed by this server's version.** The bundle and the
    /// profiles are machine-local state, and a checkout's cwd must not change
    /// where the server looks.
    #[test]
    fn the_bundle_sits_beside_forges_own_state_under_its_version() {
        let path = path().expect("resolves on a normal dev machine");
        assert!(
            path.ends_with(format!("browser-host/{}/{APP}", env!("CARGO_PKG_VERSION"))),
            "a version-keyed bundle, so an install cannot replace the one this server runs: {}",
            path.display(),
        );
        assert!(path.is_absolute(), "an absolute install path: {}", path.display());
        let exe = path.join(EXECUTABLE);
        assert_eq!(exe.file_name().and_then(|name| name.to_str()), Some("Forge Browser"));
    }

    /// **Every other version's staged bundle is pruned at boot.** Nothing
    /// else would ever remove them and each is a CEF tree; the booting
    /// server is the one process whose version must survive.
    #[test]
    fn a_bundle_staged_under_another_version_is_pruned() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser-host");
        let ours = root.join(env!("CARGO_PKG_VERSION"));
        let theirs = root.join("0.0.1");
        staged(&ours, Some(env!("CARGO_PKG_VERSION")));
        staged(&theirs, Some("0.0.1"));
        let stray = root.join("README");
        std::fs::write(&stray, b"not a bundle").expect("the stray file");

        let removed = prune_other_versions_in(&root, env!("CARGO_PKG_VERSION"), &|| true);

        assert_eq!(removed, vec![theirs.clone()], "the other version's bundle is what went");
        assert!(ours.is_dir(), "this server's own bundle survives");
        assert!(!theirs.exists(), "and the other version's is gone");
        assert!(stray.is_file(), "a non-directory entry is not a staged bundle and is left alone");
    }

    /// **A shared machine prunes nothing.** The peer check is asked again
    /// before each removal, so a peer that appeared after the boot's first
    /// look keeps its bundle.
    #[test]
    fn a_shared_machine_prunes_nothing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("browser-host");
        let theirs = root.join("0.0.1");
        staged(&theirs, Some("0.0.1"));

        let removed = prune_other_versions_in(&root, env!("CARGO_PKG_VERSION"), &|| false);

        assert!(removed.is_empty(), "nothing is removed while a peer is live");
        assert!(theirs.is_dir(), "the other version's bundle stays");
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
        assert!(
            why.contains("restart forge"),
            "a server that meets another version's bundle is the old half; the remedy is a \
             restart, not a reinstall: {why}",
        );

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

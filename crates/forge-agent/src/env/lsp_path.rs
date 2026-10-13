//! Whether a declared LSP server binary resolves on PATH.
//!
//! Lifted from the terminal's Extensions pane when the web view grew the
//! same read (rule 24): the check is a fact about the machine, not a way of
//! drawing it, so views ask it here.

/// Whether `server_binary` resolves on `path_var`: some directory on the
/// path holds it as an executable file. `path_var` is injected so tests
/// need not touch the process environment.
pub fn lsp_binary_on_path(server_binary: &str, path_var: Option<&str>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path_var) = path_var else { return false };
    path_var.split(':').any(|dir| {
        let candidate = std::path::Path::new(dir).join(server_binary);
        candidate
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a check that answers "yes" for a binary that is not there:
    /// the row would draw the reassuring half of the pair for every server.
    #[test]
    fn a_missing_binary_is_not_on_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_string_lossy().into_owned();
        assert!(!lsp_binary_on_path("definitely-not-here-xyz", Some(&path)));
        assert!(!lsp_binary_on_path("definitely-not-here-xyz", None));
    }

    /// The declared command is what gets probed, not the server key, and a
    /// file that exists but is not executable does not count.
    #[test]
    fn the_declared_command_is_probed_as_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let binary = dir.path().join("probe-ls");
        std::fs::write(&binary, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");
        let path = dir.path().to_string_lossy().into_owned();
        assert!(
            !lsp_binary_on_path("probe-ls", Some(&path)),
            "a non-executable file must not read as on PATH"
        );
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
            .expect("chmod 755");
        assert!(lsp_binary_on_path("probe-ls", Some(&path)));
    }

    /// The walk covers every `:`-separated directory, not just the first: a
    /// binary living in a later entry is on the path exactly as one in the
    /// first is.
    #[test]
    fn the_walk_covers_every_directory_on_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        let binary = second.path().join("late-binary");
        std::fs::write(&binary, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let path = format!("{}:{}", first.path().display(), second.path().display());
        assert!(
            lsp_binary_on_path("late-binary", Some(&path)),
            "a directory after the first is walked like the first"
        );
    }
}

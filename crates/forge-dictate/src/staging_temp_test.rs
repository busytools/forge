//! The staging sweep's temp handling.
//!
//! A temp name can contain `.so` (a `.so` library's own temp), so the
//! shipped sweep must recognise temps by their own pattern rather than by
//! `is_library`. And a temp whose staging process died is litter that must
//! go, while one whose owner still runs is that stage's in-flight file and
//! must survive.

#[cfg(unix)]
mod unix {
    use crate::staging::stage;

    fn dead_pid() -> std::io::Result<u32> {
        let mut child = std::process::Command::new("sh").arg("-c").arg("exit 0").spawn()?;
        let pid = child.id();
        child.wait()?;
        Ok(pid)
    }

    /// A pid that is still running: the stand-in for another cargo
    /// invocation's in-flight stage. Our own pid cannot serve, because that
    /// is the name this stage gives its own temp and it removes it first.
    fn live_pid() -> std::io::Result<(u32, std::process::Child)> {
        let child = std::process::Command::new("sh").arg("-c").arg("sleep 5").spawn()?;
        let pid = child.id();
        Ok((pid, child))
    }

    #[test]
    fn staging_sweeps_dead_temps_and_leaves_live_ones() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let dest = root.path().join("staged");
        std::fs::create_dir_all(&source)?;
        std::fs::write(source.join("libprobe.dylib"), b"first")?;
        stage(&source, &dest)?;

        let dead = dead_pid()?;
        let (live, mut live_process) = live_pid()?;
        let so_temp = dest.join(format!(".libprobe.so.0.{dead}.tmp"));
        let dylib_temp = dest.join(format!(".libprobe.dylib.{dead}.tmp"));
        let live_temp = dest.join(format!(".libprobe.dylib.{live}.tmp"));
        for path in [&so_temp, &dylib_temp, &live_temp] {
            std::fs::write(path, b"temp")?;
        }

        stage(&source, &dest)?;

        assert!(
            !so_temp.exists(),
            "a `.so` temp whose owner died must be swept, not read as a stale library"
        );
        assert!(!dylib_temp.exists(), "a dylib temp whose owner died must be swept");
        assert!(
            live_temp.exists(),
            "a live stage's in-flight temp must survive the sweep, or its rename fails"
        );
        let _ = live_process.kill();
        let _ = live_process.wait();
        Ok(())
    }
}

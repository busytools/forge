//! Shared support for the live tests: the browser a run launched, doomed to
//! die with the run.
//!
//! **A leaked browser is not only litter.** It holds its port and its profile,
//! and a build that launched without `--use-mock-keychain` leaves a process
//! raising a keychain dialog nobody can answer; nine of them were found alive
//! once, each from a run whose cleanup never happened. The tests used to kill
//! by port on their success paths only, which leaks on every panic and on
//! nextest's cancellation of a failed run - so the kill is a Drop guard now:
//! whichever way a test ends, the browser it launched goes with it.

use std::process::Command;

/// A browser a test launched. Dropping it reaps it.
pub struct Launched {
    pid: Option<u32>,
    port: u16,
}

impl Launched {
    /// Hold the browser that was launched, by the id and port the launch
    /// answered with.
    pub fn new(pid: Option<u32>, port: u16) -> Self {
        Self { pid, port }
    }

    /// Reap it now, and confirm. Runs again at drop; asking twice is fine.
    pub fn reap(&self) {
        if let Some(pid) = self.pid {
            kill(pid);
        }
        kill_the_browser_on(self.port);
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        self.reap();
    }
}

/// Kill one process: TERM, a bounded wait, then KILL.
///
/// **The escalation is not paranoia.** A Chrome shutting down under load can
/// ignore TERM long enough that the run ends first, and the tree then outlives
/// everyone who could reap it - which is exactly how nine of them were found
/// running hours later.
fn kill(pid: u32) {
    let _ = Command::new("kill").arg(pid.to_string()).status();
    for _ in 0..20 {
        if !alive(pid) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
}

/// Whether a process with that id is still there.
fn alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output()
        .is_ok_and(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Kill whatever holds `port`'s listener: the second door, for a browser this
/// run did not launch. By the PORT, never by a pattern - this machine runs
/// other browsers, and one of them belongs to the person sitting at it.
pub fn kill_the_browser_on(port: u16) {
    let Ok(listed) =
        Command::new("lsof").args(["-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"]).output()
    else {
        return;
    };
    for pid in String::from_utf8_lossy(&listed.stdout).split_whitespace() {
        if let Ok(pid) = pid.parse::<u32>() {
            kill(pid);
        }
    }
}

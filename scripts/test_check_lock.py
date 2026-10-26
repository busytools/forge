#!/usr/bin/env python3
"""Tests for check_lock.sh: two holders serialise, a dead holder is
reclaimed, the wait bound names the live holder and leaves its lock alone,
release happens on a failed command and on SIGINT, a SIGTERM stops the whole
gate process group, an ownerless lock is reclaimed after its grace, and a
reclaim that races a live holder puts that holder back.

The lock is a pid-bearing file, so the cases are its contested states: free
between two runners, left behind by one that died, left ownerless, and
replaced by a live holder inside the reclaim window.
"""

import os
import signal
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check_lock.sh")


def running(pid):
    done = subprocess.run(
        ["ps", "-o", "stat=", "-p", str(pid)],
        capture_output=True,
        text=True,
    )
    state = done.stdout.strip()
    return bool(state) and not state.startswith("Z")


class CheckLock(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.lockroot = self.root / "locks"
        self.log = self.root / "order.log"

    def env(self, **extra):
        env = dict(
            os.environ,
            FORGE_CHECK_LOCK_DIR=str(self.lockroot),
            FORGE_CHECK_LOCK_TIMEOUT="30",
            LOCK_TEST_LOG=str(self.log),
        )
        env.update(extra)
        return env

    def lock(self):
        return self.lockroot / "check.lock"

    def stop(self, process):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()

    def wait_for(self, path, what, timeout=10):
        deadline = time.monotonic() + timeout
        while not path.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(path.exists(), what)

    def run_locked(self, command, **extra):
        return subprocess.run(
            [str(SCRIPT), "run", "--", *command],
            env=self.env(**extra),
            capture_output=True,
            text=True,
            timeout=60,
        )

    def test_two_holders_serialise(self):
        # The first holds until the test releases it, so the second is
        # definitely waiting; a timed hold flakes when a start lands late.
        release = self.root / "release"
        hold = (
            'echo start >> "$LOCK_TEST_LOG"; '
            'while [ ! -e "$LOCK_TEST_RELEASE" ]; do sleep 0.05; done; '
            'echo end >> "$LOCK_TEST_LOG"'
        )
        command = ["sh", "-c", hold]
        env = self.env(LOCK_TEST_RELEASE=str(release))
        first = subprocess.Popen(
            [str(SCRIPT), "run", "--", *command],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(self.stop, first)
        self.wait_for(self.lock(), "the first run never took the lock")

        second_out = self.root / "second.out"
        with open(second_out, "w", encoding="utf8") as out:
            second = subprocess.Popen(
                [str(SCRIPT), "run", "--", *command],
                env=env,
                stdout=out,
                stderr=subprocess.PIPE,
                text=True,
            )
            self.addCleanup(self.stop, second)
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if "waiting for another gate run" in second_out.read_text(
                    encoding="utf8"
                ):
                    break
                time.sleep(0.01)
            self.assertIn(
                f"[..] check: waiting for another gate run (held by pid {first.pid})",
                second_out.read_text(encoding="utf8"),
            )
            release.write_text("go", encoding="utf8")
            _, err_second = second.communicate(timeout=60)
        _, err_first = first.communicate(timeout=60)

        self.assertEqual(first.returncode, 0, err_first)
        self.assertEqual(second.returncode, 0, err_second)
        self.assertEqual(
            self.log.read_text(encoding="utf8").split(),
            ["start", "end", "start", "end"],
            "the two runs overlapped",
        )

    def test_a_dead_holder_is_reclaimed(self):
        dead = subprocess.Popen(["sh", "-c", "exit 0"])
        dead.wait()
        self.lock().parent.mkdir(parents=True)
        self.lock().write_text(f"{dead.pid}\n", encoding="utf8")

        done = self.run_locked(["true"])

        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn(f"reclaimed a stale lock (pid {dead.pid} is gone)", done.stdout)
        self.assertFalse(self.lock().exists(), "the lock was not released")

    def test_the_wait_bound_names_the_live_holder(self):
        # The waiter's exit must not delete a lock it never held.
        holder = subprocess.Popen(["sleep", "30"])
        self.addCleanup(holder.wait)
        self.addCleanup(holder.kill)
        self.lock().parent.mkdir(parents=True)
        self.lock().write_text(f"{holder.pid}\n", encoding="utf8")
        marker = self.root / "ran"

        done = self.run_locked(
            ["touch", str(marker)], FORGE_CHECK_LOCK_TIMEOUT="0"
        )

        self.assertNotEqual(done.returncode, 0)
        self.assertIn(
            f"[ERROR] check: gave up after 0s waiting for the gate lock held by pid {holder.pid}",
            done.stderr,
        )
        self.assertIn(
            f"[ERROR] check: the gate lock was not acquired (held by pid {holder.pid})",
            done.stdout,
        )
        self.assertTrue(self.lock().exists(), "a waiter deleted the holder's lock")
        self.assertEqual(self.lock().read_text(encoding="utf8"), f"{holder.pid}\n")
        self.assertFalse(marker.exists(), "the command ran without the lock")

    def test_a_failed_command_releases_the_lock(self):
        done = self.run_locked(["sh", "-c", "exit 7"])

        self.assertEqual(done.returncode, 7, done.stderr)
        self.assertFalse(self.lock().exists(), "the lock survived a failed command")

    def test_sigint_releases_the_lock(self):
        # 130 rather than -2 is what tells the INT trap ran; the lock going
        # away is what it ran for.
        run = subprocess.Popen(
            [str(SCRIPT), "run", "--", "sleep", "30"],
            env=self.env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(self.stop, run)
        self.wait_for(self.lock(), "the run never took the lock")

        run.send_signal(signal.SIGINT)
        _, err = run.communicate(timeout=30)

        self.assertEqual(run.returncode, 130, err)
        self.assertFalse(self.lock().exists(), "the lock survived SIGINT")

    def test_a_signal_between_fork_and_pid_stops_the_child(self):
        # The handler can run after the fork and before `child=$!`, so the
        # child exists with nothing recording it. If it is not stopped it
        # holds this test's pipes until its own sleep ends.
        marker = self.root / "ran"
        done = subprocess.run(
            [str(SCRIPT), "run", "--", "sh", "-c", f"touch {marker}; sleep 30"],
            env=self.env(
                FORGE_CHECK_LOCK_TEST_AFTER_FORK='kill -INT "$(ps -o ppid= -p $$)"'
            ),
            capture_output=True,
            text=True,
            timeout=15,
        )

        self.assertEqual(done.returncode, 130, done.stderr)
        self.assertFalse(self.lock().exists(), "the lock survived the signal")

    def test_sigterm_stops_the_gate_group_and_releases(self):
        # The grandchild stands in for a group shell's cargo or CEF child:
        # TERMing only the direct child leaves it running beside the next gate.
        grandchild = self.root / "grandchild"
        err_path = self.root / "sigterm.err"
        # Files rather than pipes: an orphaned grandchild would hold a pipe
        # open and turn the assertion below into a timeout.
        with open(err_path, "w", encoding="utf8") as err_file:
            run = subprocess.Popen(
                [
                    str(SCRIPT),
                    "run",
                    "--",
                    "sh",
                    "-c",
                    'sleep 30 & echo $! > "$LOCK_TEST_GRANDCHILD"; wait',
                ],
                env=self.env(LOCK_TEST_GRANDCHILD=str(grandchild)),
                stdout=subprocess.DEVNULL,
                stderr=err_file,
            )
        self.addCleanup(self.stop, run)
        self.wait_for(self.lock(), "the run never took the lock")
        self.wait_for(grandchild, "the gate never started its grandchild")
        child_pid = int(grandchild.read_text(encoding="utf8"))

        run.send_signal(signal.SIGTERM)
        run.wait(timeout=30)
        err = err_path.read_text(encoding="utf8")

        self.assertEqual(run.returncode, 143, err)
        self.assertFalse(self.lock().exists(), "the lock survived SIGTERM")
        deadline = time.monotonic() + 10
        while running(child_pid) and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertFalse(running(child_pid), "the grandchild survived the group TERM")

    def test_an_ownerless_lock_is_reclaimed_after_the_grace(self):
        self.lock().parent.mkdir(parents=True)
        self.lock().write_text("", encoding="utf8")

        done = self.run_locked(["true"], FORGE_CHECK_LOCK_TIMEOUT="10")

        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("reclaimed an ownerless lock", done.stdout)
        self.assertFalse(self.lock().exists())

    def test_a_racing_holder_is_put_back(self):
        # The seam replaces the lock with a live holder after the reclaim
        # reads the dead pid, which is the window a reclaimer can lose a lock.
        dead = subprocess.Popen(["sh", "-c", "exit 0"])
        dead.wait()
        racer = subprocess.Popen(["sleep", "30"])
        self.addCleanup(racer.wait)
        self.addCleanup(racer.kill)
        self.lock().parent.mkdir(parents=True)
        self.lock().write_text(f"{dead.pid}\n", encoding="utf8")
        seam = (
            'rm -f "$LOCK_TEST_RACE_LOCK"; '
            'printf "%s\\n" "$LOCK_TEST_RACE_PID" > "$LOCK_TEST_RACE_LOCK"'
        )

        done = self.run_locked(
            ["true"],
            FORGE_CHECK_LOCK_TIMEOUT="0",
            FORGE_CHECK_LOCK_TEST_RACE=seam,
            LOCK_TEST_RACE_LOCK=str(self.lock()),
            LOCK_TEST_RACE_PID=str(racer.pid),
        )

        self.assertNotEqual(done.returncode, 0)
        self.assertIn(
            f"the gate lock was not acquired (held by pid {racer.pid})",
            done.stdout,
        )
        self.assertTrue(self.lock().exists(), "the racing holder's lock was deleted")
        self.assertEqual(self.lock().read_text(encoding="utf8"), f"{racer.pid}\n")


if __name__ == "__main__":
    unittest.main()

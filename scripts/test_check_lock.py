#!/usr/bin/env python3
"""Tests for check_lock.py: two holders serialise, the wait bound names the
holder, a failed command returns its code, SIGINT and SIGTERM stop the gate's
tree and release, and a hard-killed holder leaves no stale lock.

The lock is a kernel flock on a pid-bearing file, so there is no stale lock
to reclaim and no owner to verify; the pid is only the name a waiting gate
prints.
"""

import fcntl
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check_lock.py")


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
        self.lockroot.mkdir()
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

    def lock_held(self):
        fd = os.open(self.lock(), os.O_RDWR | os.O_CREAT, 0o644)
        try:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return True
            fcntl.flock(fd, fcntl.LOCK_UN)
            return False
        finally:
            os.close(fd)

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

    def wait_for_pid(self, pid, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                if self.lock().read_text(encoding="utf8").strip() == str(pid):
                    return
            except FileNotFoundError:
                pass
            time.sleep(0.01)
        self.fail(f"the lock never named pid {pid}")

    def hold_lock(self):
        proc = subprocess.Popen(
            [
                sys.executable,
                "-c",
                "import fcntl, os, sys, time\n"
                "fd = os.open(sys.argv[1], os.O_RDWR | os.O_CREAT, 0o644)\n"
                "fcntl.flock(fd, fcntl.LOCK_EX)\n"
                "os.ftruncate(fd, 0)\n"
                "os.write(fd, str(os.getpid()).encode('ascii'))\n"
                "time.sleep(60)\n",
                str(self.lock()),
            ]
        )
        self.addCleanup(proc.wait)
        self.addCleanup(proc.kill)
        self.wait_for_pid(proc.pid)
        return proc

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
        env = self.env(LOCK_TEST_RELEASE=str(release))
        first = subprocess.Popen(
            [str(SCRIPT), "run", "--", "sh", "-c", hold],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(self.stop, first)
        self.wait_for_pid(first.pid)

        second_out = self.root / "second.out"
        with open(second_out, "w", encoding="utf8") as out:
            second = subprocess.Popen(
                [str(SCRIPT), "run", "--", "sh", "-c", hold],
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

    def test_the_wait_bound_names_the_live_holder(self):
        # The waiter's exit must not touch a lock it never held.
        holder = self.hold_lock()
        marker = self.root / "ran"

        done = self.run_locked(
            ["touch", str(marker)], FORGE_CHECK_LOCK_TIMEOUT="0"
        )

        self.assertEqual(done.returncode, 1, done.stderr)
        self.assertIn(
            f"[..] check: waiting for another gate run (held by pid {holder.pid})",
            done.stdout,
        )
        self.assertIn(
            f"[ERROR] check: gave up after 0s waiting for the gate lock held by pid {holder.pid}",
            done.stderr,
        )
        self.assertIn(
            f"[ERROR] check: the gate lock was not acquired (held by pid {holder.pid})",
            done.stdout,
        )
        self.assertTrue(self.lock_held(), "a waiter released the holder's lock")
        self.assertFalse(marker.exists(), "the command ran without the lock")

    def test_a_failed_command_returns_its_code(self):
        done = self.run_locked(["sh", "-c", "exit 7"])

        self.assertEqual(done.returncode, 7, done.stderr)
        self.assertFalse(self.lock_held(), "the lock survived the run")

    def signal_run(self, signum, expected):
        grandchild = self.root / "grandchild"
        err_path = self.root / f"signal-{expected}.err"
        # Files rather than pipes: a surviving grandchild would hold a pipe
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
        self.wait_for_pid(run.pid)
        self.wait_for(grandchild, "the gate never started its grandchild")
        child_pid = int(grandchild.read_text(encoding="utf8"))

        run.send_signal(signum)
        run.wait(timeout=30)
        err = err_path.read_text(encoding="utf8")

        self.assertEqual(run.returncode, expected, err)
        deadline = time.monotonic() + 10
        while running(child_pid) and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertFalse(running(child_pid), "the grandchild survived the signal")
        self.assertFalse(self.lock_held(), "the lock survived the signal")

    def test_sigint_stops_the_gate_tree_and_releases(self):
        self.signal_run(signal.SIGINT, 130)

    def test_sigterm_stops_the_gate_tree_and_releases(self):
        self.signal_run(signal.SIGTERM, 143)

    def test_a_hard_killed_holder_leaves_no_stale_lock(self):
        child_file = self.root / "gate-child"
        run = subprocess.Popen(
            [
                str(SCRIPT),
                "run",
                "--",
                "sh",
                "-c",
                'echo $$ > "$LOCK_TEST_CHILD"; sleep 60',
            ],
            env=self.env(LOCK_TEST_CHILD=str(child_file)),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.addCleanup(self.stop, run)
        self.wait_for_pid(run.pid)
        self.wait_for(child_file, "the gate never started")
        gate_pid = int(child_file.read_text(encoding="utf8"))

        os.kill(run.pid, signal.SIGKILL)
        run.wait(timeout=10)
        self.assertEqual(run.returncode, -signal.SIGKILL)

        # SIGKILL cannot be forwarded, so the orphaned gate is cleaned by its
        # own pid; the lock itself is the kernel's to release.
        try:
            os.killpg(gate_pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

        done = self.run_locked(["true"], FORGE_CHECK_LOCK_TIMEOUT="0")

        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)


if __name__ == "__main__":
    unittest.main()

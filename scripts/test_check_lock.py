#!/usr/bin/env python3
"""Tests for check_lock.sh: two holders serialise, a dead holder is
reclaimed, and the wait bound fails naming the live holder.

The lock is a directory, so the cases are its three contested states: free
between two runners, left behind by one that died, and held by a live one
that never yields.
"""

import os
import signal
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check_lock.sh")


class CheckLock(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.lockroot = self.root / "locks"
        self.log = self.root / "order.log"

    def env(self, **extra):
        return dict(
            os.environ,
            FORGE_CHECK_LOCK_DIR=str(self.lockroot),
            LOCK_TEST_LOG=str(self.log),
            **extra,
        )

    def lock(self):
        return self.lockroot / "check.lock"

    def run_locked(self, command, **extra):
        return subprocess.run(
            [str(SCRIPT), "run", "--", *command],
            env=self.env(**extra),
            capture_output=True,
            text=True,
            timeout=15,
        )

    def test_two_holders_serialise(self):
        # The two runs append to one log; overlapping them would interleave
        # start/end, which is the collision the lock exists to prevent.
        command = [
            "sh",
            "-c",
            'echo start >> "$LOCK_TEST_LOG"; sleep 0.7; echo end >> "$LOCK_TEST_LOG"',
        ]
        first = subprocess.Popen(
            [str(SCRIPT), "run", "--", *command],
            env=self.env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.monotonic() + 5
        while not self.lock().is_dir() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(self.lock().is_dir(), "the first run never took the lock")

        second = subprocess.Popen(
            [str(SCRIPT), "run", "--", *command],
            env=self.env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        out_second, err_second = second.communicate(timeout=15)
        out_first, err_first = first.communicate(timeout=15)

        self.assertEqual(first.returncode, 0, err_first)
        self.assertEqual(second.returncode, 0, err_second)
        self.assertEqual(
            self.log.read_text(encoding="utf8").split(),
            ["start", "end", "start", "end"],
            "the two runs overlapped",
        )
        self.assertIn(
            f"[..] check: waiting for another gate run (held by pid {first.pid})",
            out_second,
        )

    def test_a_dead_holder_is_reclaimed(self):
        dead = subprocess.Popen(["sh", "-c", "exit 0"])
        dead.wait()
        self.lock().mkdir(parents=True)
        (self.lock() / "pid").write_text(f"{dead.pid}\n", encoding="utf8")

        done = self.run_locked(["true"])

        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn(
            f"reclaimed a stale lock (pid {dead.pid} is gone)", done.stdout
        )
        self.assertFalse(self.lock().exists(), "the lock was not released")

    def test_a_failed_command_releases_the_lock(self):
        # A gate step failing is the case that leaves a machine-wide lock
        # behind for an hour if the release is only on the success path.
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
        deadline = time.monotonic() + 5
        while not self.lock().is_dir() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(self.lock().is_dir(), "the run never took the lock")

        run.send_signal(signal.SIGINT)
        _, err = run.communicate(timeout=10)

        self.assertEqual(run.returncode, 130, err)
        self.assertFalse(self.lock().exists(), "the lock survived SIGINT")

    def test_the_wait_bound_names_the_live_holder(self):
        holder = subprocess.Popen(["sleep", "30"])
        self.addCleanup(holder.wait)
        self.addCleanup(holder.kill)
        self.lock().mkdir(parents=True)
        (self.lock() / "pid").write_text(f"{holder.pid}\n", encoding="utf8")
        marker = self.root / "ran"

        done = self.run_locked(
            ["touch", str(marker)], FORGE_CHECK_LOCK_TIMEOUT="0"
        )

        self.assertNotEqual(done.returncode, 0)
        self.assertIn(
            f"[ERROR] check: gave up after 0s waiting for the gate lock held by pid {holder.pid}",
            done.stderr,
        )
        self.assertFalse(marker.exists(), "the command ran without the lock")


if __name__ == "__main__":
    unittest.main()

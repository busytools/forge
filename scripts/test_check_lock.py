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

    def wait_for_pid_file(self, path, what, timeout=10):
        # The file is created before its content lands, so existence alone is
        # not the rendezvous.
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if path.exists():
                text = path.read_text(encoding="utf8").strip()
                if text.isdigit():
                    return int(text)
            time.sleep(0.01)
        self.fail(what)

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

        self.assertEqual(
            first.returncode, 0, "the first holder did not exit cleanly"
        )
        self.assertEqual(
            second.returncode, 0, "the second holder did not exit cleanly"
        )
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

        self.assertEqual(done.returncode, 1, "the zero bound did not fail the run")
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

    def test_the_wait_bound_expires_when_the_holder_stays(self):
        # The nonzero path is the one that reaches signal.alarm; the `0` case
        # short-circuits before it.
        holder = self.hold_lock()
        marker = self.root / "ran"

        done = self.run_locked(
            ["touch", str(marker)], FORGE_CHECK_LOCK_TIMEOUT="2"
        )

        self.assertEqual(done.returncode, 1, "the bound did not fail the run")
        self.assertIn(
            f"[ERROR] check: gave up after 2s waiting for the gate lock held by pid {holder.pid}",
            done.stderr,
        )
        self.assertIn(
            f"[ERROR] check: the gate lock was not acquired (held by pid {holder.pid})",
            done.stdout,
        )
        self.assertNotIn("remove", done.stderr.lower())
        self.assertTrue(self.lock_held(), "a waiter released the holder's lock")
        self.assertFalse(marker.exists(), "the command ran without the lock")

    def test_a_term_ignoring_gate_is_killed_after_the_grace(self):
        # A gate that traps TERM would otherwise hold the machine-wide lock
        # until FORGE_CHECK_LOCK_TIMEOUT and fail every other gate.
        run = subprocess.Popen(
            [str(SCRIPT), "run", "--", "sh", "-c", 'trap "" TERM; sleep 60'],
            env=self.env(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.addCleanup(self.stop, run)
        self.wait_for_pid(run.pid)

        run.send_signal(signal.SIGTERM)
        run.wait(timeout=30)

        self.assertEqual(run.returncode, 143, "the TERM-ignoring gate did not exit 143")
        self.assertFalse(self.lock_held(), "the lock survived a TERM-ignoring gate")

    def test_a_failed_command_returns_its_code(self):
        done = self.run_locked(["sh", "-c", "exit 7"])

        self.assertEqual(done.returncode, 7, "the child's exit code was not returned")
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
        child_pid = self.wait_for_pid_file(
            grandchild, "the gate never named its grandchild"
        )

        run.send_signal(signum)
        run.wait(timeout=30)
        err = err_path.read_text(encoding="utf8")

        self.assertEqual(run.returncode, expected, "the signal's exit code was not returned")
        deadline = time.monotonic() + 10
        while running(child_pid) and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertFalse(running(child_pid), "the grandchild survived the signal")
        self.assertFalse(self.lock_held(), "the lock survived the signal")

    def test_a_signal_between_fork_and_pid_stops_the_child(self):
        # A handler can run after Popen() returns and before state["proc"] is
        # assigned; the child then exists with nothing recording it, and must
        # still be stopped or it holds this test's pipes until its own sleep.
        child_file = self.root / "gate-child"
        done = subprocess.run(
            [
                str(SCRIPT),
                "run",
                "--",
                "sh",
                "-c",
                f'echo $$ > "{child_file}"; sleep 30',
            ],
            env=self.env(
                FORGE_CHECK_LOCK_TEST_AFTER_FORK='kill -INT "$(ps -o ppid= -p $$)"'
            ),
            capture_output=True,
            text=True,
            timeout=15,
        )

        self.assertEqual(
            done.returncode, 130, "the signal handler did not run"
        )
        self.assertFalse(self.lock_held(), "the lock survived the signal")
        if child_file.exists():
            text = child_file.read_text(encoding="utf8").strip()
            if text.isdigit():
                self.assertFalse(
                    running(int(text)), "the child survived the signal"
                )

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
        # Without this, an implementation that never locks would pass: the
        # real behaviour of the lock itself is proven by the serialise test.
        self.assertTrue(self.lock_held(), "the gate never held the lock")
        gate_pid = self.wait_for_pid_file(child_file, "the gate never named its child")

        os.kill(run.pid, signal.SIGKILL)
        run.wait(timeout=10)
        self.assertEqual(run.returncode, -signal.SIGKILL, "the holder was not killed")

        # SIGKILL cannot be forwarded, so the orphaned gate is cleaned by its
        # own pid; the lock itself is the kernel's to release.
        try:
            os.killpg(gate_pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

        done = self.run_locked(["true"], FORGE_CHECK_LOCK_TIMEOUT="0")

        self.assertEqual(
            done.returncode, 0, "the lock was still held after a hard kill"
        )


if __name__ == "__main__":
    unittest.main()

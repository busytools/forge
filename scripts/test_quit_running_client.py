#!/usr/bin/env python3
"""Tests for quitting a client before its bundle is replaced. Run:
python3 scripts/test_quit_running_client.py

`just release` installs the client over /Applications/forge.app, and a client
still running from that bundle must not be swapped out underneath itself. The
release quits it first - a graceful SIGTERM and a bounded wait - and refuses
only when the quit does not take: a client that ignores the signal still leaves
the bundle in use, and replacing it is the one way this fails quietly.

Stdlib only, like the other script tests: python3 is already a check
dependency.
"""

import subprocess
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "quit-running-client.sh"


class QuitRunningClientTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.app = self.root / "forge.app"
        self.exe = self.app / "Contents" / "MacOS" / "forge-client"

    def stand_in(self, body: str) -> "subprocess.Popen[bytes]":
        self.exe.parent.mkdir(parents=True)
        self.exe.write_text(body)
        self.exe.chmod(0o755)
        proc = subprocess.Popen([str(self.exe)])
        self.addCleanup(proc.wait)
        self.addCleanup(proc.kill)
        self.wait_until_in_use()
        return proc

    def wait_until_in_use(self) -> None:
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.running_pids():
                return
            time.sleep(0.05)
        self.fail(f"the stand-in client never came up at {self.exe}")

    def running_pids(self) -> "list[str]":
        result = subprocess.run(["lsof", "-t", str(self.exe)], capture_output=True, text=True)
        return result.stdout.split()

    def quit(self, bound: str = "2") -> "subprocess.CompletedProcess[str]":
        return subprocess.run([str(SCRIPT), str(self.app), bound], capture_output=True, text=True)

    def test_no_client_is_not_in_use(self) -> None:
        result = self.quit()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stderr, "", "lsof printed its usage banner for a bundle that is not there"
        )

    def test_a_client_that_exits_on_quit_is_waited_for(self) -> None:
        self.stand_in('#!/bin/sh\ntrap "exit 0" TERM\nwhile :; do sleep 0.1; done\n')

        result = self.quit()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.running_pids(), [], "the client outlived the quit")

    def test_a_client_that_ignores_quit_is_refused(self) -> None:
        self.stand_in('#!/bin/sh\ntrap "" TERM\nwhile :; do sleep 0.1; done\n')

        result = self.quit(bound="1")

        self.assertNotEqual(
            result.returncode, 0, "a client that ignored SIGTERM was not refused"
        )
        self.assertIn(
            "the client did not quit",
            result.stderr,
            "the refusal must say the quit did not take, not only that the bundle is in use",
        )
        self.assertNotEqual(self.running_pids(), [], "the refusal fired for a client that quit")


if __name__ == "__main__":
    unittest.main()

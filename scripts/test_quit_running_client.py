#!/usr/bin/env python3
"""Tests for quitting a client before its bundle is replaced. Run:
python3 scripts/test_quit_running_client.py

`just release` installs the client over /Applications/forge.app, and a client
still running from that bundle must not be swapped out underneath itself. The
release quits it first - a graceful SIGTERM and a bounded wait - and refuses
only when the quit does not take: a client that ignores the signal still leaves
the bundle in use, and replacing it is the one way this fails quietly.

The query must name the process EXECUTING the client, not every process with
the binary open, so a file-holder that ignores TERM is neither signalled nor
waited on. The stand-in is a compiled stub because `lsof -d txt` reports the
executable mapping, which a script's interpreter owns rather than the script.

Stdlib only, like the other script tests: python3, cc and lsof all ship with
the gate's runners, macOS and ubuntu alike.
"""

import subprocess
import tempfile
import time
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "quit-running-client.sh"

# Terminates on the default SIGTERM disposition. An ignoring client is the
# same stub launched through a shell that ignores TERM before the exec, so
# the ignore is in place from the process's first instant rather than from a
# line of C that could lose a race with the signal.
STUB = """\
#include <unistd.h>

int main(void) {
    for (;;) {
        pause();
    }
}
"""

# Holds the client's binary open and records any TERM that reaches it.
HOLDER = """\
#!/bin/sh
trap 'echo hit > "$2"' TERM
exec 3<"$1"
while :; do sleep 0.1; done
"""


class QuitRunningClientTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.app = self.root / "forge.app"
        self.exe = self.app / "Contents" / "MacOS" / "forge-client"
        self.exe.parent.mkdir(parents=True)
        source = self.root / "stub.c"
        source.write_text(STUB)
        built = subprocess.run(
            ["cc", "-o", str(self.exe), str(source)], capture_output=True, text=True
        )
        self.assertEqual(built.returncode, 0, built.stderr)

    def reap(self, proc: "subprocess.Popen[bytes]") -> None:
        self.addCleanup(proc.wait)
        self.addCleanup(proc.kill)

    def client(self, ignore: bool = False) -> "subprocess.Popen[bytes]":
        if ignore:
            wrapper = self.root / "ignore.sh"
            wrapper.write_text(f'#!/bin/sh\ntrap "" TERM\nexec "{self.exe}"\n')
            wrapper.chmod(0o755)
            proc = subprocess.Popen([str(wrapper)])
        else:
            proc = subprocess.Popen([str(self.exe)])
        self.reap(proc)
        self.wait_until_in_use(proc)
        return proc

    def holder(self) -> "tuple[subprocess.Popen[bytes], Path]":
        marker = self.root / "marker"
        script = self.root / "holder.sh"
        script.write_text(HOLDER)
        script.chmod(0o755)
        proc = subprocess.Popen([str(script), str(self.exe), str(marker)])
        self.reap(proc)
        self.wait_until_in_use(proc)
        return proc, marker

    def wait_until_in_use(self, proc: "subprocess.Popen[bytes]", count: int = 1) -> None:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if len(self.running_pids()) >= count:
                return
            if proc.poll() is not None:
                self.fail(f"the stand-in exited with {proc.returncode} instead of holding {self.exe}")
            time.sleep(0.05)
        self.fail(f"the stand-in never held {self.exe}")

    def running_pids(self) -> "list[str]":
        result = subprocess.run(["lsof", "-t", str(self.exe)], capture_output=True, text=True)
        return result.stdout.split()

    def quit(self, bound: str = "2") -> "subprocess.CompletedProcess[str]":
        # A regression in the script's own deadline would otherwise hang the
        # gate; the bound here only has to outlast the longest wait above.
        try:
            return subprocess.run(
                [str(SCRIPT), str(self.app), bound],
                capture_output=True,
                text=True,
                timeout=15,
            )
        except subprocess.TimeoutExpired:
            self.fail(f"the quit script hung past 15s with its own bound at {bound}")

    def test_no_client_is_not_in_use(self) -> None:
        result = self.quit()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stderr, "", "lsof printed its usage banner for a bundle that is not there"
        )

    def test_a_client_that_exits_on_quit_is_waited_for(self) -> None:
        self.client()

        result = self.quit()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.running_pids(), [], "the client outlived the quit")

    def test_a_client_that_ignores_quit_is_refused(self) -> None:
        self.client(ignore=True)

        result = self.quit(bound="1")

        self.assertNotEqual(
            result.returncode,
            0,
            f"a client that ignored SIGTERM was not refused: {result.stderr}",
        )
        self.assertIn(
            "the client did not quit",
            result.stderr,
            "the refusal must say the quit did not take, not only that the bundle is in use",
        )
        self.assertNotEqual(self.running_pids(), [], "the refusal fired for a client that quit")

    def test_a_file_holder_is_neither_signalled_nor_waited_for(self) -> None:
        holder, marker = self.holder()

        result = self.quit(bound="1")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(marker.exists(), "a process that only held the binary open was signalled")
        self.assertIsNone(holder.poll(), "the file-holder was killed")

    def test_a_holder_does_not_keep_the_wait_running(self) -> None:
        # **The wait loop's own query is pinned here.** The client exits on the
        # quit, and the holder is the only process left with the file open: a
        # plain `lsof` in the loop would keep the bundle "in use" until the
        # bound and refuse, where the executing-only query finds nothing.
        self.client()
        holder, marker = self.holder()
        self.wait_until_in_use(holder, count=2)

        result = self.quit(bound="1")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(marker.exists(), "a process that only held the binary open was signalled")
        self.assertIsNone(holder.poll(), "the file-holder was killed")


if __name__ == "__main__":
    unittest.main()

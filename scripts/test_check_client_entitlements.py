#!/usr/bin/env python3
"""Tests for `check_client_entitlements.py`, the read-back that keeps a client
bundle without the microphone entitlement from shipping. Run:
python3 scripts/test_check_client_entitlements.py

`codesign` is macOS-only, so every case here puts a stand-in first on PATH and
pins the argv the gate hands it. What the real tool returns was checked by hand
against the signed build and against an ad-hoc bundle carrying the hardened
runtime with no entitlements.

The stand-in logs each call, so a case can prove the gate actually reached
codesign rather than passing without reading anything.

Stdlib only, like the other script tests: python3 ships with the gate's
runners, macOS and ubuntu alike.
"""

import os
import plistlib
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check_client_entitlements.py")
ENTITLEMENT = "com.apple.security.device.audio-input"
USAGE = "NSMicrophoneUsageDescription"

# `CODESIGN_EXIT` makes the stand-in fail the way codesign does for an unsigned
# bundle; otherwise it prints whatever `CODESIGN_PLIST` holds, empty included.
CODESIGN = """\
#!/bin/sh
for arg in "$@"; do
    printf '%s\\n' "$arg"
done >> "$CODESIGN_LOG"
if [ -n "${CODESIGN_EXIT:-}" ]; then
    printf '%s\\n' "$CODESIGN_STDERR" >&2
    exit "$CODESIGN_EXIT"
fi
cat "$CODESIGN_PLIST"
"""


class CheckClientEntitlementsTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)

        self.bundle = self.root / "forge.app"
        (self.bundle / "Contents").mkdir(parents=True)
        self.write_info({USAGE: "Dictation records while you hold the key."})

        self.bin = self.root / "bin"
        self.bin.mkdir()
        codesign = self.bin / "codesign"
        codesign.write_text(CODESIGN)
        codesign.chmod(0o755)

        self.log = self.root / "codesign.log"
        self.plist = self.root / "entitlements.plist"
        self.write_entitlements({ENTITLEMENT: True})

    def write_info(self, entries: dict) -> None:
        with (self.bundle / "Contents" / "Info.plist").open("wb") as handle:
            plistlib.dump(entries, handle)

    def write_entitlements(self, entries: "dict | None") -> None:
        if entries is None:
            self.plist.write_bytes(b"")
            return
        with self.plist.open("wb") as handle:
            plistlib.dump(entries, handle)

    def codesign_calls(self) -> "list[str]":
        if not self.log.exists():
            return []
        return self.log.read_text().splitlines()

    def run_gate(self, bundle: "Path | None" = None, **env: str) -> "subprocess.CompletedProcess[str]":
        environ = {
            **os.environ,
            "PATH": f"{self.bin}{os.pathsep}{os.environ.get('PATH', '')}",
            "CODESIGN_LOG": str(self.log),
            "CODESIGN_PLIST": str(self.plist),
        }
        environ.update(env)
        return subprocess.run(
            [sys.executable, str(SCRIPT), str(bundle or self.bundle)],
            capture_output=True,
            text=True,
            env=environ,
        )

    def test_a_declared_entitlement_and_usage_string_pass(self) -> None:
        result = self.run_gate()

        self.assertEqual(result.returncode, 0, result.stderr)
        # The success line names the entitlement, so the positive path makes
        # the same claim the check does rather than passing on a bare OK.
        self.assertIn(f"[OK] {self.bundle.name} declares {ENTITLEMENT}", result.stdout)
        # The read-back has to be the signature's own entitlements, not the
        # config file's intent, so the argv names codesign's display mode.
        calls = self.codesign_calls()
        self.assertTrue(calls, "the gate passed without reading the signature")
        self.assertEqual(calls[-5:], ["-d", "--entitlements", "-", "--xml", str(self.bundle)])

    def test_a_bundle_with_no_entitlements_is_refused(self) -> None:
        self.write_entitlements(None)

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a bundle with no entitlements passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn(ENTITLEMENT, result.stderr)
        self.assertTrue(self.codesign_calls(), "the refusal did not come from a signature read")

    def test_an_entitlement_declared_false_is_refused(self) -> None:
        self.write_entitlements({ENTITLEMENT: False})

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "an entitlement declared false passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn(f"{ENTITLEMENT} is declared False", result.stderr)

    def test_an_entitlement_set_without_the_audio_key_is_refused(self) -> None:
        # A plist that parses and carries other entitlements is the absent-key
        # path, which an empty read does not reach.
        self.write_entitlements({"com.apple.security.cs.allow-jit": True})

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a set without the audio key passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn(f"{ENTITLEMENT} is absent, expected true", result.stderr)
        self.assertTrue(self.codesign_calls(), "the refusal did not come from a signature read")

    def test_a_bundle_without_the_usage_string_is_refused(self) -> None:
        self.write_info({"CFBundleIdentifier": "dev.vedhavyas.forge"})

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a bundle with no usage string passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn(USAGE, result.stderr)

    def test_an_unsigned_bundle_is_refused(self) -> None:
        result = self.run_gate(
            CODESIGN_EXIT="1",
            CODESIGN_STDERR=f"{self.bundle}: code object is not signed at all",
        )

        self.assertEqual(result.returncode, 1, "an unsigned bundle passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("code object is not signed at all", result.stderr)

    def test_entitlements_that_do_not_parse_are_refused(self) -> None:
        self.plist.write_text("not a plist")

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "unreadable entitlements passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("do not parse", result.stderr)

    def test_a_missing_bundle_is_refused(self) -> None:
        result = self.run_gate(bundle=self.root / "nothing.app")

        self.assertEqual(result.returncode, 1, "a missing bundle passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("no app bundle", result.stderr)
        self.assertEqual(self.codesign_calls(), [], "codesign ran for a bundle that is not there")


if __name__ == "__main__":
    unittest.main()

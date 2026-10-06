#!/usr/bin/env python3
"""Tests for `update_manifest.py`, the file every installed client reads for
updates. Run: python3 scripts/test_update_manifest.py

Stdlib only, like the unicode gate: python3 is already a check dependency.
"""

import hashlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("update_manifest.py")
RELEASE = "https://github.com/busytools/forge/releases/download/v9.9.9"
SIGNATURE = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkK\n"


class UpdateManifestTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.bundle = self.root / "client/src-tauri/target/release/bundle"
        (self.bundle / "macos").mkdir(parents=True)
        (self.bundle / "android").mkdir(parents=True)
        # Stand-ins: the contents only have to differ between files, because
        # what the test pins is WHICH file each hash was taken from.
        (self.bundle / "macos/forge.app.tar.gz").write_bytes(b"the app tarball")
        (self.bundle / "macos/forge.app.tar.gz.sig").write_text(SIGNATURE)
        self.apk = self.bundle / "android/forge-9.9.9-arm64.apk"
        self.apk.write_bytes(b"the apk")
        self.web = self.bundle / "forge-web-9.9.9.tar.gz"
        self.web.write_bytes(b"the web archive")

    def run_generator(self) -> "subprocess.CompletedProcess[str]":
        return subprocess.run(
            [sys.executable, str(SCRIPT), str(self.root), "9.9.9"],
            capture_output=True,
            text=True,
        )

    def manifest(self) -> dict:
        return json.loads((self.bundle / "latest.json").read_text())

    def test_writes_every_block_and_the_hash_of_the_file_it_names(self) -> None:
        result = self.run_generator()
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = self.manifest()

        self.assertEqual(manifest["version"], "9.9.9")
        self.assertEqual(
            manifest["platforms"]["darwin-aarch64"]["url"], f"{RELEASE}/forge.app.tar.gz"
        )
        self.assertEqual(manifest["android"]["url"], f"{RELEASE}/forge-9.9.9-arm64.apk")
        self.assertEqual(manifest["web"]["url"], f"{RELEASE}/forge-web-9.9.9.tar.gz")

        # Hashing the wrong file still exits 0, and the puller refuses every
        # update - the checksum is its only integrity check.
        self.assertEqual(
            manifest["web"]["sha256"], hashlib.sha256(self.web.read_bytes()).hexdigest()
        )
        self.assertNotEqual(
            manifest["web"]["sha256"], hashlib.sha256(self.apk.read_bytes()).hexdigest()
        )

        # The operator's line must not claim a block the file does not carry.
        for key in ("platforms", "android", "web"):
            self.assertIn(key, result.stdout)

    def test_trims_only_the_trailing_newline_off_the_signature(self) -> None:
        self.run_generator()

        # The plugin base64-decodes this value whole: one trailing newline is
        # an InvalidByte and every desktop install fails.
        self.assertEqual(
            self.manifest()["platforms"]["darwin-aarch64"]["signature"],
            SIGNATURE.rstrip(),
        )

    def test_refuses_a_missing_artifact_by_name(self) -> None:
        self.web.unlink()

        result = self.run_generator()

        self.assertEqual(result.returncode, 1)
        self.assertIn("forge-web-9.9.9.tar.gz", result.stderr)
        self.assertFalse((self.bundle / "latest.json").exists())


if __name__ == "__main__":
    unittest.main()

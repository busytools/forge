#!/usr/bin/env python3
"""Tests for check_updater_signature.py, the read-back that keeps a stale
update tarball from shipping.

Run: python3 scripts/test_check_updater_signature.py

The script shells out to codesign for the extracted app's signature and team;
the tests stand a codesign in on PATH and log its argv. The tarball itself is
built with Python's tarfile, so no tar stand-in is needed. The pinned pubkey
and the signature are hand-built minisign text whose key ids match, so the
positive path does not depend on a real keypair.
"""

import base64
import json
import os
import plistlib
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check_updater_signature.py")
KEY_ID = b"ABCDEFGH"


def minisign_text(blob: bytes, comment: str) -> str:
    """A minisign text file whose second line decodes to a blob with KEY_ID."""
    return (
        "untrusted comment: test key\n"
        f"{base64.b64encode(KEY_ID + blob).decode()}\n"
        f"trusted comment: {comment}\n"
        f"{base64.b64encode(b'signature').decode()}\n"
    )


def base64_of(text: str) -> str:
    return base64.b64encode(text.encode()).decode()


class CheckUpdaterSignatureTests(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.bundle = self.root / "client/src-tauri/target/release/bundle/macos"
        self.bundle.mkdir(parents=True)
        self.bin = self.root / "bin"
        self.bin.mkdir()

        self.write_pubkey()
        self.write_signature("1.2.19")

        self.write_codesign()
        self.write_tarball()
        self.team = "KALKD5L7NF"
        self.app_id = "KALKD5L7NF.dev.vedhavyas.forge"
        self.groups = [self.app_id]
        self.codesign_exit = ""
        self.codesign_stderr = ""

    def write_pubkey(self) -> None:
        conf = self.root / "client/src-tauri/tauri.conf.json"
        conf.parent.mkdir(parents=True, exist_ok=True)
        conf.write_text(
            json.dumps(
                {
                    "plugins": {
                        "updater": {
                            "pubkey": base64_of(minisign_text(KEY_ID, "pinned"))
                        }
                    }
                }
            )
        )

    def write_signature(self, version: str, key_id: bytes = KEY_ID) -> None:
        sig = self.bundle / "forge.app.tar.gz.sig"
        sig.write_text(base64_of(minisign_text(key_id, f"version:{version}")))

    def write_codesign(self) -> None:
        codesign = self.bin / "codesign"
        codesign.write_text(
            """#!/bin/sh
printf '%s\\n' "---" >> "$CODESIGN_LOG"
for arg in "$@"; do
  echo "$arg"
done >> "$CODESIGN_LOG"
if [ -n "$CODESIGN_EXIT" ]; then
  echo "$CODESIGN_STDERR" >&2
  exit "$CODESIGN_EXIT"
fi
case " $* " in
  *" --verbose=4 "*)
    if [ -n "$CODESIGN_NO_TEAM" ]; then
      echo "Identifier=dev.vedhavyas.forge" >&2
    else
      echo "TeamIdentifier=$CODESIGN_TEAM" >&2
    fi
    ;;
  *" --entitlements "*)
    case " $* " in
      *" --xml "*)
        echo "$CODESIGN_ENTITLEMENTS"
        ;;
      *)
        echo "[Dict]"
        echo "    [Key] com.apple.application-identifier"
        echo "    [Value]"
        echo "        [String] KALKD5L7NF.dev.vedhavyas.forge"
        ;;
    esac
    ;;
esac
"""
        )
        codesign.chmod(0o755)

    def write_tarball(
        self,
        profile: bool = True,
        appledouble: bool = False,
        extra: "dict[str, bytes] | None" = None,
    ) -> None:
        tarball = self.bundle / "forge.app.tar.gz"
        members = {}
        if appledouble:
            members["._forge.app"] = b"\x00\x05\x16\x07"
        members["forge.app/Contents/MacOS/forge-client"] = b"binary"
        members["forge.app/Contents/Info.plist"] = b"plist"
        if profile:
            members["forge.app/Contents/embedded.provisionprofile"] = b"profile"
        if extra:
            members.update(extra)
        with tarfile.open(tarball, "w:gz") as archive:
            for name, data in members.items():
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, __import__("io").BytesIO(data))

    def codesign_calls(self) -> "list[list[str]]":
        """Each codesign invocation's argv, split on the stub's marker."""
        log = self.root / "codesign.log"
        if not log.exists():
            return []
        invocations = []
        current = []
        for line in log.read_text().splitlines():
            if line == "---":
                if current:
                    invocations.append(current)
                current = []
            else:
                current.append(line)
        if current:
            invocations.append(current)
        return invocations

    def run_gate(self, **env: str) -> "subprocess.CompletedProcess[str]":
        entitlements = {
            "com.apple.application-identifier": self.app_id,
            "keychain-access-groups": self.groups,
        }
        environ = {
            **os.environ,
            "PATH": f"{self.bin}{os.pathsep}{os.environ.get('PATH', '')}",
            "CODESIGN_LOG": str(self.root / "codesign.log"),
            "CODESIGN_TEAM": self.team,
            "CODESIGN_ENTITLEMENTS": plistlib.dumps(entitlements).decode(),
            "CODESIGN_EXIT": self.codesign_exit,
            "CODESIGN_STDERR": self.codesign_stderr,
        }
        environ.update(env)
        return subprocess.run(
            [sys.executable, str(SCRIPT), str(self.root), "1.2.19"],
            capture_output=True,
            text=True,
            env=environ,
        )

    def test_a_good_tarball_passes(self) -> None:
        result = self.run_gate()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("[OK]", result.stdout)

    def test_a_tarball_without_the_profile_is_refused(self) -> None:
        self.write_tarball(profile=False)

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a tarball without the profile passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("embedded.provisionprofile", result.stderr)

    def test_a_tarball_without_the_keychain_pair_is_refused(self) -> None:
        self.groups = []

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a tarball without the pair passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("keychain-access-groups", result.stderr)

    def test_a_wrong_version_is_refused(self) -> None:
        self.write_signature("9.9.9")

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a signature for another version passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("version", result.stderr)

    def test_a_missing_tarball_is_refused(self) -> None:
        (self.bundle / "forge.app.tar.gz").unlink()

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a missing tarball passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("no updater tarball", result.stderr)

    def test_an_unsigned_app_without_the_pair_is_refused(self) -> None:
        self.team = "not set"
        self.groups = []

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "an unsigned app without the pair passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("Developer ID", result.stderr)

    def test_an_apple_double_member_is_refused(self) -> None:
        # macOS tar packs extended attributes as `._` members and writes them
        # first; the updater extracts every entry with its first path
        # component skipped, so a leading `._forge.app` unpacks onto the
        # temp dir itself and every desktop update fails.
        self.write_tarball(appledouble=True)

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "an AppleDouble tarball passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("AppleDouble", result.stderr)
        self.assertIn("._forge.app", result.stderr)

    def test_a_member_outside_the_app_is_refused(self) -> None:
        self.write_tarball(extra={"README.txt": b"outside"})

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a stray member passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("outside forge.app", result.stderr)

    def test_a_signature_from_another_key_is_refused(self) -> None:
        self.write_signature("1.2.19", key_id=b"HGFEDCBA")

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a signature from another key passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("not from the pinned key", result.stderr)

    def test_a_tarball_carrying_the_client_browser_is_refused(self) -> None:
        self.write_tarball(
            extra={
                "forge.app/Contents/Frameworks/Chromium Embedded Framework.framework/"
                "Chromium Embedded Framework": b"cef"
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a tarball carrying CEF passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("client-side browser", result.stderr)

    def test_the_extracted_app_is_the_one_codesigned(self) -> None:
        # The gate must read the signature of the app it extracted from the
        # tarball, not the build directory's copy: a stale build-dir app
        # carries the same shape and would pass every other check.
        result = self.run_gate()

        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.codesign_calls()
        self.assertTrue(calls, "the gate passed without reading the signature")
        build_app = str(self.bundle / "forge.app")
        targets = [call[-1] for call in calls]
        for target in targets:
            self.assertTrue(target.endswith("/forge.app"), f"codesign read {target}")
            self.assertNotEqual(target, build_app, "codesign read the build-dir app")
            self.assertFalse(
                target.startswith(str(self.bundle)),
                f"codesign read inside the build directory: {target}",
            )
        self.assertIn(["-d", "--verbose=4", targets[0]], calls)
        self.assertIn(["-d", "--entitlements", "-", "--xml", targets[0]], calls)

    def test_an_ad_hoc_app_with_the_pair_is_refused(self) -> None:
        # The dormant shape the gate exists to refuse: the keychain pair is
        # declared but the app is ad-hoc signed, so nothing can honour it.
        self.team = "not set"

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "an ad-hoc app with the pair passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("Developer ID", result.stderr)

    def test_a_signature_with_no_team_line_is_refused(self) -> None:
        # A verbose read that stops printing TeamIdentifier= is not an ad-hoc
        # signature; passing it would skip the profile requirement entirely.
        result = self.run_gate(CODESIGN_NO_TEAM="1")

        self.assertEqual(result.returncode, 1, "a signature with no team line passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("TeamIdentifier", result.stderr)


if __name__ == "__main__":
    unittest.main()

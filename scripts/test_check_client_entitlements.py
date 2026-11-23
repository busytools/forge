#!/usr/bin/env python3
"""Tests for `check_client_entitlements.py`, the read-back that keeps a client
bundle without the microphone entitlement or the keychain pair from shipping.
Run: python3 scripts/test_check_client_entitlements.py

`codesign` and `openssl` are macOS-only, so every case here puts stand-ins
first on PATH and pins the argv the gate hands them. What the real tools
return was checked by hand against the signed build and against an ad-hoc
bundle carrying the hardened runtime with no entitlements.

The stand-ins log each call, so a case can prove the gate actually reached
codesign and openssl rather than passing without reading anything.

Stdlib only, like the other script tests: python3 ships with the gate's
runners, macOS and ubuntu alike.
"""

import datetime
import os
import plistlib
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check_client_entitlements.py")
ENTITLEMENT = "com.apple.security.device.audio-input"
APPLICATION_IDENTIFIER = "com.apple.application-identifier"
KEYCHAIN_ACCESS_GROUPS = "keychain-access-groups"
APP_ID = "KALKD5L7NF.dev.vedhavyas.forge"
TEAM = "KALKD5L7NF"
USAGE = "NSMicrophoneUsageDescription"

# `CODESIGN_EXIT` makes the stand-in fail the way codesign does for an unsigned
# bundle; otherwise it prints whatever `CODESIGN_PLIST` holds for the
# entitlements read and `CODESIGN_VERBOSE` for the team read. The team read
# goes to stderr, where codesign writes its verbose output.
CODESIGN = """\
#!/bin/sh
for arg in "$@"; do
    printf '%s\\n' "$arg"
done >> "$CODESIGN_LOG"
if [ -n "${CODESIGN_EXIT:-}" ]; then
    printf '%s\\n' "$CODESIGN_STDERR" >&2
    exit "$CODESIGN_EXIT"
fi
case " $* " in
  *" --verbose=4 "*)
    printf '%s\\n' "$CODESIGN_VERBOSE" >&2
    ;;
  *)
    cat "$CODESIGN_PLIST"
    ;;
esac
"""

# The gate decodes an embedded profile with `openssl cms -verify`; the
# stand-in prints whatever `OPENSSL_PLIST` holds, so a case can supply the
# profile's plist without a real CMS blob. `OPENSSL_EXIT` makes it fail the
# way a corrupt blob does.
OPENSSL = """\
#!/bin/sh
for arg in "$@"; do
    printf '%s\\n' "$arg"
done >> "$OPENSSL_LOG"
if [ -n "${OPENSSL_EXIT:-}" ]; then
    printf '%s\\n' "${OPENSSL_STDERR:-openssl failed}" >&2
    exit "$OPENSSL_EXIT"
fi
cat "$OPENSSL_PLIST"
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
        openssl = self.bin / "openssl"
        openssl.write_text(OPENSSL)
        openssl.chmod(0o755)

        self.log = self.root / "codesign.log"
        self.openssl_log = self.root / "openssl.log"
        self.plist = self.root / "entitlements.plist"
        self.profile_plist = self.root / "profile.plist"
        self.write_entitlements(
            {
                ENTITLEMENT: True,
                APPLICATION_IDENTIFIER: APP_ID,
                KEYCHAIN_ACCESS_GROUPS: [APP_ID],
            }
        )
        # A valid Developer ID build by default: signed by the team and carrying
        # a matching profile. A case opts into the ad-hoc path with a "not set"
        # team, or removes the profile to exercise the refusal.
        self.verbose = f"TeamIdentifier={TEAM}"
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
            }
        )

    def write_info(self, entries: dict) -> None:
        with (self.bundle / "Contents" / "Info.plist").open("wb") as handle:
            plistlib.dump(entries, handle)

    def write_entitlements(self, entries: "dict | None") -> None:
        if entries is None:
            self.plist.write_bytes(b"")
            return
        with self.plist.open("wb") as handle:
            plistlib.dump(entries, handle)

    def write_profile_plist(self, entries: dict) -> None:
        with self.profile_plist.open("wb") as handle:
            plistlib.dump(entries, handle)

    def write_profile(self, entries: dict) -> None:
        """Embed a profile in the bundle and give the stand-in its plist."""
        (self.bundle / "Contents" / "embedded.provisionprofile").write_bytes(b"stand-in")
        self.write_profile_plist(entries)

    def remove_profile(self) -> None:
        """Take the embedded profile back out, the shape a stale build ships."""
        path = self.bundle / "Contents" / "embedded.provisionprofile"
        if path.exists():
            path.unlink()

    def codesign_calls(self) -> "list[str]":
        if not self.log.exists():
            return []
        return self.log.read_text().splitlines()

    def openssl_calls(self) -> "list[str]":
        if not self.openssl_log.exists():
            return []
        return self.openssl_log.read_text().splitlines()

    def run_gate(
        self, bundle: "Path | None" = None, **env: str
    ) -> "subprocess.CompletedProcess[str]":
        environ = {
            **os.environ,
            "PATH": f"{self.bin}{os.pathsep}{os.environ.get('PATH', '')}",
            "CODESIGN_LOG": str(self.log),
            "CODESIGN_PLIST": str(self.plist),
            "CODESIGN_VERBOSE": self.verbose,
            "OPENSSL_LOG": str(self.openssl_log),
            "OPENSSL_PLIST": str(self.profile_plist),
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
        for arg in ("-d", "--entitlements", "-", "--xml", str(self.bundle)):
            self.assertIn(arg, calls)

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

    def test_a_build_missing_the_keychain_entitlements_is_refused(self) -> None:
        # The audio entitlement is present and the keychain pair is not: the
        # shape of a build that would leave the biometric gate dormant.
        self.write_entitlements({ENTITLEMENT: True})

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a build without the keychain entitlements passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn(APPLICATION_IDENTIFIER, result.stderr)
        self.assertTrue(self.codesign_calls(), "the refusal did not come from a signature read")

    def test_a_matching_embedded_profile_passes(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.openssl_calls(), "the gate passed without decoding the profile")

    def test_a_developer_id_bundle_with_no_profile_is_refused(self) -> None:
        # The build the updater tarball shipped: keychain pair declared,
        # Developer ID signed, no profile. macOS refuses the keychain.
        self.remove_profile()

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a signed bundle with no profile passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("embedded.provisionprofile", result.stderr)
        self.assertTrue(self.codesign_calls(), "the refusal did not come from a signature read")

    def test_an_ad_hoc_bundle_with_no_profile_passes(self) -> None:
        # The dev path: no Developer ID team, so no profile is owed.
        self.remove_profile()
        self.verbose = "TeamIdentifier=not set"

        result = self.run_gate()

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_an_expired_profile_is_refused(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "ExpirationDate": datetime.datetime(2020, 1, 1),
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "an expired profile passed")
        self.assertIn("expired", result.stderr)

    def test_a_profile_with_the_wrong_team_is_refused(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": ["WRONGTEAM"],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a profile with the wrong team passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("embedded.provisionprofile", result.stderr)

    def test_a_profile_with_the_wrong_app_id_is_refused(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: "KALKD5L7NF.dev.other",
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a profile with the wrong app id passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("embedded.provisionprofile", result.stderr)

    def test_a_profile_whose_groups_do_not_authorise_the_app_id_is_refused(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: ["KALKD5L7NF.other"],
                },
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a narrow profile passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("do not authorise", result.stderr)

    def test_a_profile_with_no_groups_is_refused(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {APPLICATION_IDENTIFIER: APP_ID},
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a profile with no groups passed")
        self.assertIn(KEYCHAIN_ACCESS_GROUPS, result.stderr)

    def test_a_profile_whose_team_wildcard_authorises_the_app_id_passes(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [f"{TEAM}.*"],
                },
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_group_list_that_excludes_the_app_id_is_refused(self) -> None:
        self.write_entitlements(
            {
                ENTITLEMENT: True,
                APPLICATION_IDENTIFIER: APP_ID,
                KEYCHAIN_ACCESS_GROUPS: ["KALKD5L7NF.other"],
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a group list without the app id passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn(KEYCHAIN_ACCESS_GROUPS, result.stderr)

    def test_an_empty_group_list_is_refused(self) -> None:
        self.write_entitlements(
            {
                ENTITLEMENT: True,
                APPLICATION_IDENTIFIER: APP_ID,
                KEYCHAIN_ACCESS_GROUPS: [],
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "an empty group list passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn(KEYCHAIN_ACCESS_GROUPS, result.stderr)

    def test_an_empty_application_identifier_is_refused(self) -> None:
        self.write_entitlements(
            {
                ENTITLEMENT: True,
                APPLICATION_IDENTIFIER: "   ",
                KEYCHAIN_ACCESS_GROUPS: [APP_ID],
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a whitespace app id passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn(APPLICATION_IDENTIFIER, result.stderr)

    def test_an_openssl_failure_is_refused(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
            }
        )

        result = self.run_gate(
            OPENSSL_EXIT="1",
            OPENSSL_STDERR="cms: verification failure",
        )

        self.assertEqual(result.returncode, 1, "an undecodable profile passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("could not be decoded", result.stderr)
        self.assertTrue(self.openssl_calls(), "the refusal did not come from a profile read")

    def test_profile_bytes_that_are_not_a_plist_are_refused(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
            }
        )
        self.profile_plist.write_text("not a plist")

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "unreadable profile bytes passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("not a readable plist", result.stderr)

    def test_openssl_is_handed_the_cms_verify_argv(self) -> None:
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
            }
        )

        self.run_gate()

        calls = self.openssl_calls()
        self.assertTrue(calls, "the gate passed without decoding the profile")
        for arg in ("cms", "-verify", "-inform", "DER", "-noverify"):
            self.assertIn(arg, calls)

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

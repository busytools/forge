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
# goes to stderr, where codesign writes its verbose output. A
# `--extract-certificates=<prefix>` call writes `CODESIGN_CERT` to `<prefix>0`
# and prints an `Executable=` line, the way codesign does when it extracts the
# chain.
CODESIGN = """\
#!/bin/sh
printf '%s\\n' "---" >> "$CODESIGN_LOG"
for arg in "$@"; do
    printf '%s\\n' "$arg"
done >> "$CODESIGN_LOG"
if [ -n "${CODESIGN_EXIT:-}" ]; then
    printf '%s\\n' "$CODESIGN_STDERR" >&2
    exit "$CODESIGN_EXIT"
fi
case " $* " in
  *" --extract-certificates="*)
    prefix=""
    for arg in "$@"; do
        case "$arg" in
          --extract-certificates=*) prefix="${arg#--extract-certificates=}" ;;
        esac
    done
    cat "$CODESIGN_CERT" > "${prefix}0"
    printf '%s\\n' "Executable=$CODESIGN_EXECUTABLE"
    ;;
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
printf '%s\\n' "---" >> "$OPENSSL_LOG"
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
        self.cert = self.root / "cert.der"
        self.cert.write_bytes(b"stand-in certificate")
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "ExpirationDate": datetime.datetime(
                    2044, 11, 18, 7, 57, 50, tzinfo=datetime.timezone.utc
                ),
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
                "DeveloperCertificates": [self.cert.read_bytes()],
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
        """Embed a profile in the bundle and give the stand-in its plist.

        A valid ExpirationDate and the stand-in certificate are filled in
        unless the case supplies its own, so the expiry and certificate
        branches are exercised only by the cases that mean to.
        """
        entries = dict(entries)
        entries.setdefault(
            "ExpirationDate",
            datetime.datetime(2044, 11, 18, 7, 57, 50, tzinfo=datetime.timezone.utc),
        )
        entries.setdefault("DeveloperCertificates", [self.cert.read_bytes()])
        (self.bundle / "Contents" / "embedded.provisionprofile").write_bytes(b"stand-in")
        self.write_profile_plist(entries)

    def remove_profile(self) -> None:
        """Take the embedded profile back out, the shape a stale build ships."""
        path = self.bundle / "Contents" / "embedded.provisionprofile"
        if path.exists():
            path.unlink()

    def codesign_calls(self) -> "list[list[str]]":
        """Each codesign invocation's argv, split on the stand-in's marker."""
        if not self.log.exists():
            return []
        invocations = []
        current = []
        for line in self.log.read_text().splitlines():
            if line == "---":
                if current:
                    invocations.append(current)
                current = []
            else:
                current.append(line)
        if current:
            invocations.append(current)
        return invocations

    def openssl_calls(self) -> "list[list[str]]":
        """Each openssl invocation's argv, split on the stand-in's marker."""
        if not self.openssl_log.exists():
            return []
        invocations = []
        current = []
        for line in self.openssl_log.read_text().splitlines():
            if line == "---":
                if current:
                    invocations.append(current)
                current = []
            else:
                current.append(line)
        if current:
            invocations.append(current)
        return invocations

    def run_gate(
        self, bundle: "Path | None" = None, **env: str
    ) -> "subprocess.CompletedProcess[str]":
        environ = {
            **os.environ,
            "PATH": f"{self.bin}{os.pathsep}{os.environ.get('PATH', '')}",
            "CODESIGN_LOG": str(self.log),
            "CODESIGN_PLIST": str(self.plist),
            "CODESIGN_VERBOSE": self.verbose,
            "CODESIGN_CERT": str(self.cert),
            "CODESIGN_EXECUTABLE": str(self.bundle / "Contents" / "MacOS" / "forge-client"),
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
        self.assertIn(
            ["-d", "--entitlements", "-", "--xml", str(self.bundle)],
            calls,
            "the entitlements read did not pass the exact argv",
        )

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

    def test_an_ad_hoc_bundle_with_no_profile_is_refused(self) -> None:
        # The dormant shape the gate exists to refuse: the keychain pair is
        # declared, the bundle is ad-hoc signed and no profile is embedded.
        # Nothing can honour the pair, so the build cannot ship.
        self.remove_profile()
        self.verbose = "TeamIdentifier=not set"

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "an ad-hoc bundle with the pair passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("Developer ID", result.stderr)

    def test_a_signature_with_no_team_line_is_refused(self) -> None:
        # A verbose read that stops printing TeamIdentifier= is not an ad-hoc
        # signature; reading it as one would skip the profile requirement.
        self.verbose = "Identifier=dev.vedhavyas.forge"

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a signature with no team line passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("TeamIdentifier", result.stderr)

    def test_a_profile_embedded_but_not_signed_is_refused(self) -> None:
        # A profile is present but the bundle is ad-hoc signed: the profile
        # cannot authorise the keychain pair, so the build is refused.
        self.verbose = "TeamIdentifier=not set"

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "an ad-hoc build with a profile passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("not Developer ID-signed", result.stderr)

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

    def test_a_non_date_expiration_is_refused(self) -> None:
        # A string expiry is not a date the gate can read, so it fails closed.
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "ExpirationDate": "2020-01-01",
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a non-date expiry passed")
        self.assertIn("ExpirationDate", result.stderr)

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

    def test_an_application_identifier_for_another_team_is_refused(self) -> None:
        # The signature's application-identifier names a team the certificate
        # does not, so the pair cannot be honoured even though the profile
        # matches its own team.
        self.write_entitlements(
            {
                ENTITLEMENT: True,
                APPLICATION_IDENTIFIER: "OTHERTEAM.dev.vedhavyas.forge",
                KEYCHAIN_ACCESS_GROUPS: ["OTHERTEAM.dev.vedhavyas.forge"],
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "an application id for another team passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("names team", result.stderr)

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

    def test_a_profile_without_developer_certificates_is_refused(self) -> None:
        # The docstring and the release notes require the profile to pin the
        # signing certificate; a profile that carries none cannot be checked
        # against the signature, so it is refused rather than skipped.
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
                "DeveloperCertificates": [],
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a profile with no certificates passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("DeveloperCertificates", result.stderr)

    def test_a_profile_minted_for_another_certificate_is_refused(self) -> None:
        # The team and app id match, but the profile's DeveloperCertificates
        # does not carry the certificate the signature was made with.
        self.write_profile(
            {
                "TeamIdentifier": [TEAM],
                "Entitlements": {
                    APPLICATION_IDENTIFIER: APP_ID,
                    KEYCHAIN_ACCESS_GROUPS: [APP_ID],
                },
                "DeveloperCertificates": [b"a different certificate"],
            }
        )

        result = self.run_gate()

        self.assertEqual(result.returncode, 1, "a profile for another certificate passed")
        self.assertIn("[ERROR]", result.stderr)
        self.assertIn("does not carry the signing certificate", result.stderr)

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
        self.assertIn(f"embedded.provisionprofile carries no {KEYCHAIN_ACCESS_GROUPS}", result.stderr)

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
        self.assertIn(f"{KEYCHAIN_ACCESS_GROUPS} is absent", result.stderr)

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

        profile = self.bundle / "Contents" / "embedded.provisionprofile"
        self.assertIn(
            ["cms", "-verify", "-inform", "DER", "-in", str(profile), "-noverify"],
            self.openssl_calls(),
            "the profile decode did not pass the exact argv and profile path",
        )

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

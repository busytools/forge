#!/usr/bin/env python3
"""Tests for `notarize_client.sh`, the notarise-and-staple step `client-release`
runs on the final signed client bundle.

Run: python3 scripts/test_notarize_client.py

The notary service is Apple's, so every case here puts stand-ins for `xcrun`,
`codesign`, `openssl`, `ditto` and `spctl` first on PATH. The stand-ins pin the
argv the script hands them, flags included: a stand-in that accepted any
arguments would let a malformed real command pass, and the tests would report
a release path that only works against the fake. The stand-ins log each call,
so a case proves the script reached the tool it claims to.

Stdlib only, like the other script tests: python3 ships with the gate's
runners, macOS and ubuntu alike.
"""

import json
import os
import plistlib
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

SCRIPT = Path(__file__).with_name("notarize_client.sh")
TEAM = "KALKD5L7NF"
ACCEPTED = json.dumps({"id": "sub-1", "status": "Accepted"})
REJECTED = json.dumps({"id": "sub-9", "status": "Invalid"})

FAKES = {
    "xcrun": """#!/usr/bin/env python3
import json, os, sys, zipfile
args = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write("xcrun " + " ".join(args) + "\\n")
if args[:2] == ["notarytool", "submit"]:
    seen = {"path": args[2] if len(args) > 2 else "", "names": []}
    if seen["path"] and os.path.exists(seen["path"]):
        seen["names"] = zipfile.ZipFile(seen["path"]).namelist()
    with open(os.environ["ZIP_SEEN"], "w") as out:
        json.dump(seen, out)
    sys.stdout.write(os.environ["SUBMIT_JSON"] + "\\n")
    sys.exit(int(os.environ.get("SUBMIT_EXIT", "0")))
if args[:2] == ["notarytool", "log"]:
    sys.stdout.write(os.environ.get("LOG_OUTPUT", "{}") + "\\n")
    sys.exit(int(os.environ.get("LOG_EXIT", "0")))
if args[:2] == ["stapler", "staple"]:
    sys.exit(int(os.environ.get("STAPLE_EXIT", "0")))
if args[:2] == ["stapler", "validate"]:
    sys.exit(int(os.environ.get("VALIDATE_EXIT", "0")))
sys.stderr.write("unexpected xcrun argv: " + " ".join(args) + "\\n")
sys.exit(9)
""",
    "spctl": """#!/usr/bin/env python3
import os, sys
with open(os.environ["CALLS"], "a") as log:
    log.write("spctl " + " ".join(sys.argv[1:]) + "\\n")
sys.exit(int(os.environ.get("SPCTL_EXIT", "0")))
""",
    "codesign": """#!/usr/bin/env python3
import os, sys
with open(os.environ["CALLS"], "a") as log:
    log.write("codesign " + " ".join(sys.argv[1:]) + "\\n")
sys.stderr.write("TeamIdentifier=" + os.environ.get("FAKE_TEAM_ID", "KALKD5L7NF") + "\\n")
sys.exit(int(os.environ.get("CODESIGN_EXIT", "0")))
""",
    "openssl": """#!/usr/bin/env python3
import os, sys
with open(os.environ["CALLS"], "a") as log:
    log.write("openssl " + " ".join(sys.argv[1:]) + "\\n")
sys.stdout.write(os.environ["PROFILE_PLIST"])
sys.exit(0)
""",
    "ditto": """#!/usr/bin/env python3
import os, sys, zipfile
from pathlib import Path
args = sys.argv[1:]
with open(os.environ["CALLS"], "a") as log:
    log.write("ditto " + " ".join(args) + "\\n")
code = int(os.environ.get("DITTO_EXIT", "0"))
if code:
    sys.exit(code)
if args[:3] != ["-c", "-k", "--keepParent"] or len(args) != 5:
    sys.stderr.write("ditto argv does not pin the archive flags: " + " ".join(args) + "\\n")
    sys.exit(9)
src, dest = Path(args[-2]), Path(args[-1])
with zipfile.ZipFile(dest, "w") as archive:
    for path in sorted(src.rglob("*")):
        archive.write(path, path.relative_to(src.parent))
""",
}


class NotarizeClientTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        for name, body in FAKES.items():
            tool = self.bin / name
            tool.write_text(body)
            tool.chmod(0o755)
        self.calls = self.tmp / "calls"
        self.zip_seen = self.tmp / "zip-seen"
        self.home = self.tmp / "home"
        self.home.mkdir()
        self.app = self.tmp / "forge.app"
        (self.app / "Contents").mkdir(parents=True)
        (self.app / "Contents" / "Info.plist").write_text("plist")

    def env(self, **overrides):
        env = {
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "HOME": str(self.home),
            "CALLS": str(self.calls),
            "ZIP_SEEN": str(self.zip_seen),
            "SUBMIT_JSON": ACCEPTED,
        }
        env.update(overrides)
        return env

    def run_script(self, *args, **env):
        return subprocess.run(
            [str(SCRIPT), *args], capture_output=True, text=True, env=self.env(**env)
        )

    def call_lines(self):
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def calls_text(self):
        return "\n".join(self.call_lines())

    def submit_line(self):
        return next(
            line
            for line in self.call_lines()
            if line.startswith("xcrun notarytool submit ")
        )

    def write_properties(self, **values):
        path = self.home / ".tauri" / "forge-notary.properties"
        path.parent.mkdir(exist_ok=True)
        path.write_text("".join(f"{key}={value}\n" for key, value in values.items()))

    def test_accepted_submission_is_stapled_and_read_back(self):
        result = self.run_script(
            str(self.app),
            APPLE_ID="hi@example.com",
            APPLE_PASSWORD="app-pw",
            APPLE_TEAM_ID=TEAM,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("[OK] notarisation submission sub-1 accepted", result.stdout)
        self.assertIn('"status": "Accepted"', result.stdout)
        self.assertIn("[OK] notarised and stapled forge.app", result.stdout)

        seen = json.loads(self.zip_seen.read_text())
        zip_path = seen["path"]
        self.assertEqual(
            self.submit_line(),
            f"xcrun notarytool submit {zip_path} --apple-id hi@example.com "
            f"--password app-pw --team-id {TEAM} --wait --output-format json",
        )
        calls = self.call_lines()
        self.assertIn(f"ditto -c -k --keepParent {self.app} {zip_path}", calls)
        self.assertIn(f"xcrun stapler staple {self.app}", calls)
        self.assertIn(f"xcrun stapler validate {self.app}", calls)
        self.assertIn(f"spctl --assess --type execute --verbose=4 {self.app}", calls)
        self.assertLess(
            next(i for i, line in enumerate(calls) if "notarytool submit" in line),
            next(i for i, line in enumerate(calls) if "stapler staple" in line),
        )
        self.assertIn("forge.app/Contents/Info.plist", seen["names"])
        self.assertFalse(
            Path(zip_path).exists(), "the submission archive must be cleaned up"
        )

    def test_credential_file_is_read_when_the_environment_is_empty(self):
        self.write_properties(
            APPLE_ID="file@example.com", APPLE_PASSWORD="file-pw", APPLE_TEAM_ID=TEAM
        )

        result = self.run_script(str(self.app))

        self.assertEqual(result.returncode, 0, result.stderr)
        submit = self.submit_line()
        self.assertIn("--apple-id file@example.com", submit)
        self.assertIn("--password file-pw", submit)

    def test_environment_wins_over_the_credential_file(self):
        self.write_properties(
            APPLE_ID="file@example.com", APPLE_PASSWORD="file-pw", APPLE_TEAM_ID=TEAM
        )

        result = self.run_script(
            str(self.app),
            APPLE_ID="env@example.com",
            APPLE_PASSWORD="env-pw",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        submit = self.submit_line()
        self.assertIn("--apple-id env@example.com", submit)
        self.assertIn("--password env-pw", submit)

    def test_team_id_falls_back_to_the_signed_bundle(self):
        result = self.run_script(
            str(self.app), APPLE_ID="hi@example.com", APPLE_PASSWORD="app-pw"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any(line.startswith("codesign -dvvv") for line in self.call_lines()))
        self.assertIn(f"--team-id {TEAM}", self.submit_line())

    def test_team_id_in_the_file_wins_over_the_signature(self):
        self.write_properties(
            APPLE_ID="hi@example.com", APPLE_PASSWORD="app-pw", APPLE_TEAM_ID="FILETEAM"
        )

        result = self.run_script(str(self.app), FAKE_TEAM_ID="SIGTEAM")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--team-id FILETEAM", self.submit_line())
        self.assertNotIn("codesign", self.calls_text())

    def test_rejected_submission_fetches_the_log_and_stops(self):
        result = self.run_script(
            str(self.app),
            APPLE_ID="hi@example.com",
            APPLE_PASSWORD="app-pw",
            APPLE_TEAM_ID=TEAM,
            SUBMIT_JSON=REJECTED,
            LOG_OUTPUT='{"issues":[{"message":"bad entitlement"}]}',
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("Invalid", result.stderr)
        self.assertIn("sub-9", result.stderr)
        self.assertIn("bad entitlement", result.stderr)
        self.assertIn(
            f"xcrun notarytool log sub-9 --apple-id hi@example.com "
            f"--password app-pw --team-id {TEAM}",
            self.call_lines(),
        )
        self.assertNotIn("stapler", self.calls_text())
        self.assertNotIn("spctl", self.calls_text())
        self.assertNotIn("[OK]", result.stdout)

    def test_log_failure_still_reports_the_rejection(self):
        result = self.run_script(
            str(self.app),
            APPLE_ID="hi@example.com",
            APPLE_PASSWORD="app-pw",
            APPLE_TEAM_ID=TEAM,
            SUBMIT_JSON=REJECTED,
            LOG_EXIT="1",
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("Invalid", result.stderr)
        self.assertNotIn("stapler", self.calls_text())

    def test_submit_failure_stops_before_stapling(self):
        result = self.run_script(
            str(self.app),
            APPLE_ID="hi@example.com",
            APPLE_PASSWORD="app-pw",
            APPLE_TEAM_ID=TEAM,
            SUBMIT_EXIT="1",
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("[ERROR] notarytool could not submit forge.app", result.stderr)
        self.assertNotIn("stapler", self.calls_text())

    def test_ditto_failure_stops_before_submission(self):
        result = self.run_script(
            str(self.app),
            APPLE_ID="hi@example.com",
            APPLE_PASSWORD="app-pw",
            APPLE_TEAM_ID=TEAM,
            DITTO_EXIT="7",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("notarytool submit", self.calls_text())

    def test_staple_failure_stops_before_gatekeeper(self):
        result = self.run_script(
            str(self.app),
            APPLE_ID="hi@example.com",
            APPLE_PASSWORD="app-pw",
            APPLE_TEAM_ID=TEAM,
            STAPLE_EXIT="1",
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("could not be stapled", result.stderr)
        self.assertNotIn("spctl", self.calls_text())
        self.assertNotIn("[OK] notarised and stapled", result.stdout)

    def test_validate_failure_stops_before_gatekeeper(self):
        result = self.run_script(
            str(self.app),
            APPLE_ID="hi@example.com",
            APPLE_PASSWORD="app-pw",
            APPLE_TEAM_ID=TEAM,
            VALIDATE_EXIT="1",
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("does not validate", result.stderr)
        self.assertNotIn("spctl", self.calls_text())
        self.assertNotIn("[OK] notarised and stapled", result.stdout)

    def test_gatekeeper_rejection_is_a_failure(self):
        result = self.run_script(
            str(self.app),
            APPLE_ID="hi@example.com",
            APPLE_PASSWORD="app-pw",
            APPLE_TEAM_ID=TEAM,
            SPCTL_EXIT="3",
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("Gatekeeper does not accept", result.stderr)
        self.assertNotIn("[OK] notarised and stapled", result.stdout)

    def test_missing_credential_names_every_missing_key(self):
        result = self.run_script(
            str(self.app), APPLE_ID="hi@example.com", FAKE_TEAM_ID=""
        )

        self.assertEqual(result.returncode, 1)
        self.assertIn("APPLE_PASSWORD", result.stderr)
        self.assertIn("APPLE_TEAM_ID", result.stderr)

    def test_check_mode_reads_the_team_id_from_the_profile(self):
        profile = self.tmp / "forge.provisionprofile"
        profile.write_bytes(b"profile")
        plist = plistlib.dumps({"TeamIdentifier": [TEAM]}).decode()

        result = self.run_script(
            "--check",
            str(profile),
            APPLE_ID="hi@example.com",
            APPLE_PASSWORD="app-pw",
            PROFILE_PLIST=plist,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"(team {TEAM})", result.stdout)
        self.assertNotIn("xcrun", self.calls_text())
        self.assertNotIn("codesign", self.calls_text())
        self.assertNotIn("app-pw", result.stdout)
        self.assertNotIn("app-pw", result.stderr)

    def test_check_mode_reads_the_credential_file(self):
        self.write_properties(
            APPLE_ID="file@example.com", APPLE_PASSWORD="file-pw", APPLE_TEAM_ID=TEAM
        )

        result = self.run_script("--check")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("for file@example.com (team KALKD5L7NF)", result.stdout)
        self.assertNotIn("file-pw", result.stdout)
        self.assertNotIn("file-pw", result.stderr)

    def test_check_mode_names_a_missing_credential(self):
        result = self.run_script("--check")

        self.assertEqual(result.returncode, 1)
        for key in ("APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID"):
            self.assertIn(key, result.stderr)
        self.assertEqual(self.call_lines(), [])

    def test_usage_without_an_app(self):
        result = self.run_script()

        self.assertEqual(result.returncode, 2)
        self.assertIn("usage:", result.stderr)

    def test_missing_app_bundle_is_refused(self):
        result = self.run_script(str(self.tmp / "absent.app"))

        self.assertEqual(result.returncode, 1)
        self.assertIn("no app bundle", result.stderr)


if __name__ == "__main__":
    unittest.main()

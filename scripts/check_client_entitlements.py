#!/usr/bin/env python3
"""Read a built macOS client bundle's entitlements and microphone usage string
back off the signature.

Usage: check_client_entitlements.py <path/to/forge.app>

Under the hardened runtime macOS blocks audio input unless the app declares
`com.apple.security.device.audio-input`, and the block is silent: the device
opens, the client delivers, and every take lands with no audio. Without
`NSMicrophoneUsageDescription` in the Info.plist TCC refuses the microphone
before any prompt. Both are read off the bundle the build produced rather than
trusted from `tauri.conf.json`, so a signature made without the entitlements
file cannot ship. `client-release` runs this after the bundle is built and
signed, before anything is installed.

The same signature is read for the keychain pair the data-protection keychain
needs: `com.apple.application-identifier` and `keychain-access-groups`. When a
provisioning profile is embedded at `Contents/embedded.provisionprofile`, its
team and application identifier are read back and must match the signature, so
a build carrying the wrong profile cannot ship quietly.

Prints `[OK]` and exits 0 when everything is present; prints `[ERROR]` naming
what is missing and exits 1 otherwise. `codesign` prints nothing for a signed
bundle that declares no entitlements, so an empty read is a failure, not a
pass.
"""

import plistlib
import subprocess
import sys
from pathlib import Path

ENTITLEMENT = "com.apple.security.device.audio-input"
APPLICATION_IDENTIFIER = "com.apple.application-identifier"
KEYCHAIN_ACCESS_GROUPS = "keychain-access-groups"
USAGE = "NSMicrophoneUsageDescription"
PROFILE = "embedded.provisionprofile"


def fail(message: str) -> None:
    print(f"[ERROR] {message}", file=sys.stderr)
    sys.exit(1)


def check_usage_string(bundle: Path) -> None:
    """Refuse a bundle whose Info.plist carries no microphone usage string."""
    info = bundle / "Contents" / "Info.plist"
    try:
        with info.open("rb") as handle:
            plist = plistlib.load(handle)
    except OSError as err:
        fail(f"{info} could not be read: {err}")
    except Exception as err:
        fail(f"{info} is not a readable plist: {err}")
    value = plist.get(USAGE) if isinstance(plist, dict) else None
    if not isinstance(value, str) or not value.strip():
        fail(f"{info} carries no {USAGE}; macOS refuses the microphone without one")


def read_entitlements(bundle: Path) -> dict:
    """The entitlements the signature declares, read back with codesign."""
    try:
        result = subprocess.run(
            ["codesign", "-d", "--entitlements", "-", "--xml", str(bundle)],
            capture_output=True,
        )
    except OSError as err:
        fail(f"codesign could not be run: {err}")

    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        fail(f"{bundle} is not signed, so its entitlements cannot be read: {detail}")
    # A signed bundle with no entitlements reads back empty and still exits 0,
    # which is exactly the shape v1.2.18 shipped.
    if not result.stdout.strip():
        fail(f"{bundle} declares no entitlements, so {ENTITLEMENT} is absent")

    try:
        plist = plistlib.loads(result.stdout)
    except Exception as err:
        fail(f"the entitlements codesign printed for {bundle} do not parse: {err}")
    if not isinstance(plist, dict):
        fail(f"the entitlements codesign printed for {bundle} are not a dictionary")
    return plist


def read_profile(bundle: Path) -> "dict | None":
    """The embedded provisioning profile's plist, or None when none is embedded."""
    path = bundle / "Contents" / PROFILE
    if not path.exists():
        return None
    try:
        result = subprocess.run(
            ["openssl", "cms", "-verify", "-inform", "DER", "-in", str(path), "-noverify"],
            capture_output=True,
        )
    except OSError as err:
        fail(f"openssl could not be run to read {path}: {err}")
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        fail(f"{path} could not be decoded: {detail}")
    try:
        plist = plistlib.loads(result.stdout)
    except Exception as err:
        fail(f"{path} is not a readable plist: {err}")
    if not isinstance(plist, dict):
        fail(f"{path} is not a dictionary")
    return plist


def check_keychain_entitlements(declared: dict) -> str:
    """Refuse a signature without the keychain pair, returning its application id."""
    app_id = declared.get(APPLICATION_IDENTIFIER)
    if not isinstance(app_id, str) or not app_id.strip():
        fail(f"{APPLICATION_IDENTIFIER} is absent, so the keychain gate cannot work")
    groups = declared.get(KEYCHAIN_ACCESS_GROUPS)
    if not isinstance(groups, list) or not groups:
        fail(f"{KEYCHAIN_ACCESS_GROUPS} is absent, so the keychain gate cannot work")
    if app_id not in groups:
        fail(f"{KEYCHAIN_ACCESS_GROUPS} does not carry {app_id}")
    return app_id


def check_profile(bundle: Path, app_id: str) -> None:
    """Refuse an embedded profile whose team or application id does not match."""
    profile = read_profile(bundle)
    if profile is None:
        return
    team = app_id.split(".", 1)[0]
    teams = profile.get("TeamIdentifier")
    if not isinstance(teams, list) or team not in teams:
        fail(f"{PROFILE} names team {teams!r}, not {team!r}")
    entitlements = profile.get("Entitlements")
    profile_app_id = (
        entitlements.get(APPLICATION_IDENTIFIER) if isinstance(entitlements, dict) else None
    )
    if profile_app_id != app_id:
        fail(f"{PROFILE} carries application identifier {profile_app_id!r}, not {app_id!r}")


def main() -> None:
    if len(sys.argv) != 2:
        fail("usage: check_client_entitlements.py <path/to/forge.app>")
    bundle = Path(sys.argv[1])
    if not bundle.is_dir():
        fail(f"no app bundle at {bundle}")

    check_usage_string(bundle)
    declared = read_entitlements(bundle)
    value = declared.get(ENTITLEMENT)
    if value is not True:
        state = "absent" if ENTITLEMENT not in declared else f"declared {value!r}"
        fail(f"{bundle}: {ENTITLEMENT} is {state}, expected true")

    app_id = check_keychain_entitlements(declared)
    check_profile(bundle, app_id)

    print(f"[OK] {bundle.name} declares {ENTITLEMENT} and the keychain entitlements")


if __name__ == "__main__":
    main()

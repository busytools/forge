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

Prints `[OK]` and exits 0 when both are present; prints `[ERROR]` naming what
is missing and exits 1 otherwise. `codesign` prints nothing for a signed bundle
that declares no entitlements, so an empty read is a failure, not a pass.
"""

import plistlib
import subprocess
import sys
from pathlib import Path

ENTITLEMENT = "com.apple.security.device.audio-input"
USAGE = "NSMicrophoneUsageDescription"


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

    print(f"[OK] {bundle.name} declares {ENTITLEMENT} and its microphone usage string")


if __name__ == "__main__":
    main()

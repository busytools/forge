#!/usr/bin/env python3
"""Check a built update tarball's signature against the pinned public key.

Usage: check_updater_signature.py <repo-root> <version>

The Tauri CLI signs with whatever key `TAURI_SIGNING_PRIVATE_KEY` names, and
only WARNS when that key is not the one `tauri.conf.json` pins - a mismatched
key signs happily, the recipe would print its OK line, and every installed
desktop would then refuse the artifact at install time (fail-closed, so the
release is dead on arrival rather than dangerous). The pair is read back and
compared here instead, the way `client-android-release` reads the built APK's
signer back against its keystore.

Reads the pinned `pubkey` from `<root>/client/src-tauri/tauri.conf.json` and
the signature from the app bundle's `forge.app.tar.gz.sig`. Both are base64
of a minisign text file whose second line decodes to a blob carrying the
eight-byte key id at offset 2; the signature's trusted comment carries the
version the artifact was signed for.
"""

import base64
import json
import os
import plistlib
import re
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path


def fail(message: str) -> None:
    print(f"[ERROR] {message}", file=sys.stderr)
    sys.exit(1)


def lines_of(base64_text: str, what: str) -> list[str]:
    """The minisign text behind a base64 blob, as its lines."""
    try:
        return base64.b64decode(base64_text).decode().splitlines()
    except (ValueError, UnicodeDecodeError) as err:
        fail(f"{what} is not base64 minisign text: {err}")


def key_id(lines: list[str], what: str) -> bytes:
    """The eight key-id bytes every minisign blob carries at offset 2."""
    if len(lines) < 2:
        fail(f"{what} carries no minisign blob")
    try:
        blob = base64.b64decode(lines[1])
    except ValueError as err:
        fail(f"{what}'s blob does not decode: {err}")
    if len(blob) < 10:
        fail(f"{what}'s blob is too short to carry a key id")
    return blob[2:10]


def extract_safely(archive: tarfile.TarFile, target: str) -> None:
    """Extract every member, refusing one that would land outside the target."""
    root = Path(target).resolve()
    for member in archive.getmembers():
        member_path = (root / member.name).resolve()
        if not str(member_path).startswith(str(root) + os.sep):
            fail(f"the updater tarball carries a path outside the archive: {member.name}")
        archive.extract(member, target)


def read_team_identifier(app: Path) -> str:
    """The TeamIdentifier the extracted app's signature carries, or empty."""
    try:
        result = subprocess.run(
            ["codesign", "-d", "--verbose=4", str(app)],
            capture_output=True,
        )
    except OSError as err:
        fail(f"codesign could not be run on the extracted app: {err}")
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        fail(f"the extracted app's signature could not be read: {detail}")
    for line in result.stderr.decode(errors="replace").splitlines():
        if line.startswith("TeamIdentifier="):
            team = line.split("=", 1)[1].strip()
            return "" if team == "not set" else team
    return ""


def check_extracted_app(tarball: Path, names: set[str]) -> None:
    """Extract the tarball and require the profile and keychain pair when signed.

    The tarball's app must be the re-signed one: the recipe embeds the profile
    and signs the app again before the tarball is regenerated. The keychain
    pair is on both the pre-embed and the re-signed build, so the pair alone
    does not prove the tarball is the re-signed one. The embedded profile is
    the discriminator: require it whenever the extracted app is Developer ID
    signed. The pair read is a fail-closed sanity check on top of that.
    """
    with tempfile.TemporaryDirectory() as scratch:
        try:
            with tarfile.open(tarball, "r:gz") as archive:
                extract_safely(archive, scratch)
        except (OSError, tarfile.TarError) as err:
            fail(f"the updater tarball could not be extracted: {err}")
        app = Path(scratch) / "forge.app"
        team = read_team_identifier(app)
        if team:
            profile = "forge.app/Contents/embedded.provisionprofile"
            if profile not in names:
                fail(
                    "the extracted app is Developer ID signed but the update "
                    f"tarball carries no profile ({profile} is missing)"
                )
        try:
            result = subprocess.run(
                ["codesign", "-d", "--entitlements", "-", "--xml", str(app)],
                capture_output=True,
            )
        except OSError as err:
            fail(f"codesign could not be run on the extracted app: {err}")
        if result.returncode != 0:
            detail = result.stderr.decode(errors="replace").strip()
            fail(f"the extracted app's entitlements could not be read: {detail}")
        try:
            entitlements = plistlib.loads(result.stdout)
        except Exception as err:
            fail(f"the extracted app's entitlements do not parse: {err}")
        app_id = entitlements.get("com.apple.application-identifier")
        groups = entitlements.get("keychain-access-groups")
        if not isinstance(app_id, str) or not app_id.strip():
            fail("the extracted app declares no com.apple.application-identifier")
        if not isinstance(groups, list) or app_id not in groups:
            fail(
                "the extracted app's keychain-access-groups does not carry its "
                "application identifier"
            )


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: check_updater_signature.py <repo-root> <version>")
    root = Path(sys.argv[1])
    version = sys.argv[2]

    conf_path = root / "client/src-tauri/tauri.conf.json"
    try:
        conf = json.loads(conf_path.read_text())
        pinned_text = conf["plugins"]["updater"]["pubkey"]
    except OSError as err:
        fail(f"{conf_path} could not be read: {err}")
    except (json.JSONDecodeError, KeyError, TypeError):
        fail(f"{conf_path} carries no plugins.updater.pubkey")

    bundle = root / "client/src-tauri/target/release/bundle/macos"
    tarball = bundle / "forge.app.tar.gz"
    signature = bundle / "forge.app.tar.gz.sig"
    if not tarball.exists():
        fail(f"no updater tarball at {tarball} - the build did not produce one")
    if not signature.exists():
        fail(f"no signature at {signature} - the tarball was not signed")

    try:
        sig_text = signature.read_text()
    except OSError as err:
        fail(f"{signature} could not be read: {err}")

    pinned_id = key_id(lines_of(pinned_text, "the pinned pubkey"), "the pinned pubkey")
    sig_lines = lines_of(sig_text, "the update signature")
    signed_id = key_id(sig_lines, "the update signature")
    if signed_id != pinned_id:
        fail(
            "the update signature is not from the pinned key "
            f"({signed_id.hex()} signed, {pinned_id.hex()} pinned) - "
            "every installed client would refuse this artifact"
        )

    comment = sig_lines[2] if len(sig_lines) > 2 else ""
    match = re.search(r"version:(\S+)", comment)
    signed_version = match.group(1) if match else None
    if signed_version != version:
        fail(f"the signature is for version {signed_version or 'unknown'}, expected {version}")

    # **The tarball must carry the app and must NOT carry the client-side
    # browser.** The browser moved to the server: a client bundle that still
    # ships CEF and its helpers is the old 480 MiB shape, and an update that
    # installs it would waste the space and shadow the server's own host.
    try:
        with tarfile.open(tarball, "r:gz") as archive:
            names = set(archive.getnames())
    except (OSError, tarfile.TarError) as err:
        fail(f"the updater tarball could not be read: {err}")

    app_binary = "forge.app/Contents/MacOS/forge-client"
    if app_binary not in names:
        fail(f"the update tarball carries no client binary ({app_binary} is missing)")

    # **The tarball's app must be the re-signed one.** The release embeds the
    # profile in the app bundle and signs the app again, but tauri's build
    # creates the tarball before that. A stale tarball ships a gate-dormant
    # app through the desktop updater while a fresh install works. Extract
    # unconditionally: a Developer ID signed app must carry the profile, and
    # the keychain pair must be on its signature. The pair is on both the
    # pre-embed and the re-signed build, so the profile is the discriminator.
    check_extracted_app(tarball, names)

    forbidden = [
        "forge.app/Contents/Frameworks/Chromium Embedded Framework.framework/"
        "Chromium Embedded Framework",
        "forge.app/Contents/Frameworks/forge-client Helper.app/Contents/MacOS/"
        "forge-client Helper",
        "forge.app/Contents/Frameworks/forge-client Helper (GPU).app/Contents/MacOS/"
        "forge-client Helper (GPU)",
        "forge.app/Contents/Frameworks/forge-client Helper (Renderer).app/Contents/MacOS/"
        "forge-client Helper (Renderer)",
        "forge.app/Contents/Frameworks/forge-client Helper (Plugin).app/Contents/MacOS/"
        "forge-client Helper (Plugin)",
        "forge.app/Contents/Frameworks/forge-client Helper (Alerts).app/Contents/MacOS/"
        "forge-client Helper (Alerts)",
    ]
    present = [name for name in forbidden if name in names]
    if present:
        fail(
            "the update tarball still carries the client-side browser, which runs "
            f"on the server now; found: {', '.join(present)}"
        )

    print(
        f"[OK] the update tarball is signed by the pinned key, for version "
        f"{signed_version}, and carries the app without the client-side browser"
    )


if __name__ == "__main__":
    main()

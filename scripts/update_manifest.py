#!/usr/bin/env python3
"""Write the update manifest a release publishes.

Usage: update_manifest.py <repo-root> <version>

The one file all three halves read: the desktop updater (Tauri's static
manifest - a `platforms` entry carries the `.sig`'s content with its trailing
newline trimmed, and the asset URL), the phone (a top-level `android` block),
and the web image's puller (a top-level `web` block whose sha256 is both its
integrity check and its same-version skip).

The trim is load-bearing: the plugin base64-decodes the manifest's value
whole, so a trailing newline is an InvalidByte and every desktop install
fails.

The `android` and `web` blocks sit BESIDE `platforms` rather than inside it:
every `platforms` entry must carry both `url` and `signature` or the whole
file fails to parse, and neither of those halves has a minisign signature.
Tauri's parser ignores the unknown top-level keys.

Every artifact is required to exist in the staged bundle directory - this
runs at release time, where a manifest naming an asset that was never built
is a release no client can update from.
"""

import hashlib
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

REPOSITORY = "https://github.com/busytools/forge"


def fail(message: str) -> None:
    print(f"[ERROR] {message}", file=sys.stderr)
    sys.exit(1)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> None:
    if len(sys.argv) != 3:
        fail("usage: update_manifest.py <repo-root> <version>")
    root = Path(sys.argv[1])
    version = sys.argv[2]

    bundle = root / "client/src-tauri/target/release/bundle"
    tarball = bundle / "macos/forge.app.tar.gz"
    signature = bundle / "macos/forge.app.tar.gz.sig"
    apk = bundle / f"android/forge-{version}-arm64.apk"
    web = bundle / f"forge-web-{version}.tar.gz"
    for path in (tarball, signature, apk, web):
        if not path.exists():
            fail(f"the manifest names {path.name}, but nothing staged it at {path}")

    release = f"{REPOSITORY}/releases/download/v{version}"
    try:
        sig = signature.read_text().rstrip()
    except OSError as err:
        fail(f"{signature} could not be read: {err}")

    manifest = {
        "version": version,
        "pub_date": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "platforms": {
            "darwin-aarch64": {
                "signature": sig,
                "url": f"{release}/forge.app.tar.gz",
            }
        },
        "android": {
            "url": f"{release}/forge-{version}-arm64.apk",
        },
        "web": {
            "version": version,
            "url": f"{release}/forge-web-{version}.tar.gz",
            "sha256": sha256(web),
        },
    }

    out = bundle / "latest.json"
    out.write_text(json.dumps(manifest, indent=2) + "\n")
    # What it wrote, read back off the manifest itself: a line spelling a
    # fixed list claims blocks a mutation could have dropped.
    print(f"[OK] wrote {out} - version {version}, keys {' '.join(sorted(manifest))}")


if __name__ == "__main__":
    main()

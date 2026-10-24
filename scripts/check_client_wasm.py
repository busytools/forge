#!/usr/bin/env python3
"""The client build carries the wasm chunk the highlighter imports.

`shiki/wasm` is emitted as a `wasm-<hash>.js` chunk beside the `highlight-*.js`
chunk that imports it. A build that lost the wasm would draw every code panel
plain, and the build also emits an unrelated `wasm-*.js` chunk, so a loose glob
would pass for the wrong reason: the reference is read OUT of the highlighter's
own chunk and the file it names is what must exist.

Usage: check_client_wasm.py [dist]   (default: client/dist)
"""

import re
import sys
from pathlib import Path

# The emitted chunk names, as the highlighter's chunk spells them.
REFERENCE = re.compile(r"wasm-[A-Za-z0-9_-]+\.js")


def main(argv):
    dist = Path(argv[1]) if len(argv) > 1 else Path("client/dist")
    assets = dist / "assets"
    if not assets.is_dir():
        print("[ERROR] no built assets at {}".format(assets), file=sys.stderr)
        return 1

    chunks = sorted(assets.glob("highlight-*.js"))
    if not chunks:
        print("[ERROR] the build emitted no highlight chunk at {}".format(assets), file=sys.stderr)
        return 1

    referenced = set()
    missing = set()
    for chunk in chunks:
        for name in REFERENCE.findall(chunk.read_text(encoding="utf8")):
            referenced.add(name)
            if not (assets / name).is_file():
                missing.add(name)

    if not referenced:
        print(
            "[ERROR] the highlight chunk imports no wasm chunk: the Oniguruma engine is gone",
            file=sys.stderr,
        )
        return 1
    if missing:
        print(
            "[ERROR] the highlight chunk imports wasm chunks the build did not emit: {}".format(
                ", ".join(sorted(missing))
            ),
            file=sys.stderr,
        )
        return 1

    print("[OK] the highlight chunk carries {}".format(", ".join(sorted(referenced))))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))

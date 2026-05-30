#!/usr/bin/env python3
"""
Forbid em-dash / en-dash / horizontal-bar / curly quotes in forge-authored
source. Ellipsis U+2026 is ALLOWED (legitimate truncation glyph in TUI
render). Captured-data dirs (test baselines, reference captures) are
excluded - those mirror upstream wire payloads byte-for-byte and may
legitimately contain Unicode prose from the CLI's own logs.

When a banned codepoint is functionally required (render glyph,
ASCII-art element, legitimate punctuation in test fixtures), use the
Rust escape sequence form, e.g. `"\\u{2014}"` instead of `"-"`. The
source no longer contains the literal codepoint, the compiled binary
produces the same character. This is the same shape as the U+2026
exception in the pattern below: prefer "not in the source" over a
per-site whitelist.

Banned codepoints (kept inline for grep'ability):
  U+2013 -  en-dash
  U+2014 -  em-dash
  U+2015 -  horizontal-bar
  U+2018 -  left single curly quote
  U+2019 -  right single curly quote
  U+201C -  left double curly quote
  U+201D -  right double curly quote

NOT BANNED:
  U+2026 ellipsis (used for truncation in TUI render - legit)

Run from the repo root (or pass a directory argv). Exits 0 on clean
tree, 1 with file:line:content output on a hit.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

BANNED = re.compile(r"[–—―‘’“”]")

INCLUDE_SUFFIXES = (".rs", ".toml", ".md", ".html")

EXCLUDE_DIRS_ANY_DEPTH = {
    ".git",
    "target",
    "node_modules",
}

EXCLUDE_PATH_SUBSTRINGS = (
    "/crates/forge-test-harness/baselines/",
    "/reference-captures/",
)


def should_skip_dir(path: Path) -> bool:
    name = path.name
    if name in EXCLUDE_DIRS_ANY_DEPTH:
        return True
    return False


def should_skip_file(path: Path) -> bool:
    s = str(path)
    return any(sub in s for sub in EXCLUDE_PATH_SUBSTRINGS)


def walk(root: Path):
    stack = [root]
    while stack:
        current = stack.pop()
        try:
            entries = list(current.iterdir())
        except (PermissionError, FileNotFoundError):
            continue
        for entry in entries:
            try:
                if entry.is_symlink():
                    continue
                if entry.is_dir():
                    if not should_skip_dir(entry):
                        stack.append(entry)
                    continue
                if not entry.is_file():
                    continue
            except (PermissionError, OSError):
                continue
            if entry.suffix not in INCLUDE_SUFFIXES:
                continue
            if should_skip_file(entry):
                continue
            yield entry


def scan_file(path: Path):
    """Yield (line_number, line_text) for every line containing a banned char."""
    try:
        with path.open(encoding="utf-8", errors="strict") as f:
            for n, line in enumerate(f, start=1):
                if BANNED.search(line):
                    yield (n, line.rstrip("\n"))
    except UnicodeDecodeError:
        # Binary-ish files masquerading as a tracked suffix - skip silently.
        return


def main(argv):
    root = Path(argv[1]) if len(argv) > 1 else Path(".")
    hits = []
    for path in walk(root):
        for (line_no, text) in scan_file(path):
            hits.append((path, line_no, text))

    if not hits:
        return 0

    for (path, line_no, text) in hits:
        print(f"{path}:{line_no}:{text}")
    print(file=sys.stderr)
    print("ERROR: banned Unicode punctuation found in forge-authored source.", file=sys.stderr)
    print(
        "  - em-dash (U+2014) / en-dash (U+2013) / horizontal-bar (U+2015) / curly quotes (U+2018/2019/201C/201D)",
        file=sys.stderr,
    )
    print("  - Ellipsis U+2026 is ALLOWED (truncation glyph).", file=sys.stderr)
    print("  - Test baselines + reference-captures are excluded.", file=sys.stderr)
    print(
        "  When the codepoint is functionally required (render glyph), use the Rust",
        file=sys.stderr,
    )
    print(r'  escape form, e.g. `"\u{2014}"` instead of `"-"`.', file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))

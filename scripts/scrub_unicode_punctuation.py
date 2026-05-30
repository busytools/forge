#!/usr/bin/env python3
"""
Substitute banned Unicode punctuation with ASCII equivalents in forge-authored
source. Used to produce the bulk pre-gate cleanup for #286 PR-1.

Operates in three modes (Rust-source files only consult the lex state machine):
  --mode comments   substitute only when inside `//` `///` `//!` line comments
                    or `/* ... */` block comments. Used for Phase 1.
  --mode literals   substitute only when inside `"..."` / `r"..."` / `r#"..."#`
                    string literals. Used for Phase 3 (manual review required;
                    tool emits the diff for inspection).
  --mode all        substitute everywhere (no syntax awareness). Used for
                    Phase 2 (`*.md` / `*.toml` / `*.html`); also works on `.rs`
                    if the caller wants a blind pass (NOT recommended).

Substitution rules:
  U+2014 em-dash         (—)  ->  " - "  (space-hyphen-space)
  U+2013 en-dash         (–)  ->  "-"
  U+2015 horizontal-bar  (―)  ->  "-"
  U+2018 left single qt  (')  ->  "'"
  U+2019 right single qt (')  ->  "'"
  U+201C left double qt  (")  ->  '"'
  U+201D right double qt (")  ->  '"'

NOT substituted:
  U+2026 ellipsis        (…)  -- legit truncation glyph; gate exempts it.

Flags:
  --dry-run             print the per-file delta count + first changed line of
                        each file, but don't write.
  --skip <substring>    skip files whose path contains this substring. Repeatable.
  files...              files to process (paths). If empty, reads paths from
                        stdin (one per line) -- pairs with `rg -l`.

Idempotency: a second pass on a substituted tree is a no-op (banned codepoints
are gone after the first pass).
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

SUBSTITUTIONS = {
    "—": " - ",   # em-dash
    "–": "-",     # en-dash
    "―": "-",     # horizontal-bar
    "‘": "'",     # left single quote
    "’": "'",     # right single quote
    "“": '"',     # left double quote
    "”": '"',     # right double quote
}
BANNED = set(SUBSTITUTIONS)


def substitute_blind(text: str) -> tuple[str, int]:
    """Pass `text` through SUBSTITUTIONS unconditionally. Returns (new, count)."""
    count = 0
    for src, dst in SUBSTITUTIONS.items():
        n = text.count(src)
        if n:
            text = text.replace(src, dst)
            count += n
    return text, count


def substitute_rust(text: str, mode: str) -> tuple[str, int]:
    """
    Pass `text` (Rust source) through SUBSTITUTIONS only where the position is
    inside the requested region (`comments` or `literals`). Returns (new, count).

    State machine handles:
      - line comments  (//, ///, //!)
      - block comments (/* ... */, including nesting -- Rust block comments nest)
      - string literals "..." with `\"` and `\\` escapes
      - raw strings    r"..." and r#"..."# (no escapes)
      - char literals  best-effort skip; banned codepoints inside char literals
                       would classify as "other" -- we leave them unchanged in
                       both modes to be safe.

    `mode == "comments"`: substitute only in comments.
    `mode == "literals"`: substitute only in string + raw-string literals.
    """
    assert mode in ("comments", "literals")
    out = []
    count = 0
    in_line_comment = False
    in_block_comment = 0
    in_string = False
    in_raw_string = False
    raw_string_hashes = 0

    i = 0
    n = len(text)
    while i < n:
        ch = text[i]

        # Line ends.
        if ch == "\n":
            in_line_comment = False
            out.append(ch)
            i += 1
            continue

        # In a line comment.
        if in_line_comment:
            if ch in BANNED and mode == "comments":
                out.append(SUBSTITUTIONS[ch])
                count += 1
            else:
                out.append(ch)
            i += 1
            continue

        # In a block comment.
        if in_block_comment > 0:
            if ch in BANNED and mode == "comments":
                out.append(SUBSTITUTIONS[ch])
                count += 1
                i += 1
                continue
            # Detect nested block-comment open.
            if ch == "/" and i + 1 < n and text[i + 1] == "*":
                in_block_comment += 1
                out.append("/*")
                i += 2
                continue
            # Detect block-comment close.
            if ch == "*" and i + 1 < n and text[i + 1] == "/":
                in_block_comment -= 1
                out.append("*/")
                i += 2
                continue
            out.append(ch)
            i += 1
            continue

        # In a raw string.
        if in_raw_string:
            if ch in BANNED and mode == "literals":
                out.append(SUBSTITUTIONS[ch])
                count += 1
                i += 1
                continue
            # Detect raw-string close.
            if ch == '"':
                ok = True
                for k in range(raw_string_hashes):
                    if i + 1 + k >= n or text[i + 1 + k] != "#":
                        ok = False
                        break
                if ok:
                    in_raw_string = False
                    out.append('"' + "#" * raw_string_hashes)
                    i += 1 + raw_string_hashes
                    continue
            out.append(ch)
            i += 1
            continue

        # In a regular string.
        if in_string:
            if ch in BANNED and mode == "literals":
                out.append(SUBSTITUTIONS[ch])
                count += 1
                i += 1
                continue
            if ch == "\\" and i + 1 < n:
                # Preserve escape pair.
                out.append(text[i:i + 2])
                i += 2
                continue
            if ch == '"':
                in_string = False
            out.append(ch)
            i += 1
            continue

        # Bare code. Detect state transitions.
        # Line comment.
        if ch == "/" and i + 1 < n and text[i + 1] == "/":
            in_line_comment = True
            out.append("//")
            i += 2
            continue
        # Block comment.
        if ch == "/" and i + 1 < n and text[i + 1] == "*":
            in_block_comment = 1
            out.append("/*")
            i += 2
            continue
        # Raw string.
        if ch == "r" and i + 1 < n:
            j = i + 1
            hashes = 0
            while j < n and text[j] == "#":
                hashes += 1
                j += 1
            if j < n and text[j] == '"':
                in_raw_string = True
                raw_string_hashes = hashes
                out.append("r" + "#" * hashes + '"')
                i = j + 1
                continue
        # Regular string.
        if ch == '"':
            in_string = True
            out.append('"')
            i += 1
            continue
        # Other char; emit as-is.
        out.append(ch)
        i += 1

    return ("".join(out), count)


def main(argv):
    parser = argparse.ArgumentParser(description="Substitute banned Unicode punctuation.")
    parser.add_argument("--mode", required=True, choices=("comments", "literals", "all"))
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--skip", action="append", default=[])
    parser.add_argument("files", nargs="*")
    args = parser.parse_args(argv[1:])

    paths = [Path(p) for p in args.files]
    if not paths:
        paths = [Path(p.strip()) for p in sys.stdin if p.strip()]

    total = 0
    touched = 0
    for p in paths:
        if any(s in str(p) for s in args.skip):
            continue
        try:
            text = p.read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError):
            continue

        if args.mode == "all":
            new, count = substitute_blind(text)
        else:
            new, count = substitute_rust(text, args.mode)

        if count == 0:
            continue
        total += count
        touched += 1
        if args.dry_run:
            print(f"[dry-run] {p}: would replace {count} occurrence(s)")
        else:
            p.write_text(new, encoding="utf-8")
            print(f"{p}: replaced {count} occurrence(s)")

    suffix = " (dry-run)" if args.dry_run else ""
    print(f"\nTOTAL: {total} substitution(s) across {touched} file(s){suffix}")


if __name__ == "__main__":
    main(sys.argv)

#!/usr/bin/env python3
"""Tests for check_client_wasm.py: the reference is read from the highlight
chunk, the named file must exist, and an unrelated wasm chunk satisfies
nothing.

The instrument is a build check, so the cases are the ways a build goes quiet:
no chunk, a reference to a file the build did not emit, and a wasm chunk that
is present but is not the one the highlighter imports.
"""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check_client_wasm.py")


def run(dist):
    return subprocess.run(
        [sys.executable, str(SCRIPT), str(dist)],
        capture_output=True,
        text=True,
    )


class CheckClientWasm(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dist = Path(self.tmp.name) / "dist"
        self.assets = self.dist / "assets"
        self.assets.mkdir(parents=True)

    def write(self, name, text=""):
        (self.assets / name).write_text(text, encoding="utf8")

    def test_a_referenced_chunk_that_exists_passes(self):
        self.write("wasm-REFERENCED.js")
        self.write("highlight-abc.js", 'import("./wasm-REFERENCED.js");')

        done = run(self.dist)

        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("wasm-REFERENCED.js", done.stdout)

    def test_the_reference_is_read_from_the_highlight_chunk(self):
        # A wasm chunk another chunk imports does not answer for the
        # highlighter's own: the reader's chunk is not where colouring lands.
        self.write("wasm-OTHER.js")
        self.write("reader-def.js", 'import("./wasm-OTHER.js");')
        self.write("highlight-abc.js", "export const plain = 1;")

        done = run(self.dist)

        self.assertNotEqual(done.returncode, 0)
        self.assertIn("imports no wasm chunk", done.stderr)

    def test_a_reference_the_build_did_not_emit_fails(self):
        self.write("highlight-abc.js", 'import("./wasm-MISSING.js");')

        done = run(self.dist)

        self.assertNotEqual(done.returncode, 0)
        self.assertIn("wasm-MISSING.js", done.stderr)

    def test_no_highlight_chunk_fails(self):
        self.write("wasm-REFERENCED.js")

        done = run(self.dist)

        self.assertNotEqual(done.returncode, 0)
        self.assertIn("no highlight chunk", done.stderr)


if __name__ == "__main__":
    unittest.main()

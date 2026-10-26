"""Tests for docs/book/ui-word-count.sh: the baseline comparison, the two
set checks that stop the baseline drifting away from the pages, and the
record mode that writes the baseline.

The script is copied into a temporary book tree, so these exercise the real
script against fixture pages rather than a stub.
"""

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "docs" / "book" / "ui-word-count.sh"


class UiWordCount(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        shutil.copy2(SCRIPT, self.root / "ui-word-count.sh")
        self.pages = self.root / "src" / "ui" / "client"
        self.pages.mkdir(parents=True)
        self.baseline = self.root / "ui-word-count.baseline"

    def page(self, name, words):
        (self.pages / name).write_text(" ".join(["word"] * words) + "\n")

    def run_gate(self, *args):
        return subprocess.run(
            ["bash", str(self.root / "ui-word-count.sh"), *args],
            capture_output=True,
            text=True,
        )

    def test_a_page_at_its_baseline_passes(self):
        self.page("page.md", 3)
        self.baseline.write_text("src/ui/client/page.md 3\n")
        done = self.run_gate()
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("ok: src/ui/client/page.md (3 words)", done.stdout)

    def test_a_grown_page_fails_naming_the_page_and_both_counts(self):
        self.page("page.md", 4)
        self.baseline.write_text("src/ui/client/page.md 3\n")
        done = self.run_gate()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn(
            "OVER: src/ui/client/page.md (4 words, baseline 3)", done.stdout
        )

    def test_a_page_missing_from_the_baseline_fails(self):
        self.page("page.md", 3)
        self.baseline.write_text("")
        done = self.run_gate()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn(
            "NO BASELINE: src/ui/client/page.md (record it in the same PR)",
            done.stdout,
        )

    def test_a_baseline_entry_with_no_page_fails(self):
        self.page("page.md", 3)
        self.baseline.write_text(
            "src/ui/client/page.md 3\nsrc/ui/client/gone.md 10\n"
        )
        done = self.run_gate()
        self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
        self.assertIn(
            "STALE BASELINE: src/ui/client/gone.md (page is gone)", done.stdout
        )

    def test_record_writes_one_sorted_line_per_page(self):
        self.page("b.md", 2)
        self.page("a.md", 5)
        done = self.run_gate("--record")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(
            self.baseline.read_text(),
            "src/ui/client/a.md 5\nsrc/ui/client/b.md 2\n",
        )


if __name__ == "__main__":
    unittest.main()

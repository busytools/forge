#!/usr/bin/env python3
"""Tests for refusing to build an install from a stale checkout. Run:
python3 scripts/test_require_main.py

`just release` publishes what the checkout holds, and its build steps are
`install` and the three client recipes. A checkout behind origin/main builds
yesterday's code while every artefact reports today's version - which is what
happened on 2026-11-07, when five merged fixes were "installed" from the
release commit because local main was five commits stale.

`require-main.sh` fast-forwards a clean main and refuses everything else. These
cases are that contract, each against a real remote and a real clone: a guard
tested against a stub of `git` would prove the stub.

Stdlib only, like the other script tests.
"""

import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "require-main.sh"

GIT = ["git", "-c", "user.name=forge test", "-c", "user.email=test@forge.invalid"]


def run(cwd: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(
        [*GIT, *args], cwd=cwd, check=check, capture_output=True, text=True
    )


class RequireMain(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.origin = self.root / "origin.git"
        subprocess.run(
            ["git", "init", "--bare", "-b", "main", str(self.origin)],
            check=True,
            capture_output=True,
        )
        self.work = self.root / "work"
        subprocess.run(
            ["git", "clone", "--quiet", str(self.origin), str(self.work)],
            check=True,
            capture_output=True,
        )
        (self.work / "a.txt").write_text("one\n")
        run(self.work, "add", "a.txt")
        run(self.work, "commit", "--quiet", "-m", "first")
        run(self.work, "push", "--quiet", "origin", "main")
        # A second clone whose commit the work checkout has not seen yet.
        self.other = self.root / "other"
        subprocess.run(
            ["git", "clone", "--quiet", str(self.origin), str(self.other)],
            check=True,
            capture_output=True,
        )

    def tearDown(self) -> None:
        self.tmp.cleanup()

    def push_upstream(self, name: str) -> str:
        (self.other / name).write_text(name + "\n")
        run(self.other, "add", name)
        run(self.other, "commit", "--quiet", "-m", name)
        run(self.other, "push", "--quiet", "origin", "main")
        return run(self.other, "rev-parse", "HEAD").stdout.strip()

    def guard(self) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(SCRIPT)], cwd=self.work, capture_output=True, text=True
        )

    def test_a_clean_main_behind_origin_is_fast_forwarded(self) -> None:
        upstream = self.push_upstream("b.txt")
        result = self.guard()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            run(self.work, "rev-parse", "HEAD").stdout.strip(),
            upstream,
            "the build was allowed to run on a stale checkout",
        )
        self.assertIn("fast-forwarded", result.stdout)

    def test_a_clean_main_level_with_origin_is_allowed(self) -> None:
        result = self.guard()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("building from", result.stdout)

    def test_a_dirty_tree_is_refused(self) -> None:
        (self.work / "a.txt").write_text("changed and uncommitted\n")
        result = self.guard()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dirty", result.stderr)

    def test_another_branch_is_refused(self) -> None:
        run(self.work, "switch", "--quiet", "-c", "feature")
        result = self.guard()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not main", result.stderr)

    def test_a_diverged_main_is_refused(self) -> None:
        self.push_upstream("b.txt")
        (self.work / "c.txt").write_text("local\n")
        run(self.work, "add", "c.txt")
        run(self.work, "commit", "--quiet", "-m", "local")
        result = self.guard()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fast-forward", result.stderr)

    def test_a_tagged_commit_ahead_of_origin_is_allowed(self) -> None:
        # `just release` tags the version commit, then builds it, then pushes.
        (self.work / "release.txt").write_text("version bump\n")
        run(self.work, "add", "release.txt")
        run(self.work, "commit", "--quiet", "-m", "release v0.0.1")
        run(self.work, "tag", "-m", "v0.0.1", "v0.0.1")
        result = self.guard()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v0.0.1", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)

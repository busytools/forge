#!/usr/bin/env python3
"""Tests for the justfile's client-dependency bootstrap. Run:
python3 scripts/test_client_deps.py

A fresh worktree has no `client/node_modules`, and a worktree whose lockfile
moved under it has a stale one. Either way the client steps fail as though the
code were broken, so each installs what it needs first: a gate step when the
directory is missing or older than the lockfile and not otherwise, a release
recipe unconditionally - mtime can read a tree missing a moved dependency as
current, and a release must not bundle what the tree happens to hold.

Stdlib only, like the unicode gate: python3 is already a check dependency.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

JUSTFILE = Path(__file__).resolve().parent.parent / "justfile"

# The client steps `just check` runs, each of which needs the install.
CLIENT_STEPS = ("client-format", "client-lint", "client-typecheck", "client-test")

# The release recipes that build the client, each of which needs the install
# too: a release must not bundle whatever tree the checkout happened to carry.
RELEASE_STEPS = ("client-release", "client-android-release", "client-web-release")


class ClientDepsTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)
        self.client = self.root / "client"
        self.client.mkdir()
        self.lock = self.client / "package-lock.json"
        self.lock.write_text("{}")
        self.node_modules = self.client / "node_modules"

        # A stub npm: it answers `ci` by creating the directory the real one
        # would, records every call, and otherwise answers the way the
        # toolchain does when the install is absent - 127.
        self.log = self.root / "npm.log"
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        npm = bin_dir / "npm"
        npm.write_text(
            "#!/usr/bin/env bash\n"
            f'echo "$*" >> "{self.log}"\n'
            'if [ "$*" = "--prefix client ci" ]; then\n'
            f'  mkdir -p "{self.node_modules}"\n'
            "  exit 0\n"
            "fi\n"
            f'if [ ! -d "{self.node_modules}" ]; then\n'
            '  echo "prettier: command not found" >&2\n'
            "  exit 127\n"
            "fi\n"
        )
        npm.chmod(0o755)
        self.env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}"}

    def step(self, name: str) -> "subprocess.CompletedProcess[str]":
        return subprocess.run(
            [
                "just",
                "--justfile",
                str(JUSTFILE),
                "--working-directory",
                str(self.root),
                name,
            ],
            capture_output=True,
            text=True,
            env=self.env,
        )

    def release_plan(self, name: str) -> "subprocess.CompletedProcess[str]":
        # The bodies need the release machine, so the edge is read off the
        # dry-run plan instead: just echoes it (to stderr, so both streams are
        # read) with dependency bodies included.
        return subprocess.run(
            [
                "just",
                "--justfile",
                str(JUSTFILE),
                "--working-directory",
                str(self.root),
                "--dry-run",
                name,
                "0.0.0",
            ],
            capture_output=True,
            text=True,
            env=self.env,
        )

    def calls(self) -> list[str]:
        return self.log.read_text().splitlines() if self.log.exists() else []

    def install(self, node_modules_time: int, lock_time: int) -> None:
        self.node_modules.mkdir()
        os.utime(self.node_modules, (node_modules_time, node_modules_time))
        os.utime(self.lock, (lock_time, lock_time))

    def test_each_client_step_installs_the_deps_a_fresh_worktree_lacks(self) -> None:
        for name in CLIENT_STEPS:
            with self.subTest(step=name):
                shutil.rmtree(self.node_modules, ignore_errors=True)
                self.log.unlink(missing_ok=True)

                result = self.step(name)

                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(
                    self.calls()[0],
                    "--prefix client ci",
                    "the step ran before the install it needs",
                )

    def test_a_stale_install_is_replaced(self) -> None:
        # Keyed on absence alone, a worktree whose lockfile moved under it
        # keeps the old tree and the gate reports client-lint type errors.
        self.install(node_modules_time=1_000, lock_time=2_000)

        result = self.step("client-format")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--prefix client ci", self.calls())

    def test_a_current_install_is_not_replaced(self) -> None:
        self.install(node_modules_time=2_000, lock_time=1_000)

        result = self.step("client-format")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("--prefix client ci", self.calls())

    def test_each_release_recipe_installs_unconditionally(self) -> None:
        for name in RELEASE_STEPS:
            with self.subTest(step=name):
                result = self.release_plan(name)

                self.assertEqual(result.returncode, 0, result.stderr)
                plan = result.stdout + result.stderr
                self.assertIn(
                    "npm --prefix client ci",
                    plan,
                    "the release recipe would build without the install",
                )
                self.assertNotIn(
                    "if [ ! -d client/node_modules ]",
                    plan,
                    "the release recipe settled for the mtime-keyed install, "
                    "which a stale tree can read as current",
                )

    def test_the_release_install_runs_even_on_a_current_tree(self) -> None:
        self.install(node_modules_time=2_000, lock_time=1_000)

        result = self.step("client-release-deps")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--prefix client ci", self.calls())


if __name__ == "__main__":
    unittest.main()

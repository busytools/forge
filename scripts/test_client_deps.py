#!/usr/bin/env python3
"""Tests for the justfile's client-dependency bootstrap. Run:
python3 scripts/test_client_deps.py

A fresh worktree has no `client/node_modules`, and a worktree whose lockfile
moved under it has a stale one. Either way the client steps fail as though the
code were broken, so each step installs what it needs first, and these pin
when that install runs: when the directory is missing, or older than the
lockfile, and not otherwise.

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


if __name__ == "__main__":
    unittest.main()

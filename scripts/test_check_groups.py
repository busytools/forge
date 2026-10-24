"""Tests for check_groups.sh: the parity guard, the verdict, and a worker
that dies before it can write its report.

The runner is a stub, so these exercise the orchestrator's own decisions
rather than a real gate step: which failure is reported, what the last line
says, and whether a missing report can read as a pass.
"""

import os
import subprocess
import tempfile
import textwrap
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("check_groups.sh")


class CheckGroups(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.stub = self.root / "stub.sh"
        self.stub.write_text(
            textwrap.dedent(
                """\
                #!/usr/bin/env bash
                step="${3:-}"
                case "$step" in
                    step-fail) exit 7 ;;
                    step-killdir) rm -rf "$CHECK_LOG_DIR"; exit 1 ;;
                    *) exit 0 ;;
                esac
                """
            )
        )
        self.stub.chmod(0o755)
        self.logs = self.root / "logs"

    def gate(self, steps, groups, jobs="4"):
        env = dict(
            os.environ,
            CHECK_LOG_ROOT=str(self.logs),
            FORGE_CHECK_JOBS=jobs,
        )
        return subprocess.run(
            [str(SCRIPT), str(self.stub), "/dev/null", steps, groups],
            env=env,
            capture_output=True,
            text=True,
        )

    def test_parity_guard_refuses_a_step_in_no_group(self):
        done = self.gate("a b", "g|a")
        self.assertEqual(done.returncode, 2, done.stdout + done.stderr)
        self.assertIn("check_steps and check_groups disagree", done.stderr)

    def test_green_prints_the_verdict_last(self):
        done = self.gate("a b", "g|a b")
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        lines = done.stdout.strip().splitlines()
        self.assertEqual(lines[-1], "[OK] check: all green (a b)")
        self.assertTrue(any(line.startswith("logs: ") for line in lines), done.stdout)

    def test_failure_names_the_step_and_its_unrun_steps(self):
        done = self.gate("a step-fail c", "g|a step-fail c")
        self.assertEqual(done.returncode, 7, done.stdout + done.stderr)
        lines = done.stdout.strip().splitlines()
        self.assertEqual(
            lines[-1], "[ERROR] check: step-fail failed; not run: c"
        )
        self.assertIn("[ERROR] check: g group failed at step-fail", done.stdout)

    def test_group_that_dies_without_a_report_is_a_failure(self):
        done = self.gate("a step-killdir c", "g|a step-killdir c")
        self.assertNotEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertNotIn("[OK] check: all green", done.stdout)
        lines = done.stdout.strip().splitlines()
        self.assertEqual(lines[-1], "[ERROR] check: group reported nothing (g)")

    def test_each_run_keeps_its_own_log_directory(self):
        first = self.gate("a", "g|a")
        second = self.gate("a", "g|a")
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        self.assertEqual(second.returncode, 0, second.stdout + second.stderr)

        def logged(done):
            return next(
                line.split("logs: ", 1)[1]
                for line in done.stdout.splitlines()
                if line.startswith("logs: ")
            )

        one, two = logged(first), logged(second)
        self.assertNotEqual(one, two)
        self.assertTrue(Path(one).is_dir(), one)
        self.assertTrue(Path(two).is_dir(), two)

    def test_job_count_must_be_positive(self):
        done = self.gate("a", "g|a", jobs="0")
        self.assertEqual(done.returncode, 2, done.stdout + done.stderr)
        self.assertIn("FORGE_CHECK_JOBS must be a positive integer", done.stderr)


if __name__ == "__main__":
    unittest.main()

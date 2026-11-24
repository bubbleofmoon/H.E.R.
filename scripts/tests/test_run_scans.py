#!/usr/bin/env python3
"""Tests for scripts/ci/run-scans.py.

Run: python -I scripts/tests/test_run_scans.py
"""
from __future__ import annotations

import contextlib
import importlib.util
import io
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
RUN_SCANS_SCRIPT = ROOT / "scripts" / "ci" / "run-scans.py"


def load_run_scans():
    spec = importlib.util.spec_from_file_location("run_scans", RUN_SCANS_SCRIPT)
    assert spec is not None and spec.loader is not None, f"missing {RUN_SCANS_SCRIPT}"
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class RunScansTests(unittest.TestCase):
    def test_repo_completeness_matches_test_suites_on_disk(self):
        mod = load_run_scans()
        discovered = mod.discover_test_suites(ROOT / "scripts" / "tests")
        self.assertEqual(discovered, sorted(mod.TEST_SUITES))
        self.assertEqual(len(set(mod.SUITES)), len(mod.SUITES))
        self.assertLessEqual(set(mod.HEAVY_TEST_SUITES), set(mod.TEST_SUITES))
        self.assertEqual(mod.check_completeness(ROOT), [])
        self.assertIn("scripts/check-repo.py", mod.SUITES)
        self.assertIn("scripts/tests/test_run_scans.py", mod.SUITES)
        self.assertIn("scripts/tests/test_checks_workflow.py", mod.SUITES)

    def test_completeness_rejects_unlisted_and_missing_suites_in_temp_dir(self):
        mod = load_run_scans()
        tmp = Path(tempfile.mkdtemp(prefix="run-scans-complete-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        tests_dir = tmp / "scripts" / "tests"
        tests_dir.mkdir(parents=True)
        (tests_dir / "test_alpha.py").write_text("print('ok')\n", encoding="utf-8")
        (tests_dir / "test_unlisted.py").write_text("print('ok')\n", encoding="utf-8")

        problems = mod.check_completeness(
            tmp,
            test_suites=("test_alpha.py", "test_missing.py"),
            heavy_suites=(),
        )
        joined = "\n".join(problems)
        self.assertIn("test_unlisted.py", joined)
        self.assertIn("test_missing.py", joined)
        self.assertNotIn("test_alpha.py", joined)

    def test_completeness_rejects_duplicate_and_unknown_heavy_suites(self):
        mod = load_run_scans()
        tmp = Path(tempfile.mkdtemp(prefix="run-scans-heavy-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        tests_dir = tmp / "scripts" / "tests"
        tests_dir.mkdir(parents=True)
        (tests_dir / "test_alpha.py").write_text("print('ok')\n", encoding="utf-8")

        problems = mod.check_completeness(
            tmp,
            test_suites=("test_alpha.py",),
            heavy_suites=("test_alpha.py", "test_alpha.py", "test_unknown.py"),
        )
        joined = "\n".join(problems)
        self.assertIn("test_alpha.py: listed more than once in HEAVY_TEST_SUITES", joined)
        self.assertIn("test_unknown.py: listed in HEAVY_TEST_SUITES but missing from TEST_SUITES", joined)

    def test_run_suites_executes_passing_suites_in_parallel_and_returns_zero(self):
        mod = load_run_scans()
        tmp = Path(tempfile.mkdtemp(prefix="run-scans-pass-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)

        # Each script creates its own marker and waits for the sibling's marker;
        # sequential execution would time out after 5s instead of passing.
        for mine, peer in (("a", "b"), ("b", "a")):
            (tmp / f"{mine}.py").write_text(
                f"from pathlib import Path\n"
                f"import sys, time\n"
                f"Path('{mine}.started').write_text('1', encoding='utf-8')\n"
                f"deadline = time.monotonic() + 5.0\n"
                f"while not Path('{peer}.started').exists():\n"
                f"    if time.monotonic() > deadline:\n"
                f"        print('timed out waiting for {peer}', file=sys.stderr)\n"
                f"        sys.exit(1)\n"
                f"    time.sleep(0.02)\n"
                f"print('ok {mine}')\n",
                encoding="utf-8",
            )

        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = mod.run_suites(["a.py", "b.py"], tmp, jobs=2, timeout=15)

        out = buf.getvalue()
        self.assertEqual(code, 0, out)
        self.assertIn("PASS a.py (", out)
        self.assertIn("PASS b.py (", out)
        self.assertNotIn("ok a", out)

    def test_run_suites_runs_all_suites_on_failure_and_prints_captured_output(self):
        mod = load_run_scans()
        tmp = Path(tempfile.mkdtemp(prefix="run-scans-fail-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)

        (tmp / "s1.py").write_text("print('passing-marker-1')\n", encoding="utf-8")
        (tmp / "s2.py").write_text(
            "import sys\n"
            "print('boom-stdout-line')\n"
            "print('boom-stderr-line', file=sys.stderr)\n"
            "sys.exit(1)\n",
            encoding="utf-8",
        )
        (tmp / "s3.py").write_text("print('passing-marker-3')\n", encoding="utf-8")

        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = mod.run_suites(["s1.py", "s2.py", "s3.py"], tmp, jobs=2, timeout=15)

        out = buf.getvalue()
        self.assertNotEqual(code, 0, out)
        self.assertIn("PASS s1.py (", out)
        self.assertIn("FAIL s2.py (exit 1,", out)
        self.assertIn("PASS s3.py (", out)
        self.assertIn("boom-stdout-line", out)
        self.assertIn("boom-stderr-line", out)
        self.assertNotIn("passing-marker-1", out)
        self.assertNotIn("passing-marker-3", out)

    def test_run_suites_handles_timeout_cleanly(self):
        mod = load_run_scans()
        tmp = Path(tempfile.mkdtemp(prefix="run-scans-timeout-"))
        self.addCleanup(shutil.rmtree, tmp, ignore_errors=True)
        (tmp / "slow.py").write_text("import time\ntime.sleep(5)\n", encoding="utf-8")

        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = mod.run_suites(["slow.py"], tmp, jobs=1, timeout=0.2)

        out = buf.getvalue()
        self.assertNotEqual(code, 0, out)
        self.assertIn("FAIL slow.py (exit 124,", out)
        self.assertIn("TIMEOUT", out)

    def test_cli_list_exits_zero_with_explicit_suites(self):
        done = subprocess.run(
            [sys.executable, "-I", str(RUN_SCANS_SCRIPT), "--list"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
        self.assertEqual(done.returncode, 0, done.stderr)
        lines = done.stdout.splitlines()
        self.assertIn("scripts/check-repo.py", lines)
        self.assertIn("scripts/tests/test_run_scans.py", lines)
        self.assertIn("scripts/tests/test_checks_workflow.py", lines)

    def test_defaults_cap_workers_at_four_and_schedule_heavy_suites_first(self):
        mod = load_run_scans()
        self.assertEqual(mod.DEFAULT_TIMEOUT, 900)
        self.assertEqual(mod.default_jobs(None), 2)
        self.assertEqual(mod.default_jobs(0), 2)
        self.assertEqual(mod.default_jobs(-3), 2)
        self.assertEqual(mod.default_jobs(False), 2)
        self.assertEqual(mod.default_jobs(True), 2)
        self.assertEqual(mod.default_jobs(2), 2)
        self.assertEqual(mod.default_jobs(4), 4)
        self.assertEqual(mod.default_jobs(16), 4)
        heavy_prefix = tuple(f"scripts/tests/{name}" for name in mod.HEAVY_TEST_SUITES)
        self.assertEqual(mod.SUITES[: len(heavy_prefix)], heavy_prefix)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Tests for .github/workflows/checks.yml and scripts/ci/classify-changes.py.

Covers path classification (`is_doc_only_path`), CLI classification against real
temporary git repositories (including non-ASCII paths, mixed changes, shallow
fetches with no shared commit history, and fail-open behavior), and the
structural contract in `.github/workflows/checks.yml`.

Run: python -I scripts/tests/test_checks_workflow.py
"""
from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "checks.yml"
CLASSIFY_SCRIPT = ROOT / "scripts" / "ci" / "classify-changes.py"
GIT_ENV = {
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_TERMINAL_PROMPT": "0",
    "GIT_AUTHOR_NAME": "Checks Test",
    "GIT_AUTHOR_EMAIL": "checks.test@example.invalid",
    "GIT_COMMITTER_NAME": "Checks Test",
    "GIT_COMMITTER_EMAIL": "checks.test@example.invalid",
}

_spec = importlib.util.spec_from_file_location("classify_changes", CLASSIFY_SCRIPT)
assert _spec is not None and _spec.loader is not None
_classify = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_classify)


def clean_env() -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update(GIT_ENV)
    return env


def git(cwd: Path, *args: str) -> str:
    done = subprocess.run(
        ["git", *args],
        cwd=cwd,
        env=clean_env(),
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=True,
    )
    return done.stdout.strip()


def run_classify(repo: Path, base: str, head: str, github_output: Path | None = None) -> subprocess.CompletedProcess[str]:
    cmd = [
        sys.executable,
        "-I",
        str(CLASSIFY_SCRIPT),
        "--repo",
        str(repo),
        "--base",
        base,
        "--head",
        head,
    ]
    if github_output is not None:
        cmd.extend(["--github-output", str(github_output)])
    return subprocess.run(cmd, env=clean_env(), capture_output=True, text=True, encoding="utf-8", check=False)


class PathClassification(unittest.TestCase):
    def test_accepts_pure_documentation_paths(self):
        for path in (
            "AGENTS.md",
            "docs/adr/0008-embedded-vite-console-ui.md",
            "docs\\guide.md",
            "her/AGENTS.md",
            "her/THIRD_PARTY_NOTICES.md",
            "docs/agents/coordinator.md",
            "docs/agents/issue-tracker.MD",
        ):
            with self.subTest(path=path):
                self.assertTrue(_classify.is_doc_only_path(path))

    def test_rejects_code_test_ui_and_workflow_paths(self):
        for path in (
            "",
            "/docs/guide.md",
            "her/src/console.rs",
            "her/tests/fixtures/release-signing/README.md",
            "her-memory/src/lib.md",
            "her-memory/tests/README.md",
            "her-ui/README.md",
            "her\\src\\lib.md",
            ".github/workflows/checks.yml",
            "scripts/ci/classify-changes.py",
            "her/Cargo.toml",
            "her/src/roles/README.md",
        ):
            with self.subTest(path=path):
                self.assertFalse(_classify.is_doc_only_path(path))


class ClassifyCli(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="checks-classify-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.repo = self.tmp / "repo"
        self.repo.mkdir()
        git(self.repo, "init", "-q", "-b", "main")
        self.base = self.commit({"README.md": "init\n", "her/src/lib.rs": "fn f() {}\n"}, "base")

    def commit(self, files: dict[str, str], message: str) -> str:
        for rel, content in files.items():
            target = self.repo / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
            git(self.repo, "add", rel)
        git(self.repo, "commit", "-q", "-m", message)
        return git(self.repo, "rev-parse", "HEAD")

    def read_code_output(self, out_file: Path) -> str:
        lines = out_file.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), 1, lines)
        return lines[0]

    def test_docs_only_commit_outputs_code_false(self):
        head = self.commit(
            {
                "AGENTS.md": "updated\n",
                "docs/agents/issue-tracker.md": "notes\n",
                "her/AGENTS.md": "crate doc\n",
            },
            "docs only",
        )
        out_file = self.tmp / "out.txt"
        done = run_classify(self.repo, self.base, head, out_file)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.read_code_output(out_file), "code=false")
        self.assertIn("code=false", done.stdout)

    def test_non_ascii_doc_path_outputs_code_false(self):
        head = self.commit({"docs/说明.md": "中文说明\n"}, "unicode doc")
        out_file = self.tmp / "out.txt"
        done = run_classify(self.repo, self.base, head, out_file)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.read_code_output(out_file), "code=false")

    def test_mixed_doc_and_code_outputs_code_true(self):
        head = self.commit(
            {"docs/agents/issue-tracker.md": "notes\n", "her/src/lib.rs": "fn g() {}\n"},
            "mixed",
        )
        out_file = self.tmp / "out.txt"
        done = run_classify(self.repo, self.base, head, out_file)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.read_code_output(out_file), "code=true")

    def test_shallow_depth_one_fetches_without_shared_history(self):
        docs_head = self.commit({"docs/guide.md": "hello\n"}, "docs commit")
        code_head = self.commit({"scripts/tool.py": "print(1)\n"}, "code commit")
        server = self.repo.as_uri()

        shallow = self.tmp / "shallow"
        shallow.mkdir()
        git(shallow, "init", "-q")
        git(shallow, "remote", "add", "origin", server)
        git(shallow, "fetch", "--depth=1", "origin", self.base)
        git(shallow, "fetch", "--depth=1", "origin", docs_head)
        git(shallow, "fetch", "--depth=1", "origin", code_head)

        out_docs = self.tmp / "out-docs.txt"
        done_docs = run_classify(shallow, self.base, docs_head, out_docs)
        self.assertEqual(done_docs.returncode, 0, done_docs.stderr)
        self.assertEqual(self.read_code_output(out_docs), "code=false")

        out_code = self.tmp / "out-code.txt"
        done_code = run_classify(shallow, docs_head, code_head, out_code)
        self.assertEqual(done_code.returncode, 0, done_code.stderr)
        self.assertEqual(self.read_code_output(out_code), "code=true")

    def test_fail_open_on_missing_or_zero_or_invalid_shas(self):
        zero = "0" * 40
        unknown = "a" * 40
        for idx, (base, head) in enumerate((
            (zero, self.base),
            (unknown, self.base),
            ("not-a-sha", self.base),
            ("", self.base),
            (self.base, unknown),
            (self.base, self.base),
        )):
            with self.subTest(base=base, head=head):
                out_file = self.tmp / f"out-{idx}.txt"
                done = run_classify(self.repo, base, head, out_file)
                self.assertEqual(done.returncode, 0, done.stderr)
                self.assertEqual(self.read_code_output(out_file), "code=true")

    def test_rename_from_code_tree_to_doc_outputs_code_true(self):
        added = self.commit({"her/tests/fixtures/doc.md": "# Title\n" + ("line\n" * 10)}, "add in tests tree")
        (self.repo / "docs").mkdir(parents=True, exist_ok=True)
        git(self.repo, "mv", "her/tests/fixtures/doc.md", "docs/moved-doc.md")
        git(self.repo, "commit", "-q", "-m", "move out of tests tree")
        moved = git(self.repo, "rev-parse", "HEAD")

        out_file = self.tmp / "out-rename.txt"
        done = run_classify(self.repo, added, moved, out_file)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.read_code_output(out_file), "code=true")


class WorkflowContract(unittest.TestCase):
    @staticmethod
    def job_block(text: str, name: str) -> str:
        match = re.search(rf"^  {re.escape(name)}:\n((?:    .*\n|\n)*)", text, re.M)
        assert match is not None, f"missing job {name}"
        return match.group(1)

    def test_checks_workflow_gates_heavy_jobs_and_keeps_scans_unconditional(self):
        text = WORKFLOW.read_text(encoding="utf-8")

        changes = self.job_block(text, "changes")
        self.assertIn("outputs:", changes)
        self.assertRegex(changes, r"\bcode:\s*\$\{\{\s*steps\.classify\.outputs\.code\s*\}\}")
        self.assertIn("classify-changes.py", changes)

        scans = self.job_block(text, "scans")
        self.assertNotIn("needs:", scans)
        self.assertNotIn("needs.changes", scans)
        self.assertIn("python -I scripts/ci/run-scans.py", scans)
        self.assertIn("python -I scripts/scan-public.py", scans)
        self.assertIn('"$HER_GITLEAKS" dir . --config .gitleaks.toml --redact --no-banner', scans)
        self.assertIn("powershell.exe -NoProfile -File scripts/lifecycle/test-ps-count.ps1", scans)

        ui_build = self.job_block(text, "ui-build")
        self.assertIn("name: ui-build (ubuntu-24.04)", ui_build)
        self.assertIn("needs: changes", ui_build)
        self.assertIn("if: needs.changes.outputs.code == 'true'", ui_build)
        self.assertIn("install-node.py", ui_build)
        self.assertIn("npm ci", ui_build)
        self.assertIn("npm run typecheck", ui_build)
        self.assertIn("npm run build", ui_build)
        self.assertIn("actions/upload-artifact@v4", ui_build)
        self.assertIn("name: her-ui-dist", ui_build)
        self.assertIn("path: ${{ env.PUBLIC_TREE }}/her-ui/dist", ui_build)

        for job in ("lint", "tests"):
            block = self.job_block(text, job)
            self.assertIn("needs: [changes, ui-build]", block, job)
            self.assertIn("if: needs.changes.outputs.code == 'true'", block, job)

        dsh_contract = self.job_block(text, "dsh-contract")
        self.assertIn("name: DSH credential contract (ubuntu-22.04)", dsh_contract)
        self.assertIn("needs: changes", dsh_contract)
        self.assertIn("if: needs.changes.outputs.code == 'true'", dsh_contract)
        self.assertIn('python -I "$PUBLIC_TREE/scripts/ci/dsh-contract.py"', dsh_contract)

        windows = self.job_block(text, "tests-windows")
        self.assertIn("needs: [changes, ui-build]", windows)
        self.assertIn("if: github.event_name == 'push' && needs.changes.outputs.code == 'true'", windows)

        lint = self.job_block(text, "lint")
        self.assertNotIn("install-node.py", lint)
        self.assertNotIn("npm run build", lint)

        for job in ("tests", "tests-windows"):
            block = self.job_block(text, job)
            self.assertIn("install-node.py", block, job)
            self.assertNotIn("npm run build", block, job)

        for job in ("lint", "tests", "tests-windows"):
            block = self.job_block(text, job)
            self.assertIn("actions/download-artifact@v4", block, job)
            self.assertIn("name: her-ui-dist", block, job)
            self.assertIn("path: ${{ env.PUBLIC_TREE }}/her-ui/dist", block, job)
            self.assertRegex(block, r"HER_REQUIRE_UI_DIST:\s*'1'", job)

    def test_checks_workflow_defines_always_run_checks_gate(self):
        text = WORKFLOW.read_text(encoding="utf-8")
        gate = self.job_block(text, "gate")

        self.assertIn("name: checks-gate", gate)
        self.assertIn("needs: [changes, scans, ui-build, lint, tests, dsh-contract]", gate)
        self.assertIn("if: always()", gate)
        for expr in (
            "needs.changes.result",
            "needs.changes.outputs.code",
            "needs.scans.result",
            "needs.ui-build.result",
            "needs.lint.result",
            "needs.tests.result",
            "needs.dsh-contract.result",
        ):
            self.assertIn(expr, gate)

        self.assertIn('if [ "$CODE_CHANGED" = "false" ]; then', gate)
        self.assertIn('"$UI_BUILD_RESULT" != "skipped"', gate)
        self.assertIn('"$LINT_RESULT" != "skipped"', gate)
        self.assertIn('"$TESTS_RESULT" != "skipped"', gate)
        self.assertIn('"$DSH_CONTRACT_RESULT" != "skipped"', gate)
        self.assertIn('if [ "$UI_BUILD_RESULT" != "success" ]; then', gate)
        self.assertIn('if [ "$LINT_RESULT" != "success" ]; then', gate)
        self.assertIn('if [ "$TESTS_RESULT" != "success" ]; then', gate)
        self.assertIn('if [ "$DSH_CONTRACT_RESULT" != "success" ]; then', gate)


if __name__ == "__main__":
    unittest.main()

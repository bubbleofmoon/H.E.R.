#!/usr/bin/env python3
"""Fake-codex tests for the receipts of scripts/codex-task.sh and scripts/codex-task.ps1.

Each case runs a launcher against a fake `codex` that writes (or does not write) the report named
by `-o`, and reads the exit receipt. A run that produced no report must never read as a pass:
a missing or empty report gives `no-report`, a usage-limit message in the error log gives `quota`.
The sh cases put the fake first on PATH; the ps1 cases run under pwsh with APPDATA pointing at a
fake package folder that holds the fake as codex.exe. POSIX only: the fake is a shell script.
Run: python -I scripts/tests/test_codex_task.py
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
SH = ROOT / 'scripts' / 'codex-task.sh'
PS1 = ROOT / 'scripts' / 'codex-task.ps1'
BASH = shutil.which('bash')
PWSH = shutil.which('pwsh')
POSIX = sys.platform != 'win32'
LIMIT = "ERROR: You've hit your usage limit. Try again later."

FAKE = '''#!/usr/bin/env bash
# Fake codex: FAKE_MODE chooses what the run leaves behind.
report=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then report=$2; shift; fi
  shift
done
cat > /dev/null
case "$FAKE_MODE" in
  report) printf 'looks fine\\nPASS\\n' > "$report" ;;
  report-fail) printf 'half done\\n' > "$report"; exit 3 ;;
  report-limit) printf 'looks fine\\n' > "$report"; echo "$FAKE_LIMIT" >&2 ;;
  empty) : > "$report" ;;
  quota) echo "$FAKE_LIMIT" >&2; exit 1 ;;
  quota-empty) : > "$report"; echo "$FAKE_LIMIT" >&2; exit 1 ;;
  crash) echo "boom" >&2; exit 2 ;;
esac
exit 0
'''


class Receipts:
    """Shared cases; a subclass supplies launch(mode) -> (return code, receipt folder, name)."""

    def run_case(self, mode):
        out = Path(tempfile.mkdtemp(prefix='codex-task-test-'))
        self.addCleanup(shutil.rmtree, out, ignore_errors=True)
        rc, name, said = self.launch(mode, out)
        exit_file = out / f'{name}.exit.txt'
        self.assertTrue(exit_file.is_file(), f'no exit receipt written: {said}')
        return rc, exit_file.read_text(encoding='ascii').strip(), out / f'{name}.report.md'

    def test_report_present_passes(self):
        rc, receipt, report = self.run_case('report')
        self.assertEqual((rc, receipt), (0, '0'))
        self.assertIn('PASS', report.read_text())

    def test_empty_report_is_no_report(self):
        rc, receipt, _ = self.run_case('empty')
        self.assertEqual(receipt, 'no-report')
        self.assertNotEqual(rc, 0)

    def test_missing_report_is_no_report(self):
        rc, receipt, _ = self.run_case('crash')
        self.assertEqual(receipt, 'no-report')
        self.assertNotEqual(rc, 0)

    def test_missing_report_with_exit_zero_is_no_report(self):
        rc, receipt, _ = self.run_case('none')
        self.assertEqual(receipt, 'no-report')
        self.assertNotEqual(rc, 0)

    def test_usage_limit_without_report_is_quota(self):
        rc, receipt, _ = self.run_case('quota')
        self.assertEqual(receipt, 'quota')
        self.assertNotEqual(rc, 0)

    def test_usage_limit_with_empty_report_is_quota(self):
        rc, receipt, _ = self.run_case('quota-empty')
        self.assertEqual(receipt, 'quota')
        self.assertNotEqual(rc, 0)

    def test_report_with_usage_limit_text_keeps_the_numeric_code(self):
        rc, receipt, _ = self.run_case('report-limit')
        self.assertEqual((rc, receipt), (0, '0'))

    def test_report_with_failing_exit_keeps_the_numeric_code(self):
        rc, receipt, _ = self.run_case('report-fail')
        self.assertEqual((rc, receipt), (3, '3'))


def prepare(out, mode):
    prompt = out / 'prompt.md'
    prompt.write_text('review\n', encoding='utf-8')
    work = out / 'work'
    work.mkdir()
    env = dict(os.environ, FAKE_MODE=mode, FAKE_LIMIT=LIMIT)
    return prompt, work, env


@unittest.skipUnless(POSIX and BASH, 'needs bash on a POSIX host')
class ShReceipts(Receipts, unittest.TestCase):
    def launch(self, mode, out):
        prompt, work, env = prepare(out, mode)
        bin_dir = out / 'bin'
        bin_dir.mkdir()
        fake = bin_dir / 'codex'
        fake.write_text(FAKE, encoding='utf-8')
        fake.chmod(0o755)
        env['PATH'] = f'{bin_dir}{os.pathsep}{env["PATH"]}'
        done = subprocess.run([BASH, str(SH), 'case', str(work), str(prompt), 'gpt-6-astra', 'high', 'read-only', '1'],
                              env=env, capture_output=True, text=True, timeout=120)
        return done.returncode, 'case', done.stdout + done.stderr


@unittest.skipUnless(POSIX and PWSH, 'needs pwsh on a POSIX host')
class Ps1Receipts(Receipts, unittest.TestCase):
    def launch(self, mode, out):
        prompt, work, env = prepare(out, mode)
        package = out / 'appdata' / 'npm' / 'node_modules' / '@openai' / 'codex' / 'node_modules' / '@openai' / \
            'codex-win32-x64' / 'vendor' / 'x86_64-pc-windows-msvc' / 'bin'
        package.mkdir(parents=True)
        fake = package / 'codex.exe'
        fake.write_text(FAKE, encoding='utf-8')
        fake.chmod(0o755)
        env['APPDATA'] = str(out / 'appdata')
        env['USERPROFILE'] = str(out / 'home')
        done = subprocess.run([PWSH, '-NoProfile', '-File', str(PS1), '-Name', 'case', '-WorkDir', str(work),
                               '-PromptFile', str(prompt), '-Sandbox', 'read-only', '-TimeoutMinutes', '1'],
                              env=env, capture_output=True, text=True, timeout=120)
        return done.returncode, 'case', done.stdout + done.stderr


if __name__ == '__main__':
    unittest.main()

#!/usr/bin/env python3
"""Tests for scripts/plan-export.sh and scripts/plan-export.ps1.

Each case copies the launcher into a throwaway repository whose origin/main differs from its
working tree (an uncommitted file and a local-only commit), points the launcher at a throwaway
orchestration root and checks what lands in <root>/<ticket>/scratch. The ps1 cases run under pwsh.
Run: python -I scripts/tests/test_plan_export.py
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
BASH = shutil.which('bash')
PWSH = shutil.which('pwsh')
POSIX = sys.platform != 'win32'
GIT_ENV = {
    'GIT_CONFIG_NOSYSTEM': '1', 'GIT_TERMINAL_PROMPT': '0',
    'GIT_AUTHOR_NAME': 'Export Test', 'GIT_AUTHOR_EMAIL': 'export.test@example.invalid',
    'GIT_COMMITTER_NAME': 'Export Test', 'GIT_COMMITTER_EMAIL': 'export.test@example.invalid',
}


class Export:
    """Shared cases; a subclass supplies launcher(), command(ticket, replace) and the orch folder."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix='plan-export-test-'))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.repo = self.tmp / 'repo'
        (self.repo / 'scripts').mkdir(parents=True)
        self.orch = self.tmp / 'orch'
        self.git('init', '-q', '-b', 'main')
        (self.repo / 'a.txt').write_text('main a\n', encoding='utf-8')
        (self.repo / 'sub').mkdir()
        (self.repo / 'sub' / 'b.txt').write_text('main b\n', encoding='utf-8')
        self.git('add', 'a.txt', 'sub')
        self.git('commit', '-q', '-m', 'main')
        self.main = self.git('rev-parse', 'HEAD')
        self.git('update-ref', 'refs/remotes/origin/main', self.main)
        (self.repo / 'a.txt').write_text('local a\n', encoding='utf-8')
        self.git('commit', '-q', '-am', 'local only')
        (self.repo / 'untracked.txt').write_text('x\n', encoding='utf-8')
        self.install()
        self.scratch = self.orch / '77' / 'scratch'

    def git(self, *args):
        done = subprocess.run(['git', *args], cwd=self.repo, env={**os.environ, **GIT_ENV},
                              capture_output=True, text=True)
        self.assertEqual(done.returncode, 0, done.stderr)
        return done.stdout.strip()

    def run_export(self, ticket='77', replace=False):
        done = subprocess.run(self.command(ticket, replace), cwd=self.repo, env={**os.environ, **GIT_ENV},
                              capture_output=True, text=True, timeout=120)
        return done.returncode, done.stdout + done.stderr

    def test_exports_origin_main_and_prints_the_commit(self):
        code, out = self.run_export()
        self.assertEqual(code, 0, out)
        self.assertIn(self.main, out)
        self.assertEqual((self.scratch / 'a.txt').read_text(), 'main a\n')
        self.assertEqual((self.scratch / 'sub' / 'b.txt').read_text(), 'main b\n')
        self.assertFalse((self.scratch / 'untracked.txt').exists())
        self.assertFalse((self.scratch / '.git').exists())

    def test_empty_existing_scratch_is_filled(self):
        self.scratch.mkdir(parents=True)
        code, out = self.run_export()
        self.assertEqual(code, 0, out)
        self.assertTrue((self.scratch / 'a.txt').is_file())

    def test_non_empty_scratch_is_refused_and_left_alone(self):
        self.scratch.mkdir(parents=True)
        (self.scratch / 'keep.txt').write_text('mine\n', encoding='utf-8')
        code, out = self.run_export()
        self.assertNotEqual(code, 0)
        self.assertIn('--replace', out.replace('-Replace', '--replace'))
        self.assertEqual((self.scratch / 'keep.txt').read_text(), 'mine\n')
        self.assertFalse((self.scratch / 'a.txt').exists())

    def test_replace_clears_scratch_and_scratch_evidence(self):
        evidence = self.orch / '77' / 'scratch-evidence'
        self.scratch.mkdir(parents=True)
        evidence.mkdir()
        (self.scratch / 'stale.txt').write_text('old\n', encoding='utf-8')
        (evidence / 'log.txt').write_text('old\n', encoding='utf-8')
        code, out = self.run_export(replace=True)
        self.assertEqual(code, 0, out)
        self.assertFalse((self.scratch / 'stale.txt').exists())
        self.assertFalse(evidence.exists())
        self.assertEqual((self.scratch / 'a.txt').read_text(), 'main a\n')

    def test_stale_scratch_evidence_alone_is_refused(self):
        evidence = self.orch / '77' / 'scratch-evidence'
        evidence.mkdir(parents=True)
        (evidence / 'log.txt').write_text('old\n', encoding='utf-8')
        code, out = self.run_export()
        self.assertNotEqual(code, 0, out)
        self.assertTrue((evidence / 'log.txt').exists())

    def test_ticket_must_be_a_number(self):
        for bad in ('../x', 'abc', '7 7', ''):
            with self.subTest(ticket=bad):
                code, out = self.run_export(ticket=bad)
                self.assertNotEqual(code, 0, out)
        self.assertFalse(self.orch.exists())

    def test_missing_origin_main_creates_nothing(self):
        self.git('update-ref', '-d', 'refs/remotes/origin/main')
        code, out = self.run_export()
        self.assertNotEqual(code, 0)
        self.assertFalse(self.scratch.exists())


@unittest.skipUnless(POSIX and BASH, 'needs bash on a POSIX host')
class ShExport(Export, unittest.TestCase):
    def install(self):
        script = self.repo / 'scripts' / 'plan-export.sh'
        shutil.copy(ROOT / 'scripts' / 'plan-export.sh', script)
        script.chmod(0o755)

    def command(self, ticket, replace):
        return ['env', f'HER_ORCH_ROOT={self.orch}', BASH, str(self.repo / 'scripts' / 'plan-export.sh'), ticket, *(['--replace'] if replace else [])]


@unittest.skipUnless(POSIX and PWSH, 'needs pwsh on a POSIX host')
class Ps1Export(Export, unittest.TestCase):
    def install(self):
        shutil.copy(ROOT / 'scripts' / 'plan-export.ps1', self.repo / 'scripts' / 'plan-export.ps1')

    def command(self, ticket, replace):
        return [PWSH, '-NoProfile', '-File', str(self.repo / 'scripts' / 'plan-export.ps1'),
                '-Ticket', ticket, '-OrchRoot', str(self.orch), *(['-Replace'] if replace else [])]


if __name__ == '__main__':
    unittest.main()

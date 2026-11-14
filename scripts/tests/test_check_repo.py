#!/usr/bin/env python3
"""Tests for the tracked-file mode check in scripts/check-repo.py.

Each case builds a throwaway git repository in a temporary directory, copies check-repo.py
into it, stages one hook script with a chosen git mode (set through the index, so the result
does not depend on the host's filesystem) and runs the copy. The assertions are about the exit
code and the mode finding line. Run: python -I scripts/tests/test_check_repo.py
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent.parent
CHECK = ROOT / 'scripts' / 'check-repo.py'
GIT_ENV = {
    'GIT_CONFIG_NOSYSTEM': '1', 'GIT_TERMINAL_PROMPT': '0',
    'GIT_AUTHOR_NAME': 'Check Test', 'GIT_AUTHOR_EMAIL': 'check.test@example.invalid',
    'GIT_COMMITTER_NAME': 'Check Test', 'GIT_COMMITTER_EMAIL': 'check.test@example.invalid',
}


class FileModeCheck(unittest.TestCase):
    def setUp(self):
        self.repo = Path(tempfile.mkdtemp(prefix='check-repo-test-'))
        self.addCleanup(shutil.rmtree, self.repo, ignore_errors=True)
        self.git('init', '-q')
        (self.repo / 'scripts').mkdir()
        shutil.copyfile(CHECK, self.repo / 'scripts' / 'check-repo.py')

    def git(self, *args):
        env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
        env.update(GIT_ENV)
        subprocess.run(['git', *args], cwd=self.repo, env=env, check=True, capture_output=True)

    def stage_hook(self, mode_flag):
        hook = self.repo / '.githooks' / 'pre-push'
        hook.parent.mkdir(parents=True, exist_ok=True)
        hook.write_text('#!/usr/bin/env bash\nexit 0\n', encoding='utf-8', newline='\n')
        self.git('add', '.githooks/pre-push')
        self.git('update-index', f'--chmod={mode_flag}', '.githooks/pre-push')

    def run_check(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}   # the fixture's repository only
        return subprocess.run([sys.executable, '-I', str(self.repo / 'scripts' / 'check-repo.py')],
                              cwd=self.repo, capture_output=True, text=True, env=env)

    def test_non_executable_hook_fails(self):
        self.stage_hook('-x')
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('mode: .githooks/pre-push is 100644, expected 100755', result.stdout)

    def test_executable_hook_passes(self):
        self.stage_hook('+x')
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn('mode:', result.stdout)

    def test_inherited_git_variables_do_not_point_the_check_at_another_repository(self):
        # A decoy repository holds the bad mode; the fixture holds the good one. The inherited
        # variables must not make the check read the decoy's index.
        decoy = Path(tempfile.mkdtemp(prefix='check-repo-decoy-'))
        self.addCleanup(shutil.rmtree, decoy, ignore_errors=True)
        env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
        env.update(GIT_ENV)

        def decoy_git(*args):
            subprocess.run(['git', '-C', str(decoy), *args], env=env, check=True, capture_output=True)
        decoy_git('init', '-q')
        (decoy / '.githooks').mkdir()
        (decoy / '.githooks' / 'pre-push').write_text('#!/usr/bin/env bash\nexit 0\n', encoding='utf-8', newline='\n')
        decoy_git('add', '.githooks/pre-push')
        decoy_git('update-index', '--chmod=-x', '.githooks/pre-push')
        self.stage_hook('+x')
        inherited = {'GIT_DIR': str(decoy / '.git'), 'GIT_WORK_TREE': str(decoy),
                     'GIT_INDEX_FILE': str(decoy / '.git' / 'index')}
        with mock.patch.dict(os.environ, inherited):
            result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn('mode:', result.stdout)


if __name__ == '__main__':
    unittest.main(verbosity=2)

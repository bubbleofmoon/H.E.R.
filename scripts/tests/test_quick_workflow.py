#!/usr/bin/env python3
"""Tests for the dispatch contract of .github/workflows/quick.yml.

The concurrency group is checked as text. The "Fetch source" step is cut out of the workflow and run
under bash against a throwaway server repository (GITHUB_SERVER_URL points at a file:// folder), so
the `sha` input is exercised for real: an ancestor of the ref head is checked out, anything else
fails the step. POSIX only (file:// URLs, bash). Run: python -I scripts/tests/test_quick_workflow.py
"""
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
WORKFLOW = ROOT / '.github' / 'workflows' / 'quick.yml'
BASH = shutil.which('bash')
GIT_ENV = {
    'GIT_CONFIG_NOSYSTEM': '1', 'GIT_TERMINAL_PROMPT': '0',
    'GIT_AUTHOR_NAME': 'Quick Test', 'GIT_AUTHOR_EMAIL': 'quick.test@example.invalid',
    'GIT_COMMITTER_NAME': 'Quick Test', 'GIT_COMMITTER_EMAIL': 'quick.test@example.invalid',
}


def clean_env():
    """This process's environment without any GIT_* variable (as test_agent_guard.py does), plus the fixed identity."""
    env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
    env.update(GIT_ENV)
    return env


def step_script(name):
    """The `run: |` body of the named step."""
    lines = WORKFLOW.read_text(encoding='utf-8').splitlines()
    start = lines.index(f'      - name: {name}')
    body = lines.index('        run: |', start) + 1
    out = []
    for line in lines[body:]:
        if line.strip() and not line.startswith(' ' * 10):
            break
        out.append(line[10:])
    return '\n'.join(out) + '\n'


class Concurrency(unittest.TestCase):
    def test_group_names_ref_target_os_and_filter(self):
        text = WORKFLOW.read_text(encoding='utf-8')
        self.assertIn('group: quick-${{ github.ref }}-${{ inputs.target }}-${{ inputs.os }}-${{ inputs.filter }}-${{ inputs.sha }}', text)

    def test_run_name_keeps_the_plain_title_and_appends_the_sha_only_when_set(self):
        text = WORKFLOW.read_text(encoding='utf-8')
        self.assertIn("run-name: quick ${{ inputs.os }} target=${{ inputs.target }} filter=${{ inputs.filter }}"
                      "${{ inputs.sha && format(' sha={0}', inputs.sha) || '' }}\n", text)

    def test_sha_is_an_optional_input(self):
        text = WORKFLOW.read_text(encoding='utf-8')
        block = re.search(r'^      sha:\n((?:        .*\n)+)', text, re.M)
        self.assertIsNotNone(block, 'no sha input')
        self.assertIn('required: false', block.group(1))


@unittest.skipUnless(sys.platform != 'win32' and BASH, 'needs bash on a POSIX host')
class FetchSource(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix='quick-workflow-test-'))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.server = self.tmp / 'srv' / 'owner' / 'repo.git'
        self.server.mkdir(parents=True)
        self.git(self.server, 'init', '-q', '-b', 'main')
        self.first = self.commit('one')
        self.second = self.commit('two')
        self.git(self.server, 'checkout', '-q', '-b', 'side', self.first)
        self.side = self.commit('side')
        self.git(self.server, 'checkout', '-q', 'main')
        self.tree = self.tmp / 'tree'

    def git(self, cwd, *args):
        done = subprocess.run(['git', *args], cwd=cwd, env=clean_env(),
                              capture_output=True, text=True, check=True)
        return done.stdout.strip()

    def commit(self, text):
        (self.server / 'f.txt').write_text(text, encoding='utf-8')
        self.git(self.server, 'add', 'f.txt')
        self.git(self.server, 'commit', '-q', '-m', text)
        return self.git(self.server, 'rev-parse', 'HEAD')

    def fetch(self, sha=''):
        env = {**clean_env(), 'PUBLIC_TREE': str(self.tree), 'QUICK_SHA': sha, 'GITHUB_SHA': self.second,
               'GITHUB_REF': 'refs/heads/main', 'GITHUB_REPOSITORY': 'owner/repo',
               'GITHUB_SERVER_URL': (self.tmp / 'srv').as_uri()}
        return subprocess.run([BASH, '-e', '-c', step_script('Fetch source')], env=env, capture_output=True, text=True)

    def head(self):
        return self.git(self.tree, 'rev-parse', 'HEAD')

    def test_no_sha_checks_out_the_ref_head(self):
        done = self.fetch()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.head(), self.second)

    def test_ancestor_sha_is_checked_out(self):
        done = self.fetch(self.first)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.head(), self.first)

    def test_abbreviated_ancestor_sha_is_checked_out(self):
        done = self.fetch(self.first[:10])
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.head(), self.first)

    def test_ref_head_itself_is_allowed(self):
        done = self.fetch(self.second)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.head(), self.second)

    def test_commit_outside_the_ref_history_fails(self):
        done = self.fetch(self.side)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn('not an ancestor', done.stdout + done.stderr)

    def test_commit_already_in_the_tree_but_outside_the_ref_history_fails(self):
        # The side commit is fetched before the step runs, so only the ancestry clause can refuse it.
        self.tree.mkdir()
        self.git(self.tree, 'init', '-q')
        self.git(self.tree, 'fetch', '-q', str(self.server), 'side')
        self.assertEqual(self.git(self.tree, 'rev-parse', '--verify', f'{self.side}^{{commit}}'), self.side)
        done = self.fetch(self.side)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn('not an ancestor', done.stdout + done.stderr)
        head = subprocess.run(['git', 'rev-parse', '-q', '--verify', 'HEAD'], cwd=self.tree, env=clean_env(),
                              capture_output=True, text=True)
        self.assertNotEqual(head.stdout.strip(), self.side)

    def test_unknown_sha_fails(self):
        done = self.fetch('abcdef' * 6 + 'abcd')
        self.assertNotEqual(done.returncode, 0)
        self.assertIn('not an ancestor', done.stdout + done.stderr)

    def test_value_that_is_not_a_sha_fails(self):
        for bad in ('--help', 'main', 'HEAD~1', 'abc'):
            with self.subTest(sha=bad):
                done = self.fetch(bad)
                self.assertNotEqual(done.returncode, 0)
                self.assertIn('not a commit sha', done.stdout + done.stderr)
                shutil.rmtree(self.tree, ignore_errors=True)


if __name__ == '__main__':
    unittest.main()

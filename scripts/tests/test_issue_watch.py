#!/usr/bin/env python3
"""Tests for scripts/issue_watch.py (the baton watcher) with GitHub replaced by a fake.

The module is loaded from its file, HOME and USERPROFILE point at a temporary folder, and the
token and the HTTP fetch are replaced, so no case contacts GitHub or touches the real home.
Run: python -I scripts/tests/test_issue_watch.py
"""
import importlib.util
import io
import os
from pathlib import Path
import shutil
import sys
import tempfile
from contextlib import redirect_stdout
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent.parent
PUBLIC = 'bubbleofmoon/' + 'H.E.R.'
VAULT = PUBLIC + '-vault'


def load_module():
    spec = importlib.util.spec_from_file_location('issue_watch', ROOT / 'scripts' / 'issue_watch.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FakeGitHub:
    """Answers the issue listing of each repository from `issues`; `unchanged` answers 304."""

    def __init__(self):
        self.issues = {}
        self.unchanged = False
        self.urls = []

    def __call__(self, url, tok, etag):
        self.urls.append(url)
        if self.unchanged:
            return etag, None
        for repo, issues in self.issues.items():
            if f'/repos/{repo}/issues?' in url:
                return '"etag-' + repo + '"', list(issues)
        return None, []


def issue(number, title='a held baton', pull_request=False):
    item = {'number': number, 'title': title, 'html_url': f'https://github.example.invalid/i/{number}'}
    if pull_request:
        item['pull_request'] = {}
    return item


class IssueWatch(unittest.TestCase):
    def setUp(self):
        self.home = Path(tempfile.mkdtemp(prefix='issue-watch-home-'))
        self.addCleanup(shutil.rmtree, self.home, True)
        patcher = mock.patch.dict(os.environ, {'HOME': str(self.home), 'USERPROFILE': str(self.home)})
        patcher.start()
        self.addCleanup(patcher.stop)
        self.module = load_module()
        self.module.token = lambda: 't'
        self.github = FakeGitHub()
        self.module.fetch = self.github

    def run_watch(self, *args):
        buf = io.StringIO()
        with mock.patch.object(sys, 'argv', ['issue_watch.py', *args]), redirect_stdout(buf):
            code = self.module.main()
        return code, buf.getvalue()

    # --- one state per repository ----------------------------------------------------
    def test_switching_repos_never_re_reports_held_issues(self):
        self.github.issues = {PUBLIC: [issue(5)], VAULT: [issue(7)]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 0)
        self.assertIn('BATON #5', out)
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', VAULT)
        self.assertEqual(code, 0)
        self.assertIn('BATON #7', out)
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), 'no batons', out)

    def test_default_state_file_names_machine_and_repo(self):
        self.github.issues = {PUBLIC: [issue(5)]}
        code, out = self.run_watch('--machine', 'linux', '--once')
        self.assertEqual(code, 0)
        self.assertIn('BATON #5', out)
        self.assertTrue((self.home / '.her-baton-watch-linux-bubbleofmoon-H.E.R..json').is_file())
        self.assertTrue(any(f'/repos/{PUBLIC}/issues?' in url for url in self.github.urls), self.github.urls)

    def test_a_malformed_repo_is_refused_without_a_state_file(self):
        for repo in ('a/b/c', '../x', 'a/..', 'a', 'a b/c'):
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', repo)
            self.assertEqual(code, 2, f'{repo}: {out}')
            self.assertEqual(list(self.home.iterdir()), [], repo)

    def test_an_explicit_state_file_is_used(self):
        state = self.home / 'chosen-state.json'
        self.github.issues = {PUBLIC: [issue(5)]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(code, 0)
        self.assertIn('BATON #5', out)
        self.assertTrue(state.is_file())
        self.assertFalse((self.home / '.her-baton-watch-linux-bubbleofmoon-H.E.R..json').exists())

    # --- what wakes the watcher -----------------------------------------------------
    def test_an_issue_that_returns_is_reported_again(self):
        self.github.issues = {PUBLIC: [issue(5)]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertIn('BATON #5', out)
        self.github.issues = {PUBLIC: []}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out.strip(), 'no batons')
        self.github.issues = {PUBLIC: [issue(5)]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertIn('BATON #5', out)

    def test_pull_requests_and_unchanged_answers_wake_nobody(self):
        self.github.issues = {PUBLIC: [issue(9, pull_request=True)]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out.strip(), 'no batons', out)
        state = self.home / 'state.json'
        state.write_text('{"etag": "held", "holding": [5]}', encoding='utf-8')
        self.github.unchanged = True
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out.strip(), 'no batons', out)
        self.assertIn('[5]', state.read_text(encoding='utf-8'))


if __name__ == '__main__':
    unittest.main(verbosity=2)

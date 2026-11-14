#!/usr/bin/env python3
"""Tests for scripts/issue_watch.py (the baton watcher) with GitHub replaced by a fake.

The module is loaded from its file, HOME and USERPROFILE point at a temporary folder, and the
token and the HTTP fetch are replaced, so no case contacts GitHub or touches the real home.
Run: python -I scripts/tests/test_issue_watch.py
"""
import email.message
import gc
import importlib.util
import io
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import urllib.error
import warnings
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
            return etag, None, None
        for repo, issues in self.issues.items():
            if f'/repos/{repo}/issues?' in url:
                return '"etag-' + repo + '"', list(issues), None
        return None, [], None


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
        self.real_fetch = self.module.fetch
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

    # --- names, state writes and rate limits (fix round 1) ---------------------------------
    def test_repository_names_are_encoded_without_collisions(self):
        one = Path(self.module.default_state_path('linux', 'a/b-c')).name
        two = Path(self.module.default_state_path('linux', 'a-b/c')).name
        self.assertNotEqual(one, two)
        self.assertEqual(one, '.her-baton-watch-linux-a-b%2Dc.json')
        self.assertEqual(two, '.her-baton-watch-linux-a%2Db-c.json')

    def test_a_failed_write_keeps_the_old_state(self):
        state = self.home / 'state.json'
        state.write_text('{"etag": "old", "holding": [5]}', encoding='utf-8')
        with mock.patch.object(self.module.json, 'dump', side_effect=OSError('disk full')):
            with self.assertRaises(OSError):
                self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(state.read_text(encoding='utf-8'), '{"etag": "old", "holding": [5]}')
        self.assertEqual([p.name for p in self.home.iterdir() if p.suffix == '.tmp'], [])

    def test_the_state_file_is_closed(self):
        state = self.home / 'state.json'
        state.write_text('{"etag": null, "holding": []}', encoding='utf-8')
        gc.collect()                       # earlier tests' HTTP error objects are finalised outside this check
        with warnings.catch_warnings(record=True) as seen:
            warnings.simplefilter('always', ResourceWarning)
            self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
            gc.collect()
        self.assertEqual([w for w in seen if issubclass(w.category, ResourceWarning)], [])

    def http_error(self, code, headers):
        message = email.message.Message()
        for key, value in headers.items():
            message[key] = value
        return urllib.error.HTTPError('https://api.github.com/x', code, 'error', message, io.BytesIO(b''))

    def test_a_rate_limit_waits_until_the_reset(self):
        reset = str(int(time.time()) + 30)
        error = self.http_error(403, {'X-RateLimit-Remaining': '0', 'X-RateLimit-Reset': reset})
        with mock.patch.object(self.module.urllib.request, 'urlopen', side_effect=error):
            etag, issues, wait = self.real_fetch('https://api.github.com/x', 't', None)
        self.assertIsNone(issues)
        self.assertTrue(30 <= wait <= 32, wait)

    def test_retry_after_is_honoured(self):
        with mock.patch.object(self.module.urllib.request, 'urlopen', side_effect=self.http_error(429, {'Retry-After': '7'})):
            self.assertEqual(self.real_fetch('https://api.github.com/x', 't', None)[2], 8)

    def test_a_forbidden_answer_without_rate_limit_headers_exits_2(self):
        with mock.patch.object(self.module.urllib.request, 'urlopen', side_effect=self.http_error(403, {})):
            with self.assertRaises(SystemExit) as stop:
                self.real_fetch('https://api.github.com/x', 't', None)
        self.assertEqual(stop.exception.code, 2)

    def test_the_watcher_sleeps_out_a_rate_limit_then_reports(self):
        answers = iter([(None, None, 5), (None, [issue(5)], None)])
        self.module.fetch = lambda url, tok, etag: next(answers)
        with mock.patch.object(self.module.time, 'sleep') as sleep:
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 0)
        self.assertIn('BATON #5', out)
        sleep.assert_called_once_with(5)

    def test_once_without_an_answer_exits_2(self):
        self.module.fetch = lambda url, tok, etag: (None, None, 0)
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 2)
        self.assertNotIn('no batons', out)

    def test_a_full_page_is_refused(self):
        self.module.fetch = lambda url, tok, etag: (None, [issue(n) for n in range(1, 101)], None)
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 2)


if __name__ == '__main__':
    unittest.main(verbosity=2)

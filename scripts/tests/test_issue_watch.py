#!/usr/bin/env python3
"""Tests for scripts/issue_watch.py (the baton watcher) with GitHub replaced by a fake.

The module is loaded from its file, HOME and USERPROFILE point at a temporary folder, and the
token and the HTTP fetch are replaced, so no case contacts GitHub or touches the real home.
Run: python -I scripts/tests/test_issue_watch.py
"""
import calendar
import email.message
import gc
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import types
import urllib.error
import urllib.parse
import warnings
from contextlib import redirect_stderr, redirect_stdout
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
    """Answers the issue listing of each repository from `issues`; `unchanged` answers 304.

    Comment pages come from `comments` (issue number -> list of comments, oldest first), cut to the comments updated
    since the `since` argument when there is one; `fail_comments` makes them unanswered. `reads` lists every answered
    comment page as (issue number, page, since or None); `after_page`, when set, is called with (issue number, page)
    after a page was answered, so a test can change the comments between two pages.
    """

    def __init__(self):
        self.issues = {}
        self.comments = {}
        self.fail_comments = False
        self.comment_wait = 0
        self.unchanged = False
        self.urls = []
        self.etags = []
        self.reads = []
        self.after_page = None

    def __call__(self, url, tok, etag):
        self.urls.append(url)
        self.etags.append(etag)
        if '/comments?' in url:
            if self.fail_comments:
                return None, None, self.comment_wait
            number = int(url.split('/issues/')[1].split('/')[0])
            query = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)
            page = int(query['page'][0])
            items = self.comments.get(number, [])
            since = query['since'][0] if 'since' in query else None
            if since:
                items = [c for c in items if c['updated_at'] >= since]
            answer = items[(page - 1) * 100:page * 100]
            self.reads.append((number, page, since))
            if self.after_page:
                self.after_page(number, page)
            return None, answer, None
        if self.unchanged:
            return etag, None, None
        for repo, issues in self.issues.items():
            if f'/repos/{repo}/issues?' in url:
                return '"etag-' + repo + '"', list(issues), None
        return None, [], None


def stamp(k):
    """The k-th second of a fixed past hour, in GitHub's format; comment k is created at stamp(k)."""
    return f'2026-01-01T10:{k // 60:02d}:{k % 60:02d}Z'


def issue(number, title='a held baton', pull_request=False, comments=0, updated_at=None):
    """A listed issue; its `updated_at` is the creation time of its newest comment unless given."""
    item = {'number': number, 'title': title, 'html_url': f'https://github.example.invalid/i/{number}',
            'comments': comments, 'updated_at': updated_at or stamp(comments)}
    if pull_request:
        item['pull_request'] = {}
    return item


def comment(k, body, at=None):
    return {'id': k, 'html_url': f'https://github.example.invalid/c/{k}', 'body': body,
            'created_at': at or stamp(k), 'updated_at': at or stamp(k)}


class StopFollow(Exception):
    """Raised by the replaced sleep to end a `--follow` run, which never ends by itself."""


class Lines(io.StringIO):
    """Standard output that remembers how much of its text was flushed."""
    flushed = 0

    def flush(self):
        self.flushed = len(self.getvalue())
        super().flush()


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

    def http_error(self, code, headers, body=b''):
        message = email.message.Message()
        for key, value in headers.items():
            message[key] = value
        return urllib.error.HTTPError('https://api.github.com/x', code, 'error', message, io.BytesIO(body))

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
        answers = iter([(None, None, 500), (None, [issue(5)], None)])
        self.module.fetch = lambda url, tok, etag: (None, [], None) if '/comments' in url else next(answers)
        with mock.patch.object(self.module.time, 'sleep') as sleep:
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 0)
        self.assertIn('BATON #5', out)
        sleep.assert_called_once_with(500)

    def test_once_without_an_answer_exits_2(self):
        self.module.fetch = lambda url, tok, etag: (None, None, 0)
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 2)
        self.assertNotIn('no batons', out)

    def test_a_full_page_is_refused(self):
        self.module.fetch = lambda url, tok, etag: (None, [issue(n) for n in range(1, 101)], None)
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 2)

    # --- the shared API budget (#90) ----------------------------------------------------------
    def test_the_interval_is_at_least_180_seconds(self):
        self.github.issues = {PUBLIC: []}
        slept = []

        class Stop(Exception):
            pass

        def stop(seconds):
            slept.append(seconds)
            raise Stop

        with mock.patch.object(self.module.time, 'sleep', stop), self.assertRaises(Stop):
            self.run_watch('--machine', 'linux', '--repo', PUBLIC)
        self.assertEqual(slept, [180])
        for value in ('179', '60', 'x'):
            with self.subTest(value=value), redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as caught:
                self.run_watch('--machine', 'linux', '--once', '--interval', value)
            self.assertEqual(caught.exception.code, 2)
        code, out = self.run_watch('--machine', 'linux', '--once', '--interval', '180')
        self.assertEqual(code, 0)

    def test_a_short_rate_limit_wait_still_waits_one_interval(self):
        answers = iter([(None, None, 5), (None, [issue(5)], None)])
        self.module.fetch = lambda url, tok, etag: (None, [], None) if '/comments' in url else next(answers)
        with mock.patch.object(self.module.time, 'sleep') as sleep:
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 0)
        self.assertIn('BATON #5', out)
        sleep.assert_called_once_with(180)

    def test_a_secondary_rate_limit_without_headers_is_waited_out(self):
        body = b'{"message": "You have exceeded a secondary rate limit."}'
        with mock.patch.object(self.module.urllib.request, 'urlopen', side_effect=self.http_error(403, {}, body)):
            etag, issues, wait = self.real_fetch('https://api.github.com/x', 't', None)
        self.assertIsNone(issues)
        self.assertEqual(wait, 180)

    # --- a new handoff comment wakes the side that holds the baton (#134) -------------------
    def test_a_new_handoff_comment_on_a_held_issue_wakes(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'BATON #67 a held baton https://github.example.invalid/i/67\n')
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green\n\nmore'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(code, 0)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')

    def test_other_comments_never_wake(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67].append(comment(2, 'progress note'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out.strip(), 'no batons', out)
        self.github.issues = {PUBLIC: [issue(67, comments=3)]}
        self.github.comments[67].append(comment(3, '**Handoff → linux: next'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/3\n')

    def test_a_handoff_to_the_other_side_never_wakes(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67].append(comment(2, '**Handoff → windows**'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out.strip(), 'no batons', out)
        self.github.issues = {PUBLIC: [issue(67, comments=3)]}
        self.github.comments[67].append(comment(3, '**Handoff → linux: next'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/3\n')

    def test_a_failed_comment_read_is_retried(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        before = json.loads(state.read_text(encoding='utf-8'))['comments']
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green'))
        self.github.fail_comments = True
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(code, 2)
        self.assertEqual(out, '')
        saved = json.loads(state.read_text(encoding='utf-8'))
        self.assertIsNone(saved['etag'])
        self.assertEqual(saved['comments'], before)
        self.github.fail_comments = False
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')

    def test_a_long_read_follows_the_pages(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.github.issues = {PUBLIC: [issue(67, comments=200)]}
        self.github.comments[67] += [comment(k, 'progress') for k in range(2, 200)]
        self.github.comments[67].append(comment(200, '**Handoff → linux: R5 green'))
        first = len(self.github.reads)
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/200\n')
        self.assertEqual({page for number, page, since in self.github.reads[first:]}, {1, 2, 3}, self.github.reads)

    def test_a_deletion_between_two_pages_never_hides_a_handoff(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.github.issues = {PUBLIC: [issue(67, comments=101)]}
        self.github.comments[67] += [comment(k, 'progress') for k in range(2, 101)]
        self.github.comments[67].append(comment(101, '**Handoff → linux: R5 green'))

        def delete_the_first(number, page):
            if page == 1 and self.github.comments[67][0]['id'] == 1:
                del self.github.comments[67][0]      # after page 1 was served, comment 101 moves up to place 100

        self.github.after_page = delete_the_first
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/101\n')

    def test_an_old_state_file_without_comment_counts_loads(self):
        state = self.home / 'state.json'
        state.write_text('{"etag": null, "holding": [67]}', encoding='utf-8')
        self.github.issues = {PUBLIC: [issue(67, comments=5)]}
        self.github.comments = {67: [comment(k, 'progress') for k in range(1, 6)]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), 'no batons', out)
        self.assertEqual(len(self.github.reads), 1, self.github.reads)      # the first sighting is read once
        self.github.issues = {PUBLIC: [issue(67, comments=6)]}
        self.github.comments[67].append(comment(6, '**Handoff → linux: R4 green'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/6\n')

    def test_a_state_file_with_plain_counts_migrates(self):
        state = self.home / 'state.json'
        state.write_text(json.dumps({'etag': '"E"', 'holding': [67], 'comments': {'67': 1}}), encoding='utf-8')
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments = {67: [comment(1, 'progress'), comment(2, '**Handoff → linux: R4 green')]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertIsNone(self.github.etags[0])
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')
        self.github.issues = {PUBLIC: [issue(67, comments=3)]}
        self.github.comments[67].append(comment(3, '**Handoff → linux: next'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/3\n')

    def test_a_plain_count_with_a_deleted_comment_still_reports_the_handoff(self):
        state = self.home / 'state.json'
        state.write_text(json.dumps({'etag': None, 'holding': [67], 'comments': {'67': 1}}), encoding='utf-8')
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}      # the old comment and the handoff were counted
        self.github.comments = {67: [comment(2, '**Handoff → linux: R4 green')]}      # the old one is gone at the read
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out.strip(), 'no batons', out)

    # --- a deleted comment never hides a handoff (#136) ----------------------------------------------
    def test_a_comment_deleted_before_the_read_never_hides_a_handoff(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}      # the listing counted the old comment and the handoff
        self.github.comments = {67: [comment(2, '**Handoff → linux: R4 green')]}      # the old one is gone at the read
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')
        self.assertEqual({since for number, page, since in self.github.reads}, {'2026-01-01T09:50:01Z'}, self.github.reads)
        self.github.issues = {PUBLIC: [issue(67, comments=1, updated_at=stamp(3))]}      # the deletion itself
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out.strip(), 'no batons', out)

    def test_a_deleted_newest_comment_and_an_unchanged_count_never_hide_a_handoff(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67].append(comment(2, 'progress'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out.strip(), 'no batons', out)
        self.github.issues = {PUBLIC: [issue(67, comments=2, updated_at=stamp(3))]}      # comment 2 deleted, comment 3 added
        self.github.comments = {67: [comment(1, 'progress'), comment(3, '**Handoff → linux: R4 green')]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/3\n')

    def test_an_unchanged_old_listing_reads_no_comments_after_the_first_sighting(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, '**Handoff → linux: old')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(len(self.github.reads), 1, self.github.reads)      # the first sighting is read once
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out.strip(), 'no batons', out)
        self.assertEqual(len(self.github.reads), 1, self.github.reads)

    def test_a_same_second_arrival_after_the_first_sighting_is_reported(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1, updated_at=stamp(5))]}
        self.github.comments = {67: [comment(1, 'progress', at=stamp(5))]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'BATON #67 a held baton https://github.example.invalid/i/67\n')
        self.github.issues = {PUBLIC: [issue(67, comments=2, updated_at=stamp(5))]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green', at=stamp(5)))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')

    def test_a_replaced_comment_with_an_equal_count_and_time_is_read_while_the_update_is_young(self):
        state = self.home / 'state.json'
        now = calendar.timegm(time.strptime(stamp(5), '%Y-%m-%dT%H:%M:%SZ')) + 1
        self.github.issues = {PUBLIC: [issue(67, comments=2, updated_at=stamp(5))]}
        self.github.comments = {67: [comment(1, 'progress', at=stamp(4)), comment(2, 'progress', at=stamp(5))]}
        with mock.patch.object(self.module.time, 'time', return_value=now):
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
            self.assertEqual(out, 'BATON #67 a held baton https://github.example.invalid/i/67\n')
            self.assertIsNone(json.loads(state.read_text(encoding='utf-8'))['etag'])      # the listing is asked again
            self.github.comments = {67: [comment(1, 'progress', at=stamp(4)),
                                         comment(3, '**Handoff → linux: R4 green', at=stamp(5))]}      # 2 deleted, 3 added
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/3\n')

    def test_an_old_unchanged_update_is_not_read_again(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments = {67: [comment(1, 'progress'), comment(2, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertIsNotNone(json.loads(state.read_text(encoding='utf-8'))['etag'])
        self.assertEqual(len(self.github.reads), 1, self.github.reads)
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(len(self.github.reads), 1, self.github.reads)

    def test_a_reported_handoff_is_never_reported_again(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')
        self.github.issues = {PUBLIC: [issue(67, comments=3)]}      # a progress comment and an edit of the handoff
        self.github.comments[67][1]['updated_at'] = stamp(3)
        self.github.comments[67].append(comment(3, 'progress'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out.strip(), 'no batons', out)

    def test_an_old_handoff_edited_later_is_not_new(self):
        state = self.home / 'state.json'
        state.write_text(json.dumps({'etag': None, 'holding': [67], 'comments': {
            '67': {'count': 3, 'updated_at': stamp(2000), 'seen': [], 'recent': False}}}), encoding='utf-8')
        self.github.issues = {PUBLIC: [issue(67, comments=4, updated_at=stamp(2100))]}
        old = dict(comment(1, '**Handoff → linux: old', at=stamp(1)), updated_at=stamp(2050))      # edited in the window
        self.github.comments = {67: [old, comment(2, '**Handoff → linux: new', at=stamp(2100))]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')

    def test_only_the_newest_handoff_ids_are_kept(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=60)]}
        self.github.comments = {67: [comment(k, '**Handoff → linux: round') for k in range(1, 61)]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        saved = json.loads(state.read_text(encoding='utf-8'))
        self.assertEqual(saved['comments']['67']['seen'], list(range(11, 61)))

    def test_a_failed_first_read_is_made_again(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, '**Handoff → linux: old')]}
        self.github.fail_comments = True
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'BATON #67 a held baton https://github.example.invalid/i/67\n')
        saved = json.loads(state.read_text(encoding='utf-8'))
        self.assertNotIn('67', saved['comments'])
        self.assertIsNone(saved['etag'])
        self.github.fail_comments = False
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out.strip(), 'no batons', out)
        self.assertIn('67', json.loads(state.read_text(encoding='utf-8'))['comments'])

    def test_a_newly_landed_baton_prints_only_baton(self):
        self.github.issues = {PUBLIC: [issue(67, comments=3)]}
        self.github.comments = {67: [comment(1, 'progress'), comment(2, 'progress'),
                                     comment(3, '**Handoff → linux: R4 green')]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'BATON #67 a held baton https://github.example.invalid/i/67\n')
        self.assertEqual(len(self.github.reads), 1, self.github.reads)      # the existing handoff is read, not reported
        self.github.issues = {PUBLIC: [issue(67, comments=4)]}
        self.github.comments[67].append(comment(4, '**Handoff → linux: next'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/4\n')

    # --- fix round 1 (#134 self-review)-------------------------------------------------------
    def test_a_handoff_must_be_the_first_line(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67].append(comment(2, '\n**Handoff → linux: R4 green'))
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.assertEqual(out.strip(), 'no batons', out)

    def test_a_comment_after_the_listing_wakes_once_and_early(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67] = [comment(1, 'progress'), comment(2, 'progress'),
                                    comment(3, '**Handoff → linux: R4 green')]
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/3\n')
        self.github.issues = {PUBLIC: [issue(67, comments=3)]}
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out.strip(), 'no batons', out)

    def test_a_rate_limited_comment_read_waits_for_the_reset(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green'))
        self.github.fail_comments = True
        self.github.comment_wait = 600

        def recover(seconds):
            self.github.fail_comments = False

        with mock.patch.object(self.module.time, 'sleep', side_effect=recover) as sleep:
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC)
        sleep.assert_called_once_with(600)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')

    # --- fix round 2 (#134 Codex gate) -----------------------------------------------------------
    def test_a_rate_limit_stops_further_comment_reads(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=1), issue(68, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')], 68: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        before = json.loads(state.read_text(encoding='utf-8'))['comments']
        self.github.issues = {PUBLIC: [issue(67, comments=2), issue(68, comments=2)]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green'))
        self.github.comments[68].append(comment(2, '**Handoff → linux: R4 green'))
        self.github.fail_comments = True
        self.github.comment_wait = 600
        first_read = len(self.github.urls)

        class Stop(Exception):
            pass

        def stop(seconds):
            raise Stop

        with mock.patch.object(self.module.time, 'sleep', stop), self.assertRaises(Stop):
            self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        comment_urls = [u for u in self.github.urls[first_read:] if '/comments?' in u]
        self.assertTrue(any('/issues/67/' in u for u in comment_urls), comment_urls)
        self.assertFalse(any('/issues/68/' in u for u in comment_urls), comment_urls)
        saved = json.loads(state.read_text(encoding='utf-8'))
        self.assertEqual(saved['comments'], before)

    def test_a_rate_limit_wait_survives_an_event_exit(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.github.issues = {PUBLIC: [issue(67, comments=2), issue(68)]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green'))
        self.github.fail_comments = True
        self.github.comment_wait = 600
        with mock.patch.object(self.module.time, 'sleep') as sleep:
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'BATON #68 a held baton https://github.example.invalid/i/68\n')
        sleep.assert_not_called()
        saved = json.loads(state.read_text(encoding='utf-8'))
        self.assertGreater(saved['retry_after'], time.time() + 500)
        self.github.fail_comments = False
        with mock.patch.object(self.module.time, 'sleep') as sleep:
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        first = sleep.call_args_list[0].args[0]
        self.assertTrue(500 <= first <= 600, first)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')

    def test_a_short_comment_wait_still_keeps_the_minimum_interval(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.github.issues = {PUBLIC: [issue(67, comments=2), issue(68)]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green'))
        self.github.fail_comments = True
        self.github.comment_wait = 8
        with mock.patch.object(self.module.time, 'sleep') as sleep:
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out, 'BATON #68 a held baton https://github.example.invalid/i/68\n')
        sleep.assert_not_called()
        saved = json.loads(state.read_text(encoding='utf-8'))
        self.assertGreater(saved['retry_after'], time.time() + 170)
        self.github.fail_comments = False
        with mock.patch.object(self.module.time, 'sleep') as sleep:
            code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        first = sleep.call_args_list[0].args[0]
        self.assertTrue(170 <= first <= 180, first)
        self.assertEqual(out, 'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')

    def test_an_old_state_file_drops_its_etag(self):
        state = self.home / 'state.json'
        state.write_text(json.dumps({'etag': '"E"', 'holding': [67]}), encoding='utf-8')
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertIsNone(self.github.etags[0])

    def test_once_exits_2_when_a_comment_read_gets_no_answer(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        before = json.loads(state.read_text(encoding='utf-8'))['comments']
        self.github.issues = {PUBLIC: [issue(67, comments=2)]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green'))
        self.github.fail_comments = True
        self.github.comment_wait = 0
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(code, 2)
        self.assertEqual(out, '')
        saved = json.loads(state.read_text(encoding='utf-8'))
        self.assertIsNone(saved['etag'])
        self.assertEqual(saved['comments'], before)

    # --- --follow keeps running and prints one line per event (#184) ------------------------------------
    def follow(self, steps, *extra):
        """Runs `--follow` with `time.sleep` replaced: the k-th sleep calls steps[k], and the first sleep after the last
        step raises StopFollow, which ends the run. Returns a namespace: `outcome` (the exit code, or 'running' when
        StopFollow ended the run), `out`, `err`, `slept` (the seconds of every sleep) and `unflushed` (at every sleep,
        the characters printed but not flushed)."""
        slept, unflushed, todo, out, err = [], [], iter(steps), Lines(), io.StringIO()

        def sleep(seconds):
            slept.append(seconds)
            unflushed.append(len(out.getvalue()) - out.flushed)
            step = next(todo, None)
            if step is None:
                raise StopFollow
            step()

        argv = ['issue_watch.py', '--machine', 'linux', '--follow', '--repo', PUBLIC, *extra]
        with mock.patch.object(sys, 'argv', argv), mock.patch.object(self.module.time, 'sleep', sleep), \
                redirect_stdout(out), redirect_stderr(err):
            try:
                outcome = self.module.main()
            except StopFollow:
                outcome = 'running'
            except SystemExit as stop:
                outcome = stop.code
        return types.SimpleNamespace(outcome=outcome, out=out.getvalue(), err=err.getvalue(), slept=slept,
                                     unflushed=unflushed)

    def test_follow_prints_each_event_and_keeps_running(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(5)]}

        def second_baton():
            self.github.issues = {PUBLIC: [issue(5), issue(6)]}

        run = self.follow([second_baton], '--state', str(state))
        self.assertEqual(run.outcome, 'running', run.err)
        self.assertEqual(run.out, 'BATON #5 a held baton https://github.example.invalid/i/5\n'
                                  'BATON #6 a held baton https://github.example.invalid/i/6\n')
        self.assertEqual(run.slept, [180, 180])
        code, out = self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.assertEqual(out.strip(), 'no batons', out)      # both events were recorded in the state file

    def test_follow_prints_a_handoff_after_a_baton_without_restarting(self):
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}

        def handoff():
            self.github.issues = {PUBLIC: [issue(67, comments=2)]}
            self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green'))

        run = self.follow([handoff])
        self.assertEqual(run.outcome, 'running', run.err)
        self.assertEqual(run.out, 'BATON #67 a held baton https://github.example.invalid/i/67\n'
                                  'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')

    def test_follow_sends_the_etag_and_an_unchanged_listing_prints_nothing(self):
        def unchanged():
            self.github.unchanged = True

        def changed():
            self.github.unchanged = False
            self.github.issues = {PUBLIC: [issue(5), issue(6)]}

        self.github.issues = {PUBLIC: [issue(5)]}
        run = self.follow([unchanged, changed])
        self.assertEqual(run.outcome, 'running', run.err)
        self.assertEqual(run.out, 'BATON #5 a held baton https://github.example.invalid/i/5\n'
                                  'BATON #6 a held baton https://github.example.invalid/i/6\n')
        tag = '"etag-' + PUBLIC + '"'
        listing = [etag for url, etag in zip(self.github.urls, self.github.etags) if '/comments?' not in url]
        self.assertEqual(listing, [None, tag, tag])
        self.assertEqual(run.slept, [180, 180, 180])

    def test_follow_flushes_every_event_before_it_sleeps(self):
        self.github.issues = {PUBLIC: [issue(5)]}

        def two_more():
            self.github.issues = {PUBLIC: [issue(5), issue(6), issue(7)]}

        run = self.follow([two_more])
        self.assertEqual(run.outcome, 'running', run.err)
        self.assertEqual(run.out.count('\n'), 3, run.out)
        self.assertEqual(run.unflushed, [0, 0])

    def test_follow_prints_an_event_before_it_writes_the_state(self):
        self.github.issues = {PUBLIC: [issue(5)]}
        out = Lines()
        argv = ['issue_watch.py', '--machine', 'linux', '--follow', '--repo', PUBLIC]
        with mock.patch.object(sys, 'argv', argv), \
                mock.patch.object(self.module, 'write_state', side_effect=OSError('disk full')), \
                redirect_stdout(out), self.assertRaises(OSError):
            self.module.main()
        self.assertEqual(out.getvalue(), 'BATON #5 a held baton https://github.example.invalid/i/5\n')

    def test_follow_waits_out_a_rate_limit_and_goes_on(self):
        answers = iter([(None, None, 500), (None, [issue(5)], None)])
        self.module.fetch = lambda url, tok, etag: (None, [], None) if '/comments' in url else next(answers)
        run = self.follow([lambda: None])
        self.assertEqual(run.outcome, 'running', run.err)
        self.assertEqual(run.slept, [500, 180])
        self.assertEqual(run.out, 'BATON #5 a held baton https://github.example.invalid/i/5\n')

    def test_follow_retries_a_poll_nobody_answered(self):
        answers = iter([(None, None, 0), (None, [issue(5)], None)])
        self.module.fetch = lambda url, tok, etag: (None, [], None) if '/comments' in url else next(answers)
        run = self.follow([lambda: None])
        self.assertEqual(run.outcome, 'running', run.err)
        self.assertEqual(run.slept, [180, 180])
        self.assertEqual(run.out, 'BATON #5 a held baton https://github.example.invalid/i/5\n')

    def test_follow_waits_out_a_comment_read_limit_after_an_event(self):
        state = self.home / 'state.json'
        self.github.issues = {PUBLIC: [issue(67, comments=1)]}
        self.github.comments = {67: [comment(1, 'progress')]}
        self.run_watch('--machine', 'linux', '--once', '--repo', PUBLIC, '--state', str(state))
        self.github.issues = {PUBLIC: [issue(67, comments=2), issue(68)]}
        self.github.comments[67].append(comment(2, '**Handoff → linux: R4 green'))
        self.github.fail_comments = True
        self.github.comment_wait = 600

        def recover():
            self.github.fail_comments = False

        run = self.follow([recover], '--state', str(state))
        self.assertEqual(run.outcome, 'running', run.err)
        self.assertEqual(run.out, 'BATON #68 a held baton https://github.example.invalid/i/68\n'
                                  'HANDOFF #67 a held baton https://github.example.invalid/c/2\n')
        self.assertEqual(run.slept, [600, 180])

    def test_follow_exits_2_on_an_api_failure_after_an_event(self):
        answers = iter([(None, [issue(5)], None)])

        def fetch(url, tok, etag):
            if '/comments' in url:
                return None, [], None
            answer = next(answers, None)
            return answer if answer else self.real_fetch(url, tok, etag)

        self.module.fetch = fetch
        error = self.http_error(401, {}, b'{"message": "Bad credentials"}')
        with mock.patch.object(self.module.urllib.request, 'urlopen', side_effect=error):
            run = self.follow([lambda: None])
        self.assertEqual(run.outcome, 2, run.err)
        self.assertEqual(run.out, 'BATON #5 a held baton https://github.example.invalid/i/5\n')
        self.assertEqual(run.slept, [180])
        self.assertIn('GitHub API 401', run.err)

    def test_follow_exits_2_on_a_full_page(self):
        self.module.fetch = lambda url, tok, etag: (None, [issue(n) for n in range(1, 101)], None)
        run = self.follow([])
        self.assertEqual(run.outcome, 2)
        self.assertEqual(run.out, '')
        self.assertEqual(run.slept, [])
        self.assertIn('carry this baton', run.err)

    def test_follow_and_once_exclude_each_other(self):
        err = io.StringIO()
        with redirect_stderr(err), self.assertRaises(SystemExit) as stop:
            self.run_watch('--machine', 'linux', '--once', '--follow')
        self.assertEqual(stop.exception.code, 2)
        self.assertIn('not allowed with', err.getvalue())


if __name__ == '__main__':
    unittest.main(verbosity=2)

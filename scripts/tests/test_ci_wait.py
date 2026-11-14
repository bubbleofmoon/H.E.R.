#!/usr/bin/env python3
"""Tests for scripts/ci-wait.py (the shared CI waiter) against a local fake GitHub API.

The fake API is an HTTP server on 127.0.0.1. It serves a workflow run list with an ETag (a matching If-None-Match
gets 304), rate-limit headers, a 403 while a rate limit holds, other error modes, a job list and a job log behind
a redirect, and records every call. Waiters run as separate processes with HER_GITHUB_API pointing at it, a
temporary cache directory and a 1-second refresh window, so nothing contacts GitHub or touches the real cache.
Run: python -I scripts/tests/test_ci_wait.py
"""
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent.parent
SCRIPT = ROOT / 'scripts' / 'ci-wait.py'
REPO = 'bubbleofmoon/' + 'H.E.R.'
REFRESH = 1.0
GH_REAL = 'gh-real-zq'
SHA_A = 'a' * 40
SHA_B = 'b' * 40
TITLE = 'quick linux target=unit filter='
WAIT = ('--branch', 'her/1-x', '--sha', SHA_A, '--target', 'unit')
FAILING = ('--branch', 'her/1-x', '--sha', SHA_B, '--target', 'unit')
STAMPED = '2026-11-14' + 'T02:00:00.5Z '
LOG_TEXT = '\n'.join(STAMPED + line for line in [
    'running 2 tests',
    'test a::b ... FAILED',
    'noise that the filter drops',
    '',
    "thread 'a::b' panicked at src/a.rs:1:5:",
    'assertion `left == right` failed',
    '  left: 1',
    '',
    'more noise after the blank line',
    'test result: FAILED. 1 passed; 1 failed; 0 ignored',
    'EXIT unit: 101',
])


def load_module():
    spec = importlib.util.spec_from_file_location('ci_wait', SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def iso(t):
    return time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(t))


def closed_port_api():
    probe = socket.socket()
    probe.bind(('127.0.0.1', 0))
    port = probe.getsockname()[1]
    probe.close()
    return f'http://127.0.0.1:{port}'


class FakeGitHub(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(('127.0.0.1', 0), Handler)
        self.lock = threading.Lock()
        self.calls = []                # (kind, wall time, request headers lower-cased, status, path)
        self.runs = lambda: []
        self.jobs = []
        self.log = ''
        self.remaining = '4000'
        self.limit_until = 0           # list calls answer 403 until then
        self.jobs_limit_until = 0      # job-list calls answer 403 until then
        self.list_status = 200         # 401, 404 or 500 for every list call; 'junk' a non-JSON 200
        self.hold = None               # an Event the first list call waits for before answering
        self.api = f'http://127.0.0.1:{self.server_address[1]}'


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def answer(self):
        server, path = self.server, self.path
        limits = {'x-ratelimit-remaining': server.remaining, 'x-ratelimit-reset': str(int(time.time()) + 3600)}
        if '/actions/workflows/' in path:
            if time.time() < server.limit_until:
                return 'list', 403, b'{"message": "API rate limit exceeded"}', {
                    'x-ratelimit-remaining': '0', 'x-ratelimit-reset': str(server.limit_until)}
            if server.list_status == 'junk':
                return 'list', 200, b'not json', limits
            if server.list_status != 200:
                return 'list', server.list_status, b'{"message": "no"}', limits
            body = json.dumps({'workflow_runs': server.runs()}).encode('utf-8')
            etag = '"' + hashlib.sha256(body).hexdigest()[:16] + '"'
            if self.headers.get('If-None-Match') == etag:
                return 'list', 304, b'', dict(limits, ETag=etag)
            return 'list', 200, body, dict(limits, ETag=etag)
        if '/jobs?' in path:
            if time.time() < server.jobs_limit_until:
                return 'jobs', 403, b'{"message": "API rate limit exceeded"}', {
                    'x-ratelimit-remaining': '0', 'x-ratelimit-reset': str(server.jobs_limit_until)}
            return 'jobs', 200, json.dumps({'jobs': server.jobs}).encode('utf-8'), limits
        if path.endswith('/logs'):
            return 'logs', 302, b'', dict(limits, Location=f"{server.api}/blob/{path.split('/')[-2]}")
        if path.startswith('/blob/'):
            return 'blob', 200, server.log.encode('utf-8'), {}
        return 'other', 404, b'{}', limits

    def do_GET(self):
        kind, status, body, headers = self.answer()
        server = self.server
        with server.lock:
            first = kind == 'list' and not any(call[0] == 'list' for call in server.calls)
            server.calls.append((kind, time.time(), {k.lower(): v for k, v in self.headers.items()}, status, self.path))
        if first and server.hold is not None:
            server.hold.wait(30)
        try:
            self.send_response(status)
            for name, value in headers.items():
                self.send_header(name, value)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except OSError:
            pass                       # the caller was killed


class CiWait(unittest.TestCase):
    def setUp(self):
        self.began = time.time()
        self.server = FakeGitHub()
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.tmp = Path(tempfile.mkdtemp(prefix='ci-wait-test-'))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.cache = self.tmp / 'cache'
        self.procs = []
        self.addCleanup(self.stop_all)

    def stop_all(self):
        for proc in self.procs:
            if proc.poll() is None:
                proc.kill()
                proc.wait()

    def make_run(self, run_id, sha=SHA_A, status='completed', conclusion='success', title=TITLE,
                 event='workflow_dispatch', created=None):
        return {'id': run_id, 'display_title': title, 'head_branch': 'her/1-x', 'head_sha': sha, 'event': event,
                'status': status, 'conclusion': conclusion if status == 'completed' else None,
                'html_url': f'https://github.example.invalid/runs/{run_id}',
                'created_at': iso(self.began if created is None else created)}

    def start(self, *args, cwd=None, token='t', api=None, cache=None):
        env = dict(os.environ, HER_CI_WAIT_TEST_TOKEN=token, GH_TOKEN=GH_REAL, HER_GITHUB_API=api or self.server.api,
                   HER_CI_WAIT_DIR=str(cache or self.cache), HER_CI_WAIT_REFRESH=str(REFRESH))
        proc = subprocess.Popen([sys.executable, '-I', str(SCRIPT), *args], cwd=cwd or str(self.tmp), env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding='utf-8')
        self.procs.append(proc)
        return proc

    def finish(self, proc):
        out, err = proc.communicate(timeout=60)
        return proc.returncode, out, err

    def wait(self, *args, **kwargs):
        return self.finish(self.start(*args, **kwargs))

    def list_calls(self):
        return [call for call in self.server.calls if call[0] == 'list']

    # --- one list call per window, 304, budget --------------------------------------------------
    def test_ten_waiters_make_one_list_call_per_refresh_window(self):          # first red of run C
        self.server.runs = lambda: ([self.make_run(11, status='in_progress')] if time.time() < self.began + 3.5
                                    else [self.make_run(11)])
        results = [self.finish(proc) for proc in [self.start(*WAIT) for _ in range(10)]]
        elapsed = time.time() - self.began
        for code, out, err in results:
            self.assertEqual(code, 0, out + err)
            self.assertIn('CI success', out)
        calls = self.list_calls()
        self.assertGreaterEqual(len(calls), 2, calls)
        self.assertLessEqual(len(calls), int(elapsed / REFRESH) + 1, calls)
        times = [call[1] for call in calls]
        for earlier, later in zip(times, times[1:]):
            self.assertGreaterEqual(later - earlier, REFRESH * 0.9, times)
        self.assertTrue(all('if-none-match' in call[2] for call in calls[1:]), calls)
        self.assertGreaterEqual(len([call for call in calls if call[3] == 304]), 1, calls)

    def test_a_rate_limit_403_holds_every_waiter_until_the_reset(self):
        self.server.limit_until = int(time.time()) + 2
        self.server.runs = lambda: [self.make_run(11)]
        results = [self.finish(proc) for proc in [self.start(*WAIT) for _ in range(3)]]
        for code, out, err in results:
            self.assertEqual(code, 0, out + err)
            self.assertIn('GitHub unavailable until', out)
            self.assertIn('CI success', out)
        calls = self.list_calls()
        self.assertEqual([call[3] for call in calls], [403, 200], calls)
        self.assertGreaterEqual(calls[1][1], self.server.limit_until)

    def test_a_wait_that_ends_inside_a_rate_limit_is_unavailable_never_failed(self):
        self.server.limit_until = int(time.time()) + 60
        self.server.runs = lambda: [self.make_run(11, conclusion='failure')]
        results = [self.finish(proc) for proc in [self.start(*WAIT, '--timeout', '1.5') for _ in range(3)]]
        for code, out, err in results:
            self.assertEqual(code, 5, out + err)
            self.assertIn('GitHub unavailable', out)
            self.assertNotIn('CI failure', out)
        self.assertEqual(len(self.list_calls()), 1)

    def test_a_rate_limit_seen_on_one_workflow_stops_calls_for_every_workflow(self):
        self.server.limit_until = int(time.time()) + 30
        code, out, err = self.wait(*WAIT, '--timeout', '1')
        self.assertEqual(code, 5, out + err)
        code, out, err = self.wait('--workflow', 'checks.yml', '--sha', SHA_A, '--timeout', '1')
        self.assertEqual(code, 5, out + err)
        self.assertIn('GitHub unavailable until', out)
        self.assertEqual([call for call in self.server.calls if 'checks.yml' in call[4]], [])

    def test_backoff_follows_the_remaining_calls_and_the_reset(self):
        module = load_module()
        for remaining, expected in ((None, 100), (1000, 100), (999, 300), (200, 300), (199, 900), (0, 900)):
            self.assertEqual(module.interval(remaining, 100.0), expected, remaining)
        now = 1000.0
        limited = module.rate_limited_until
        self.assertEqual(limited(403, {'x-ratelimit-remaining': '0', 'x-ratelimit-reset': '1500'}, now, 900), 1501)
        self.assertEqual(limited(403, {'x-ratelimit-remaining': '0', 'x-ratelimit-reset': '10'}, now, 900), now)
        self.assertEqual(limited(429, {'retry-after': '30'}, now, 900), 1031)
        self.assertEqual(limited(429, {}, now, 900), 1900)
        self.assertIsNone(limited(403, {'x-ratelimit-remaining': '12'}, now, 900))
        self.assertIsNone(limited(200, {'x-ratelimit-remaining': '0'}, now, 900))

    def test_few_calls_left_slow_the_refresh(self):
        self.server.runs = lambda: [self.make_run(11, status='in_progress')]
        self.server.remaining = '150'
        code, out, err = self.wait(*WAIT, '--timeout', '3')
        self.assertEqual(code, 4, out + err)
        self.assertIn('CI still running', out)
        self.assertEqual(len(self.list_calls()), 1)
        shutil.rmtree(self.cache)
        self.server.calls.clear()
        self.server.remaining = '1000'
        code, out, err = self.wait(*WAIT, '--timeout', '3')
        self.assertEqual(code, 4, out + err)
        self.assertGreaterEqual(len(self.list_calls()), 2)

    def test_a_killed_refresher_costs_at_most_one_call_per_window(self):
        self.server.hold = threading.Event()
        self.addCleanup(self.server.hold.set)
        self.server.runs = lambda: [self.make_run(11)]
        first = self.start(*WAIT)
        deadline = time.time() + 20
        while not self.list_calls() and time.time() < deadline:
            time.sleep(0.05)
        self.assertEqual(len(self.list_calls()), 1)
        first.kill()
        first.wait()
        code, out, err = self.wait(*WAIT, '--timeout', '10')       # the OS released the dead holder's lock
        self.assertEqual(code, 0, out + err)
        calls = self.list_calls()
        self.assertEqual(len(calls), 2, calls)
        self.assertGreaterEqual(calls[1][1] - calls[0][1], REFRESH * 0.9)

    def test_a_held_lock_means_no_call_and_its_release_one_call(self):           # V1
        module = load_module()
        self.cache.mkdir()
        self.server.runs = lambda: [self.make_run(11, status='in_progress')]
        with module.lock(str(self.cache), wait=False) as held:
            self.assertTrue(held)
            code, out, err = self.wait(*WAIT, '--timeout', '1.5')               # the list is due, but locked
        self.assertEqual(code, 4, out + err)
        self.assertEqual(self.list_calls(), [])
        code, out, err = self.wait(*WAIT, '--timeout', '0.5')
        self.assertEqual(code, 4, out + err)
        self.assertEqual(len(self.list_calls()), 1)

    def test_every_wait_names_its_run_and_how_to_resume(self):                  # V2
        self.server.runs = lambda: [self.make_run(11, status='in_progress')]
        code, out, err = self.wait(*WAIT, '--timeout', '1.5')
        self.assertEqual(code, 4, out + err)
        self.assertIn('waiting for https://github.example.invalid/runs/11 (resume with --run 11)', out)
        self.assertIn('resume with --run 11', out.strip().splitlines()[-1])
        time.sleep(REFRESH * 1.2)
        self.server.limit_until = int(time.time()) + 30
        code, out, err = self.wait(*WAIT, '--timeout', '1')
        self.assertEqual(code, 5, out + err)
        self.assertIn('resume with --run 11', out.strip().splitlines()[-1])
        old = self.began - 600
        self.server.limit_until = 0
        shutil.rmtree(self.cache, True)                  # the first half's block and its next_at are over
        self.server.runs = lambda: [self.make_run(13, sha=SHA_B, conclusion='failure', created=old)]
        time.sleep(REFRESH * 1.2)
        code, out, err = self.wait(*FAILING, '--timeout', '1.5')
        self.assertEqual(code, 4, out + err)
        first_lines = out.splitlines()[:2]
        self.assertTrue(any('read it with --run 13' in line for line in first_lines), out)   # on first sight
        self.assertIn('--run 13', out.strip().splitlines()[-1])

    def test_an_unreadable_list_state_means_no_call_and_no_write(self):
        module = load_module()
        self.cache.mkdir()
        blocker = self.cache / module.list_state_name(REPO, 'quick.yml')
        blocker.mkdir()                                     # opening it fails on every OS
        code, out, err = self.wait(*WAIT, '--timeout', '1')
        self.assertEqual(code, 5, out + err)
        self.assertIn('cannot be read', out)
        self.assertEqual(self.list_calls(), [])
        self.assertTrue(blocker.is_dir())

    def test_prune_drops_old_logs_and_leftover_temporary_files_only(self):
        module = load_module()
        self.cache.mkdir()
        now = time.time()
        ages = {'log-1.json': 8 * 86400, 'log-2.json': 3600, 'x.json.9.tmp': 7200, 'y.json.9.tmp': 60,
                'results.json': 30 * 86400}
        for name, age in ages.items():
            path = self.cache / name
            path.write_text('{}', encoding='utf-8')
            os.utime(path, (now - age, now - age))
        module.prune(str(self.cache), now)
        self.assertEqual(sorted(p.name for p in self.cache.iterdir()), ['log-2.json', 'results.json', 'y.json.9.tmp'])

    # --- what is cached and what is reported ---------------------------------------------------
    def test_a_green_commit_is_reused_without_a_call(self):
        self.server.runs = lambda: [self.make_run(11)]
        code, out, err = self.wait(*WAIT)
        self.assertEqual(code, 0, out + err)
        before = len(self.server.calls)
        time.sleep(REFRESH * 1.2)          # the list is due again, so only the result cache explains no call
        code, out, err = self.wait(*WAIT)
        self.assertEqual(code, 0, out + err)
        self.assertIn('(cached)', out)
        self.assertEqual(len(self.server.calls), before)
        code, out, err = self.wait('--branch', 'her/1-x', '--sha', SHA_A, '--target', 'other', '--timeout', '0.5')
        self.assertEqual(code, 4, out + err)                         # another target is not this commit's result
        self.assertNotIn('(cached)', out)

    def test_a_known_result_keeps_its_code_when_caching_or_the_log_fails(self):
        module = load_module()
        self.server.runs = lambda: [self.make_run(12, sha=SHA_B, conclusion='failure'), self.make_run(11)]
        env = {'HER_CI_WAIT_DIR': str(self.cache), 'HER_GITHUB_API': self.server.api, 'HER_CI_WAIT_REFRESH': str(REFRESH)}
        with mock.patch.dict(os.environ, env), mock.patch.object(module, 'remember', side_effect=OSError('x')), \
                mock.patch.object(module, 'failed_log', side_effect=OSError('x')), \
                mock.patch('sys.stdout', new_callable=io.StringIO) as out, \
                mock.patch('sys.stderr', new_callable=io.StringIO):
            self.assertEqual(module.main(list(WAIT)), 0)
            self.assertEqual(module.main(list(FAILING)), 1)
        self.assertIn('log unavailable: internal error', out.getvalue())

    def test_a_local_api_uses_its_own_cache_folder(self):
        module = load_module()
        home = str(self.tmp)
        base = {'HOME': home, 'USERPROFILE': home, 'LOCALAPPDATA': home, 'HER_CI_WAIT_DIR': '',
                'HER_CI_WAIT_REFRESH': '', 'HER_CI_WAIT_TEST_TOKEN': ''}
        with mock.patch.dict(os.environ, dict(base, HER_GITHUB_API='')):
            self.assertEqual(os.path.basename(module.settings()[1]), 'her-ci-wait')
        with mock.patch.dict(os.environ, dict(base, HER_GITHUB_API=self.server.api)):
            self.assertEqual(os.path.basename(module.settings()[1]), 'her-ci-wait-test')

    def test_the_skew_edge_and_an_unreadable_date(self):
        module = load_module()
        start = float(86400 * 400)
        self.assertTrue(module.fresh_enough({'created_at': iso(start - 30)}, start))
        self.assertFalse(module.fresh_enough({'created_at': iso(start - 31)}, start))
        self.assertEqual(module.created({'created_at': 'junk'}), 0)
        self.assertFalse(module.fresh_enough({'created_at': 'junk'}, start))

    def test_checks_results_are_never_cached_and_the_event_is_chosen(self):
        self.server.runs = lambda: [self.make_run(12, conclusion='failure', title='checks', event='push'),
                                    self.make_run(11, title='checks', event='pull_request')]
        args = ('--workflow', 'checks.yml', '--sha', SHA_A, '--event', 'pull_request')
        code, out, err = self.wait(*args)
        self.assertEqual(code, 0, out + err)
        self.assertIn('pull_request', out)
        results = self.cache / 'results.json'
        self.assertFalse(results.exists() and 'checks.yml' in results.read_text(encoding='utf-8'))
        lists = len(self.list_calls())
        time.sleep(REFRESH * 1.2)
        code, out, err = self.wait(*args)
        self.assertEqual(code, 0, out + err)
        self.assertNotIn('(cached)', out)
        self.assertGreater(len(self.list_calls()), lists)

    def test_logs_only_for_a_failed_run_and_only_the_first_failed_job(self):
        self.server.runs = lambda: [self.make_run(12, sha=SHA_B, conclusion='failure'), self.make_run(11)]
        self.server.jobs = [{'id': 21, 'name': 'build', 'conclusion': 'success'},
                            {'id': 22, 'name': 'quick (linux, unit)', 'conclusion': 'failure'},
                            {'id': 23, 'name': 'extra', 'conclusion': 'failure'}]
        self.server.log = LOG_TEXT
        code, out, err = self.wait(*WAIT)
        self.assertEqual(code, 0, out + err)
        self.assertEqual([call[0] for call in self.server.calls], ['list'])     # green: no job list, no log
        code, out, err = self.wait(*FAILING)
        self.assertEqual(code, 1, out + err)
        self.assertIn('CI failure', out)
        self.assertIn('failed jobs: quick (linux, unit), extra', out)
        self.assertIn('job quick (linux, unit):', out)
        for line in ('test a::b ... FAILED', "thread 'a::b' panicked at src/a.rs:1:5:", 'assertion `left == right` failed',
                     '  left: 1', 'test result: FAILED.', 'EXIT unit: 101'):
            self.assertIn('\n' + line, out)
        self.assertNotIn('noise', out)
        logs = [call for call in self.server.calls if call[0] == 'logs']
        self.assertEqual([call[4] for call in logs], [f'/repos/{REPO}/actions/jobs/22/logs'])
        self.assertIn('authorization', logs[0][2])
        blobs = [call for call in self.server.calls if call[0] == 'blob']
        self.assertEqual(len(blobs), 1)
        self.assertNotIn('authorization', blobs[0][2])                  # the token never follows the redirect
        lists = len(self.list_calls())
        time.sleep(REFRESH * 1.2)
        code, out, err = self.wait(*FAILING)                            # a failure is not reused ...
        self.assertEqual(code, 1, out + err)
        self.assertGreater(len(self.list_calls()), lists)
        self.assertNotIn('(cached)', out)
        self.assertEqual(len([c for c in self.server.calls if c[0] in ('jobs', 'logs', 'blob')]), 3)   # ... its log is

    def test_a_rate_limit_on_the_log_is_recorded_for_every_waiter(self):
        self.server.runs = lambda: [self.make_run(12, sha=SHA_B, conclusion='failure')]
        self.server.jobs = [{'id': 22, 'name': 'quick (linux, unit)', 'conclusion': 'failure'}]
        self.server.jobs_limit_until = int(time.time()) + 60
        results = [self.finish(proc) for proc in [self.start(*FAILING) for _ in range(3)]]
        for code, out, err in results:
            self.assertEqual(code, 1, out + err)
            self.assertIn('log unavailable', out)
        self.assertEqual(len([call for call in self.server.calls if call[0] == 'jobs']), 1)
        self.assertEqual([call for call in self.server.calls if call[0] in ('logs', 'blob')], [])

    def test_a_failure_from_before_the_wait_is_not_reported_unless_named(self):
        old = self.began - 600
        self.server.runs = lambda: [self.make_run(11, conclusion='failure', created=old)]
        code, out, err = self.wait(*WAIT, '--timeout', '1.5')
        self.assertEqual(code, 4, out + err)
        self.assertIn('finished before this wait began', out)
        code, out, err = self.wait('--run', '11', '--timeout', '1.5')
        self.assertEqual(code, 1, out + err)
        self.server.runs = lambda: [self.make_run(12), self.make_run(11, conclusion='failure', created=old)]
        time.sleep(REFRESH * 1.2)
        code, out, err = self.wait(*WAIT)
        self.assertEqual(code, 0, out + err)
        self.assertIn('/runs/12', out)

    def test_a_cancelled_run_is_reported_as_cancelled_without_a_log(self):
        self.server.runs = lambda: [self.make_run(11, conclusion='cancelled')]
        code, out, err = self.wait(*WAIT)
        self.assertEqual(code, 3, out + err)
        self.assertIn('CI cancelled', out)
        self.assertEqual({call[0] for call in self.server.calls}, {'list'})

    def test_once_prints_the_age_of_the_run_list(self):
        self.server.runs = lambda: [self.make_run(11, status='in_progress')]
        code, out, err = self.wait(*WAIT, '--once')
        self.assertEqual(code, 4, out + err)
        self.assertIn('run list fetched', out)

    def test_the_commit_comes_from_the_local_branch(self):
        repo = self.tmp / 'repo'
        git = ['git', '-c', 'user.name=ci-wait-test', '-c', 'user.email=ci-wait-test@example.invalid',
               '-c', 'init.defaultBranch=her/1-x']
        subprocess.run([*git, 'init', '-q', str(repo)], check=True)
        subprocess.run([*git, '-C', str(repo), 'commit', '-q', '--allow-empty', '-m', 'x'], check=True)
        sha = subprocess.run(['git', '-C', str(repo), 'rev-parse', 'HEAD'], capture_output=True, text=True,
                             check=True).stdout.strip()
        self.server.runs = lambda: [self.make_run(11, sha=sha)]
        code, out, err = self.wait('--branch', 'her/1-x', '--target', 'unit', cwd=str(repo))
        self.assertEqual(code, 0, out + err)

    # --- errors are never a result ---------------------------------------------------------------
    def test_github_errors_are_never_a_failure(self):
        for mode, needle in ((500, 'GitHub API 500'), ('junk', 'unreadable run list')):
            with self.subTest(mode=mode):
                shutil.rmtree(self.cache, True)
                self.server.list_status = mode
                code, out, err = self.wait(*WAIT, '--timeout', '1.5')
                self.assertEqual(code, 5, out + err)
                self.assertIn(needle, out)
                self.assertNotIn('CI failure', out)
        shutil.rmtree(self.cache, True)
        code, out, err = self.wait(*WAIT, '--timeout', '1.5', api=closed_port_api())
        self.assertEqual(code, 5, out + err)
        self.assertIn('unreachable', out)

    def test_a_refused_token_stops_every_waiter_and_a_missing_workflow_is_usage(self):
        self.server.list_status = 401
        code, out, err = self.wait(*WAIT)
        self.assertEqual(code, 5, out + err)
        self.assertIn('token was refused', out)
        code, out, err = self.wait(*WAIT)
        self.assertEqual(code, 5, out + err)
        self.assertEqual(len(self.list_calls()), 1)                    # the second waiter made no call
        shutil.rmtree(self.cache)
        self.server.list_status = 404
        code, out, err = self.wait(*WAIT)
        self.assertEqual(code, 2, out + err)
        self.assertIn('404', err)

    def test_an_internal_error_is_never_a_failure(self):
        module = load_module()
        env = {'HER_CI_WAIT_DIR': str(self.cache), 'GH_TOKEN': 't', 'HER_GITHUB_API': self.server.api}
        out = io.StringIO()
        with mock.patch.dict(os.environ, env), mock.patch.object(module, 'refresh_if_due', side_effect=RuntimeError('x')), \
                mock.patch('sys.stdout', out):
            self.assertEqual(module.main(list(WAIT)), 5)
        self.assertIn('internal error', out.getvalue())

    def test_bad_options_and_overrides_without_a_local_api_are_refused(self):
        module = load_module()
        env = {'HER_CI_WAIT_DIR': str(self.cache), 'GH_TOKEN': 't', 'HER_GITHUB_API': self.server.api}
        cases = [
            (['--branch', 'her/1-x', '--sha', 'xyz', '--target', 'unit'], {}),
            (['--branch', 'her/1-x', '--sha', SHA_A], {}),
            (['--run', '5', '--sha', SHA_A], {}),
            (['--workflow', 'checks.yml', '--sha', SHA_A, '--target', 'unit'], {}),
            (['--sha', SHA_A, '--target', 'unit', '--event', 'push'], {}),
            (['--repo', 'a/../b', '--sha', SHA_A, '--target', 'unit'], {}),
            (list(WAIT), {'HER_GITHUB_API': 'https://example.invalid'}),
            (list(WAIT), {'HER_GITHUB_API': ''}),                                       # a private cache on GitHub
            (list(WAIT), {'HER_GITHUB_API': '', 'HER_CI_WAIT_DIR': '', 'HER_CI_WAIT_REFRESH': '1'}),
            (list(WAIT), {'HER_GITHUB_API': '', 'HER_CI_WAIT_DIR': '', 'HER_CI_WAIT_TEST_TOKEN': 'm'}),
        ]
        for argv, override in cases:
            with self.subTest(argv=argv, override=override), mock.patch.dict(os.environ, dict(env, **override)), \
                    mock.patch('sys.stderr', new_callable=io.StringIO):
                self.assertEqual(module.main(argv), 2)
        self.assertEqual(self.server.calls, [])

    def test_the_token_is_read_once_per_call_and_never_for_a_test_server(self):
        module = load_module()
        self.cache.mkdir()
        calls = []

        def fake_run(argv, **kwargs):
            calls.append(argv)
            return subprocess.CompletedProcess(argv, 0, stdout='t\n', stderr='')
        env = {key: value for key, value in os.environ.items()
               if key not in ('GH_TOKEN', 'HER_GITHUB_API', 'HER_CI_WAIT_DIR', 'HER_CI_WAIT_REFRESH')}
        ctx = module.Ctx(module.DEFAULT_API, str(self.cache), 100.0)
        state = str(self.cache / 'runs.json')
        with mock.patch.dict(os.environ, env, clear=True), mock.patch.object(module.subprocess, 'run', fake_run), \
                mock.patch.object(module, 'request', return_value=(304, {}, b'')):
            module.refresh_if_due(ctx, state, module.DEFAULT_API + '/x')
            module.refresh_if_due(ctx, state, module.DEFAULT_API + '/x')   # not due: no token read, no call
            with mock.patch.dict(os.environ, {'GH_TOKEN': GH_REAL}):
                self.assertEqual(module.token(self.server.api), module.LOCAL_TOKEN)
                with mock.patch.dict(os.environ, {'HER_CI_WAIT_TEST_TOKEN': 'm'}):
                    self.assertEqual(module.token(self.server.api), 'm')
                    self.assertEqual(module.token(module.DEFAULT_API), GH_REAL)   # the test token never reaches GitHub
        self.assertEqual(calls, [['gh', 'auth', 'token']])

    def test_the_token_never_reaches_a_file_or_the_output(self):
        marker = 'tok-marker-zq'
        fresh = self.tmp / 'cache2'
        self.server.runs = lambda: [self.make_run(12, sha=SHA_B, conclusion='failure'), self.make_run(11)]
        self.server.jobs = [{'id': 22, 'name': 'quick', 'conclusion': 'failure'}]
        self.server.log = LOG_TEXT
        outputs = [self.wait(*WAIT, token=marker), self.wait(*FAILING, token=marker)]
        time.sleep(REFRESH * 1.2)
        self.server.limit_until = int(time.time()) + 30
        other = ('--branch', 'her/1-x', '--sha', SHA_A, '--target', 'other', '--timeout', '1')
        outputs.append(self.wait(*other, token=marker))
        outputs.append(self.wait(*other, token=marker, api=closed_port_api(), cache=fresh))
        seen = [call[2].get('authorization') for call in self.server.calls]
        self.assertIn('Bearer ' + marker, seen)
        self.assertFalse(any(GH_REAL in (value or '') for value in seen))      # GH_TOKEN never goes to a test server
        for code, out, err in outputs:
            self.assertNotIn(marker, out + err)
            self.assertNotIn(GH_REAL, out + err)
        for path in [*self.cache.rglob('*'), *fresh.rglob('*')]:
            if path.is_file():
                data = path.read_bytes()
                self.assertNotIn(marker.encode('utf-8'), data, str(path))
                self.assertNotIn(GH_REAL.encode('utf-8'), data, str(path))

    # --- small pieces -----------------------------------------------------------------------------
    def test_state_names_are_distinct_and_the_cache_is_private(self):
        module = load_module()
        self.assertNotEqual(module.list_state_name('a-b/c', 'quick.yml'), module.list_state_name('a/b-c', 'quick.yml'))
        self.server.runs = lambda: [self.make_run(11)]
        code, out, err = self.wait(*WAIT)
        self.assertEqual(code, 0, out + err)
        if os.name != 'nt':
            self.assertEqual(stat.S_IMODE(self.cache.stat().st_mode), 0o700)

    def test_the_log_filter_keeps_panic_messages_and_caps_its_output(self):
        module = load_module()
        kept = module.filter_log(LOG_TEXT.split('\n'))
        self.assertIn('assertion `left == right` failed', kept)
        self.assertNotIn('more noise after the blank line', kept)
        many = module.filter_log(['test result: ok. 1 passed'] * 300)
        self.assertEqual(len(many), 201)
        self.assertEqual(many[-1], '(100 more lines)')
        with mock.patch.object(module, 'MAX_LOG_BYTES', 4096):
            self.assertEqual(list(module.capped([b'x' * 1024] * 5))[-1], '(log truncated after 64 MiB)')


if __name__ == '__main__':
    unittest.main(verbosity=2)

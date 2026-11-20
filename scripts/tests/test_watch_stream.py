#!/usr/bin/env python3
"""Tests for scripts/watch_stream.py with temporary files and a fake pid check.

Run: python -I scripts/tests/test_watch_stream.py
"""
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest import mock

ROOT = Path(__file__).resolve().parent.parent.parent


def load_module():
    spec = importlib.util.spec_from_file_location('watch_stream', ROOT / 'scripts' / 'watch_stream.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class WatchStreamTests(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory()
        self.addCleanup(self.folder.cleanup)
        self.events = Path(self.folder.name) / 'watch-events.log'
        self.events.write_text('', encoding='utf-8')
        self.offset_file = Path(str(self.events) + '.offset')
        self.module = load_module()

    def run_once(self, *extra):
        out = io.StringIO()
        with redirect_stdout(out):
            code = self.module.main(['--events', str(self.events), '--once', *extra])
        self.assertEqual(code, 0)
        self.raw = out.getvalue()
        return self.raw.splitlines()

    def test_starts_with_one_status_line(self):
        lines = self.run_once()
        self.assertEqual(len(lines), 1)
        self.assertTrue(lines[0].startswith('watch: offset 0 watcher none'), lines[0])

    def test_prints_only_events_after_the_saved_offset(self):
        self.events.write_bytes(b'event: one\n')
        first = self.run_once()
        self.assertIn('event: one', first)
        self.assertEqual(self.offset_file.read_text().strip(), str(len(b'event: one\n')))

        with open(self.events, 'ab') as handle:
            handle.write(b'event: two\n')
        second = self.run_once()
        self.assertNotIn('event: one', second)
        self.assertIn('event: two', second)

    def test_holds_a_line_until_its_newline_arrives(self):
        self.events.write_bytes(b'event: par')
        first = self.run_once()
        self.assertNotIn('event: par', first)
        self.assertEqual(self.offset_file.read_text().strip(), '0')

        with open(self.events, 'ab') as handle:
            handle.write(b'tial\n')
        second = self.run_once()
        self.assertIn('event: partial', second)

    def test_replays_from_the_start_when_the_events_file_was_truncated(self):
        self.events.write_bytes(b'event: long name\n')
        self.run_once()
        self.events.write_bytes(b'new\n')
        lines = self.run_once()
        self.assertIn('new', lines)

    def test_strips_windows_line_endings_and_the_starting_byte_order_mark(self):
        self.events.write_bytes(b'\xef\xbb\xbfevent: one\r\nevent: two\r\n')
        lines = self.run_once()
        self.assertNotIn('\r', self.raw)
        self.assertNotIn('﻿', self.raw)
        self.assertIn('event: one', lines)
        self.assertIn('event: two', lines)

    def test_reports_an_old_state_file_in_the_status_line(self):
        state = Path(self.folder.name) / 'state.json'
        state.write_text('{}', encoding='utf-8')
        old = os.stat(state).st_mtime - 2000
        os.utime(state, (old, old))
        lines = self.run_once('--state', str(state), '--stale-after', '1000')
        self.assertIn('state ', lines[0])
        self.assertIn('old', lines[0])

    def test_status_line_reports_watcher_liveness(self):
        pidfile = Path(self.folder.name) / 'watch-public.pid.txt'
        pidfile.write_text('﻿pid 4242 created 2026-01-01T00:00:00\n', encoding='utf-8')

        def status(alive):
            with mock.patch.object(self.module, 'pid_alive', return_value=alive):
                return self.run_once('--pidfile', str(pidfile))[0]

        self.assertIn('watcher 4242 alive', status(True))
        self.assertIn('watcher 4242 not running', status(False))
        self.assertIn('watcher 4242 unknown', status(None))

    def test_reports_a_missing_state_file_without_an_age(self):
        lines = self.run_once('--state', str(Path(self.folder.name) / 'missing.json'))
        self.assertNotIn(' state ', lines[0])

    def test_pid_alive_reads_the_platform_process_listing(self):
        if os.name != 'nt':
            self.skipTest('the tasklist branch runs on Windows')
        listed = mock.Mock(returncode=0, stdout='python.exe 4242 Console 1 10,000 K\n')
        missing = mock.Mock(returncode=0, stdout='INFO: No tasks are running which match the specified criteria.\n')
        failed = mock.Mock(returncode=1, stdout='')
        with mock.patch.object(subprocess, 'run', return_value=listed):
            self.assertTrue(self.module.pid_alive(4242))
        with mock.patch.object(subprocess, 'run', return_value=missing):
            self.assertFalse(self.module.pid_alive(4242))
        with mock.patch.object(subprocess, 'run', return_value=failed):
            self.assertIsNone(self.module.pid_alive(4242))

    def test_status_line_reports_the_state_age_in_seconds(self):
        args = mock.Mock(pidfile=None)
        self.assertEqual(self.module.status_line(args, 12, None, None, 2000.4),
                         'watch: offset 12 watcher none state 2000s old')

    def test_state_age_is_measured_from_the_given_time(self):
        state = Path(self.folder.name) / 'state.json'
        state.write_text('{}', encoding='utf-8')
        os.utime(state, (1000.0, 1000.0))
        self.assertAlmostEqual(self.module.state_age(state, now=3000.0), 2000.0)

    def test_a_missing_state_is_not_a_recovery(self):
        self.assertEqual(self.module.state_event(True, None, 1000.0), (True, None))
        self.assertEqual(self.module.state_event(True, 5.0, 1000.0), (False, 'fresh'))
        self.assertEqual(self.module.state_event(False, 2000.0, 1000.0), (True, 'stale'))
        self.assertEqual(self.module.state_event(False, 5.0, 1000.0), (False, None))

    def test_saves_the_offset_by_replacement_and_reports_a_failed_save(self):
        self.events.write_bytes(b'event: one\n')
        self.run_once()
        self.assertFalse(Path(str(self.offset_file) + '.tmp').exists())

        err = io.StringIO()
        with mock.patch.object(self.module.os, 'replace', side_effect=OSError('denied')):
            with redirect_stderr(err), redirect_stdout(io.StringIO()):
                self.module.main(['--events', str(self.events), '--once'])
        self.assertIn('cannot save the offset', err.getvalue())


if __name__ == '__main__':
    unittest.main()
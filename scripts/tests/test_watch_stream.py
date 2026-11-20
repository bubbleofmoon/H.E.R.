#!/usr/bin/env python3
"""Tests for scripts/watch_stream.py with temporary files and a fake pid check.

Run: python -I scripts/tests/test_watch_stream.py
"""
import importlib.util
import io
import os
from pathlib import Path
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
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
        return out.getvalue().splitlines()

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


if __name__ == '__main__':
    unittest.main()
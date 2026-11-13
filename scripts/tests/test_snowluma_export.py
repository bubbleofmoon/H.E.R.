#!/usr/bin/env python3
"""Synthetic-only tests for scripts/snowluma_export.py (the SnowLuma export adapter, #58).

Every database here is built in a temp directory from invented rows. Ids are short invented
numbers, and the Chinese text is invented. Run: python -I scripts/tests/test_snowluma_export.py
"""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sqlite3
import tempfile
from unittest import mock
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
EXPORT = ROOT / 'scripts' / 'snowluma_export.py'
STRIP = ROOT / 'scripts' / 'strip_history.py'

SELF = '9001'
PEER = '1002'
MEMBER = '1001'
GROUP = '420042'

DDL = """CREATE TABLE messages (
    message_hash INTEGER PRIMARY KEY, is_group INTEGER, session_id TEXT, sequence INTEGER,
    sequence_authoritative INTEGER, event_name TEXT, client_sequence INTEGER,
    private_direction INTEGER, random INTEGER, timestamp INTEGER, data TEXT,
    classification_version INTEGER)"""


def load(path, name):
    """A script as a module. A missing file is a failed assertion, not an import error."""
    if not path.is_file():
        raise AssertionError(f'{path.relative_to(ROOT)} does not exist yet')
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def export_module():
    return load(EXPORT, 'snowluma_export')


def make_store(path, rows):
    """A synthetic SnowLuma-shaped store: rows are (hash, is_group, session, seq, pd, ts, data)."""
    conn = sqlite3.connect(path)
    conn.execute(DDL)
    conn.execute('ALTER TABLE messages ADD COLUMN media_data TEXT')
    for hash_, is_group, session, seq, pd, ts, data in rows:
        conn.execute('INSERT INTO messages (message_hash, is_group, session_id, sequence, event_name, '
                     'private_direction, timestamp, data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)',
                     (hash_, is_group, session, seq, 'message', pd, ts, data))
    conn.commit()
    conn.close()


def standard_rows():
    group_event = {
        'post_type': 'message', 'message_type': 'group', 'group_id': int(GROUP), 'group_name': '测试群',
        'self_id': int(SELF), 'user_id': int(MEMBER),
        'sender': {'user_id': int(MEMBER), 'card': '明明', 'nickname': '小明'},
        'message': [
            {'type': 'reply', 'data': {'id': '-55'}},
            {'type': 'at', 'data': {'qq': SELF}},
            {'type': 'text', 'data': {'text': '今天吃什么'}},
            {'type': 'image', 'data': {'file': 'x.jpg'}},
            {'type': 'forward', 'data': {'id': 'f1'}},
            {'type': 'face', 'data': {'id': '14'}},
            {'type': 'record', 'data': {'file': 'x.amr'}},
        ],
    }
    self_event = {'post_type': 'message', 'message_type': 'group', 'group_id': int(GROUP),
                  'self_id': int(SELF), 'user_id': int(SELF), 'sender': {'user_id': int(SELF)},
                  'message': [{'type': 'text', 'data': {'text': '收到'}}]}
    peer_in = {'post_type': 'message', 'message_type': 'private', 'self_id': int(SELF), 'user_id': int(PEER),
               'sender': {'user_id': int(PEER)}, 'message': [{'type': 'text', 'data': {'text': '你好'}}]}
    peer_out = {'post_type': 'message_sent', 'message_type': 'private', 'self_id': int(SELF),
                'user_id': int(SELF), 'target_id': int(PEER), 'sender': {'user_id': int(SELF)},
                'message': [{'type': 'text', 'data': {'text': '在的'}}]}
    return [
        (101, 1, GROUP, 1, -1, 1000, json.dumps(group_event, ensure_ascii=False)),
        (102, 1, GROUP, 2, -1, 1001, json.dumps(self_event, ensure_ascii=False)),
        (103, 0, PEER, 3, 0, 1002, json.dumps(peer_in, ensure_ascii=False)),
        (104, 0, PEER, 4, 1, 1003, json.dumps(peer_out, ensure_ascii=False)),
        (105, 1, GROUP, 5, -1, 1004, None),
        (106, 0, PEER, 6, 0, 1005, '{bad'),
    ]


class ExportMapping(unittest.TestCase):
    def test_export_maps_rows_to_schema_v1(self):
        module = export_module()
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / 'copy.db'
            out = Path(tmp) / 'out.jsonl'
            make_store(db, standard_rows())
            printed = io.StringIO()
            with contextlib.redirect_stdout(printed):
                code = module.export(db, out, convs=[])
            self.assertEqual(code, 0)
            records = [json.loads(line) for line in out.read_text(encoding='utf-8').splitlines()]
            self.assertEqual(len(records), 4, 'the no-data and bad-json rows are not written')
            self.assertEqual([r['t'] for r in records], [1000 * 1000, 1001 * 1000, 1002 * 1000, 1003 * 1000])
            self.assertEqual(records[0], {
                'v': 1, 'src': 'snowluma', 'conv': f'group:{GROUP}', 'dir': 'in',
                'sender': {'id': MEMBER, 'card': '明明', 'nick': '小明'}, 'convName': '测试群',
                't': 1000 * 1000, 'mid': '101',
                'segs': [{'k': 'reply', 'mid': '-55'}, {'k': 'at', 'id': SELF}, {'k': 'text', 'text': '今天吃什么'},
                         {'k': 'media', 'kind': 'image'}, {'k': 'forward'}, {'k': 'face', 'id': '14'},
                         {'k': 'media', 'kind': 'voice'}],
            })
            self.assertEqual(records[1]['dir'], 'out')
            self.assertEqual(records[2]['conv'], f'private:{PEER}')
            self.assertEqual(records[2]['dir'], 'in')
            self.assertEqual(records[3]['dir'], 'out')
            self.assertEqual(records[3]['conv'], f'private:{PEER}')
            self.assertIn('no_data=1', printed.getvalue())
            self.assertIn('bad_json=1', printed.getvalue())


class CqAndFilters(unittest.TestCase):
    def test_cq_string_message_is_parsed(self):
        module = export_module()
        segs = module.segments('[CQ:at,qq=1001] 你好[CQ:image,file=x.jpg]&#91;不是码&#93;')
        self.assertEqual(segs, [{'k': 'at', 'id': '1001'}, {'k': 'text', 'text': ' 你好'},
                                {'k': 'media', 'kind': 'image'}, {'k': 'text', 'text': '[不是码]'}])

    def test_conv_filter_keeps_only_named_conversations(self):
        module = export_module()
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / 'copy.db'
            out = Path(tmp) / 'out.jsonl'
            make_store(db, standard_rows())
            printed = io.StringIO()
            with contextlib.redirect_stdout(printed):
                code = module.export(db, out, convs=[f'group:{GROUP}'])
            self.assertEqual(code, 0)
            records = [json.loads(line) for line in out.read_text(encoding='utf-8').splitlines()]
            self.assertEqual([r['mid'] for r in records], ['101', '102'])
            self.assertTrue(all(r['conv'] == f'group:{GROUP}' for r in records))
            self.assertIn('filtered=2', printed.getvalue())


class Snapshot(unittest.TestCase):
    def test_snapshot_copies_committed_wal_pages_without_new_files(self):
        module = export_module()
        with tempfile.TemporaryDirectory() as tmp:
            live_dir = Path(tmp) / 'live'
            copy_dir = Path(tmp) / 'copy'
            live_dir.mkdir()
            copy_dir.mkdir()
            live = live_dir / 'messages.db'
            writer = sqlite3.connect(live)
            try:
                writer.execute('PRAGMA journal_mode=WAL')
                writer.execute('PRAGMA wal_autocheckpoint=0')
                writer.execute(DDL)
                writer.executemany('INSERT INTO messages (message_hash, is_group, session_id, sequence, event_name, '
                                   'private_direction, timestamp, data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)',
                                   [(r[0], r[1], r[2], r[3], 'message', r[4], r[5], r[6]) for r in standard_rows()])
                writer.commit()
                self.assertTrue((live_dir / 'messages.db-wal').stat().st_size > 0, 'committed rows must sit in -wal')
                before = sorted(os.listdir(live_dir))
                copy = copy_dir / 'messages.db'
                self.assertEqual(module.snapshot(live, copy), 0)
                self.assertEqual(sorted(os.listdir(live_dir)), before, 'the live directory must not change')
                self.assertEqual(sorted(os.listdir(copy_dir)), ['messages.db'], 'the copy must be a single file')
                check = sqlite3.connect(copy)
                try:
                    self.assertEqual(check.execute('SELECT COUNT(*) FROM messages').fetchone()[0], 6)
                    self.assertEqual(check.execute('PRAGMA journal_mode').fetchone()[0], 'delete')
                finally:
                    check.close()
            finally:
                writer.close()


    def test_snapshot_refuses_without_wal_and_shm(self):
        module = export_module()
        with tempfile.TemporaryDirectory() as tmp:
            live_dir = Path(tmp) / 'live'
            copy_dir = Path(tmp) / 'copy'
            live_dir.mkdir()
            copy_dir.mkdir()
            live = live_dir / 'messages.db'
            make_store(live, standard_rows())
            before = sorted(os.listdir(live_dir))
            copy = copy_dir / 'messages.db'
            self.assertEqual(module.snapshot(live, copy), 2)
            self.assertFalse(copy.exists(), 'a refused snapshot writes no copy')
            self.assertEqual(sorted(os.listdir(live_dir)), before, 'no file may be created beside the live store')
            self.assertEqual(os.listdir(copy_dir), [])


class StripClean(unittest.TestCase):
    def test_export_output_strips_clean(self):
        module = export_module()
        strip = load(STRIP, 'strip_history')
        key = bytes(range(32))
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / 'copy.db'
            out = Path(tmp) / 'out.jsonl'
            make_store(db, standard_rows())
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(module.export(db, out, convs=[]), 0)
            records = [json.loads(line) for line in out.read_text(encoding='utf-8').splitlines()]
            stripper = strip.Stripper(key, records)
            stripped = [stripper.record(r) for r in records]
            for record in stripped:
                self.assertEqual(dict(strip.self_scan(record, stripper.name_re)), {},
                                 f'leftovers in stripped record {record["mid"]}')
            self.assertNotIn(MEMBER, json.dumps(stripped, ensure_ascii=False))
            self.assertNotIn('小明', json.dumps(stripped, ensure_ascii=False))


class FailAfterFirstWrite:
    """Wraps a real file so the second write raises, as a full disk would."""

    def __init__(self, handle):
        self.handle, self.writes = handle, 0

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.handle.close()
        return False

    def write(self, text):
        self.writes += 1
        if self.writes > 1:
            raise OSError(28, 'simulated write failure')
        return self.handle.write(text)


class BrokenBackup:
    """A source connection whose online backup fails after the copy file already exists."""

    def __init__(self, conn):
        self.conn = conn

    def __getattr__(self, name):
        return getattr(self.conn, name)

    def backup(self, *args, **kwargs):
        raise sqlite3.OperationalError('simulated backup failure')


class FailurePaths(unittest.TestCase):
    def test_export_write_error_removes_the_partial_file(self):
        module = export_module()
        real_open = open
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / 'copy.db'
            out = Path(tmp) / 'out.jsonl'
            make_store(db, standard_rows())

            def failing_open(path, mode='r', *args, **kwargs):
                return FailAfterFirstWrite(real_open(path, mode, *args, **kwargs))

            with mock.patch.object(module, 'open', failing_open, create=True), \
                    contextlib.redirect_stdout(io.StringIO()):
                code = module.export(db, out, convs=[])
            self.assertEqual(code, 1, 'a write error must fail the export')
            self.assertFalse(Path(str(out) + '.partial').exists(), 'the partial file must be removed')
            self.assertFalse(out.exists(), 'no output may appear after a failed write')

    def test_snapshot_backup_error_removes_the_partial_copy(self):
        module = export_module()
        real_connect = sqlite3.connect
        with tempfile.TemporaryDirectory() as tmp:
            live_dir = Path(tmp) / 'live'
            copy_dir = Path(tmp) / 'copy'
            live_dir.mkdir()
            copy_dir.mkdir()
            live = live_dir / 'messages.db'
            writer = sqlite3.connect(live)
            try:
                writer.execute('PRAGMA journal_mode=WAL')
                writer.execute(DDL)
                writer.commit()
                before = sorted(os.listdir(live_dir))
                copy = copy_dir / 'messages.db'

                def connect(target, *args, **kwargs):
                    conn = real_connect(target, *args, **kwargs)
                    return BrokenBackup(conn) if 'mode=ro' in str(target) else conn

                with mock.patch.object(module.sqlite3, 'connect', connect), \
                        contextlib.redirect_stdout(io.StringIO()):
                    code = module.snapshot(live, copy)
                self.assertEqual(code, 1, 'a backup error must fail the snapshot')
                self.assertFalse(copy.exists(), 'the partial copy must be removed')
                self.assertEqual(os.listdir(copy_dir), [], 'no copy or sidecar may remain')
                self.assertEqual(sorted(os.listdir(live_dir)), before)
            finally:
                writer.close()


if __name__ == '__main__':
    unittest.main()

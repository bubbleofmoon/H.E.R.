#!/usr/bin/env python3
"""Synthetic-only tests for scripts/snowluma_export.py (the SnowLuma export adapter, #58).

Every database here is built in a temp directory from invented rows. Ids are short invented
numbers, and the Chinese text is invented. Run: python -I scripts/tests/test_snowluma_export.py
"""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
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

CHILD = '\n'.join([
    'import os, sqlite3, sys',
    'c = sqlite3.connect(sys.argv[1])',
    "c.execute('PRAGMA journal_mode=WAL')",
    "c.execute('PRAGMA wal_autocheckpoint=0')",
    f'c.execute({DDL!r})',
    "c.execute(\"INSERT INTO messages (message_hash,is_group,session_id,sequence,event_name,private_direction,timestamp,data) VALUES (1,1,'g',1,'message',-1,1000,'{}')\")",
    'c.commit()',
    "if sys.argv[2] == 'crash':",
    '    os._exit(0)',
    "print('ready', flush=True)",
    'sys.stdin.readline()',
    'c.close()',
])


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


def make_sealed_store(path, rows):
    make_store(path, rows)
    module = export_module()
    if not hasattr(module, 'seal'):
        raise AssertionError('snowluma_export.py has no seal() yet: the export cannot require a sealed snapshot')
    module.seal(Path(path), len(rows))


def open_wal_store(path, rows):
    """A WAL store with no checkpoint on commit; the caller keeps the connection open."""
    writer = sqlite3.connect(path)
    writer.execute('PRAGMA journal_mode=WAL')
    writer.execute('PRAGMA wal_autocheckpoint=0')
    writer.execute(DDL)
    writer.executemany('INSERT INTO messages (message_hash, is_group, session_id, sequence, event_name, '
                       'private_direction, timestamp, data) VALUES (?, ?, ?, ?, ?, ?, ?, ?)',
                       [(r[0], r[1], r[2], r[3], 'message', r[4], r[5], r[6]) for r in rows])
    writer.commit()
    return writer


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def read_ready(proc, seconds):
    """The child's first stdout line, or None when it does not arrive within the deadline."""
    box = {}
    reader = threading.Thread(target=lambda: box.update(line=proc.stdout.readline()), daemon=True)
    reader.start()
    reader.join(seconds)
    if reader.is_alive():
        return None
    return box.get('line', '')


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
            make_sealed_store(db, standard_rows())
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
            make_sealed_store(db, standard_rows())
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
                self.assertEqual(sorted(os.listdir(copy_dir)), ['messages.db', 'messages.db.snapshot.json'])
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
            make_sealed_store(db, standard_rows())
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
            make_sealed_store(db, standard_rows())

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


class Sealing(unittest.TestCase):
    def test_export_refuses_an_unsealed_store(self):
        module = export_module()
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / 'copy.db'
            out = Path(tmp) / 'out.jsonl'
            writer = open_wal_store(db, standard_rows())
            try:
                with contextlib.redirect_stdout(io.StringIO()):
                    code = module.export(db, out, convs=[])
            finally:
                writer.close()
            self.assertEqual(code, 2, 'an unsealed store must be refused')
            self.assertFalse(out.exists(), 'a refused export writes no output')

    def test_export_refuses_a_modified_snapshot(self):
        module = export_module()
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / 'copy.db'
            out = Path(tmp) / 'out.jsonl'
            make_sealed_store(db, standard_rows())
            conn = sqlite3.connect(db)
            conn.execute("INSERT INTO messages (message_hash, is_group, session_id, sequence, event_name, "
                         "private_direction, timestamp, data) VALUES (999, 0, 'p', 99, 'message', 0, 2000, '{}')")
            conn.commit()
            conn.close()
            with contextlib.redirect_stdout(io.StringIO()):
                code = module.export(db, out, convs=[])
            self.assertEqual(code, 2, 'a snapshot changed after sealing must be refused')
            self.assertFalse(out.exists())


class Surrogates(unittest.TestCase):
    def test_lone_surrogate_is_replaced_and_no_partial_remains(self):
        module = export_module()
        lone = '{"post_type":"message","message_type":"group","message":[{"type":"text","data":{"text":"\\ud800"}}]}'
        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / 'copy.db'
            out = Path(tmp) / 'out.jsonl'
            make_sealed_store(db, [(201, 1, GROUP, 1, -1, 1000, lone)])
            printed = io.StringIO()
            try:
                with contextlib.redirect_stdout(printed):
                    code = module.export(db, out, convs=[])
            except UnicodeError as error:
                self.fail(f'export raised {error}: a lone surrogate must become U+FFFD, not an exception')
            self.assertEqual(code, 0)
            self.assertIn('\ufffd', out.read_text(encoding='utf-8'))
            self.assertIn('surrogates_replaced=1', printed.getvalue())
            self.assertFalse(Path(str(out) + '.partial').exists(), 'no partial file may remain')


class CqParsing(unittest.TestCase):
    def test_cq_card_payload_never_becomes_text(self):
        module = export_module()
        segs = module.segments('看[CQ:json,data={"app":"x","prompt":"[秘密]卡片"}]后面')
        self.assertEqual(segs, [{'k': 'text', 'text': '看'}, {'k': 'media', 'kind': 'card'}])

    def test_unparsable_cq_code_never_becomes_text(self):
        module = export_module()
        segs = module.segments('a[CQ:image,file=[x]b')
        for seg in segs:
            if seg['k'] == 'text':
                self.assertNotIn('x', seg['text'], f'payload leaked into text: {segs!r}')

    def test_cq_at_name_is_kept_for_the_stripper(self):
        module = export_module()
        self.assertEqual(module.segments('[CQ:at,qq=1001,name=阿强]'),
                         [{'k': 'at', 'id': '1001', 'name': '阿强'}])


class SnapshotRefusals(unittest.TestCase):
    def test_snapshot_refuses_a_copy_inside_the_live_directory(self):
        module = export_module()
        with tempfile.TemporaryDirectory() as tmp:
            live_dir = Path(tmp) / 'live'
            live_dir.mkdir()
            live = live_dir / 'messages.db'
            writer = open_wal_store(live, standard_rows())
            try:
                before = sorted(os.listdir(live_dir))
                for copy in (live_dir / 'sub' / 'x.db', live_dir / 'x.db'):
                    with self.subTest(copy=str(copy.relative_to(live_dir))):
                        with contextlib.redirect_stdout(io.StringIO()):
                            self.assertEqual(module.snapshot(live, copy), 2)
                        self.assertFalse(copy.exists(), 'a refused snapshot writes no copy')
                        self.assertFalse((live_dir / 'sub').exists(), 'no directory may be created')
                        self.assertEqual(sorted(os.listdir(live_dir)), before)
            finally:
                writer.close()

    def test_rejected_snapshot_leaves_no_copy(self):
        module = export_module()
        real_listdir = os.listdir
        with tempfile.TemporaryDirectory() as tmp:
            live_dir = Path(tmp) / 'live'
            copy_dir = Path(tmp) / 'copy'
            live_dir.mkdir()
            copy_dir.mkdir()
            live = live_dir / 'messages.db'
            writer = open_wal_store(live, standard_rows())
            calls = {'live': 0}
            copy = copy_dir / 'messages.db'
            marker = copy_dir / 'messages.db.snapshot.json'

            def listdir(path='.'):
                names = real_listdir(path)
                if Path(os.fspath(path)).resolve() == live_dir.resolve():
                    calls['live'] += 1
                    if calls['live'] >= 2:
                        return names + ['new.file']
                return names

            try:
                with mock.patch.object(module.os, 'listdir', side_effect=listdir), \
                        contextlib.redirect_stdout(io.StringIO()):
                    code = module.snapshot(live, copy)
            finally:
                writer.close()
            self.assertEqual(code, 1, 'a changed live directory must reject the snapshot')
            self.assertFalse(copy.exists(), 'a rejected snapshot leaves no copy')
            self.assertFalse(marker.exists(), 'a rejected snapshot leaves no marker')

    def test_snapshot_without_messages_table_is_a_clean_refusal(self):
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
                writer.execute('CREATE TABLE other (x)')
                writer.commit()
                try:
                    with contextlib.redirect_stdout(io.StringIO()):
                        code = module.snapshot(live, copy_dir / 'messages.db')
                except sqlite3.Error as error:
                    self.fail(f'snapshot raised {error}: a store without a messages table must be a clean refusal')
                self.assertEqual(code, 1)
                self.assertEqual(os.listdir(copy_dir), [], 'no copy may remain')
            finally:
                writer.close()


class SnapshotGate(unittest.TestCase):
    """Real child processes hold the store. Every child is waited for or killed in a finally."""

    def test_snapshot_of_a_crashed_idle_store_keeps_its_files(self):
        module = export_module()
        with tempfile.TemporaryDirectory() as tmp:
            live_dir = Path(tmp) / 'live'
            copy_dir = Path(tmp) / 'copy'
            live_dir.mkdir()
            copy_dir.mkdir()
            live = live_dir / 'messages.db'
            subprocess.run([sys.executable, '-I', '-c', CHILD, str(live), 'crash'],
                           check=True, capture_output=True, timeout=120,
                           creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
            wal, shm = live_dir / 'messages.db-wal', live_dir / 'messages.db-shm'
            self.assertTrue(wal.is_file() and shm.is_file(), 'a crashed store must keep its -wal and -shm')
            before_list, before_db, before_wal = sorted(os.listdir(live_dir)), digest(live), digest(wal)
            printed = io.StringIO()
            with contextlib.redirect_stdout(printed):
                code = module.snapshot(live, copy_dir / 'messages.db')
            self.assertEqual(code, 0, printed.getvalue())
            self.assertEqual(sorted(os.listdir(live_dir)), before_list, 'the live directory must not change')
            self.assertEqual(digest(live), before_db, 'the live database must not change')
            self.assertEqual(digest(wal), before_wal, 'the live -wal must not change')
            check = sqlite3.connect(copy_dir / 'messages.db')
            try:
                self.assertEqual(check.execute('SELECT COUNT(*) FROM messages').fetchone()[0], 1)
            finally:
                check.close()
            self.assertIn('no_ckpt_on_close=', printed.getvalue())

    def test_snapshot_while_another_process_holds_the_store(self):
        module = export_module()
        with tempfile.TemporaryDirectory() as tmp:
            live_dir = Path(tmp) / 'live'
            copy_dir = Path(tmp) / 'copy'
            live_dir.mkdir()
            copy_dir.mkdir()
            live = live_dir / 'messages.db'
            proc = subprocess.Popen([sys.executable, '-I', '-c', CHILD, str(live), 'hold'],
                                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    text=True, encoding='utf-8',
                                    creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
            try:
                ready = read_ready(proc, 30)
                self.assertIsNotNone(ready, 'the child did not report ready within 30 s')
                self.assertEqual(ready.strip(), 'ready', 'the child must hold the store and report ready')
                before_list = sorted(os.listdir(live_dir))
                with contextlib.redirect_stdout(io.StringIO()):
                    code = module.snapshot(live, copy_dir / 'messages.db')
                self.assertEqual(code, 0, 'a store held by another process must still snapshot')
                self.assertEqual(sorted(os.listdir(live_dir)), before_list)
                proc.stdin.write('\n')
                proc.stdin.close()
                self.assertEqual(proc.wait(timeout=60), 0, 'the child must exit cleanly')
            finally:
                if proc.poll() is None:
                    proc.kill()
                proc.wait(timeout=60)
                for pipe in (proc.stdin, proc.stdout, proc.stderr):
                    if pipe is not None:
                        pipe.close()


class RawBracket(unittest.TestCase):
    def test_raw_bracket_in_cq_parameters_makes_the_code_unparsable(self):
        module = export_module()
        segs = module.segments('a[CQ:image,file=[x]secret]')
        for seg in segs:
            if seg['k'] == 'text':
                self.assertNotIn('secret', seg['text'], f'payload leaked into text: {segs!r}')
                self.assertNotIn('x', seg['text'], f'payload leaked into text: {segs!r}')


class VerifyCleanup(unittest.TestCase):
    def test_verification_error_removes_the_copy(self):
        module = export_module()
        real_listdir = os.listdir
        with tempfile.TemporaryDirectory() as tmp:
            live_dir = Path(tmp) / 'live'
            copy_dir = Path(tmp) / 'copy'
            live_dir.mkdir()
            copy_dir.mkdir()
            live = live_dir / 'messages.db'
            copy = copy_dir / 'messages.db'
            writer = open_wal_store(live, standard_rows())
            try:
                calls = {'n': 0}

                def listdir(path='.'):
                    calls['n'] += 1
                    if calls['n'] == 3:
                        raise PermissionError(13, 'denied')
                    return real_listdir(path)

                try:
                    with mock.patch.object(module.os, 'listdir', side_effect=listdir), \
                            contextlib.redirect_stdout(io.StringIO()):
                        code = module.snapshot(live, copy)
                except PermissionError:
                    self.fail('the verification error escaped snapshot() and skipped the cleanup')
            finally:
                writer.close()
            self.assertEqual(code, 1, 'a verification error must fail the snapshot')
            self.assertFalse(copy.exists(), 'the copy must be removed')
            self.assertFalse(module.marker_path(copy).exists(), 'the marker must be removed')


class SealRace(unittest.TestCase):
    def test_a_change_between_the_seal_check_and_the_read_is_refused(self):
        module = export_module()
        real_sealed = module.sealed
        inserted = {'done': False}

        def sealed_then_insert(path):
            ok = real_sealed(path)
            if not inserted['done']:
                inserted['done'] = True
                conn = sqlite3.connect(path)
                conn.execute("INSERT INTO messages (message_hash, is_group, session_id, sequence, event_name, "
                             "private_direction, timestamp, data) VALUES (998, 0, 'p', 98, 'message', 0, 2000, '{}')")
                conn.commit()
                conn.close()
            return ok

        with tempfile.TemporaryDirectory() as tmp:
            db = Path(tmp) / 'copy.db'
            out = Path(tmp) / 'out.jsonl'
            make_sealed_store(db, standard_rows())
            with mock.patch.object(module, 'sealed', side_effect=sealed_then_insert), \
                    contextlib.redirect_stdout(io.StringIO()):
                code = module.export(db, out, convs=[])
            self.assertTrue(inserted['done'], 'the change must happen after the seal check')
            self.assertEqual(code, 2, 'a snapshot that changed before the read must be refused')
            self.assertFalse(out.exists(), 'a refused export writes no output')


class ReadyWait(unittest.TestCase):
    def test_readiness_wait_gives_up_after_its_deadline(self):
        helper = globals().get('read_ready')
        self.assertTrue(callable(helper), 'read_ready(proc, seconds) does not exist yet')
        proc = subprocess.Popen([sys.executable, '-I', '-c', 'import time; time.sleep(3600)'],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, encoding='utf-8',
                                creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
        try:
            self.assertIsNone(helper(proc, 1), 'a child that never reports ready must time out')
        finally:
            if proc.poll() is None:
                proc.kill()
            proc.wait(timeout=60)
            for pipe in (proc.stdin, proc.stdout, proc.stderr):
                if pipe is not None:
                    pipe.close()


if __name__ == '__main__':
    unittest.main()

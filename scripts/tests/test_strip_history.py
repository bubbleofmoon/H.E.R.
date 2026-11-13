#!/usr/bin/env python3
"""Synthetic-only tests for scripts/strip_history.py (the history strip tool, #58).

Every value is invented and assembled from parts, so this file passes the public guard.
Run: HER_GITLEAKS=<gitleaks> python -I scripts/tests/test_strip_history.py
"""
from collections import Counter
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
STRIP = ROOT / 'scripts' / 'strip_history.py'

QQ_A = '31415' + '9265'            # invented 9-digit account
QQ_B = '27182' + '8182'            # invented 9-digit account, never in a record field
GROUP = '16180' + '339'            # invented 8-digit group
PHONE = '138' + '0013' + '8000'    # invented mobile number
FULLWIDTH_QQ = '３１４１５' + '９２６５'   # QQ_A written with full-width digits
FIVE_DIGITS = '12' + '345'         # a run of exactly five digits
FOUR_DIGITS = '2024'               # a run of exactly four digits, below the threshold


def gitleaks():
    """The pinned secret scanner: HER_GITLEAKS, then PATH (CI installs it first)."""
    override = os.environ.get('HER_GITLEAKS')
    if override and Path(override).is_file():
        return override
    return shutil.which('gitleaks')


REQUIRES_SCANNER = unittest.skipUnless(gitleaks(), 'gitleaks not found: set HER_GITLEAKS or run scripts/ci/install-gitleaks.py')


def load():
    """The strip tool as a module. A missing file is a failed assertion, not an import error."""
    if not STRIP.is_file():
        raise AssertionError('scripts/strip_history.py does not exist: the strip tool is not written yet')
    spec = importlib.util.spec_from_file_location('strip_history', STRIP)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class TextReplacements(unittest.TestCase):
    def test_text_replacements_remove_every_identifier(self):
        s_mod = load()
        key = bytes(range(32))
        records = [{
            'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
            'sender': {'id': QQ_A, 'card': '明明', 'nick': '小明'}, 'convName': '测试群',
            't': 1000, 'mid': '77', 'segs': [{'k': 'text', 'text': 'hello'}],
        }]
        stripper = s_mod.Stripper(key, records)
        text = (f"小明的QQ是{QQ_B}，手机{PHONE}，邮箱 fake.user@example.com，"
                "地址上海市浦东新区世纪大道100号3栋201室，"
                "看 https://example.com/s?token=abc 和 b23.tv/xyz ，"
                f"还有{FULLWIDTH_QQ}，年份{FOUR_DIGITS}，编号{FIVE_DIGITS}")
        out = stripper.text(text)
        for raw in (QQ_B, PHONE, 'fake.user', 'example.com', 'b23.tv', '世纪大道', '小明', '３１４１５'):
            self.assertNotIn(raw, out, f'raw value {raw!r} survived the strip')
        for placeholder in ('<电话>', '<邮箱>', '<链接>', '<地址>', '〔P-'):
            self.assertIn(placeholder, out, f'placeholder {placeholder!r} missing from {out!r}')
        self.assertTrue('<数字>' in out or '〔P-' in out, 'digit run not replaced')
        self.assertIn(FOUR_DIGITS, out, 'a four-digit run must stay: the threshold is five')
        self.assertIn('<数字>', out, 'a five-digit run must be replaced')
        self.assertNotIn(FIVE_DIGITS, out, 'a five-digit run survived the strip')
        self.assertEqual(stripper.counts['short_names_unreplaced'], 0)
        leftovers = s_mod.self_scan({'segs': [{'k': 'text', 'text': out}]}, stripper.name_re)
        self.assertEqual(dict(leftovers), {}, 'self-scan found leftovers in the stripped text')


class Pseudonyms(unittest.TestCase):
    def test_pseudonyms_are_stable_keyed_and_letters_only(self):
        s_mod = load()
        k1, k2 = bytes(range(32)), bytes(range(1, 33))
        first = s_mod.code(k1, 'acct', QQ_A)
        self.assertEqual(first, s_mod.code(k1, 'acct', QQ_A), 'same key and value must give the same code')
        self.assertNotEqual(first, s_mod.code(k2, 'acct', QQ_A), 'a different key must give a different code')
        self.assertRegex(first, r'^P-[a-z]{8}$')

    def test_known_id_in_text_maps_to_the_person_code(self):
        s_mod = load()
        key = bytes(range(32))
        records = [{'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
                    'sender': {'id': QQ_A, 'card': '', 'nick': ''}, 'segs': []}]
        stripper = s_mod.Stripper(key, records)
        self.assertEqual(stripper.text(QQ_A), f'〔{s_mod.code(key, "acct", QQ_A)}〕')


class RecordStructure(unittest.TestCase):
    def test_record_structure_is_kept_with_pseudonyms(self):
        s_mod = load()
        key = bytes(range(32))
        replied = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
                   'sender': {'id': QQ_B, 'card': '', 'nick': ''}, 'mid': '-55', 't': 900, 'segs': []}
        record = {
            'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
            'sender': {'id': QQ_A, 'card': '明明', 'nick': '小明'}, 'convName': '测试群', 'rt': 5000,
            't': 1000, 'mid': '77', 'raw': {'anything': 'x'},
            'segs': [
                {'k': 'reply', 'mid': '-55'},
                {'k': 'at', 'id': QQ_B},
                {'k': 'at', 'id': 'all'},
                {'k': 'text', 'text': 'hi'},
                {'k': 'forward'},
                {'k': 'face', 'id': '14'},
                {'k': 'media', 'kind': 'image', 'url': 'https://example.com/x.png'},
                {'k': 'media', 'kind': 'voice', 'file': 'x.amr'},
                {'k': 'media', 'kind': 'file'},
            ],
        }
        stripper = s_mod.Stripper(key, [replied, record])
        out = stripper.record(record)
        replied_out = stripper.record(replied)
        self.assertEqual(out['time'], '1970-01-01T00:00:01Z')
        self.assertEqual([s['k'] for s in out['segs']],
                         ['reply', 'at', 'at', 'text', 'forward', 'face', 'media', 'media', 'media'])
        self.assertEqual(out['segs'][0]['mid'], replied_out['mid'], 'reply must point at the replied record')
        self.assertEqual(out['segs'][0]['mid'], s_mod.code(key, 'mid', '-55'))
        self.assertEqual(out['segs'][1]['id'], s_mod.code(key, 'acct', QQ_B))
        self.assertEqual(out['segs'][2]['id'], 'all')
        self.assertNotIn('who', out['segs'][1], 'at segments keep the input key id')
        self.assertEqual(out['segs'][5], {'k': 'face', 'id': '14'})
        self.assertEqual(out['segs'][6:], [{'k': 'media', 'kind': 'image'},
                                           {'k': 'media', 'kind': 'voice'},
                                           {'k': 'media', 'kind': 'file'}])
        for dropped in ('card', 'nick', 'convName', 'rt', 'raw'):
            self.assertNotIn(dropped, out, f'{dropped!r} must not reach the stripped record')
        self.assertNotIn(QQ_A, repr(out))
        self.assertNotIn('小明', repr(out))


class SelfScan(unittest.TestCase):
    def test_self_scan_reports_leftovers(self):
        s_mod = load()
        name_re = re.compile('小明')
        leaked = {'segs': [{'k': 'text', 'text': '号码' + '12345' + '6789' + '，邮箱 a.b@example.com，小明'}]}
        found = s_mod.self_scan(leaked, name_re)
        self.assertEqual(found.get('digits'), 1, found)
        self.assertEqual(found.get('email'), 1, found)
        self.assertEqual(found.get('name'), 1, found)
        clean = {'segs': [{'k': 'text', 'text': '号码' + '1234' + '，' + '<邮箱>' + '，〔P-abcdefgh〕'}]}
        self.assertEqual(dict(s_mod.self_scan(clean, name_re)), {}, 'four digits and placeholders must pass')
        path = {'segs': [{'k': 'text', 'text': 'see ' + '/Users' + '/fakeuser/x.txt'}]}
        self.assertEqual(s_mod.self_scan(path, None).get('user-path'), 1)


class ReadRecords(unittest.TestCase):
    def test_torn_tail_is_skipped_and_a_bad_middle_line_fails(self):
        s_mod = load()
        good = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in', 'segs': []}
        with tempfile.TemporaryDirectory() as tmp:
            torn = Path(tmp) / 'torn.jsonl'
            torn.write_text(json.dumps(good) + '\n' + json.dumps(good) + '\n' + '{"v":1,"src":"ing', encoding='utf-8')
            counts = Counter()
            records = s_mod.read_records([torn], counts)
            self.assertEqual(len(records), 2, 'the two complete lines must be read')
            self.assertEqual(counts['torn_tail'], 1, 'a torn last line without a newline is counted, not fatal')

            bad = Path(tmp) / 'bad.jsonl'
            bad.write_text(json.dumps(good) + '\n' + '{not json' + '\n' + json.dumps(good) + '\n', encoding='utf-8')
            with self.assertRaises(SystemExit) as raised:
                s_mod.read_records([bad], Counter())
            self.assertIn('bad.jsonl:2', str(raised.exception))
            self.assertNotIn(str(tmp), str(raised.exception), 'errors print the file name, never the path')

            wrong = Path(tmp) / 'wrong.jsonl'
            wrong.write_text(json.dumps({'v': 2}) + '\n' + json.dumps(good) + '\n', encoding='utf-8')
            with self.assertRaises(SystemExit) as raised:
                s_mod.read_records([wrong], Counter())
            self.assertIn('wrong.jsonl:1', str(raised.exception))


class KeyFile(unittest.TestCase):
    def test_init_key_refuses_to_replace(self):
        s_mod = load()
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            s_mod.init_key(key_file)
            body = key_file.read_bytes()
            self.assertRegex(body.decode('ascii'), r'^[0-9a-f]{64}\n$', 'key file must hold 64 hex characters and a newline')
            with self.assertRaises(SystemExit):
                s_mod.init_key(key_file)
            self.assertEqual(key_file.read_bytes(), body, 'a second init-key must not replace the key')
            self.assertEqual(len(s_mod.load_key(key_file)), 32)


def cli(args, env=None):
    merged = dict(os.environ)
    found = gitleaks()
    if found:
        merged['HER_GITLEAKS'] = found
    merged.update(env or {})
    proc = subprocess.run([sys.executable, '-I', str(STRIP)] + args, capture_output=True, text=True,
                          encoding='utf-8', env=merged)
    return proc.returncode, (proc.stdout or '') + (proc.stderr or '')


def synthetic_records():
    member = {'id': QQ_A, 'card': '明明', 'nick': '小明'}
    return [
        {'v': 1, 'src': 'snowluma', 'conv': f'group:{GROUP}', 'dir': 'in', 'sender': member,
         'convName': '测试群', 't': 1000 * 1000, 'mid': '77', 'segs': [
             {'k': 'reply', 'mid': '76'}, {'k': 'at', 'id': QQ_B},
             {'k': 'text', 'text': f"小明的QQ是{QQ_B}，手机{PHONE}，邮箱 fake.user@example.com，看 https://example.com/s?x=1，编号{FIVE_DIGITS}"},
             {'k': 'media', 'kind': 'image'}]},
        {'v': 1, 'src': 'ingress', 'conv': f'private:{QQ_B}', 'dir': 'out', 'sender': None,
         't': 1001 * 1000, 'mid': '78', 'segs': [{'k': 'text', 'text': '你好'}]},
    ]


class StripCli(unittest.TestCase):
    def test_missing_key_writes_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / 'in.jsonl'
            source.write_text('\n'.join(json.dumps(r, ensure_ascii=False) for r in synthetic_records()) + '\n', encoding='utf-8')
            out_dir = Path(tmp) / 'out'
            code, output = cli(['strip', '--key-file', str(Path(tmp) / 'absent.key'), '--input', str(source),
                                '--out-dir', str(out_dir), '--name', 'case'])
            self.assertNotEqual(code, 0, 'strip with a missing key must exit non-zero: ' + output)
            self.assertTrue(not out_dir.exists() or not any(out_dir.iterdir()), 'a failed strip must write nothing')

    @REQUIRES_SCANNER
    def test_cli_strip_writes_jsonl_and_counts_only_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            code, output = cli(['init-key', '--key-file', str(key_file)])
            self.assertEqual(code, 0, output)
            self.assertTrue(key_file.is_file(), 'init-key must write the key file')
            source = Path(tmp) / 'in.jsonl'
            source.write_text('\n'.join(json.dumps(r, ensure_ascii=False) for r in synthetic_records()) + '\n', encoding='utf-8')
            out_dir = Path(tmp) / 'out'
            code, output = cli(['strip', '--key-file', str(key_file), '--input', str(source),
                                '--out-dir', str(out_dir), '--name', 'case'])
            self.assertEqual(code, 0, output)
            self.assertTrue(out_dir.is_dir(), 'strip must create the output directory')
            self.assertEqual(sorted(p.name for p in out_dir.iterdir()), ['case.jsonl', 'case.report.json'],
                             'no temp directory or other artifact may remain')
            body = (out_dir / 'case.jsonl').read_bytes()
            report_bytes = (out_dir / 'case.report.json').read_bytes()
            for raw in (QQ_A, QQ_B, GROUP, PHONE, 'fake.user', '测试群', '小明', '明明', FIVE_DIGITS):
                self.assertNotIn(raw.encode('utf-8'), body, f'raw value {raw!r} in the stripped body')
                self.assertNotIn(raw.encode('utf-8'), report_bytes, f'raw value {raw!r} in the report')
            report = json.loads(report_bytes.decode('utf-8'))
            self.assertEqual(report['self_scan_findings'], 0)
            self.assertEqual(report['records_in'], 2)
            self.assertEqual(report['records_out'], 2)
            code, output = cli(['strip', '--key-file', str(key_file), '--input', str(source),
                                '--out-dir', str(out_dir), '--name', 'case'])
            self.assertNotEqual(code, 0, 'a second strip under the same name must refuse to overwrite')
            self.assertEqual((out_dir / 'case.jsonl').read_bytes(), body)


    def test_failed_strip_leaves_no_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            code, output = cli(['init-key', '--key-file', str(key_file)])
            self.assertEqual(code, 0, output)
            # A user path is not a text rule, so the strip leaves it and the self-scan must refuse.
            leaky = [{'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in', 'sender': None,
                      'mid': '9', 't': 1000, 'segs': [{'k': 'text', 'text': 'see ' + '/Users' + '/fakeuser/x.txt'}]}]
            source = Path(tmp) / 'leaky.jsonl'
            source.write_text(json.dumps(leaky[0], ensure_ascii=False) + '\n', encoding='utf-8')
            out_dir = Path(tmp) / 'out'
            code, output = cli(['strip', '--key-file', str(key_file), '--input', str(source),
                                '--out-dir', str(out_dir), '--name', 'leaky'])
            self.assertNotEqual(code, 0, 'a record the self-scan flags must stop the strip: ' + output)
            self.assertNotIn('fakeuser', output, 'refusals print kinds and positions, never the text')
            self.assertTrue(not out_dir.exists() or not any(out_dir.iterdir()), 'a refused strip writes nothing')

            clean_source = Path(tmp) / 'clean.jsonl'
            clean_source.write_text(json.dumps(synthetic_records()[1], ensure_ascii=False) + '\n', encoding='utf-8')
            code, output = cli(['strip', '--key-file', str(key_file), '--input', str(clean_source),
                                '--out-dir', str(out_dir), '--name', 'clean', '--gitleaks',
                                str(Path(tmp) / 'no-such-gitleaks')])
            self.assertNotEqual(code, 0, 'a missing gitleaks binary must fail the strip closed: ' + output)
            self.assertEqual(sorted(x.name for x in out_dir.iterdir()), [], 'a failed secret scan leaves no temp dir or output')


if __name__ == '__main__':
    unittest.main()

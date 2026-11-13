#!/usr/bin/env python3
"""Synthetic-only tests for scripts/strip_history.py (the history strip tool, #58).

Every value is invented and assembled from parts, so this file passes the public guard.
Run: HER_GITLEAKS=<gitleaks> python -I scripts/tests/test_strip_history.py
"""
import argparse
from collections import Counter
import importlib.util
import inspect
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

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
        self.assertEqual(found.get('at-sign'), 1, found)
        self.assertEqual(found.get('name'), 1, found)
        clean = {'segs': [{'k': 'text', 'text': '号码' + '1234' + '，' + '<邮箱>' + '，〔P-abcdefgh〕'}]}
        self.assertEqual(dict(s_mod.self_scan(clean, name_re)), {}, 'four digits and placeholders must pass')
        path = {'segs': [{'k': 'text', 'text': 'see ' + '/Users' + '/fakeuser/x.txt'}]}
        self.assertEqual(s_mod.self_scan(path, None).get('user-path'), 1)


class ReadRecords(unittest.TestCase):
    def test_unparseable_lines_are_skipped_and_counted(self):
        s_mod = load()
        good = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in', 'segs': []}
        with tempfile.TemporaryDirectory() as tmp:
            torn = Path(tmp) / 'torn.jsonl'
            torn.write_text(json.dumps(good) + '\n' + json.dumps(good) + '\n' + '{"v":1,"src":"ing', encoding='utf-8')
            counts = Counter()
            records = s_mod.read_records([torn], counts)
            self.assertEqual(len(records), 2, 'the two complete lines must be read')
            self.assertEqual(counts['torn_tail'], 1, 'a torn last line without a newline is counted, not fatal')
            self.assertEqual(counts['bad_lines'], 1, 'the torn last line is also an unparseable line')

            middle = Path(tmp) / 'middle.jsonl'
            middle.write_text(json.dumps(good) + '\n' + '{not json' + '\n' + json.dumps(good) + '\n', encoding='utf-8')
            counts = Counter()
            records = s_mod.read_records([middle], counts)
            self.assertEqual(len(records), 2, 'an unparseable middle line is skipped and the rest is read')
            self.assertEqual(counts['bad_lines'], 1)
            self.assertEqual(counts['torn_tail'], 0, 'only an unterminated last line is a torn tail')

            wrong = Path(tmp) / 'wrong.jsonl'
            wrong.write_text(json.dumps({'v': 2}) + '\n' + json.dumps(good) + '\n', encoding='utf-8')
            with self.assertRaises(SystemExit) as raised:
                s_mod.read_records([wrong], Counter())
            self.assertIn('wrong.jsonl:1', str(raised.exception))

    def test_input_where_every_line_is_bad_is_refused(self):
        s_mod = load()
        with tempfile.TemporaryDirectory() as tmp:
            only_bad = Path(tmp) / 'only-bad.jsonl'
            only_bad.write_text('{not json\n{also not json\n', encoding='utf-8')
            with self.assertRaises(SystemExit) as raised:
                s_mod.read_records([only_bad], Counter())
            self.assertIn('only-bad.jsonl', str(raised.exception))
            self.assertNotIn(str(tmp), str(raised.exception), 'errors print the file name, never the path')


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


    @REQUIRES_SCANNER
    def test_report_counts_unparseable_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            code, output = cli(['init-key', '--key-file', str(key_file)])
            self.assertEqual(code, 0, output)
            first, second = synthetic_records()[:2]
            source = Path(tmp) / 'in.jsonl'
            source.write_text(json.dumps(first, ensure_ascii=False) + '\n{not json\n'
                              + json.dumps(second, ensure_ascii=False) + '\n', encoding='utf-8')
            out_dir = Path(tmp) / 'out'
            code, output = cli(['strip', '--key-file', str(key_file), '--input', str(source),
                                '--out-dir', str(out_dir), '--name', 'bad'])
            self.assertEqual(code, 0, output)
            report = json.loads((out_dir / 'bad.report.json').read_text(encoding='utf-8'))
            self.assertEqual(report['bad_lines'], 1, 'the unparseable middle line is counted in the report')
            self.assertEqual(report['records_in'], 2, 'the two readable records are stripped')
            self.assertEqual(report['torn_tail'], 0)

    def test_failed_strip_leaves_no_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            code, output = cli(['init-key', '--key-file', str(key_file)])
            self.assertEqual(code, 0, output)
            out_dir = Path(tmp) / 'out'

            clean_source = Path(tmp) / 'clean.jsonl'
            clean_source.write_text(json.dumps(synthetic_records()[1], ensure_ascii=False) + '\n', encoding='utf-8')
            code, output = cli(['strip', '--key-file', str(key_file), '--input', str(clean_source),
                                '--out-dir', str(out_dir), '--name', 'clean', '--gitleaks',
                                str(Path(tmp) / 'no-such-gitleaks')])
            self.assertNotEqual(code, 0, 'a missing gitleaks binary must fail the strip closed: ' + output)
            self.assertEqual(sorted(x.name for x in out_dir.iterdir()), [], 'a failed secret scan leaves no temp dir or output')


def strip_one(s_mod, text, records=None):
    key = bytes(range(32))
    stripper = s_mod.Stripper(key, records or [])
    out = stripper.text(text)
    return stripper, out


def require(test, s_mod, name):
    test.assertTrue(hasattr(s_mod, name), f'scripts/strip_history.py has no {name} yet')


def assert_stripped(test, s_mod, stripper, out, raws):
    for raw in raws:
        test.assertNotIn(raw, out, f'raw value {raw!r} survived the strip: {out!r}')
    leftovers = s_mod.self_scan({'segs': [{'k': 'text', 'text': out}]}, stripper.name_re)
    test.assertEqual(dict(leftovers), {}, f'self-scan found leftovers in {out!r}')
    test.assertEqual(stripper.text(out), out, 'stripping the stripped text again must change nothing')


def no_findings(*args, **kwargs):
    return [], None


def strip_args(key_file, source, out_dir):
    return argparse.Namespace(key_file=str(key_file), input=[str(source)], out_dir=str(out_dir),
                              name='case', gitleaks=None)


SID = 'S-1-5-21-' + '1-2-3-1001'          # invented user SID
OTHER_SID = 'S-1-5-21-' + '9-9-9-1002'    # invented second SID


class R1DigitsBeforeNames(unittest.TestCase):
    def test_digit_only_names_are_not_learned(self):
        s_mod = load()
        record = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
                  'sender': {'id': QQ_A, 'card': '1' + '2', 'nick': '66' + '6'}, 'segs': []}
        stripper, out = strip_one(s_mod, '20' + '12年和' + '66' + '6', [record])
        self.assertEqual(out, '20' + '12年和' + '66' + '6', 'digit-only names must not replace digits')
        self.assertNotIn('〔', out)
        self.assertEqual(stripper.counts['unsafe_names_skipped'], 2)


class R2SeparatedDigits(unittest.TestCase):
    CASES = (
        '314 ' + '159 ' + '265',
        '3141-' + '5926-' + '5',
        '\u200b'.join('3141' + '59265'),
        '138.' + '0013.' + '8000',
        '138－' + '0013－' + '8000',
        ''.join(d + '\ufe0f\u20e3' for d in '3141' + '59265'),
        '③①④①⑤⑨②⑥⑤',
        '三一四一五九二六五',
    )

    def test_separated_digit_runs_are_replaced(self):
        s_mod = load()
        for case in self.CASES:
            with self.subTest(case=case):
                stripper, out = strip_one(s_mod, '号码' + case + '好')
                leftovers = s_mod.self_scan({'segs': [{'k': 'text', 'text': out}]}, stripper.name_re)
                self.assertEqual(dict(leftovers), {}, f'self-scan found leftovers in {out!r}')
                placeholders = ('<数字>', '〔P-', '<电话>') if case == '138－0013－8000' else ('<数字>', '〔P-')
                self.assertTrue(any(p in out for p in placeholders), f'digit run not replaced: {out!r}')

    def test_identity_number_with_x_is_replaced_whole(self):
        s_mod = load()
        stripper, out = strip_one(s_mod, '号码' + '110105 ' + '1949' + '1231' + ' 002X' + '好')
        self.assertNotIn('X', out, out)
        self.assertTrue('<数字>' in out or '〔P-' in out, f'digit run not replaced: {out!r}')
        leftovers = s_mod.self_scan({'segs': [{'k': 'text', 'text': out}]}, stripper.name_re)
        self.assertEqual(dict(leftovers), {}, f'self-scan found leftovers in {out!r}')

    def test_valid_date_is_kept(self):
        s_mod = load()
        _, out = strip_one(s_mod, '今天是20' + '26-11-13')
        self.assertEqual(out, '今天是20' + '26-11-13')


class R3Links(unittest.TestCase):
    LINKS = (
        't.co/x',
        'youtu.be/x?si=abc',
        'example.dev/s?token=abc',
        'example.com?token=abc',
        'example.com#frag=abc',
        'host.example:8080',
        '10.0.0.1:8080/x',
        'mqqapi://card/show?uin=1',
        'tg://resolve?domain=x',
        'magnet:?xt=urn:btih:abc',
        'https：//example.com/x',
        'ｈｔｔｐｓ://ｅｘａｍｐｌｅ.com/x',
        'https://example.com/?q="abc"&token=xyz',
    )

    def test_each_link_form_is_replaced(self):
        s_mod = load()
        for link in self.LINKS:
            with self.subTest(link=link):
                stripper, out = strip_one(s_mod, f'看 {link} 好的')
                self.assertEqual(out, '看 <链接> 好的')
                assert_stripped(self, s_mod, stripper, out, (link,))


class R4Emails(unittest.TestCase):
    def test_email_forms_are_replaced(self):
        s_mod = load()
        cases = (
            ('邮箱：张三@163.com 好的', '邮箱：<邮箱> 好的'),
            ('fake＠example.com', '<邮箱>'),
            ('fake#qq.com', '<邮箱>'),
            ('fake.user @ example.com', '<邮箱>'),
        )
        for text, expected in cases:
            with self.subTest(text=text):
                stripper, out = strip_one(s_mod, text)
                self.assertEqual(out, expected)
                assert_stripped(self, s_mod, stripper, out, ('@', '＠', '163.com', 'example.com'))


class R5IndependentScan(unittest.TestCase):
    def test_residual_kinds_are_reported(self):
        s_mod = load()
        require(self, s_mod, 'residuals')
        cases = (
            ('3 1 4 1 5', 'digits'),
            ('a＠b', 'at-sign'),
            ('x://y', 'scheme'),
            ('abc.xyz/1', 'host'),
            ('三一四一五', 'cjk-digits'),
            ('<数字>7', 'adjacent-digits'),
            ('/ho' + 'me/fake', 'user-path'),
        )
        for text, kind in cases:
            with self.subTest(kind=kind):
                found = s_mod.residuals(text, None)
                self.assertIn(kind, found, f'{kind} not reported for this case: {dict(found)}')


class R6Names(unittest.TestCase):
    def test_card_decoration_is_not_a_name_but_its_parts_are(self):
        s_mod = load()
        record = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
                  'sender': {'id': QQ_A, 'card': '【管理】小明-上海', 'nick': ''}, 'segs': []}
        stripper, out = strip_one(s_mod, '小明在上海，管理一下', [record])
        self.assertEqual(out.count('〔P-'), 2, out)
        self.assertIn('管理', out, out)
        assert_stripped(self, s_mod, stripper, out, ('小明', '上海'))

    def test_latin_nick_matches_in_any_case(self):
        s_mod = load()
        record = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
                  'sender': {'id': QQ_A, 'card': '', 'nick': 'Alice'}, 'segs': []}
        stripper, out = strip_one(s_mod, 'ALICE说', [record])
        assert_stripped(self, s_mod, stripper, out, ('ALICE',))

    def test_short_latin_nick_matches_only_whole_words(self):
        s_mod = load()
        record = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
                  'sender': {'id': QQ_A, 'card': '', 'nick': 'al'}, 'segs': []}
        stripper, out = strip_one(s_mod, 'Also al说', [record])
        self.assertTrue(out.startswith('Also '), out)
        self.assertIn('〔P-', out, out)

    def test_at_segment_name_is_replaced(self):
        s_mod = load()
        record = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in', 'sender': None,
                  'segs': [{'k': 'at', 'id': QQ_B, 'name': '阿强'}]}
        stripper, out = strip_one(s_mod, '阿强你好', [record])
        assert_stripped(self, s_mod, stripper, out, ('阿强',))

    def test_mention_is_replaced(self):
        s_mod = load()
        _, out = strip_one(s_mod, '@某某某 你好')
        self.assertEqual(out, '<提及> 你好')


class S7Paths(unittest.TestCase):
    def test_windows_home_path_loses_the_user_name(self):
        s_mod = load()
        stripper, out = strip_one(s_mod, 'see C:' + chr(92) + 'Users' + chr(92) + 'fakeuser' + chr(92) + 'x.txt')
        self.assertIn('<路径>', out)
        assert_stripped(self, s_mod, stripper, out, ('fakeuser',))

    def test_posix_home_path_loses_the_user_name(self):
        s_mod = load()
        stripper, out = strip_one(s_mod, 'see ' + '/ho' + 'me/fakeuser/x')
        self.assertIn('<路径>', out)
        assert_stripped(self, s_mod, stripper, out, ('fakeuser',))


class SurrogateInput(unittest.TestCase):
    def test_lone_surrogate_becomes_replacement_character(self):
        s_mod = load()
        _, out = strip_one(s_mod, 'a\ud800b')
        self.assertEqual(out, 'a\ufffdb')


class KeyAcl(unittest.TestCase):
    def test_private_dacl_accepts_one_protected_full_access_ace(self):
        s_mod = load()
        require(self, s_mod, 'private_dacl')
        self.assertTrue(s_mod.private_dacl(f'O:BAD:PAI(A;;FA;;;{SID})', SID))

    def test_private_dacl_rejects_other_shapes(self):
        s_mod = load()
        require(self, s_mod, 'private_dacl')
        cases = {
            'not protected': f'D:AI(A;;FA;;;{SID})',
            'second ACE': f'O:BAD:PAI(A;;FA;;;{SID})(A;;FR;;;BU)',
            'inheritance flag': f'O:BAD:PAI(A;ID;FA;;;{SID})',
            'read-only rights': f'O:BAD:PAI(A;;FR;;;{SID})',
            'other SID': f'O:BAD:PAI(A;;FA;;;{OTHER_SID})',
        }
        for label, sddl in cases.items():
            with self.subTest(label=label):
                self.assertFalse(s_mod.private_dacl(sddl, SID), f'{label} must not count as private')

    def test_dacl_problem_names_the_failed_condition(self):
        s_mod = load()
        require(self, s_mod, 'dacl_problem')
        self.assertIsNone(s_mod.dacl_problem(f'O:BAD:PAI(A;;FA;;;{SID})', SID))
        self.assertEqual(s_mod.dacl_problem('O:BAD:PAI(A;;FA;;;S-1-5-21-1-2-3-500)', SID),
                         'entry for another account: A;;FA;domain-rid-500')
        self.assertEqual(s_mod.dacl_problem(f'D:AI(A;;FA;;;{SID})', SID), 'not protected')

    def test_dacl_problem_describes_each_ace_without_identifiers(self):
        s_mod = load()
        require(self, s_mod, 'dacl_problem')
        require(self, s_mod, 'describe_ace')
        self.assertEqual(s_mod.dacl_problem(f'O:BAD:PAI(A;;FA;;;{SID})(A;;FR;;;{SID})', SID),
                         'entry count 2: A;;FA;current-user A;;FR;current-user')
        self.assertEqual(s_mod.dacl_problem(f'O:BAD:PAI(A;;FA;;;{SID})(A;;FR;;;S-1-5-21-1-2-3-500)', SID),
                         'entry for another account: A;;FR;domain-rid-500')
        cases = [('A;;FA;;;SY', 'A;;FA;SY'), ('A;ID;FR;;;S-1-5-21-1-2-3-500', 'A;ID;FR;domain-rid-500'),
                 ('A;;FA;;;S-1-5-18', 'A;;FA;S-1-5-18'), ('A;;FA;;;S-1-5-32-544', 'A;;FA;S-1-5-32-544'),
                 ('A;;FA;;;S-1-15-2-1', 'A;;FA;other-account'), (f'A;;FA;;;{SID}', 'A;;FA;current-user')]
        for ace, label in cases:
            with self.subTest(ace=ace):
                self.assertEqual(s_mod.describe_ace(ace, SID), label)

    def test_dacl_problem_allows_only_privileged_builtins_beside_the_user(self):
        s_mod = load()
        require(self, s_mod, 'dacl_problem')
        privileged = 'O:BAD:PAI(A;;FA;;;S-1-3-4)(A;;FA;;;S-1-5-18)(A;;FA;;;S-1-5-32-544)'
        self.assertIsNone(s_mod.dacl_problem(f'{privileged}(A;;FA;;;{SID})', SID))
        self.assertEqual(s_mod.dacl_problem(f'{privileged}(A;;FA;;;{SID})(A;;FR;;;S-1-5-32-545)', SID),
                         'entry for another account: A;;FR;S-1-5-32-545')
        self.assertEqual(s_mod.dacl_problem('O:BAD:PAI(A;;FA;;;S-1-3-4)(A;;FA;;;S-1-5-18)', SID),
                         'current user lacks full access')
        self.assertEqual(s_mod.dacl_problem(f'O:BAD:PAI(A;;FA;;;{SID})(A;ID;FA;;;S-1-5-18)', SID), 'entry is inherited')

    @unittest.skipIf(os.name == 'nt', 'POSIX mode bits')
    def test_load_key_refuses_a_group_readable_file(self):
        s_mod = load()
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            s_mod.init_key(key_file)
            os.chmod(key_file, 0o644)
            with self.assertRaises(SystemExit):
                s_mod.load_key(key_file)

    @unittest.skipUnless(os.name == 'nt', 'Windows ACL check')
    def test_windows_key_file_is_private_after_init(self):
        s_mod = load()
        require(self, s_mod, '_windows_sddl')
        require(self, s_mod, '_windows_sid')
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            s_mod.init_key(key_file)
            self.assertTrue(s_mod.private_dacl(s_mod._windows_sddl(key_file), s_mod._windows_sid()))

    @unittest.skipUnless(os.name == 'nt', 'Windows ACL check')
    def test_windows_inherited_acl_is_refused(self):
        s_mod = load()
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            s_mod.init_key(key_file)
            subprocess.run(['icacls', str(key_file), '/inheritance:e'], check=True, capture_output=True)
            with self.assertRaises(SystemExit):
                s_mod.load_key(key_file)


class Publication(unittest.TestCase):
    def test_rollback_when_the_second_replace_fails(self):
        s_mod = load()
        params = inspect.signature(s_mod.strip).parameters
        self.assertTrue({'scan', 'secret_scan'} <= set(params), 'strip() has no scan and secret_scan hooks yet')
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            s_mod.init_key(key_file)
            source = Path(tmp) / 'clean.jsonl'
            source.write_text(json.dumps(synthetic_records()[1], ensure_ascii=False) + '\n', encoding='utf-8')
            out_dir = Path(tmp) / 'out'
            real_replace, calls = os.replace, []

            def second_fails(src, dst, *args, **kwargs):
                calls.append(dst)
                if len(calls) == 1:
                    return real_replace(src, dst)
                raise OSError(28, 'no space left')

            with mock.patch.object(s_mod.os, 'replace', side_effect=second_fails):
                code = s_mod.strip(strip_args(key_file, source, out_dir), secret_scan=no_findings)
            self.assertEqual(len(calls), 2, 'the test must reach the second publish')
            self.assertEqual(code, 1, 'a failed second publish must fail the strip')
            leftovers = sorted(p.name for p in out_dir.iterdir()) if out_dir.exists() else []
            self.assertEqual(leftovers, [], 'a failed publish must remove what it published')

    def test_scan_finding_writes_nothing(self):
        s_mod = load()
        params = inspect.signature(s_mod.strip).parameters
        self.assertTrue({'scan', 'secret_scan'} <= set(params), 'strip() has no scan and secret_scan hooks yet')
        with tempfile.TemporaryDirectory() as tmp:
            key_file = Path(tmp) / 'history.key'
            s_mod.init_key(key_file)
            source = Path(tmp) / 'clean.jsonl'
            source.write_text(json.dumps(synthetic_records()[1], ensure_ascii=False) + '\n', encoding='utf-8')
            out_dir = Path(tmp) / 'out'
            code = s_mod.strip(strip_args(key_file, source, out_dir),
                               scan=lambda record, name_re: Counter({'digits': 1}), secret_scan=no_findings)
            self.assertEqual(code, 1, 'a scan finding must fail the strip')
            self.assertTrue(not out_dir.exists() or not any(out_dir.iterdir()), 'a scan finding must write nothing')


DIGIT_LIKE = re.compile(r'[0-9零〇一二三四五六七八九两幺壹贰叁肆伍陆柒捌玖拾]')


class R7Separators(unittest.TestCase):
    CASES = (
        '138,0013,8000', '138/0013/8000', '138*0013*8000', '138，0013，8000',
        '3141    5926    5', '314,159,265', '1三8零0一3八0零0', '壹叁捌零零壹叁捌零零零',
    )

    def test_every_separated_or_mixed_digit_run_is_replaced(self):
        s_mod = load()
        for case in self.CASES:
            with self.subTest(case=case):
                stripper, out = strip_one(s_mod, '号码' + case + '好')
                self.assertEqual(out, '号码<数字>好', f'digit run survived: {out!r}')
                assert_stripped(self, s_mod, stripper, out, ())

    def test_dates_and_four_digit_years_stay(self):
        s_mod = load()
        _, out = strip_one(s_mod, '今天是20' + '26-11-13')
        self.assertEqual(out, '今天是20' + '26-11-13')
        _, out = strip_one(s_mod, FOUR_DIGITS + '年')
        self.assertEqual(out, FOUR_DIGITS + '年')

    def test_residual_scan_counts_commas_spaces_and_mixed_numerals(self):
        s_mod = load()
        require(self, s_mod, 'residuals')
        for text in ('314,159,265', '3    1    4    1    5', '1三8零0一3八0零0'):
            with self.subTest(text=text):
                self.assertIn('digits', s_mod.residuals(text, None), f'residual scan missed {text!r}')


class R8Urls(unittest.TestCase):
    CASES = (
        'https://example.com/s?name=张三&token=AbCdEf123',
        'example.com/u/张三/key/AbCdEf',
        'example.com？token=AbCdEf',
        'example.com/中文?token=abc',
    )

    def test_url_tails_run_through_cjk_and_query_pairs(self):
        s_mod = load()
        for case in self.CASES:
            with self.subTest(case=case):
                stripper, out = strip_one(s_mod, '看 ' + case + ' 好的')
                self.assertEqual(out, '看 <链接> 好的', f'tail survived: {out!r}')
                assert_stripped(self, s_mod, stripper, out, ('token', 'name', 'AbCdEf', '张三', '中文'))

    def test_residual_scan_flags_a_link_followed_by_a_query(self):
        s_mod = load()
        require(self, s_mod, 'residuals')
        self.assertIn('link-tail', s_mod.residuals('<链接>？token=x', None), 'a link followed by a query pair must be flagged')
        self.assertIn('link-tail', s_mod.residuals('<链接>&a=b', None), 'a link followed by & must be flagged')
        self.assertIn('query-fragment', s_mod.residuals('a?name=x', None), 'a query fragment must be flagged')

    def test_ordinary_text_after_a_link_is_not_a_finding(self):
        s_mod = load()
        require(self, s_mod, 'residuals')
        stripper, out = strip_one(s_mod, '看 https://example.com/s？好的')
        self.assertEqual(out, '看 <链接>？好的', f'stripped text: {out!r}')
        self.assertEqual(dict(s_mod.residuals(out, None)), {}, 'a link followed by ordinary ？ text must pass the residual scan')


class R9BareHosts(unittest.TestCase):
    def test_letters_only_tlds_of_any_length_are_links(self):
        s_mod = load()
        for host in ('example.museum', 'pan.example.cloud', 'zhangsan.moe'):
            with self.subTest(host=host):
                stripper, out = strip_one(s_mod, '看 ' + host + ' 好的')
                self.assertEqual(out, '看 <链接> 好的')
                assert_stripped(self, s_mod, stripper, out, (host,))

    def test_ordinary_dotted_text_is_unchanged(self):
        s_mod = load()
        for text in ('你好.我们', 'e.g. this'):
            with self.subTest(text=text):
                _, out = strip_one(s_mod, text)
                self.assertEqual(out, text)


class R10UnicodeNames(unittest.TestCase):
    def test_casefolded_spellings_of_a_nick_are_removed(self):
        s_mod = load()
        record = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
                  'sender': {'id': QQ_A, 'card': '', 'nick': 'Straße'}, 'segs': []}
        stripper, out = strip_one(s_mod, 'Straße你好 STRASSE', [record])
        assert_stripped(self, s_mod, stripper, out, ('Straße', 'STRASSE'))

    def test_decomposed_text_matches_a_precomposed_nick(self):
        s_mod = load()
        record = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
                  'sender': {'id': QQ_A, 'card': '', 'nick': 'José'}, 'segs': []}
        stripper, out = strip_one(s_mod, 'José 你好', [record])
        assert_stripped(self, s_mod, stripper, out, ('José', 'Jose'))


class R11DecoratedNames(unittest.TestCase):
    CASES = (
        ('card', '小明12345', '小明 你好'),
        ('card', '张三2024', '张三 你好'),
        ('card', '✨阿强✨', '阿强 你好'),
        ('card', '꧁阿伟꧂', '阿伟 你好'),
        ('card', '李四ゞ', '李四 你好'),
        ('nick', '小明🌸', '小明 你好'),
    )

    def test_decorated_names_teach_their_bare_name(self):
        s_mod = load()
        for field, value, text in self.CASES:
            with self.subTest(value=value):
                sender = {'id': QQ_A, 'card': '', 'nick': ''}
                sender[field] = value
                record = {'v': 1, 'src': 'ingress', 'conv': f'group:{GROUP}', 'dir': 'in',
                          'sender': sender, 'segs': []}
                stripper, out = strip_one(s_mod, text, [record])
                bare = text.split(' ')[0]
                self.assertNotIn(bare, out, f'bare name survived: {out!r}')
                self.assertIn('〔P-', out, out)


if __name__ == '__main__':
    unittest.main()

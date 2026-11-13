#!/usr/bin/env python3
"""Strip normalised chat history for the memory eval (#58). Real data is stripped on the Windows
host only; tests use synthetic records.

    python -I scripts/strip_history.py init-key --key-file KEY
    python -I scripts/strip_history.py strip --key-file KEY --input FILE [--input FILE ...]
                                       --out-dir DIR --name NAME [--gitleaks EXE]

Input: schema v1 records, one JSON object per line, from scripts/snowluma_export.py or the
runtime's data/history/*.jsonl (her/src/history_log.rs): v, src, conv, dir, sender {id, card,
nick}, convName, t (epoch ms), mid, segs [text | at | reply | forward | face | media], and for
sends batch, idx, phase. Output DIR/NAME.jsonl keeps the shape: account, group, message and
batch ids become keyed pseudonyms (HMAC-SHA256, letters only), names are dropped, time is an
ISO UTC second, and text has links, emails, phone numbers, addresses, known names and ids and
every run of five or more digits replaced. Media keeps only its kind. DIR/NAME.report.json holds
counts only. Nothing is written unless the self-scan and the gitleaks secret scan are clean.
The key file stays on the Windows host and is never committed.
"""
import argparse
import datetime
import hashlib
import hmac
import importlib.util
import json
import os
from collections import Counter
from pathlib import Path
import re
import secrets
import sys
import tempfile
import unicodedata

ROOT = Path(__file__).resolve().parent.parent


def _load_scan_public():
    spec = importlib.util.spec_from_file_location('scan_public', ROOT / 'scripts' / 'scan-public.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


SCAN = _load_scan_public()

STOP = '\\s<>"\'，。！？、；：）」』】《》'
URL_SCHEME = re.compile(r'(?i)(?:https?|ftp)://[^' + STOP + r']+')
URL_WWW = re.compile(r'(?i)(?<![A-Za-z0-9.-])www\.[^' + STOP + r']+')
EMAIL = re.compile(r'[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+')
URL_BARE = re.compile(r'(?i)(?<![A-Za-z0-9.@-])[a-z0-9-]+(?:\.[a-z0-9-]+)*\.(?:com|cn|net|org|tv|io|me|cc|top|xyz|link|ly|gg|app)'
                      r'(?![A-Za-z0-9-])(?:/[^' + STOP + r']*)?')
PHONE = re.compile(r'(?<!\d)(?:\+?86[- ]?)?1[3-9]\d(?:[- ]?\d{4}){2}(?!\d)|(?<!\d)0\d{2,3}-\d{7,8}(?!\d)')
CJK = '\u4e00-\u9fff'
NUM = r'[\d一二三四五六七八九十百零〇]'
UNIT = r'[\dA-Za-z一二三四五六七八九十]+'
ADDRESS = re.compile(
    rf'(?:[{CJK}]{{2,7}}(?:省|自治区|特别行政区))?(?:[{CJK}]{{2,7}}(?:市|州|盟))?(?:[{CJK}]{{1,7}}(?:区|县|旗))?'
    rf'[{CJK}\d]{{1,12}}(?:大道|路|街|巷|弄|胡同)\s*{NUM}+(?:号|弄)(?:\s*{UNIT}(?:号楼|栋|幢|座|单元|层|楼|室))*')
ESTATE = re.compile(
    rf'[{CJK}]{{2,10}}(?:小区|花园|公寓|大厦|家园|苑)\s*{UNIT}(?:号楼|栋|幢|座|单元|室)(?:\s*{UNIT}(?:号楼|栋|幢|座|单元|层|室|号))*')
DIGITS = re.compile(r'\d{5,}')          # Unicode digits: full-width runs count
PLACEHOLDER = re.compile(r'〔[PGMB]-[a-z]{8,10}〕|<(?:链接|邮箱|电话|地址|数字)>')
TEXT_RULES = (('link', URL_SCHEME, '<链接>'), ('link', URL_WWW, '<链接>'), ('email', EMAIL, '<邮箱>'),
              ('link', URL_BARE, '<链接>'), ('phone', PHONE, '<电话>'), ('address', ADDRESS, '<地址>'),
              ('address', ESTATE, '<地址>'))
SCAN_RULES = (('digits', DIGITS),) + tuple((kind, pattern) for kind, pattern, _ in TEXT_RULES)
CODES = {'acct': ('P', 8), 'group': ('G', 8), 'mid': ('M', 10), 'batch': ('B', 10)}
MEDIA_KINDS = {'image', 'video', 'voice', 'file', 'sticker', 'card', 'markdown', 'other'}
SOURCES = ('snowluma', 'ingress', 'send')


def code(key, space, value):
    prefix, length = CODES[space]
    digest = hmac.new(key, f'{space}\0{value}'.encode('utf-8'), hashlib.sha256).digest()
    return prefix + '-' + ''.join(chr(97 + byte % 26) for byte in digest[:length])


class Stripper:
    def __init__(self, key, records):
        self.key, self.counts, self.ids, self.names = key, Counter(), {}, {}
        for record in records:
            self._learn(record)
        usable = sorted((name for name in self.names if len(name) >= 2), key=len, reverse=True)
        self.counts['short_names_unreplaced'] = len(self.names) - len(usable)
        self.name_re = re.compile('|'.join(map(re.escape, usable))) if usable else None

    def field(self, space, raw):
        return code(self.key, space, str(raw))

    def _learn(self, record):
        kind, _, conv_id = str(record.get('conv') or '').partition(':')
        if conv_id:
            self.ids.setdefault(conv_id, self.field('group' if kind == 'group' else 'acct', conv_id))
            if kind == 'group' and record.get('convName'):
                self.names.setdefault(str(record['convName']).strip(), self.ids[conv_id])
        sender = record.get('sender') or {}
        if sender.get('id'):
            sid = str(sender['id'])
            self.ids.setdefault(sid, self.field('acct', sid))
            for name in (sender.get('card'), sender.get('nick')):
                if name and str(name).strip():
                    self.names.setdefault(str(name).strip(), self.field('acct', sid))
        for seg in record.get('segs') or []:
            if isinstance(seg, dict) and seg.get('k') == 'at' and seg.get('id') not in (None, '', 'all'):
                self.ids.setdefault(str(seg['id']), self.field('acct', seg['id']))

    def text(self, value):
        for kind, pattern, placeholder in TEXT_RULES:
            value, n = pattern.subn(placeholder, value)
            self.counts[kind] += n
        if self.name_re:
            value, n = self.name_re.subn(lambda m: f'〔{self.names[m.group()]}〕', value)
            self.counts['name'] += n

        def digits(match):
            known = self.ids.get(unicodedata.normalize('NFKC', match.group()))
            self.counts['known_id' if known else 'digit_run'] += 1
            return f'〔{known}〕' if known else '<数字>'
        return DIGITS.sub(digits, value)

    def segment(self, seg):
        k = seg.get('k')
        if k == 'text':
            return {'k': 'text', 'text': self.text(str(seg.get('text', '')))}
        if k == 'at':
            who = seg.get('id')
            return {'k': 'at', 'id': 'all' if who == 'all' else (self.field('acct', who) if who else None)}
        if k == 'reply':
            return {'k': 'reply', 'mid': self.field('mid', seg['mid']) if seg.get('mid') else None}
        if k == 'forward':
            return {'k': 'forward'}
        if k == 'face':
            fid = str(seg.get('id', ''))
            return {'k': 'face', 'id': fid if re.fullmatch(r'[0-9]{1,4}', fid) else None}
        kind = seg.get('kind') if k == 'media' and seg.get('kind') in MEDIA_KINDS else 'other'
        self.counts['media_' + kind] += 1
        return {'k': 'media', 'kind': kind}

    def record(self, r):
        kind, _, conv_id = str(r.get('conv') or '').partition(':')
        sender, t = r.get('sender') or {}, r.get('t')
        out = {
            'v': 1, 'src': r.get('src') if r.get('src') in SOURCES else None,
            'conv': f"{kind}:{self.field('group' if kind == 'group' else 'acct', conv_id)}"
                    if kind in ('group', 'private') and conv_id else None,
            'dir': r.get('dir') if r.get('dir') in ('in', 'out') else None,
            'sender': self.field('acct', sender['id']) if sender.get('id') else None,
            'time': datetime.datetime.fromtimestamp(t // 1000, datetime.timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')
                    if isinstance(t, int) and not isinstance(t, bool) and t > 0 else None,
            'mid': self.field('mid', r['mid']) if r.get('mid') not in (None, '') else None,
            'segs': [self.segment(s) for s in r.get('segs') or [] if isinstance(s, dict)],
        }
        if r.get('src') == 'send':
            out['batch'] = self.field('batch', r['batch']) if r.get('batch') else None
            out['idx'] = r['idx'] if isinstance(r.get('idx'), int) and 0 <= r['idx'] < 1000 else None
            out['phase'] = r.get('phase') if r.get('phase') in ('intent', 'confirmed') else None
        return out


def load_key(path):
    try:
        raw = Path(path).read_text(encoding='ascii').strip()
    except UnicodeDecodeError:
        raise SystemExit('key file must hold 64 hex characters')
    except OSError:
        raise SystemExit('key file is unreadable')
    try:
        key = bytes.fromhex(raw)
    except ValueError:
        key = b''
    if len(key) != 32:
        raise SystemExit('key file must hold 64 hex characters')
    return key


def init_key(path):
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        raise SystemExit('key file exists; refusing to replace it')
    except OSError:
        raise SystemExit('key file cannot be created')
    with os.fdopen(fd, 'w', encoding='ascii') as handle:
        handle.write(secrets.token_hex(32) + '\n')


def read_records(paths, counts):
    """Schema v1 records from the input files; a torn last line is counted, any other bad line fails."""
    records = []
    for path in paths:
        name = Path(path).name
        try:
            text = Path(path).read_bytes().decode('utf-8-sig')
        except UnicodeDecodeError:
            raise SystemExit(f'input {name}: not UTF-8 text')
        lines = text.split('\n')
        for number, line in enumerate(lines, 1):
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                if number == len(lines) and not text.endswith('\n'):
                    counts['torn_tail'] += 1
                    continue
                raise SystemExit(f'input {name}:{number}: not a JSON record')
            if not isinstance(record, dict) or record.get('v') != 1:
                raise SystemExit(f'input {name}:{number}: not a schema v1 record')
            records.append(record)
    return records


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from strings(item)
    elif isinstance(value, dict):
        for key, item in value.items():
            yield key
            yield from strings(item)


def self_scan(record, name_re):
    found = Counter()
    for text in strings(record):
        bare = PLACEHOLDER.sub(' ', text)
        for kind, pattern in SCAN_RULES:
            if pattern.search(bare):
                found[kind] += 1
        if any(pattern.search(bare) for pattern in SCAN.USER_PATH_PATTERNS):
            found['user-path'] += 1
    if name_re:
        for seg in record.get('segs') or []:
            if seg.get('k') == 'text' and name_re.search(PLACEHOLDER.sub(' ', seg['text'])):
                found['name'] += 1
    return found


NAME_RE = re.compile(r'[A-Za-z0-9._-]+')


def strip(args):
    key = load_key(args.key_file)
    if not NAME_RE.fullmatch(args.name):
        raise SystemExit('name must match [A-Za-z0-9._-]+')
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    body_path, report_path = out_dir / f'{args.name}.jsonl', out_dir / f'{args.name}.report.json'
    if body_path.exists() or report_path.exists():
        raise SystemExit('output exists; refusing to overwrite')
    if args.gitleaks:
        os.environ['HER_GITLEAKS'] = args.gitleaks
    counts = Counter()
    records = read_records(args.input, counts)
    stripper = Stripper(key, records)
    out = [stripper.record(r) for r in records]

    leftovers, bad = Counter(), []
    for index, record in enumerate(out):
        found = self_scan(record, stripper.name_re)
        if found:
            leftovers.update(found)
            bad.append(index)
    if bad:
        shown = ' '.join(str(i) for i in bad[:20])
        detail = ', '.join(f'{kind}={n}' for kind, n in sorted(leftovers.items()))
        print(f'self-scan: {detail}; records {shown}', file=sys.stderr)
        return 1

    with tempfile.TemporaryDirectory(dir=out_dir) as tmp:
        staged_body = Path(tmp) / f'{args.name}.jsonl'
        with open(staged_body, 'w', encoding='utf-8') as handle:
            for record in out:
                handle.write(json.dumps(record, ensure_ascii=False, separators=(',', ':')) + '\n')
        findings, error = SCAN.secret_findings(Path(tmp), ROOT / '.gitleaks.toml', scan_skipped=True)
        if error or findings:
            print(f'secret-scan: {error or str(len(findings)) + " findings"}', file=sys.stderr)
            return 1
        report = report_for(records, out, stripper, counts)
        staged_report = Path(tmp) / f'{args.name}.report.json'
        staged_report.write_text(json.dumps(report, indent=2, sort_keys=True) + '\n', encoding='utf-8')
        os.replace(staged_body, body_path)
        os.replace(staged_report, report_path)
    print(f'strip: records_in={report["records_in"]} records_out={report["records_out"]} '
          f'torn_tail={report["torn_tail"]} self_scan_findings=0 secret_scan=clean')
    return 0


def report_for(records, out, stripper, counts):
    """Counts only: no pseudonym, text, name or id appears in the report."""
    days = sorted(r['time'][:10] for r in out if r['time'])
    return {
        'schema': 'her.history-strip.v1',
        'records_in': len(records),
        'records_out': len(out),
        'torn_tail': counts['torn_tail'],
        'conversations': {
            'group': len({r['conv'] for r in out if r['conv'] and r['conv'].startswith('group:')}),
            'private': len({r['conv'] for r in out if r['conv'] and r['conv'].startswith('private:')}),
        },
        'persons': len({r['sender'] for r in out if r['sender']}),
        'by_src': dict(Counter(r['src'] or 'none' for r in out)),
        'by_dir': dict(Counter(r['dir'] or 'none' for r in out)),
        'replaced': {k: stripper.counts[k] for k in
                     ('link', 'email', 'phone', 'address', 'name', 'known_id', 'digit_run')},
        'media': {k[len('media_'):]: n for k, n in stripper.counts.items() if k.startswith('media_')},
        'short_names_unreplaced': stripper.counts['short_names_unreplaced'],
        'first_day': days[0] if days else None,
        'last_day': days[-1] if days else None,
        'self_scan_findings': 0,
        'secret_scan': 'clean',
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description='Strip normalised chat history (#58).')
    commands = parser.add_subparsers(dest='command', required=True)
    init = commands.add_parser('init-key', help='create a new 32-byte key file (refuses to replace one)')
    init.add_argument('--key-file', required=True)
    run = commands.add_parser('strip', help='strip schema v1 records into DIR/NAME.jsonl')
    run.add_argument('--key-file', required=True)
    run.add_argument('--input', action='append', required=True)
    run.add_argument('--out-dir', required=True)
    run.add_argument('--name', required=True)
    run.add_argument('--gitleaks')
    args = parser.parse_args(argv)
    if args.command == 'init-key':
        init_key(args.key_file)
        return 0
    return strip(args)


if __name__ == '__main__':
    sys.exit(main())

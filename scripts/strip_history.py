#!/usr/bin/env python3
"""Strip normalised chat history for the memory eval (#58). Real data is stripped on the Windows
host only; tests use synthetic records.

    python -I scripts/strip_history.py init-key --key-file KEY
    python -I scripts/strip_history.py strip --key-file KEY --input FILE [--input FILE ...]
                                       --out-dir DIR --name NAME [--gitleaks EXE]

Input: schema v1 records, one JSON object per line, from scripts/snowluma_export.py or the
runtime's data/history/*.jsonl (her/src/history_log.rs): v, src, conv, dir, sender {id, card,
nick}, convName, t (epoch ms), mid, segs [text | at | reply | forward | face | media], and for
sends batch, idx, phase. Output DIR/NAME.jsonl keeps the shape: account, group, message and batch ids become keyed
pseudonyms (HMAC-SHA256, letters only), names are dropped, time is an ISO UTC second. Text is
first folded (format characters, variation selectors and keycap marks dropped; any digit and
full-width letters and @ mapped to ASCII; Chinese punctuation kept), then account-home paths,
links (any scheme, host with path, query or port, IPv4, known TLDs), emails, addresses, phone
numbers, digit sequences with five or more digits ignoring separators (valid dates kept),
Chinese-numeral runs, known names and name parts, and @mentions are replaced, repeated until
stable. A separate stricter residual scan and a field whitelist must pass, then gitleaks;
otherwise nothing is written, and a failed publish removes what this run published.
DIR/NAME.report.json holds counts only. The key file is restricted to the current user (POSIX
mode 600, Windows protected DACL with full access for the user and the PRIVILEGED_SIDS only, verified before the
key is written and on every load), stays on the Windows host and is never committed.
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
import subprocess
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

FOLDABLE = frozenset('abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ' '01234' '56789' '@.-_#%=&+~')


def _dropped(ch):
    o = ord(ch)
    return unicodedata.category(ch) == 'Cf' or 0xFE00 <= o <= 0xFE0F or 0xE0100 <= o <= 0xE01EF or o == 0x20E3


def fold(text):
    """Compose to NFC; drop format characters, variation selectors and keycap marks; map every digit and
    full-width letters and @.-_#%=&+~ to ASCII; keep Chinese text and punctuation; lone
    surrogates become U+FFFD."""
    text = unicodedata.normalize('NFC', text)
    out = []
    for ch in text:
        if 0xD800 <= ord(ch) <= 0xDFFF:
            out.append('\ufffd')
            continue
        if _dropped(ch):
            continue
        if not ch.isascii():
            digit = unicodedata.digit(ch, None)
            if digit is not None:
                out.append(str(digit))
                continue
            folded = unicodedata.normalize('NFKC', ch)
            if folded and all(c in FOLDABLE for c in folded):
                out.append(folded)
                continue
        out.append(ch)
    return ''.join(out)


TAIL = r'(?:[^\s。，！？；（）、「」『』【】《》]|？(?=[A-Za-z_]+=))*'
QSEP = r'(?:[/／?#]|？(?=[A-Za-z_]+=))'
HOME_WIN = re.compile(r'(?i)(?<![A-Za-z])[a-z]:[\\/]+users[\\/]+[^\s\\/]+')
HOME_POSIX = re.compile(r'(?i)(?:/home|/users|/[a-z]/users)/[^\s/]+')
SCHEME = re.compile(r'(?i)(?<![A-Za-z0-9+.-])[a-z][a-z0-9+.-]{0,30}[:：]\s?(?:[/／]\s?[/／]|\?)' + TAIL)
EMAIL_LOCAL = r'(?:[A-Za-z0-9._%+-]{1,64}|[\w.+-]{1,12})'
EMAIL = re.compile(EMAIL_LOCAL + r'\s?@\s?[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+')
EMAIL_HASH = re.compile(EMAIL_LOCAL + r'\s?#\s?(?:qq|163|126|139|yeah|gmail|outlook|hotmail|foxmail|sina|sohu|'
                        r'yahoo|icloud|live|aliyun)\.(?:com|cn|net)(?:\.cn)?', re.IGNORECASE)
WWW = re.compile(r'(?i)(?<![A-Za-z0-9.-])www\.' + TAIL)
IPV4 = re.compile(r'(?<![\d.])(?:\d{1,3}\.){3}\d{1,3}(?!\d)(?::\d{1,5})?(?:' + QSEP + TAIL + ')?')
HOST_PATH = re.compile(r'(?i)(?<![A-Za-z0-9@.-])(?:[a-z0-9-]+\.)+[a-z][a-z0-9-]{1,23}'
                       r'(?:(?::\d{1,5})?' + QSEP + r'|:\d{1,5}(?!\d))' + TAIL)
HOST_DOT = re.compile(r'(?i)(?<![A-Za-z0-9-])[a-z0-9-]+\s*[。点]\s*(?:com|cn|net|org)(?![a-z])')
HOST_BARE = re.compile(r'(?i)(?<![A-Za-z0-9@.-])(?:[a-z0-9-]+\.)*(?=[a-z0-9-]*[a-z])[a-z0-9-]{2,}\.[a-z]{2,24}'
                       r'(?![A-Za-z0-9-])')
PHONE = re.compile(r'(?<!\d)(?:\+?86[- ]?)?1[3-9]\d(?:[- ]?\d{4}){2}(?!\d)|(?<!\d)0\d{2,3}-\d{7,8}(?!\d)')
CJK = '\u4e00-\u9fff'
NUM = r'[\d一二三四五六七八九十百零〇]'
UNIT = r'[\dA-Za-z一二三四五六七八九十]+'
ADDRESS = re.compile(
    rf'(?:[{CJK}]{{2,7}}(?:省|自治区|特别行政区))?(?:[{CJK}]{{2,7}}(?:市|州|盟))?(?:[{CJK}]{{1,7}}(?:区|县|旗))?'
    rf'[{CJK}\d]{{1,12}}(?:大道|路|街|巷|弄|胡同)\s*{NUM}+(?:号|弄)(?:\s*{UNIT}(?:号楼|栋|幢|座|单元|层|楼|室))*')
ESTATE = re.compile(
    rf'[{CJK}]{{2,10}}(?:小区|花园|公寓|大厦|家园|苑)\s*{UNIT}(?:号楼|栋|幢|座|单元|室)(?:\s*{UNIT}(?:号楼|栋|幢|座|单元|层|室|号))*')
NUMERAL_CHARS = '零〇一二三四五六七八九两幺壹贰叁肆伍陆柒捌玖拾'
NUMERAL = r'[\d零〇一二三四五六七八九两幺壹贰叁肆伍陆柒捌玖拾]'
SEP_CHAR = r'(?!\w)[\s\S]'
SEPS = r'(?:\s{1,6}|(?:' + SEP_CHAR + r'){1,3})?'
DATE = re.compile(r'(?<!\d)(?:19|20)\d{2}[-./年](?:0?[1-9]|1[0-2])[-./月](?:0?[1-9]|[12]\d|3[01])日?(?!\d)')
DATE_SPLIT = re.compile('(' + DATE.pattern + ')')
DIGIT_SEQ = re.compile(NUMERAL + '(?:' + SEPS + NUMERAL + ')*[Xx]?')
CJK_NUM = re.compile(r'[零〇一二三四五六七八九两幺](?:[\s\-.,，、·]?[零〇一二三四五六七八九两幺]){4,}')
MENTION = re.compile(r'@(?!〔)[^\s@〔<]{0,16}')
MENTION_CODE = re.compile(r'@(?=〔)')
PLACEHOLDER = re.compile(r'〔[PGMB]-[a-z]{8,10}〕|<(?:链接|邮箱|电话|地址|数字|路径|提及)>')
NAME_SPLIT = re.compile(r'[^A-Za-z\u00c0-\u00d6\u00d8-\u00f6\u00f8-\u024f\u4e00-\u9fff\u3400-\u4dbf]+')
NOT_A_NAME = re.compile(r'[\d\W_]+')
DECOR_STOP = frozenset({'管理', '管理员', '群主', '群员', '成员', '萌新', '新人', '本人', '官方', 'admin', 'mod', 'bot', 'owner'})
TEXT_RULES = (('path', HOME_WIN, '<路径>'), ('path', HOME_POSIX, '<路径>'), ('link', SCHEME, '<链接>'),
              ('email', EMAIL, '<邮箱>'), ('email', EMAIL_HASH, '<邮箱>'), ('link', WWW, '<链接>'),
              ('link', IPV4, '<链接>'), ('link', HOST_PATH, '<链接>'), ('link', HOST_DOT, '<链接>'),
              ('link', HOST_BARE, '<链接>'), ('address', ADDRESS, '<地址>'), ('address', ESTATE, '<地址>'),
              ('phone', PHONE, '<电话>'))
# Residual scan: stricter, written apart from the replacement rules where it can be.
SCAN_DIGITS = re.compile(NUMERAL + '(?:' + SEPS + NUMERAL + ')*')
ADJACENT = re.compile(r'\d(?:<数字>|<电话>)|(?:<数字>|<电话>)\d')
SCAN_RULES = (
    ('cjk-digits', re.compile(r'(?:[零〇一二三四五六七八九两幺][\s\-.,，、·]?){5,}')),
    ('at-sign', re.compile(r'[@＠]')),
    ('scheme', re.compile(r'[A-Za-z][:：]\s?(?:[/／]\s?[/／]|\?)')),
    ('host', re.compile(r'(?i)[a-z0-9-]\.[a-z][a-z0-9-]{1,23}(?::\d|[/／?#])')),
    ('host', HOST_BARE), ('host', HOST_DOT),
    ('ipv4', re.compile(r'(?<![\d.])(?:\d{1,3}\.){3}\d{1,3}(?!\d)')),
    ('user-path', HOME_WIN), ('user-path', HOME_POSIX),
    ('address', ADDRESS), ('address', ESTATE),
)
CODES = {'acct': ('P', 8), 'group': ('G', 8), 'mid': ('M', 10), 'batch': ('B', 10)}
MEDIA_KINDS = {'image', 'video', 'voice', 'file', 'sticker', 'card', 'markdown', 'other'}
SOURCES = ('snowluma', 'ingress', 'send')
TOP_KEYS = {'v', 'src', 'conv', 'dir', 'sender', 'time', 'mid', 'segs', 'batch', 'idx', 'phase'}
FORMATS = {'conv': re.compile(r'(?:group:G|private:P)-[a-z]{8}'), 'sender': re.compile(r'P-[a-z]{8}'),
           'mid': re.compile(r'M-[a-z]{10}'), 'batch': re.compile(r'B-[a-z]{10}'),
           'time': re.compile(r'\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z')}
SEG_KEYS = {'text': {'k', 'text'}, 'at': {'k', 'id'}, 'reply': {'k', 'mid'}, 'forward': {'k'},
            'face': {'k', 'id'}, 'media': {'k', 'kind'}}


def name_pattern(spellings):
    forms = set()
    for spelling in spellings:
        forms.update((spelling, spelling.lower(), spelling.casefold()))
    items = []
    for form in sorted(forms, key=lambda f: (-len(f), f)):
        escaped = re.escape(form)
        items.append(rf'(?<![A-Za-z0-9]){escaped}(?![A-Za-z0-9])' if form.isascii() else escaped)
    return re.compile('|'.join(items), re.IGNORECASE) if items else None


def code(key, space, value):
    prefix, length = CODES[space]
    digest = hmac.new(key, f'{space}\0{value}'.encode('utf-8'), hashlib.sha256).digest()
    return prefix + '-' + ''.join(chr(97 + byte % 26) for byte in digest[:length])


class Stripper:
    def __init__(self, key, records):
        self.key, self.counts, self.ids, self.names = key, Counter(), {}, {}
        self.spellings, self.short_names, self.unsafe_names = {}, set(), set()
        for record in records:
            self._learn(record)
        self.counts['short_names_unreplaced'] = len(self.short_names)
        self.counts['unsafe_names_skipped'] = len(self.unsafe_names)
        self.name_re = name_pattern(self.spellings)

    def field(self, space, raw):
        return code(self.key, space, str(raw))

    def _learn_name(self, raw, code_):
        name = fold(str(raw)).strip()
        # Decorated names: the whole name, the name without digits, and every part between non-letters.
        candidates = [name, ''.join(c for c in name if not c.isdigit()), *NAME_SPLIT.split(name)] if name else []
        for part in candidates:
            part = part.strip()
            if not part:
                continue
            key = part.casefold()
            if len(part) < 2:
                self.short_names.add(key)
            elif NOT_A_NAME.fullmatch(part) or key in DECOR_STOP:
                self.unsafe_names.add(key)
            else:
                self.names.setdefault(key, code_)
                self.spellings.setdefault(part, key)

    def _learn(self, record):
        kind, _, conv_id = str(record.get('conv') or '').partition(':')
        if conv_id:
            self.ids.setdefault(conv_id, self.field('group' if kind == 'group' else 'acct', conv_id))
            if kind == 'group' and record.get('convName'):
                self._learn_name(record['convName'], self.ids[conv_id])
        sender = record.get('sender') or {}
        if sender.get('id'):
            sid = str(sender['id'])
            self.ids.setdefault(sid, self.field('acct', sid))
            for name in (sender.get('card'), sender.get('nick')):
                if name:
                    self._learn_name(name, self.field('acct', sid))
        for seg in record.get('segs') or []:
            if isinstance(seg, dict) and seg.get('k') == 'at' and seg.get('id') not in (None, '', 'all'):
                aid = str(seg['id'])
                self.ids.setdefault(aid, self.field('acct', aid))
                if seg.get('name'):
                    self._learn_name(seg['name'], self.field('acct', aid))

    def _digits(self, value):
        def run(match):
            raw = match.group()
            numerals = [c for c in raw if c.isdigit() or c in NUMERAL_CHARS]
            if len(numerals) < 5:
                return raw
            tail = raw[-1] if raw[-1] in 'Xx' else ''
            digits = ''.join(c for c in raw if c.isdigit())
            known = self.ids.get(digits) if len(digits) == len(numerals) else None
            self.counts['known_id' if known else 'digit_run'] += 1
            return (f'〔{known}〕' if known else '<数字>') + (tail if tail and len(numerals) < 15 else '')
        pieces = DATE_SPLIT.split(value)
        return ''.join(piece if index % 2 else DIGIT_SEQ.sub(run, piece) for index, piece in enumerate(pieces))

    def _pass(self, value):
        for kind, pattern, placeholder in TEXT_RULES:
            value, n = pattern.subn(placeholder, value)
            self.counts[kind] += n
        value = self._digits(value)
        value, n = CJK_NUM.subn('<数字>', value)
        self.counts['cjk_digits'] += n
        if self.name_re:
            def name(match):
                code_ = self.names.get(match.group().casefold())
                return f'〔{code_}〕' if code_ else '<提及>'
            value, n = self.name_re.subn(name, value)
            self.counts['name'] += n
        value = MENTION_CODE.sub('', value)
        value, n = MENTION.subn('<提及>', value)
        self.counts['mention'] += n
        return value

    def text(self, value):
        """Fold, then apply every rule until the text no longer changes (at most six passes)."""
        value = fold(value)
        for _ in range(6):
            new = self._pass(value)
            if new == value:
                break
            value = new
        return value

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


def describe_ace(ace, sid):
    """type;flags;rights;who for one ACE: who is the alias or well-known SID, current-user, domain-rid-<RID> (never the domain part) or other-account; no object GUID."""
    fields = ace.split(';')
    if len(fields) < 6:
        return 'malformed'
    who = fields[5]
    domain_rid = re.fullmatch(r'S-1-5-21-[0-9-]+-([0-9]+)', who)
    if re.fullmatch(r'[A-Z]{2}', who):
        label = who
    elif who == sid:
        label = 'current-user'
    elif re.fullmatch(r'S-1-[0-9]-[0-9]{1,2}|S-1-5-32-[0-9]{3}|S-1-5-80-0', who):
        label = who
    elif domain_rid:
        label = f'domain-rid-{domain_rid.group(1)}'
    else:
        label = 'other-account'
    return f'{fields[0]};{fields[1]};{fields[2]};{label}'


# SYSTEM, Administrators, OWNER RIGHTS: the privileged built-ins OpenSSH for Windows accepts on a private key.
PRIVILEGED_SIDS = ('S-1-5-18', 'S-1-5-32-544', 'S-1-3-4')


def dacl_problem(sddl, sid):
    """None when the DACL is protected, and every ACE is a non-inherited allow entry for sid or a PRIVILEGED_SIDS entry, with exactly one full-access entry for sid; else the first failed condition."""
    match = re.search(r'D:([A-Z]*)((?:\([^)]*\))*)', sddl)
    if not match or 'P' not in match.group(1):
        return 'not protected'
    aces = re.findall(r'\(([^)]*)\)', match.group(2))
    if not aces:
        return 'entry count 0'
    entries = [ace.split(';') for ace in aces]
    for ace, fields in zip(aces, entries):
        if len(fields) < 6:
            return 'malformed entry'
        if fields[0] != 'A':
            return 'entry is not an allow entry'
        if 'ID' in fields[1]:
            return 'entry is inherited'
        if fields[5] != sid and fields[5] not in PRIVILEGED_SIDS:
            return 'entry for another account: ' + describe_ace(ace, sid)
    user = [fields for fields in entries if fields[5] == sid]
    if len(user) > 1:
        return f'entry count {len(aces)}: ' + ' '.join(describe_ace(ace, sid) for ace in aces)
    if not user or user[0][2] not in ('FA', '0x1f01ff'):
        return 'current user lacks full access'
    return None


def private_dacl(sddl, sid):
    """True when dacl_problem finds no problem."""
    return dacl_problem(sddl, sid) is None


def _windows_sid():
    try:
        proc = subprocess.run(['whoami', '/user', '/fo', 'csv', '/nh'], capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.SubprocessError):
        raise SystemExit('cannot read the current user SID')
    sid = proc.stdout.strip().rsplit(',', 1)[-1].strip().strip('"')
    if proc.returncode != 0 or not re.fullmatch(r'S-1-[0-9-]+', sid):
        raise SystemExit('cannot read the current user SID')
    return sid


def _windows_sddl(path):
    """The file's SDDL with trustee aliases expanded to full SIDs, so an alias account (for example the built-in administrator, `LA`) still matches."""
    env = dict(os.environ, HER_KEY_PATH=str(path))
    try:
        proc = subprocess.run(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command',
                               "$s = (Get-Acl -LiteralPath $env:HER_KEY_PATH).Sddl; "
                               "[regex]::Replace($s, '(?<=;)([A-Z]{2})(?=\\))', { param($m) "
                               "(New-Object System.Security.Principal.SecurityIdentifier($m.Value)).Value })"],
                              capture_output=True, text=True, timeout=120, env=env)
    except (OSError, subprocess.SubprocessError):
        raise SystemExit('cannot read the key file ACL')
    if proc.returncode != 0:
        raise SystemExit('cannot read the key file ACL')
    return proc.stdout.strip()


def check_key_file_private(path):
    if os.name != 'nt':
        if os.stat(path).st_mode & 0o077:
            raise SystemExit('key file is readable by others; restrict it to its owner')
        return
    problem = dacl_problem(_windows_sddl(path), _windows_sid())
    if problem:
        raise SystemExit(f'key file access is not restricted to the current user ({problem})')


def restrict_key_file(path):
    if os.name != 'nt':
        os.chmod(path, 0o600)
        return
    sid = _windows_sid()
    try:
        proc = subprocess.run(['icacls', str(path), '/inheritance:r', '/grant:r', f'*{sid}:F'],
                              capture_output=True, text=True, timeout=120)
    except (OSError, subprocess.SubprocessError):
        raise SystemExit('key file access could not be restricted')
    if proc.returncode != 0:
        raise SystemExit('key file access could not be restricted')


def load_key(path):
    try:
        check_key_file_private(path)
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
    """Create the file empty, restrict and verify its access, and only then write the key."""
    path = Path(path)
    try:
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        raise SystemExit('key file exists; refusing to replace it')
    except OSError:
        raise SystemExit('key file cannot be created')
    os.close(fd)
    try:
        restrict_key_file(path)
        check_key_file_private(path)
        with open(path, 'w', encoding='ascii') as handle:   # truncating keeps the verified ACL
            handle.write(secrets.token_hex(32) + '\n')
    except BaseException:
        try:
            path.unlink()
        except OSError:
            pass
        raise


def read_records(paths, counts):
    """Schema v1 records from the input files. An unparseable line is skipped and counted as bad_lines
    (an unterminated last line is also counted as torn_tail); input that yields no record but has bad
    lines is refused, naming the files only."""
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
                counts['bad_lines'] += 1
                if number == len(lines) and not text.endswith('\n'):
                    counts['torn_tail'] += 1
                continue
            if not isinstance(record, dict) or record.get('v') != 1:
                raise SystemExit(f'input {name}:{number}: not a schema v1 record')
            records.append(record)
    if not records and counts['bad_lines']:
        names = ', '.join(Path(path).name for path in paths)
        raise SystemExit(f'input {names}: no readable schema v1 record ({counts["bad_lines"]} unparseable lines)')
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


def residuals(text, name_re):
    """Leftovers in one stripped text, by kind; independent of the replacement passes."""
    found = Counter()
    text = fold(text)
    if ADJACENT.search(text):
        found['adjacent-digits'] += 1
    if re.search(r'<链接>[/?&＆]|<链接>？(?=[A-Za-z_]{1,24}=)', text):
        found['link-tail'] += 1
    if re.search(r'[?&＆][A-Za-z_]{1,24}=', text):
        found['query-fragment'] += 1
    bare = PLACEHOLDER.sub('\x00', text)
    if any(sum(c.isdigit() or c in NUMERAL_CHARS for c in m.group()) >= 5
           for piece in DATE_SPLIT.split(bare)[::2] for m in SCAN_DIGITS.finditer(piece)):
        found['digits'] += 1
    for kind, pattern in SCAN_RULES:
        if pattern.search(bare):
            found[kind] += 1
    if 'user-path' not in found and any(pattern.search(bare) for pattern in SCAN.USER_PATH_PATTERNS):
        found['user-path'] += 1
    if name_re and name_re.search(bare):
        found['name'] += 1
    return found


def field_findings(record):
    found = Counter()
    for key, value in record.items():
        if key not in TOP_KEYS:
            found['field'] += 1
        elif key in FORMATS and value is not None and not (isinstance(value, str) and FORMATS[key].fullmatch(value)):
            found['field'] += 1
    checks = (('v', lambda v: v == 1), ('src', lambda v: v in SOURCES or v is None),
              ('dir', lambda v: v in ('in', 'out', None)), ('phase', lambda v: v in ('intent', 'confirmed', None)),
              ('idx', lambda v: v is None or (isinstance(v, int) and not isinstance(v, bool) and 0 <= v < 1000)))
    for key, ok in checks:
        if key in record and not ok(record[key]):
            found['field'] += 1
    for seg in record.get('segs') or []:
        if not isinstance(seg, dict) or seg.get('k') not in SEG_KEYS or set(seg) - SEG_KEYS[seg['k']]:
            found['field'] += 1
            continue
        k = seg['k']
        bad = ((k == 'at' and seg.get('id') not in (None, 'all') and not FORMATS['sender'].fullmatch(str(seg.get('id'))))
               or (k == 'reply' and seg.get('mid') is not None and not FORMATS['mid'].fullmatch(str(seg['mid'])))
               or (k == 'face' and seg.get('id') is not None and not re.fullmatch(r'[0-9]{1,4}', str(seg['id'])))
               or (k == 'media' and seg.get('kind') not in MEDIA_KINDS)
               or (k == 'text' and not isinstance(seg.get('text'), str)))
        if bad:
            found['field'] += 1
    return found


def self_scan(record, name_re):
    found = field_findings(record)
    for seg in record.get('segs') or []:
        if isinstance(seg, dict) and seg.get('k') == 'text' and isinstance(seg.get('text'), str):
            found.update(residuals(seg['text'], name_re))
    return found


NAME_RE = re.compile(r'[A-Za-z0-9._-]+')


def strip(args, scan=None, secret_scan=None):
    scan, secret_scan = scan or self_scan, secret_scan or SCAN.secret_findings
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
        found = scan(record, stripper.name_re)
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
        staged_report = Path(tmp) / f'{args.name}.report.json'
        try:
            with open(staged_body, 'w', encoding='utf-8') as handle:
                for record in out:
                    handle.write(json.dumps(record, ensure_ascii=False, separators=(',', ':')) + '\n')
        except (OSError, UnicodeError):
            print('strip failed: the output could not be staged', file=sys.stderr)
            return 1
        findings, error = secret_scan(Path(tmp), ROOT / '.gitleaks.toml', scan_skipped=True)
        if error or findings:
            print(f'secret-scan: {error or str(len(findings)) + " findings"}', file=sys.stderr)
            return 1
        report = report_for(records, out, stripper, counts)
        published = []
        try:
            staged_report.write_text(json.dumps(report, indent=2, sort_keys=True) + '\n', encoding='utf-8')
            for staged, final in ((staged_body, body_path), (staged_report, report_path)):
                os.replace(staged, final)
                published.append(final)
        except OSError:
            for final in published:
                try:
                    final.unlink()
                except OSError:
                    pass
            print('strip failed: the output could not be published; nothing was kept', file=sys.stderr)
            return 1
    print(f'strip: records_in={report["records_in"]} records_out={report["records_out"]} '
          f'bad_lines={report["bad_lines"]} torn_tail={report["torn_tail"]} self_scan_findings=0 secret_scan=clean')
    return 0


def report_for(records, out, stripper, counts):
    """Counts only: no pseudonym, text, name or id appears in the report."""
    days = sorted(r['time'][:10] for r in out if r['time'])
    return {
        'schema': 'her.history-strip.v1',
        'records_in': len(records),
        'records_out': len(out),
        'bad_lines': counts['bad_lines'],
        'torn_tail': counts['torn_tail'],
        'conversations': {
            'group': len({r['conv'] for r in out if r['conv'] and r['conv'].startswith('group:')}),
            'private': len({r['conv'] for r in out if r['conv'] and r['conv'].startswith('private:')}),
        },
        'persons': len({r['sender'] for r in out if r['sender']}),
        'by_src': dict(Counter(r['src'] or 'none' for r in out)),
        'by_dir': dict(Counter(r['dir'] or 'none' for r in out)),
        'replaced': {k: stripper.counts[k] for k in
                     ('path', 'link', 'email', 'phone', 'address', 'name', 'mention', 'known_id', 'digit_run', 'cjk_digits')},
        'media': {k[len('media_'):]: n for k, n in stripper.counts.items() if k.startswith('media_')},
        'short_names_unreplaced': stripper.counts['short_names_unreplaced'],
        'unsafe_names_skipped': stripper.counts['unsafe_names_skipped'],
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

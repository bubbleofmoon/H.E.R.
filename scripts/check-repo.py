#!/usr/bin/env python3
"""Check Markdown/JSON control characters, tracked hook script modes, and optional EOL parity."""
import argparse
from pathlib import Path
import fnmatch
import os
import re
import subprocess
import sys

CONTROL = re.compile(r'[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]')
EXECUTABLE_PATTERNS = ('*.sh', '.githooks/*')
EXECUTABLE_MODE = '100755'

parser = argparse.ArgumentParser(add_help=True)
parser.add_argument('--staged', action='store_true')
parser.add_argument('--eol-base', default=None, help='Check modified text files retain base LF/CRLF style')
args = parser.parse_args()

root = Path(__file__).resolve().parent.parent
staged = args.staged
clean_env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
if staged:
    result = subprocess.run(['git', 'diff', '--cached', '--name-only', '-z', '--diff-filter=ACMR'],
                            cwd=root, env=clean_env, capture_output=True, check=True)
    paths = [p.decode('utf-8') for p in result.stdout.split(b'\0') if p]
else:
    paths = [p.relative_to(root).as_posix() for p in root.rglob('*')
             if p.is_file() and not any(part in {'.git', 'target', 'node_modules', '__pycache__', 'ci-evidence', '.test-fixtures'}
                                       for part in p.relative_to(root).parts)]
findings = []
for path in sorted(paths):
    if not path.endswith(('.md', '.json')):
        continue
    raw = subprocess.check_output(['git', 'show', ':' + path], cwd=root, env=clean_env) if staged else (root / path).read_bytes()
    text = raw.decode('utf-8-sig')
    for number, line in enumerate(text.split('\n'), 1):
        match = CONTROL.search(line)
        if match:
            findings.append(f'{path}:{number}: raw control character U+{ord(match.group()):04X}')
for finding in findings:
    print(finding)
print(f'Control-character scan: {len(findings)} findings')

# The index holds the modes that git will commit; a checkout made with core.fileMode off can lose
# the executable bit silently, so the check reads the index and never the working copy.
# A plain snapshot has no index to read and skips this check.
mode_findings = []
if (root / '.git').exists():
    listing = subprocess.run(['git', 'ls-files', '-s', '-z'], cwd=root, env=clean_env, capture_output=True, check=True).stdout
    for entry in listing.split(b'\0'):
        if not entry:
            continue
        meta, path = entry.decode('utf-8').split('\t', 1)
        mode = meta.split(' ', 1)[0]
        if mode != EXECUTABLE_MODE and any(fnmatch.fnmatchcase(path, pattern) for pattern in EXECUTABLE_PATTERNS):
            mode_findings.append(f'mode: {path} is {mode}, expected {EXECUTABLE_MODE}')
for finding in mode_findings:
    print(finding)
print(f'File-mode scan: {len(mode_findings)} findings')


def _eol_style(raw):
    if b'\0' in raw or b'\n' not in raw:
        return None
    if b'\r\n' in raw:
        return 'CRLF'
    return 'LF'


def _resolve_eol_base(ref):
    res = subprocess.run(['git', 'merge-base', ref, 'HEAD'], cwd=root, env=clean_env, capture_output=True, text=True)
    merged = res.stdout.strip()
    return merged if res.returncode == 0 and merged else ref


def _is_autocrlf():
    res = subprocess.run(['git', 'config', '--get', 'core.autocrlf'], cwd=root, env=clean_env, capture_output=True, text=True)
    return res.returncode == 0 and res.stdout.strip().lower() == 'true'


def _autocrlf_allows_work_crlf(rel):
    res = subprocess.run(['git', 'check-attr', '-z', 'text', 'eol', '--', rel], cwd=root, env=clean_env, capture_output=True, check=True)
    parts = res.stdout.decode('utf-8').split('\0')
    attrs = {parts[i + 1]: parts[i + 2] for i in range(0, len(parts) - 2, 3)}
    return attrs.get('text') != 'unset' and attrs.get('eol') != 'lf'


eol_findings = []
if args.eol_base and (root / '.git').exists():
    base_rev = _resolve_eol_base(args.eol_base)
    autocrlf = _is_autocrlf()
    diff_cmd = ['git', 'diff', '--name-only', '--diff-filter=M', '-z']
    if staged:
        diff_cmd.append('--cached')
    diff_cmd.append(base_rev)
    diff_res = subprocess.run(diff_cmd, cwd=root, env=clean_env, capture_output=True, check=True)
    for rel_b in sorted(p for p in diff_res.stdout.split(b'\0') if p):
        rel = rel_b.decode('utf-8')
        base_raw = subprocess.check_output(['git', 'show', f'{base_rev}:{rel}'], cwd=root, env=clean_env)
        base_style = _eol_style(base_raw)
        if base_style is None:
            continue
        index_raw = subprocess.check_output(['git', 'show', f':{rel}'], cwd=root, env=clean_env)
        index_style = _eol_style(index_raw)
        if index_style and index_style != base_style:
            eol_findings.append(f'eol: {rel} changed from {base_style} to {index_style}')
            continue
        work = root / rel
        if not staged and work.is_file():
            work_style = _eol_style(work.read_bytes())
            if work_style and work_style != base_style:
                if autocrlf and base_style == 'LF' and work_style == 'CRLF' and _autocrlf_allows_work_crlf(rel):
                    continue
                eol_findings.append(f'eol: {rel} changed from {base_style} to {work_style}')
    for finding in eol_findings:
        print(finding)
    print(f'EOL-parity scan: {len(eol_findings)} findings')

sys.exit(bool(findings or mode_findings or eol_findings))

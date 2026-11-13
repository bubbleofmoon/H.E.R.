#!/usr/bin/env python3
"""Check Markdown/JSON control characters and tracked hook script modes in a Git checkout or a plain snapshot."""
from pathlib import Path
import fnmatch
import re
import subprocess
import sys

CONTROL = re.compile(r'[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]')
EXECUTABLE_PATTERNS = ('*.sh', '.githooks/*', '.claude/hooks/*')
EXECUTABLE_MODE = '100755'
root = Path(__file__).resolve().parent.parent
staged = '--staged' in sys.argv
if staged:
    result = subprocess.run(['git', 'diff', '--cached', '--name-only', '-z', '--diff-filter=ACMR'],
                            cwd=root, capture_output=True, check=True)
    paths = [p.decode('utf-8') for p in result.stdout.split(b'\0') if p]
else:
    paths = [p.relative_to(root).as_posix() for p in root.rglob('*')
             if p.is_file() and not any(part in {'.git', 'target', 'node_modules', '__pycache__', 'ci-evidence', '.test-fixtures'}
                                       for part in p.relative_to(root).parts)]
findings = []
for path in sorted(paths):
    if not path.endswith(('.md', '.json')):
        continue
    raw = subprocess.check_output(['git', 'show', ':' + path], cwd=root) if staged else (root / path).read_bytes()
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
    listing = subprocess.run(['git', 'ls-files', '-s', '-z'], cwd=root, capture_output=True, check=True).stdout
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
sys.exit(bool(findings or mode_findings))

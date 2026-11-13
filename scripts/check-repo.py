#!/usr/bin/env python3
"""Check Markdown/JSON control characters in a Git checkout or a plain snapshot."""
from pathlib import Path
import re
import subprocess
import sys

CONTROL = re.compile(r'[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]')
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
sys.exit(bool(findings))

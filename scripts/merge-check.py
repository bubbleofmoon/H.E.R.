#!/usr/bin/env python3
"""Check a merge of two Rust branches before it is committed.

Run from the worktree with `python3 -I scripts/merge-check.py`: with a merge in progress (MERGE_HEAD)
ours is HEAD, theirs is MERGE_HEAD and the base is their merge base; otherwise pass all of
`--ours REV --theirs REV --base REV` (the working tree then holds the merge result to check).

For every *.rs file that both sides changed relative to the base, the working file must hold
  - the `#[test]` count of ours + theirs - base (`#[tokio::test]` and other `<path>::test` count too), and
  - the `{` minus `}` balance of ours + theirs - base.
Both counts are linear in a merge that keeps both sides' changes, so raw counts are compared as deltas: a brace
inside a string, char literal or comment appears in every side alike and cancels. A dropped test or a closing brace
lost in a conflict hunk shows as a mismatch.

Exit 0 when every file matches, 1 on any mismatch (file and numbers printed), 2 on a usage or git error.
"""
import argparse
from pathlib import Path
import re
import subprocess
import sys

TEST_ATTR = re.compile(r'^[ \t]*#\[(?:\w+::)*test(?:\([^\]]*\))?\]', re.M)


def git(*args):
    done = subprocess.run(['git', *args], capture_output=True)
    return done.returncode, done.stdout


def fail(message):
    print(f'merge-check: {message}', file=sys.stderr)
    sys.exit(2)


def resolve(rev):
    code, out = git('rev-parse', '--verify', '--quiet', f'{rev}^{{commit}}')
    if code:
        fail(f'not a commit: {rev}')
    return out.decode().strip()


def changed_rs(base, side):
    code, out = git('diff', '--name-only', '--no-renames', '-z', base, side)
    if code:
        fail(f'git diff {base} {side} failed')
    return {name for name in out.decode('utf-8', 'surrogateescape').split('\0') if name.endswith('.rs')}


def counts(text):
    return len(TEST_ATTR.findall(text)), text.count('{') - text.count('}')


def revision_counts(rev, path):
    code, out = git('show', f'{rev}:{path}')  # a file the revision lacks counts as empty
    return counts(out.decode('utf-8', 'replace') if code == 0 else '')


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--ours')
    ap.add_argument('--theirs')
    ap.add_argument('--base')
    args = ap.parse_args()
    code, top = git('rev-parse', '--show-toplevel')
    if code:
        fail('not inside a git worktree')
    root = Path(top.decode().strip())
    given = [args.ours, args.theirs, args.base]
    if all(given):
        ours, theirs, base = (resolve(rev) for rev in given)
    elif any(given):
        fail('pass all of --ours, --theirs and --base, or none of them during a merge')
    else:
        code, head = git('rev-parse', '--verify', '--quiet', 'MERGE_HEAD')
        if code:
            fail('no merge in progress (MERGE_HEAD missing) and no --ours/--theirs/--base given')
        ours, theirs = resolve('HEAD'), head.decode().strip()
        code, mb = git('merge-base', ours, theirs)
        if code:
            fail('no merge base for HEAD and MERGE_HEAD')
        base = mb.decode().strip()

    files = sorted(changed_rs(base, ours) & changed_rs(base, theirs))
    bad = 0
    for path in files:
        file = root / path
        working = counts(file.read_text(encoding='utf-8', errors='replace')) if file.is_file() else (0, 0)
        b, o, t = (revision_counts(rev, path) for rev in (base, ours, theirs))
        problems = []
        for i, label in enumerate(('tests', 'braces')):
            want = o[i] + t[i] - b[i]
            if working[i] != want:
                problems.append(f'{label}: working {working[i]}, expected {want} '
                                f'(ours {o[i]}, theirs {t[i]}, base {b[i]})')
        if problems:
            bad += 1
            print(f'MISMATCH {path}: ' + '; '.join(problems))
        else:
            print(f'ok {path}: {working[0]} tests, brace balance {working[1]}')
    print(f'merge-check: {len(files)} Rust file(s) changed on both sides, {bad} mismatch(es)')
    return 1 if bad else 0


sys.exit(main())

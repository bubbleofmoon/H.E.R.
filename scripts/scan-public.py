#!/usr/bin/env python3
"""One public guard for a whole tree, for a single text on stdin and for published history.

    python -I scripts/scan-public.py                     scan this checkout
    python -I scripts/scan-public.py --root DIR          scan another tree
    python -I scripts/scan-public.py --stdin             scan text read from stdin
    python -I scripts/scan-public.py --revs-from-stdin   scan the trees of the revisions
                                                         named on stdin (one sha per line)
    python -I scripts/scan-public.py --added-from-stdin  scan only the blobs the commits named
                                                         on stdin add over their parents
    python -I scripts/scan-public.py --objects-from-stdin  scan the message payload of every
                                                         commit or tag object named on stdin

It refuses, in that order: secrets (the gitleaks rules in .gitleaks.toml plus the
gitleaks default ruleset), digit runs of 7 or more that have no reviewed exception in
.public-scan.json, account-home paths (a drive-qualified, `/c/`-qualified or macOS
`/Users/<name>` profile path naming a person's folder, in any casing), and lines that
look like quoted chat or session records (turn-ledger, live-log and transcript shapes).
`scripts/gh-public.ps1`, `scripts/gh-public.sh` and `.githooks/pre-push` call this one
script, so a comment, a push and the history a push publishes are checked the same way.

A finding prints as `KIND path:line` and never repeats the matched text. Text mode has
no reviewed exceptions: nothing pasted into an issue or comment has been reviewed yet,
so every digit run, account-home path and record shape is a finding. Chat and session
record shapes are checked in every line of stdin and, in tree mode, in Markdown and
text files; source files legitimately contain those same JSON shapes as code, so they
are checked by the other three rules instead. Digit and account-home exceptions bind to
one file, one line hash and a written reason in .public-scan.json, and never widen a
rule. A missing gitleaks binary is itself a finding: the secret scan never silently
skips.

History mode is what a push owes the public: it scans every tracked blob of every
revision it is given, including a blob whose path sits under a directory the tree scan
skips locally, because a committed blob is published even when the working copy ignores
it. Exceptions are read from the checkout being pushed: a reviewed synthetic value stays
synthetic in an older revision of the same path. A git or scan error is itself a finding,
so history mode fails closed. Exit code 0 means zero findings, 3 means gitleaks could not be found (the other findings are still printed), 1 any other finding.

Added mode is history mode for a branch that moves forward from a commit the remote already has: for each commit it scans the blobs that differ from each parent (`git diff-tree -r --no-renames`), because a blob a commit shares with a parent is published by that parent. The caller passes only commits whose every parent is passed too or is already on the remote. A commit without a parent, or with a parent this repository cannot read, is scanned as a whole tree. Deletions and submodule links publish no blob content.

Message mode covers the other half of what a push publishes: the message of every commit and
annotated tag object it is given. A clean tree with a sensitive commit or tag message is
published prose, so it is scanned as such; no reviewed exception applies to a message, and an
object that cannot be inspected is a finding. Only the message payload is read: the structural
headers of an object (its tree, parents, hashes and timestamps) are not prose and are never
scanned as if they were.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

SKIP_PARTS = {'.git', 'target', 'node_modules', '__pycache__', 'ci-evidence', '.test-fixtures', 'graphify-out'}
CHAT_TREE_SUFFIXES = {'.md', '.txt'}

# An account home names the person who owns the machine, so it never belongs in public.
# The macOS spelling is written with a bracketed first letter on purpose: the pattern
# source must not match itself when the guard scans its own file, and the value stops at
# `|` so a product regular expression that lists `/Users/` as an alternative is not a hit.
USER_PATH_PATTERNS = (
    re.compile(r'(?i)\b[a-z]:[\\/]+users[\\/]+\S'),
    re.compile(r'(?i)(?:^|[\s"\'`(=])/[a-z]/users/[^\s"\'`),;]+'),
    re.compile(r'(?i)(?:^|[\s"\'`(=/\[{<])/[u]sers/[A-Za-z0-9._+-]+'),
)

# Chat and session record shapes: turn-ledger records, live-log records, transcripts.
# A documented log shape stays clean because these require a real record payload: a
# conversation key, a quoted identifier or a numeric field, never a placeholder.
CHAT_PATTERNS = (
    ('ledger kind', re.compile(r'"k"\s*:\s*"')),
    ('turn id', re.compile(r'"turn"\s*:\s*(?:"|-?\d)')),
    ('turn id field', re.compile(r'"turnId"\s*:\s*"')),
    ('ledger inbound', re.compile(r'"inbound"\s*:\s*\[')),
    ('conversation key', re.compile(r'"key"\s*:\s*"(?:group|private)[:]')),
    ('rpc id', re.compile(r'"rpcId"\s*:\s*"')),
    ('session id', re.compile(r'"sessionId"\s*:\s*"')),
    ('live log record', re.compile(r'"ts"\s*:\s*\d')),
    ('live log predicate', re.compile(r'"predicate"\s*:\s*"')),
    ('transcript role', re.compile(r'"(?:role|sender)"\s*:\s*"(?:user|assistant|member|owner|admin)"')),
    ('timestamped record line', re.compile(r'^\s*[">]?\s*\[?\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}')),
    ('chat log prefix', re.compile(r'^\s*[">]?\s*\[(?:recv|send|私聊|群聊|QQ)\]')),
    ('prompt chat history', re.compile(r'最近的聊天记录')),
    ('live out-log line', re.compile(r'\[reserved2\]\s+(?:group|private):\d')),
)


MISSING_SCANNER = 'secret-scan <scan>:0: gitleaks not found (set HER_GITLEAKS or install it with scripts/ci/install-gitleaks.py)'


def gitleaks_path():
    override = os.environ.get('HER_GITLEAKS')
    if override:
        candidate = Path(override)
        return candidate if candidate.is_file() else None
    return shutil.which('gitleaks')


def secret_findings(source, config, scan_skipped=False):
    """(findings, error): findings are (relative path, line) pairs, error is a string or None.

    `scan_skipped` keeps the cache-like directory names in SKIP_PARTS in scope: a locally
    generated folder is not a reason to skip a blob that is actually published.
    """
    exe = gitleaks_path()
    if exe is None:
        return [], MISSING_SCANNER
    with tempfile.TemporaryDirectory() as tmp:
        report = Path(tmp) / 'gitleaks.json'
        command = [str(exe), 'detect', '--no-git']
        if config.is_file():
            command += ['--config', str(config)]
        command += ['--source', str(source), '--redact', '--no-banner',
                    '--report-format', 'json', '--report-path', str(report)]
        try:
            proc = subprocess.run(command, capture_output=True, text=True, encoding='utf-8', timeout=1800)
        except (OSError, subprocess.TimeoutExpired) as error:
            return [], f'secret-scan <scan>:0: gitleaks could not run ({type(error).__name__})'
        if proc.returncode not in (0, 1):
            return [], f'secret-scan <scan>:0: gitleaks exited {proc.returncode}'
        if not report.is_file():
            # gitleaks writes no report when it finds nothing; exit 0 already proves that.
            if proc.returncode == 0:
                return [], None
            return [], 'secret-scan <scan>:0: gitleaks reported leaks without writing a report'
        root = str(source).replace('\\', '/').rstrip('/') + '/'
        findings = []
        for item in json.loads(report.read_text(encoding='utf-8') or '[]'):
            rel = str(item.get('File', '')).replace('\\', '/')
            if rel.startswith(root):
                rel = rel[len(root):]
            if not scan_skipped and any(part in SKIP_PARTS for part in Path(rel).parts):
                continue
            findings.append((rel, int(item.get('StartLine') or 1)))
        return findings, None


def scan_lines(rel, text, allowed, path_allowed, counters, findings, chat_rules):
    for number, line in enumerate(text.splitlines(), 1):
        digits = list(re.finditer(r'[0-9]{7,}', line))
        digest = None
        if digits or any(pattern.search(line) for pattern in USER_PATH_PATTERNS):
            digest = hashlib.sha256(line.encode()).hexdigest()
        for pattern in USER_PATH_PATTERNS:
            if pattern.search(line):
                entry = path_allowed.get((rel, digest))
                if entry and entry.strip():
                    counters['paths reviewed'] += 1
                else:
                    findings.append(f'user-path {rel}:{number}')
                break
        if chat_rules:
            for _, pattern in chat_rules:
                if pattern.search(line):
                    findings.append(f'chat-record {rel}:{number}')
                    break
        for match in digits:
            key = (rel, '-'.join(match.group()), digest)
            if key in allowed and allowed[key].strip():
                counters['reviewed'] += 1
            else:
                findings.append(f'digit {rel}:{number}')


def scan_tree(root, config, allowed, path_allowed, counters, findings, scan_skipped=False):
    """Scan the files of a materialized tree; returns the file count."""
    files = 0
    for path in sorted(root.rglob('*')):
        rel = path.relative_to(root)
        if not path.is_file():
            continue
        if not scan_skipped and any(part in SKIP_PARTS for part in rel.parts):
            continue
        files += 1
        try:
            text = path.read_text(encoding='utf-8-sig')
        except UnicodeDecodeError:
            findings.append(f'non-text {rel.as_posix()}:1')
            continue
        chat_rules = CHAT_PATTERNS if path.suffix.lower() in CHAT_TREE_SUFFIXES else ()
        scan_lines(rel.as_posix(), text, allowed, path_allowed, counters, findings, chat_rules)
    secrets, error = secret_findings(root, config, scan_skipped=scan_skipped)
    if error:
        findings.append(error)
    return files, secrets


def git_stdout(repo, *args):
    """(stdout bytes, error): a git failure is reported, never swallowed."""
    try:
        proc = subprocess.run(['git', '-C', str(repo), *args], capture_output=True, timeout=1800)
    except (OSError, subprocess.TimeoutExpired) as error:
        return b'', f'git {args[0]} could not run ({type(error).__name__})'
    if proc.returncode != 0:
        return b'', f'git {args[0]} exited {proc.returncode}'
    return proc.stdout, None


def collect_blobs(repo, revs):
    """(blobs, error): blobs maps (path, oid) to the short revisions that publish it."""
    blobs, seen = {}, set()
    for rev in revs:
        listing, error = git_stdout(repo, 'ls-tree', '-r', '-z', '--full-name', rev)
        if error:
            return {}, f'{error} (revision {rev[:12]})'
        short = rev[:12]
        seen.add(short)
        for record in listing.split(b'\0'):
            if not record:
                continue
            try:
                meta, path_bytes = record.split(b'\t', 1)
                mode, kind, oid = meta.decode().split(' ', 2)
            except ValueError:
                return {}, f'git ls-tree output could not be read (revision {short})'
            if kind != 'blob' or mode == '160000':
                continue  # a tree, a submodule or a gitlink publishes no blob content
            path = path_bytes.decode('utf-8', 'surrogateescape')
            blobs.setdefault((path, oid), []).append(short)
    return blobs, None


def collect_added_blobs(repo, commits):
    """(blobs, whole, error): the blobs each commit adds over its parents.

    `blobs` maps (path, oid) to the short commits that add it, as in collect_blobs. A commit
    without a parent, or with a parent this repository cannot read, goes to `whole` so its full
    tree is scanned instead.
    """
    blobs, whole = {}, []
    for commit in commits:
        raw_commit, error = git_stdout(repo, 'cat-file', 'commit', commit)
        if error:
            return {}, [], f'{error} (commit {commit[:12]})'
        header = raw_commit.partition(b'\n\n')[0]
        # The recorded parents, read from the object itself: a parent missing here (a shallow
        # boundary or a pruned object) is then seen as unreadable, never as absent.
        parents = [line[len(b'parent '):].decode('ascii', 'replace').strip()
                   for line in header.split(b'\n') if line.startswith(b'parent ')]
        if not parents or any(git_stdout(repo, 'cat-file', '-e', f'{parent}^{{commit}}')[1] for parent in parents):
            whole.append(commit)
            continue
        short = commit[:12]
        for parent in parents:
            raw, error = git_stdout(repo, 'diff-tree', '-r', '-z', '--no-renames', '--raw', parent, commit)
            if error:
                return {}, [], f'{error} (commit {short})'
            fields = raw.split(b'\0')
            if fields and fields[-1] == b'':
                fields.pop()
            if len(fields) % 2:
                return {}, [], f'git diff-tree output could not be read (commit {short})'
            for meta, path_bytes in zip(fields[0::2], fields[1::2]):
                try:
                    _, new_mode, _, new_oid, status = meta.decode('ascii').lstrip(':').split(' ')
                except (UnicodeDecodeError, ValueError):
                    return {}, [], f'git diff-tree output could not be read (commit {short})'
                if status == 'D' or new_mode in ('000000', '160000'):
                    continue  # a deletion or a submodule link publishes no blob content
                blobs.setdefault((path_bytes.decode('utf-8', 'surrogateescape'), new_oid), []).append(short)
    return blobs, whole, None


def tree_snapshots(blobs):
    """Group (path, oid) blobs so one snapshot holds at most one content per path.

    A path published with different content in the range has to be scanned once per
    content; grouping keeps the number of scans to the highest number of versions any
    single path has, which is one for an ordinary push.
    """
    snapshots, paths = [], []
    for key, revs in sorted(blobs.items()):
        path, _ = key
        for index, used in enumerate(paths):
            if path not in used:
                snapshots[index][key] = revs
                used.add(path)
                break
        else:
            snapshots.append({key: revs})
            paths.append({path})
    return snapshots


def object_message(repo, oid):
    """(kind, message, error): the message payload a commit or tag object publishes.

    Only the message is read. A commit's structural block (tree, parents, author, committer,
    timestamps, hashes) carries no prose, and `git show -s --format=%B` returns the message
    alone; a tag object's header block is stripped to the payload after the blank line.
    """
    kind_bytes, error = git_stdout(repo, 'cat-file', '-t', oid)
    if error:
        return '', '', f'{error} (object {oid[:12]})'
    kind = kind_bytes.decode('ascii', 'replace').strip()
    if kind == 'commit':
        raw, error = git_stdout(repo, 'show', '-s', '--format=%B', oid)
    elif kind == 'tag':
        raw, error = git_stdout(repo, 'cat-file', '-p', oid)
        if not error:
            _, _, raw = raw.partition(b'\n\n')
    else:
        return '', '', f'object {oid[:12]} has an unexpected type {kind or "unknown"}'
    if error:
        return '', '', f'{error} (object {oid[:12]})'
    try:
        return kind, raw.decode('utf-8'), None
    except UnicodeDecodeError:
        return '', '', f'the {kind} message of object {oid[:12]} is not UTF-8 text'


def history_context(blobs, flagged):
    """Name one publishing revision per flagged path, so the coordinator can find the hunk.

    These lines are context, not findings: the count of findings stays the count of refused
    contents. A revision sha and a path are public facts and carry no matched text.
    """
    lines, shown = [], 0
    for (path, _), revs in sorted(blobs.items()):
        if path not in flagged:
            continue
        lines.append(f'published-in {revs[0]} {path}')
        shown += 1
        if shown >= 20:
            lines.append('published-in ... more flagged paths are not listed')
            break
    return lines


def scan_blobs(root, blobs, allowed, path_allowed, counters, findings):
    """Materialize (path, oid) blobs snapshot by snapshot and scan them: (files, snapshots, flagged)."""
    files, flagged = 0, set()
    snapshots = tree_snapshots(blobs)
    for snapshot in snapshots:
        with tempfile.TemporaryDirectory() as tmp:
            stage = Path(tmp) / 'tree'
            stage.mkdir()
            shutil.copyfile(root / '.gitleaks.toml', stage / '.gitleaks.toml')
            for (path, oid), _ in snapshot.items():
                content, blob_error = git_stdout(root, 'cat-file', 'blob', oid)
                if blob_error:
                    findings.append(f'history {path}:1: {blob_error}')
                    continue
                target = stage / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(content)
            scanned, secrets = scan_tree(stage, root / '.gitleaks.toml', allowed, path_allowed,
                                         counters, findings, scan_skipped=True)
            files += scanned
            for rel, line in secrets:
                findings.append(f'secret {rel}:{line}')
            for rel, _ in secrets:
                flagged.add(rel)
    return files, len(snapshots), flagged


def main():
    parser = argparse.ArgumentParser(add_help=True)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument('--stdin', action='store_true', help='scan text read from stdin instead of a tree')
    parser.add_argument('--revs-from-stdin', action='store_true',
                        help='scan every tracked blob of every revision named on stdin, one sha per line')
    parser.add_argument('--objects-from-stdin', action='store_true',
                        help='scan the message payload of every commit or tag object named on stdin')
    parser.add_argument('--added-from-stdin', action='store_true',
                        help='scan only the blobs the commits named on stdin add over their parents')
    args = parser.parse_args()
    root = args.root.resolve()
    config_path = root / '.public-scan.json'
    allowed, path_allowed = {}, {}
    if config_path.is_file():
        config = json.loads(config_path.read_text(encoding='utf-8'))
        allowed = {(x['path'], x['value'], x['line_sha256'].replace('-', '')): x['reason']
                   for x in config.get('numeric_exceptions', [])}
        path_allowed = {(x['path'], x['line_sha256'].replace('-', '')): x['reason']
                        for x in config.get('path_exceptions', [])}
    findings = []
    context = []
    counters = {'reviewed': 0, 'paths reviewed': 0}
    mode, files, summary_extra = 'tree', 0, ''

    if args.stdin:
        mode = 'text'
        raw = sys.stdin.buffer.read()
        try:
            text = raw.decode('utf-8-sig')
        except UnicodeDecodeError:
            print('non-text <stdin>:1: input is not UTF-8 text')
            print('Public scan (text): 1 finding (non-text=1)')
            return 1
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / 'stdin.txt').write_bytes(raw)
            secrets, error = secret_findings(Path(tmp), root / '.gitleaks.toml')
        if error:
            findings.append(error)
        for _, line in secrets:
            findings.append(f'secret <stdin>:{line}')
        scan_lines('<stdin>', text, {}, {}, counters, findings, CHAT_PATTERNS)
        files = 1
    elif args.objects_from_stdin:
        mode = 'messages'
        oids = [line.strip() for line in sys.stdin.read().splitlines() if line.strip()]
        checked = 0
        messages = {}                       # staged file name -> "kind short-oid"
        with tempfile.TemporaryDirectory() as tmp:
            stage = Path(tmp)
            for oid in oids:
                kind, text, error = object_message(root, oid)
                checked += 1
                if error:
                    findings.append(f'history <object {oid[:12]}:0>: {error}')
                    continue
                # A message carries no reviewed exception: exceptions bind to a file path, and a
                # commit or tag message is published prose nobody has reviewed yet.
                scan_lines(f'{kind} {oid[:12]}', text, {}, {}, counters, findings, CHAT_PATTERNS)
                name = f'message-{len(messages):06d}.txt'
                (stage / name).write_bytes(text.encode('utf-8'))
                messages[name] = f'{kind} {oid[:12]}'
            if oids:
                # The secret rules cover messages as they cover every other published text: one
                # gitleaks pass over the staged messages, failing closed if gitleaks cannot run.
                secrets, error = secret_findings(stage, root / '.gitleaks.toml', scan_skipped=True)
                if error:
                    findings.append(error)
                for rel, line in secrets:
                    findings.append(f'secret {messages.get(rel, "message")}:{line}')
        summary_extra = f'{checked} message objects'
    elif args.added_from_stdin:
        mode = 'added'
        commits = [line.strip() for line in sys.stdin.read().splitlines() if line.strip()]
        if not commits:
            print('Public scan (added): 0 findings (none); 0 commits, 0 whole trees, 0 blobs, 0 tree snapshots')
            return 0
        blobs, whole, error = collect_added_blobs(root, commits)
        if not error and whole:
            full, error = collect_blobs(root, whole)
            for key, publishing in full.items():
                blobs.setdefault(key, []).extend(publishing)
        if error:
            findings.append(f'history <scan>:0: {error}')
            blobs = {}
        files, snapshot_count, flagged = scan_blobs(root, blobs, allowed, path_allowed, counters, findings)
        for finding in findings:
            kind, _, position = finding.partition(' ')
            if kind in ('digit', 'user-path', 'chat-record', 'non-text', 'secret'):
                flagged.add(position.rsplit(':', 1)[0])
        context = history_context(blobs, flagged) if blobs else []
        summary_extra = f'{len(commits)} commits, {len(whole)} whole trees, {len(blobs)} blobs, {snapshot_count} tree snapshots'
    elif args.revs_from_stdin:
        mode = 'history'
        revs = [line.strip() for line in sys.stdin.read().splitlines() if line.strip()]
        blobs = {}
        if not revs:
            print('Public scan (history): 0 findings (none); 0 revisions, 0 blobs, 0 tree snapshots; 0 files')
            return 0
        blobs, error = collect_blobs(root, revs)
        if error:
            findings.append(f'history <scan>:0: {error}')
        files, snapshot_count, flagged = scan_blobs(root, blobs, allowed, path_allowed, counters, findings)
        blob_count = len(blobs)
        for finding in findings:
            kind, _, position = finding.partition(' ')
            if kind in ('digit', 'user-path', 'chat-record', 'non-text', 'secret'):
                flagged.add(position.rsplit(':', 1)[0])
        context = history_context(blobs, flagged) if blobs else []
        summary_extra = f'{len(revs)} revisions, {blob_count} blobs, {snapshot_count} tree snapshots'
    else:
        files, secrets = scan_tree(root, root / '.gitleaks.toml', allowed, path_allowed, counters, findings)
        for rel, line in secrets:
            findings.append(f'secret {rel}:{line}')

    if not summary_extra:
        summary_extra = f'{files} files'
    for finding in findings:
        print(finding)
    for line in context:
        print(line)
    kinds = {}
    for finding in findings:
        kinds[finding.split(' ', 1)[0]] = kinds.get(finding.split(' ', 1)[0], 0) + 1
    summary = '; '.join(f'{kind}={count}' for kind, count in sorted(kinds.items())) or 'none'
    print(f'Public scan ({mode}): {len(findings)} findings ({summary}); '
          f'{counters["reviewed"]} reviewed digit runs and {counters["paths reviewed"]} reviewed '
          f'account-home lines; {summary_extra}')
    if MISSING_SCANNER in findings:
        return 3      # no secret scanner: a setup problem, not a statement about the text
    return bool(findings)


if __name__ == '__main__':
    sys.exit(main())

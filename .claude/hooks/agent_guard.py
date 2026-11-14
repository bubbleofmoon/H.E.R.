#!/usr/bin/env python3
"""PreToolUse guard for worker subagents (.claude/agents/*.md), run by agent-guard.sh <role>.

Reads the hook JSON on stdin; exit 2 with a reason on stderr blocks the call, exit 0 allows it.
Anything that can publish or move refs passes only as an exact whole command, optionally after one
`cd <dir> &&` (subagent shells start in the project folder on every call):
- a command mentioning `push` must be `git push [-u|--set-upstream] origin <branch>` (optionally after
  HER_GITLEAKS=<path to gitleaks>), from an implementer, where <branch> is the current branch of the
  effective cwd and has the form her/<n>-<slug>;
- `git commit` passes for an implementer only as one plain command on a her/<n>-<slug> branch;
- `gh` passes only as one plain read (`gh [-R x] pr|run|issue|workflow view|list|diff|checks|status|watch|download ...`),
  or for an implementer exactly `gh workflow run quick.yml --ref <cwd branch> -f target=<x> [-f filter=<y>] [-f os=linux|windows]`.
scripts/handoff.sh|ps1 is the coordinator's alone.
A worker's shell is not a sandbox: this stops mistakes and plain attempts; server-side rules on main are the boundary.
Tests: scripts/tests/test_agent_guard.py.
"""
import json
import os
import re
import shlex
import subprocess
import sys

ROLES = ('implementer', 'reviewer', 'gate')
TICKET_BRANCH = re.compile(r'her/[0-9]+-[A-Za-z0-9._-]+')
OPERATOR_CHARS = '();<>|&`'
UNSAFE = re.compile(r'[\n\r`$]')
GH_GROUPS = {'pr', 'run', 'issue', 'workflow'}
GH_READ_VERBS = {'view', 'list', 'diff', 'checks', 'status', 'watch', 'download'}
REVIEWER_GIT_WRITES = {'commit', 'merge', 'rebase', 'reset', 'checkout', 'switch', 'stash', 'restore',
                       'cherry-pick', 'revert', 'worktree', 'clean', 'tag', 'am', 'apply'}
REVIEWER_FILE_WRITES = {'cargo', 'rm', 'mv', 'cp', 'chmod', 'chattr', 'truncate'}
QUICK_INPUTS = {'target': re.compile(r'[A-Za-z0-9_]+(,[A-Za-z0-9_]+)*'),
                'filter': re.compile(r'[A-Za-z0-9_][A-Za-z0-9_:.-]*'),
                'os': re.compile(r'linux|windows')}
GITLEAKS_PREFIX = re.compile(r'HER_GITLEAKS=[A-Za-z0-9_./:-]*gitleaks(\.exe)?')
COMMIT_FLAGS = {'-a', '--all', '-q', '--quiet', '-s', '--signoff'}
PUSH_WORD = re.compile(r'\bpush\b')
COMMIT_WORD = re.compile(r'\bcommit\b')
GIT_WORD = re.compile(r'\bgit\b')
GH_WORD = re.compile(r'(?<![\w.-])gh(?![\w.-])')


class Deny(Exception):
    pass


def split(text):
    lexer = shlex.shlex(text, posix=True, punctuation_chars=OPERATOR_CHARS)
    lexer.whitespace_split = True
    lexer.commenters = ''  # bash starts a comment only at a word start; shlex would hide `x#;cmd`
    return list(lexer)


def is_operator(word):
    return not word.strip(OPERATOR_CHARS)


def flatten(text, depth=0):
    """Every word of the command, dequoted, with nested command strings (bash -c "...") split too."""
    try:
        found = split(text)
    except ValueError:
        if depth == 0:
            raise
        return text.split()
    out = []
    for word in found:
        out.append(word)
        if depth < 3 and word != text and not is_operator(word) and re.search(r'[\s();<>|&`]', word):
            out.extend(flatten(word, depth + 1))
    return out


def base(word):
    name = word.replace('\\', '/').rsplit('/', 1)[-1].lower()
    return name[:-4] if name.endswith('.exe') else name


def branch_of(path):
    if not path:
        return None
    try:
        result = subprocess.run(['git', '-C', path, 'rev-parse', '--abbrev-ref', 'HEAD'],
                                capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        return None
    return result.stdout.strip() if result.returncode == 0 else None


def simple(text, cwd):
    """(words, cwd) of one plain command, optionally after one `cd <dir> &&`; None otherwise."""
    if UNSAFE.search(text):
        return None
    try:
        found = split(text)
    except ValueError:
        return None
    if len(found) > 3 and found[0] == 'cd' and found[2] == '&&' and not is_operator(found[1]):
        cwd = os.path.join(cwd or '', os.path.expanduser(found[1]))
        found = found[3:]
    if not found or any(is_operator(word) for word in found):
        return None
    return found, cwd


def without_repo(found):
    out, skip = [], False
    for word in found:
        if skip:
            skip = False
        elif word in ('-R', '--repo'):
            skip = True
        elif not word.startswith(('--repo=', '-R')):
            out.append(word)
    return out


def push_allowed(found, branch):
    if found and GITLEAKS_PREFIX.fullmatch(found[0]):
        found = found[1:]
    if found[:2] != ['git', 'push']:
        return False
    rest = found[2:]
    if rest[:1] in (['-u'], ['--set-upstream']):
        rest = rest[1:]
    return rest == ['origin', branch]


def commit_allowed(found):
    if found[:2] != ['git', 'commit']:
        return False
    rest, i = found[2:], 0
    while i < len(rest):
        word = rest[i]
        if word == '--':
            return True
        if word in ('-m', '--message', '-F', '--file') or re.fullmatch(r'-[aqs]*m', word):
            if i + 1 >= len(rest):
                return False
            i += 2
            continue
        if not (word in COMMIT_FLAGS or re.fullmatch(r'-[aqs]+', word) or re.fullmatch(r'-[aqs]*m.+', word)
                or word.startswith(('--message=', '--file=')) or not word.startswith('-')):
            return False
        i += 1
    return True


def quick_allowed(found, branch):
    rest = without_repo(found)
    if rest[:4] != ['gh', 'workflow', 'run', 'quick.yml']:
        return False
    items, refs, inputs, i = rest[4:], [], {}, 0
    while i < len(items):
        if i + 1 >= len(items):
            return False
        flag, value = items[i], items[i + 1]
        i += 2
        if flag == '--ref':
            refs.append(value)
        elif flag in ('-f', '--raw-field'):
            key, sep, val = value.partition('=')
            if not sep or key not in QUICK_INPUTS or key in inputs or not QUICK_INPUTS[key].fullmatch(val):
                return False
            inputs[key] = val
        else:
            return False
    return refs == [branch] and 'target' in inputs


def gh_read(found, role):
    rest = without_repo(found)
    verbs = GH_READ_VERBS - {'download'} if role == 'reviewer' else GH_READ_VERBS
    return len(rest) >= 3 and rest[0] == 'gh' and rest[1] in GH_GROUPS and rest[2] in verbs


def check_bash(role, command, hook_cwd):
    if "$'" in command or re.search(r'\\\r?\n', command):
        raise Deny('no ANSI-C quoting ($\'...\') and no backslash-newline continuations; write one plain command.')
    plain = simple(command, hook_cwd)
    words, cwd = plain if plain else (None, hook_cwd)
    branch = branch_of(cwd)
    on_ticket = bool(branch and TICKET_BRANCH.fullmatch(branch))
    # Exact forms first: each is one plain command, so its message or values cannot run anything else.
    if role == 'implementer' and words and on_ticket and (
            push_allowed(words, branch) or commit_allowed(words) or quick_allowed(words, branch)):
        return
    try:
        flat = flatten(command)
    except ValueError:
        raise Deny('cannot parse this command; write it as one plain command.')
    text = command + '\n' + ' '.join(flat)
    names = {base(word) for word in flat}
    wordset = set(flat)
    has_git = 'git' in names or bool(GIT_WORD.search(text))
    if '--no-verify' in text or 'hookspath' in text.lower():
        raise Deny('hooks stay on: no --no-verify and no core.hooksPath.')
    if 'gh-public' in text:
        raise Deny('only the coordinator posts (scripts/gh-public); put it in your report.')
    if re.search(r'handoff\.(?:sh|ps1)', text):
        raise Deny('only the coordinator hands off (scripts/handoff); put it in your report. To read the script, use the Read tool.')
    if 'build-slot.lock' in text:
        raise Deny('use the build slot (scripts/cargo-slot.ps1 on Windows, scripts/cargo-slot.sh on Linux); never create or remove its lock by hand.')
    if re.search(r'(>|rm |mv |cp .* |sed -i|tee ).*/root/js-baseline-01|/root/her/backups/js-rust-baseline-01', command) \
            and not re.match(r'\s*(cat|ls|sha256sum|grep|head|tail|node|diff|find)\s', command):
        raise Deny('the frozen JS baseline is read-only.')
    if has_git:
        if 'alias.' in text:
            raise Deny('git aliases are not allowed.')
        if wordset & {'update-ref', 'symbolic-ref'} \
                or (wordset & {'switch', 'checkout', 'branch'} and wordset & {'main', 'refs/heads/main'}) \
                or ('fetch' in wordset and any(re.search(r':(refs/heads/)?main$', word) for word in flat)):
            raise Deny('only the coordinator moves main.')
        if 'reset' in wordset and branch == 'main':
            raise Deny('never reset main; work in your ticket worktree.')
        if '-C' in wordset and wordset & {'commit', 'push'}:
            raise Deny('cd into your worktree (cd <worktree> && git ...); no git -C with commit or push.')
    if PUSH_WORD.search(text):
        raise Deny('workers push only their own ticket branch, as the whole command: [cd <worktree> &&] '
                   '[HER_GITLEAKS=<gitleaks>] git push [-u] origin her/<n>-<slug>; reviews and gates never push. '
                   'To search text for this word, use the Grep tool.')
    if has_git and COMMIT_WORD.search(text):
        raise Deny('workers commit only on their her/<n>-<slug> branch, as one plain command: [cd <worktree> &&] '
                   'git commit [-a] -m <msg> | -F <file> (no chaining, no $); reviews and gates never commit.')
    if ('gh' in names or GH_WORD.search(text)) and not (words and gh_read(words, role)):
        raise Deny('gh is read-only for workers: one plain `gh [-R repo] pr|run|issue|workflow '
                   'view|list|diff|checks|status|watch|download ...`; implementers may also run exactly '
                   '`gh workflow run quick.yml --ref <your branch> -f target=<name> [-f filter=<test>] [-f os=linux|windows]`.')
    if role == 'implementer' and re.search(r'(^|[;&|\s])git\s+(merge|rebase|cherry-pick|revert|am|tag)(\s|$)', command):
        raise Deny('workers never merge, rebase or tag; the coordinator merges through a pull request.')
    if role == 'reviewer':
        if ('git' in names and wordset & REVIEWER_GIT_WRITES) or names & REVIEWER_FILE_WRITES:
            raise Deny('reviews are read-only (no git writes, builds or file changes); read code and git history only.')
        if re.search(r'(^|[^0-9&>])>{1,2}\s*[^&\s]', command) and not re.search(r'>{1,2}\s*/dev/null', command):
            raise Deny('reviews are read-only; no output redirection to files.')


def check(role, data):
    if role not in ROLES:
        raise Deny('unknown role.')
    tool = data.get('tool_name') or ''
    tool_input = data.get('tool_input') or {}
    path = (tool_input.get('file_path') or tool_input.get('notebook_path') or '').replace('\\', '/')
    if path.startswith('/root/js-baseline-01/') or '/backups/js-rust-baseline-01/' in path:
        raise Deny('the frozen JS baseline is read-only.')
    if tool == 'Bash':
        check_bash(role, tool_input.get('command') or '', data.get('cwd') or '')
    if tool in ('Edit', 'Write', 'NotebookEdit'):
        if role == 'reviewer':
            raise Deny('reviews are read-only; put findings in your report.')
        if role == 'gate' and re.search(r'/her/src/|/her/tests/|/her-memory/src/|/Cargo\.toml$', path):
            raise Deny('merge gates do not change product code or tests; classify the failure and report it.')


def main():
    role = sys.argv[1] if len(sys.argv) > 1 else 'implementer'
    try:
        check(role, json.loads(sys.stdin.buffer.read().decode('utf-8')))
    except Deny as reason:
        print(f'agent-guard ({role}): {reason}', file=sys.stderr)
        return 2
    except Exception:  # an unreadable call is refused, never allowed
        print(f'agent-guard ({role}): cannot check this call; refusing.', file=sys.stderr)
        return 2
    return 0


if __name__ == '__main__':
    sys.exit(main())

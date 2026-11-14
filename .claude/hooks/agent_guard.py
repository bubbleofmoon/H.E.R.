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
- a planner writes only its plan file and files in its scratch export (no link, junction or reparse point on the way,
  no .git, stream or 8.3 part); planners and reviewers run git only with read verbs and nothing at a command position
  that builds, formats or writes files;
- Codex starts only as the implementer's self-review through /root/her/scripts/codex-task.sh or
  C:/her/scripts/codex-task.ps1 with gpt-6-astra, high and read-only.
Command words are matched at command positions: after operators, unquoted newlines, assignments, shell keywords
(if then else elif do while until ! { eval builtin coproc function), wrappers and find -exec, and inside quoted strings.
So `cat scripts/codex-task.sh` stays allowed, and a quoted multi-word pattern that names a tool is refused for
read-only roles (use the Grep tool).
A worker's shell is not a sandbox: this stops mistakes and plain attempts. python runs unrestricted for every role,
so any agent can still write files or run code through it; server-side rules on main are the boundary.
Tests: scripts/tests/test_agent_guard.py.
"""
import json
import os
import re
import shlex
import subprocess
import sys

ROLES = ('implementer', 'reviewer', 'gate', 'planner')
READ_ONLY_ROLES = ('reviewer', 'planner')
TICKET_BRANCH = re.compile(r'her/[0-9]+-[A-Za-z0-9._-]+')
OPERATOR_CHARS = '();<>|&`'
UNSAFE = re.compile(r'[\n\r`$]')
GH_GROUPS = {'pr', 'run', 'issue', 'workflow'}
GH_READ_VERBS = {'view', 'list', 'diff', 'checks', 'status', 'watch', 'download'}
GIT_READ_VERBS = {'log', 'diff', 'show', 'status', 'blame', 'grep', 'ls-files', 'ls-tree', 'rev-parse', 'rev-list', 'cat-file',
                  'merge-base', 'describe', 'for-each-ref', 'show-ref', 'shortlog', 'diff-tree', 'name-rev'}
GIT_READ_DENIED_OPTIONS = re.compile(r'(?:--out|--ext|--open|-O).*', re.S)
BRANCH_LIST_FLAGS = {'-a', '-r', '-v', '-vv', '--all', '--remotes', '--verbose'}
REVIEWER_FILE_WRITES = {'cargo', 'rm', 'rmdir', 'unlink', 'mv', 'cp', 'chmod', 'chattr', 'truncate', 'tee', 'ln', 'touch', 'mkdir',
                        'dd', 'install', 'patch', 'tar', 'unzip', 'curl', 'wget', 'rsync', 'rustc', 'rustfmt', 'cargo-fmt',
                        'make', 'npm', 'npx', 'strace', 'cargo-slot.sh', 'cargo-slot.ps1'}
SED_IN_PLACE = re.compile(r'-[A-Za-z]*i.*|--in-place.*', re.S)
FIND_WRITES = {'-delete', '-fprint', '-fprint0', '-fprintf', '-fls'}
QUICK_INPUTS = {'target': re.compile(r'[A-Za-z0-9_]+(,[A-Za-z0-9_]+)*'),
                'filter': re.compile(r'[A-Za-z0-9_][A-Za-z0-9_:.-]*'),
                'os': re.compile(r'linux|windows')}
GITLEAKS_PREFIX = re.compile(r'HER_GITLEAKS=[A-Za-z0-9_./:-]*gitleaks(\.exe)?')
COMMIT_FLAGS = {'-a', '--all', '-q', '--quiet', '-s', '--signoff'}
PUSH_WORD = re.compile(r'\bpush\b')
COMMIT_WORD = re.compile(r'\bcommit\b')
GIT_WORD = re.compile(r'\bgit\b')
GH_WORD = re.compile(r'(?<![\w.-])gh(?![\w.-])')
PLANNER_WRITE = re.compile(r'(?:/root/orch/her|[Cc]:/her-orch)/[1-9][0-9]*/(?:plan\.md|scratch/.+)')
REPARSE_POINT = 0x400
CODEX_NAMES = {'codex', 'codex.cmd', 'codex.ps1', 'codex.js', 'codex-task.sh', 'codex-task.ps1'}
CODEX_SH = '/root/her/scripts/codex-task.sh'
CODEX_PS1 = 'c:/her/scripts/codex-task.ps1'
CODEX_NAME = re.compile(r'[A-Za-z0-9][A-Za-z0-9_-]*')
CODEX_MINUTES = re.compile(r'[1-9][0-9]{0,2}')
PS_HOST_OPTIONS = {'-noprofile', '-noninteractive', '-nologo', '-executionpolicy', 'bypass'}
CODEX_PS_PARAMS = {'-name', '-workdir', '-promptfile', '-model', '-effort', '-sandbox', '-timeoutminutes'}
WRAPPERS = {'bash', 'sh', 'env', 'timeout', 'nice', 'nohup', 'exec', 'command', 'xargs', 'sudo', 'time', 'stdbuf',
            'npx', 'npm', 'pnpm', 'yarn', 'bunx', 'node', 'pwsh', 'powershell', 'cmd', 'busybox',
            'setsid', 'ionice', 'taskset', 'flock', 'strace', 'script', 'doas', 'su', 'runuser', 'winpty', 'start', 'call'}
FIND_EXEC = {'-exec', '-execdir', '-ok', '-okdir'}
ASSIGNMENT = re.compile(r'[A-Za-z_][A-Za-z0-9_]*=.*', re.S)
SEGMENT_BREAKS = OPERATOR_CHARS + '\n'
KEYWORDS = {'if', 'then', 'else', 'elif', 'do', 'while', 'until', '!', '{', 'eval', 'builtin', 'coproc', 'function'}


class Deny(Exception):
    pass


def split(text):
    lexer = shlex.shlex(text, posix=True, punctuation_chars=OPERATOR_CHARS)
    lexer.whitespace_split = True
    lexer.commenters = ''  # bash starts a comment only at a word start; shlex would hide `x#;cmd`
    return list(lexer)


def is_operator(word):
    return not word.strip(OPERATOR_CHARS)


def split_commands(text):
    """Like split(), but an unquoted newline is a token of its own: it ends a command as ';' does."""
    lexer = shlex.shlex(text, posix=True, punctuation_chars=SEGMENT_BREAKS)
    lexer.whitespace = ' \t\r'
    lexer.whitespace_split = True
    lexer.commenters = ''
    return list(lexer)


def is_break(word):
    # An empty quoted word is an argument, never a break: a break there would drop the arguments after it.
    return bool(word) and not word.strip(SEGMENT_BREAKS)


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


def segments(text, depth=0):
    """Every command segment as (words from a command position on, plain).

    A command position is:
    - the start of the text;
    - the word after each operator or unquoted newline;
    - the word after leading VAR=value assignments and after the shell keywords in KEYWORDS
      (if then else elif do while until ! { eval builtin coproc function);
    - a standalone `{` after other words (`function f { ...; }`);
    - the word after find -exec/-execdir/-ok/-okdir;
    - every word after a wrapper (bash, env, timeout, xargs, setsid, ...) that is itself at a command position.
    Nested command strings (bash -c "...", eval "...", a quoted pattern) are split too.
    `plain` is False for a segment reached through an assignment, a wrapper, find -exec or a nested string.
    Over-counting a word as a command fails closed.
    """
    try:
        found = split_commands(text)
    except ValueError:
        if depth == 0:
            raise
        found = text.split()
    out, raw, current, assigned = [], [], [], False
    for word in found:
        if is_break(word):
            if current:
                raw.append((current, assigned))
            current, assigned = [], False
            continue
        if word == '{' and current:
            raw.append((current, assigned))
            current, assigned = [], False
            continue
        if depth < 3 and word != text and re.search(r'[\s();<>|&`]', word):
            out.extend((words, False) for words, _ in segments(word, depth + 1))
        if not current and ASSIGNMENT.fullmatch(word):
            assigned = True
            continue
        if not current and word in KEYWORDS:
            continue
        current.append(word)
    if current:
        raw.append((current, assigned))
    for seg, assigned in raw:
        out.append((seg, not assigned))
        starts = {0}
        for i, word in enumerate(seg):
            if word in FIND_EXEC:
                starts.add(i + 1)
            if i in starts and base(word) in WRAPPERS:
                starts.update(range(i + 1, len(seg)))
        out.extend((seg[i:], False) for i in sorted(starts) if 0 < i < len(seg))
    return out


def base(word):
    name = word.replace('\\', '/').rsplit('/', 1)[-1].lower()
    return name[:-4] if name.endswith('.exe') else name


def git_read_only(seg):
    """A git segment that only reads.

    Accepted: git [-C <dir>] [--no-pager|-P] followed by a read verb; git branch [--list]; git remote [-v];
    git worktree list; git --version. Refused: any argument starting with --out, --ext, --open or -O, and a grep
    with a bundled -O.
    """
    if seg[1:] == ['--version']:
        return True
    i = 1
    while i < len(seg) and seg[i].startswith('-'):
        if seg[i] == '-C' and i + 1 < len(seg):
            i += 2
        elif seg[i] in ('--no-pager', '-P'):
            i += 1
        else:
            return False                      # -c, --git-dir, --work-tree, --exec-path, ...
    if i >= len(seg):
        return False
    verb, args = seg[i], seg[i + 1:]
    if any(GIT_READ_DENIED_OPTIONS.fullmatch(a) for a in args):
        return False
    if verb == 'grep' and any(re.fullmatch(r'-[A-Za-z]*O.*', a, re.S) for a in args):
        return False
    if verb == 'worktree':
        return args[:1] == ['list'] and all(a in ('--porcelain', '-v', '--verbose') for a in args[1:])
    if verb == 'branch':
        if '--list' in args:
            return all(a == '--list' or a in BRANCH_LIST_FLAGS or not a.startswith('-') for a in args)
        return all(a in BRANCH_LIST_FLAGS for a in args)   # a bare name would create a branch
    if verb == 'remote':
        return args in ([], ['-v'], ['--verbose'])
    return verb in GIT_READ_VERBS


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


def codex_allowed(found):
    """The implementer's read-only Codex self-review through the integration checkout's codex-task script."""
    if found[:1] == ['bash']:
        found = found[1:]
    if found and found[0] == CODEX_SH:
        return (len(found) in (7, 8) and bool(CODEX_NAME.fullmatch(found[1]))
                and found[4:7] == ['gpt-6-astra', 'high', 'read-only']
                and (len(found) == 7 or bool(CODEX_MINUTES.fullmatch(found[7]))))
    if found and base(found[0]) in ('powershell', 'pwsh'):
        i = 1
        while i < len(found) and found[i].lower() in PS_HOST_OPTIONS:   # host options only before -File
            i += 1
        rest = found[i:]
        if len(rest) < 2 or rest[0].lower() != '-file' or rest[1].replace('\\', '/').lower() != CODEX_PS1:
            return False
        params, pairs = rest[2:], {}
        if len(params) % 2:
            return False
        for key, value in zip(params[0::2], params[1::2]):
            key = key.lower()
            if key not in CODEX_PS_PARAMS or key in pairs:
                return False
            pairs[key] = value
        return (CODEX_PS_PARAMS - {'-timeoutminutes'} <= set(pairs)
                and pairs['-model'] == 'gpt-6-astra' and pairs['-effort'] == 'high' and pairs['-sandbox'] == 'read-only'
                and bool(CODEX_NAME.fullmatch(pairs['-name']))
                and ('-timeoutminutes' not in pairs or bool(CODEX_MINUTES.fullmatch(pairs['-timeoutminutes']))))
    return False


def gh_read(found, role):
    rest = without_repo(found)
    verbs = GH_READ_VERBS - {'download'} if role in READ_ONLY_ROLES else GH_READ_VERBS
    return len(rest) >= 3 and rest[0] == 'gh' and rest[1] in GH_GROUPS and rest[2] in verbs


def is_link(path):
    """A symlink, a junction or any other reparse point; a stat error other than not-found counts as one."""
    if os.path.islink(path):
        return True
    isjunction = getattr(os.path, 'isjunction', None)
    if isjunction and isjunction(path):
        return True
    try:
        attributes = getattr(os.lstat(path), 'st_file_attributes', 0)
    except FileNotFoundError:
        return False
    except OSError:
        return True
    return bool(attributes & REPARSE_POINT)


def planner_may_write(path):
    """The plan file or a file in the scratch export, with every part checked the way NTFS reads it."""
    if not PLANNER_WRITE.fullmatch(path):
        return False
    head, rest = ('/', path[1:]) if path.startswith('/') else (path[:3], path[3:])
    parts = rest.split('/')
    for part in parts:
        folded = part.lower().rstrip('. ')
        if (not folded or folded == '.git' or ':' in part or re.fullmatch(r'git~[0-9]+', folded)
                or any(ord(c) < 32 for c in part)):
            return False                      # '', '.', '..', .git in any spelling, streams, 8.3 names, controls
    return not any(is_link(head + '/'.join(parts[:i + 1])) for i in range(len(parts)))


def check_bash(role, command, hook_cwd):
    if "$'" in command or re.search(r'\\\r?\n', command):
        raise Deny('no ANSI-C quoting ($\'...\') and no backslash-newline continuations; write one plain command.')
    plain = simple(command, hook_cwd)
    words, cwd = plain if plain else (None, hook_cwd)
    branch = branch_of(cwd)
    on_ticket = bool(branch and TICKET_BRANCH.fullmatch(branch))
    # Exact forms first: each is one plain command, so its message or values cannot run anything else.
    if role == 'implementer' and words and on_ticket and (
            push_allowed(words, branch) or commit_allowed(words) or quick_allowed(words, branch)
            or codex_allowed(words)):
        return
    try:
        flat = flatten(command)
    except ValueError:
        raise Deny('cannot parse this command; write it as one plain command.')
    text = command + '\n' + ' '.join(flat)
    segs = segments(command)
    commands = {base(words[0]) for words, _ in segs}
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
    if commands & CODEX_NAMES:
        raise Deny('Codex runs only as the implementer\'s read-only self-review: [cd <worktree> &&] /root/her/scripts/codex-task.sh '
                   '<name> <worktree> <prompt> gpt-6-astra high read-only [minutes] (Windows: powershell -NoProfile -File '
                   'C:/her/scripts/codex-task.ps1 -Name <name> -WorkDir <worktree> -PromptFile <prompt> -Model gpt-6-astra '
                   '-Effort high -Sandbox read-only). To read the script, use the Read tool.')
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
    if role in READ_ONLY_ROLES:
        for words, plain in segs:
            name = base(words[0])
            if name == 'git' and not (plain and git_read_only(words)):
                raise Deny('reviews and plans read git only, as a plain command (no VAR=value, env or wrapper before it): '
                           'git [-C <dir>] [--no-pager] log|diff|show|status|blame|grep|ls-files|ls-tree|'
                           'rev-parse|rev-list|cat-file|merge-base|describe|for-each-ref|show-ref|shortlog|diff-tree|'
                           'name-rev, git branch [--list], git remote -v, git worktree list or git --version.')
            if (name == 'sed' and any(SED_IN_PLACE.fullmatch(a) for a in words[1:])) \
                    or (name == 'find' and FIND_WRITES & set(words[1:])):
                raise Deny('reviews and plans change no files (no sed -i, no find -delete or -fprint); '
                           'use the Read or Grep tool.')
        if commands & REVIEWER_FILE_WRITES:
            raise Deny('reviews and plans are read-only (no builds, formatting, downloads or file changes); read code and '
                       'git history only. A quoted pattern that names a tool counts as a command here: search with the '
                       'Grep tool, read with the Read tool.')
        if re.search(r'(^|[^0-9&>])>{1,2}\s*[^&\s]', command) and not re.search(r'>{1,2}\s*/dev/null', command):
            raise Deny('reviews and plans are read-only; no output redirection to files.')


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
        if role == 'planner' and (tool == 'NotebookEdit' or not planner_may_write(path)):
            raise Deny('planners write only their plan file (/root/orch/her/<n>/plan.md, C:/her-orch/<n>/plan.md) '
                       'and files in their scratch export (/root/orch/her/<n>/scratch/, C:/her-orch/<n>/scratch/).')
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

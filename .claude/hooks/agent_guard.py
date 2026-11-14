#!/usr/bin/env python3
"""PreToolUse guard for worker subagents (.claude/agents/*.md), run by agent-guard.sh <role>.

Reads the hook JSON on stdin; exit 2 with a reason on stderr blocks the call, exit 0 allows it.

Reviewers and planners (read-only roles) get an allow-list. A Bash call is one pipeline joined only by `|`. Each
command is an allowed reader by its bare name: git with the plain read verbs, gh reads, ls, cat, head, tail, wc, grep,
rg, find with tests and -print/-print0, stat, file, diff, cmp, sha256sum, sort, uniq, cut, tr, jq, date, pwd, echo,
basename, dirname, realpath. Options of these readers that write or run something are refused. Arguments
are plain or quoted; only 2>/dev/null and 2>&1 redirect. Nothing else runs: no `;`, `&`, newline, `$`, backtick,
substitution, glob, brace or backslash outside quotes. A planner writes only its plan file and files in its scratch
export (no link, junction or reparse point on the way, no .git, stream or 8.3 part).

Implementers and gates get deny rules over the parsed command, with command words matched at command positions:
after operators, unquoted newlines, assignments, keywords, redirection operands, `{`, wrappers (including eval,
builtin, coproc, source and .) and find -exec, and inside nested strings; comments are dropped and quoted operators
stay inside their word. Publishing and moving refs pass only as exact whole commands, optionally after one
`cd <dir> &&`:
- `git push [-u|--set-upstream] origin <branch>` (optionally after HER_GITLEAKS=<path to gitleaks>), from an
  implementer, where <branch> is the current her/<n>-<slug> branch of the effective cwd;
- `git commit` for an implementer only as one plain command on a her/<n>-<slug> branch;
- `gh` only as one plain read (`gh [-R x] pr|run|issue|workflow view|list|diff|checks|status|download ...`, never watch
  or --watch, or `gh api rate_limit [--jq <x>]`), or for an implementer exactly
  `gh workflow run quick.yml --ref <cwd branch> -f target=<x> [-f filter=<y>] [-f os=linux|windows]`;
- Codex only as the implementer's self-review through /root/her/scripts/codex-task.sh or C:/her/scripts/codex-task.ps1
  with gpt-6-astra, high and read-only, its own worktree as the workdir and
  <orch>/<n>/review/codex-self-<k>.md (its branch's ticket, not a link) as the prompt.
scripts/gh-public and scripts/handoff are the coordinator's alone; they, and codex, are also refused by name anywhere in
a command that has a wrapper, a $ or a backtick.
A worker's shell is not a sandbox. For implementers and gates this stops mistakes and plain attempts only; the
containment boundary is #85. python runs unrestricted for them.
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
GH_READ_VERBS = {'view', 'list', 'diff', 'checks', 'status', 'download'}
GIT_READ_VERBS = {'log', 'diff', 'show', 'status', 'blame', 'grep', 'ls-files', 'ls-tree', 'rev-parse', 'rev-list', 'cat-file',
                  'merge-base', 'describe', 'for-each-ref', 'show-ref', 'shortlog', 'diff-tree', 'name-rev'}
GIT_READ_DENIED_OPTIONS = re.compile(r'(?:--out|--ext|--op|-O).*', re.S)
BRANCH_LIST_FLAGS = {'-a', '-r', '-v', '-vv', '--all', '--remotes', '--verbose'}
READ_ONLY_COMMANDS = {'git', 'gh', 'ls', 'cat', 'head', 'tail', 'wc', 'grep', 'rg', 'find', 'stat', 'file', 'diff', 'cmp',
                      'sha256sum', 'sort', 'uniq', 'cut', 'tr', 'jq', 'date', 'pwd', 'echo', 'basename',
                      'dirname', 'realpath'}
READ_ONLY_BARE = re.compile(r"[A-Za-z0-9_@%+=:,./~^'\"-]+")   # an unquoted word; quoted text arrives as letters
READ_ONLY_STDERR = ('2>/dev/null', '2>&1')
READ_ONLY_REFUSAL = ('read-only roles run only simple read commands; use the Read, Grep and Glob tools. Allowed: one '
                     'pipeline joined by | of git reads ([-C <dir>] [--no-pager] log|diff|show|status|blame|grep|ls-files|'
                     'ls-tree|rev-parse|rev-list|cat-file|merge-base|describe|for-each-ref|show-ref|shortlog|diff-tree|'
                     'name-rev, branch [--list], remote -v, worktree list, --version), gh reads, ls, cat, head, tail, wc, '
                     'grep, rg, find (tests and -print/-print0), stat, file, diff, cmp, sha256sum, sort, uniq, cut, tr, jq, '
                     'date, pwd, echo, basename, dirname and realpath; arguments plain or quoted (no $ or '
                     'backtick in double quotes, no unquoted * ? [ ] { } \\); only 2>/dev/null or 2>&1.')
FIND_TESTS = {'-name', '-iname', '-path', '-ipath', '-wholename', '-iwholename', '-lname', '-ilname', '-regex', '-iregex',
              '-regextype', '-type', '-xtype', '-size', '-empty', '-newer', '-mtime', '-mmin', '-ctime', '-cmin', '-atime',
              '-amin', '-used', '-perm', '-user', '-group', '-uid', '-gid', '-nouser', '-nogroup', '-links', '-inum',
              '-samefile', '-readable', '-writable', '-executable', '-maxdepth', '-mindepth', '-depth', '-xdev', '-mount',
              '-noleaf', '-daystart', '-follow', '-ignore_readdir_race', '-noignore_readdir_race', '-not', '-and', '-or',
              '-a', '-o', '-true', '-false', '-prune', '-print', '-print0', '-L', '-H', '-P'}
CODEX_PROMPT = re.compile(r'(?:/root/orch/her|[Cc]:/her-orch)/([1-9][0-9]*)/review/codex-self-[1-9][0-9]*\.md')
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
            'setsid', 'ionice', 'taskset', 'flock', 'strace', 'script', 'doas', 'su', 'runuser', 'winpty', 'start', 'call',
            'eval', 'builtin', 'coproc', 'source', '.'}
FIND_EXEC = {'-exec', '-execdir', '-ok', '-okdir'}
ASSIGNMENT = re.compile(r'[A-Za-z_][A-Za-z0-9_]*=.*', re.S)
SEGMENT_BREAKS = OPERATOR_CHARS + '\n'
KEYWORDS = {'if', 'then', 'else', 'elif', 'do', 'while', 'until', '!', '{', 'function'}
HIDDEN = {c: chr(0xE000 + k) for k, c in enumerate(SEGMENT_BREAKS)}
SHOWN = {v: k for k, v in HIDDEN.items()}
REDIRECT = re.compile(r'&>>?|>>|>&|>\||>|<<<|<<|<&|<>|<')
REDIRECT_AFTER_BREAK = re.compile(r'([;|&()`\n]+?)(&>>?|>>|>&|>\||>|<<<|<<|<&|<>|<)')
STDIN_REDIRECTS = ('<', '<<<', '<>')
NAMED_DESCRIPTOR = re.compile(r'\{[A-Za-z_][A-Za-z0-9_]*\}')   # `{fd}>file` opens a named descriptor, as `2>file` does


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


def protect(text):
    """Prepare a command for segments(): hide quoted or escaped break characters, and drop comments.

    shlex drops quotes and backslashes, after which a quoted ';' or an escaped '\\;' would look like an operator and
    cut the arguments after it out of their command's segment. shlex also knows no comments, so an apostrophe in
    `# it's` would open a quote that swallows the next line. restore() turns the hidden characters back.
    A '#' starts a comment only at the start of a word, tracked from what was emitted, not from the raw text.
    """
    out, i, quote, at_word_start = [], 0, None, True
    while i < len(text):
        c = text[i]
        if quote is None and c == '#' and at_word_start:
            end = text.find('\n', i)
            i = len(text) if end < 0 else end          # the newline stays: it still ends the command
            continue
        if quote is None and c == '\\' and i + 1 < len(text):
            following = text[i + 1]
            out.append(HIDDEN[following] if following in HIDDEN else c + following)
            at_word_start = False
            i += 2
            continue
        if quote == '"' and c == '\\' and i + 1 < len(text) and text[i + 1] in '"\\':
            out.append(c + text[i + 1])
            at_word_start = False
            i += 2
            continue
        if quote is None and c in '\'"':
            quote = c
        elif quote is not None and c == quote:
            quote = None
        elif quote is not None and c in HIDDEN:
            c = HIDDEN[c]
        at_word_start = quote is None and (c in ' \t\n' or c in OPERATOR_CHARS)
        out.append(c)
        i += 1
    return ''.join(out)


def restore(word):
    return ''.join(SHOWN.get(c, c) for c in word)


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
    """Every command segment as (words from a command position on, plain), for the implementer and gate denies.

    A command position is:
    - the start of the text, and the word after each operator or unquoted newline;
    - the word after leading VAR=value assignments and after the keywords in KEYWORDS;
    - the word after a redirection's operand (`</dev/null rm x`);
    - the word after `{`, and after find -exec/-execdir/-ok/-okdir;
    - every word after a wrapper (bash, env, eval, source, ., timeout, xargs, ...) that is itself at a command position.
    The operand of `<`, `<<<` or `<>` is a one-word segment too, because a shell can run it from stdin (`bash -s < x.sh`).
    Quoted or escaped operator characters stay inside their word, and comments are dropped. Nested strings
    (bash -c "...", a quoted pattern) are split too; one that cannot be lexed counts every word as a command.
    `plain` is False for a segment reached through an assignment, a wrapper, find -exec, `{` or a nested string.
    Over-counting a word as a command fails closed. An operator this lexer cannot read raises ValueError, at depth 0 only.
    """
    try:
        tokens = [(token, True) if is_break(token) else (restore(token), False)
                  for token in split_commands(protect(text))]
    except ValueError:
        if depth == 0:
            raise
        return [([word], False) for word in text.split()]
    out, raw, current, assigned = [], [], [], False

    def close():
        nonlocal current, assigned
        if current:
            raw.append((current, assigned))
        current, assigned = [], False

    def nested(word):
        if depth < 3 and word != text and re.search(r'[\s();<>|&`]', word):
            out.extend((words, False) for words, _ in segments(word, depth + 1))

    i = 0
    while i < len(tokens):
        word, operator = tokens[i]
        i += 1
        if operator:
            if not REDIRECT.fullmatch(word):
                joined = REDIRECT_AFTER_BREAK.fullmatch(word)
                if joined:                               # `ls;>x`: a break, then a redirection
                    close()
                    word = joined.group(2)
            if REDIRECT.fullmatch(word):
                if current and (current[-1].isdigit() or NAMED_DESCRIPTOR.fullmatch(current[-1])):
                    current.pop()                        # the descriptor of `2>file` or `{fd}>file`
                target, target_is_operator = tokens[i] if i < len(tokens) else ('', True)
                if not target_is_operator:
                    i += 1
                    nested(target)
                    if word in STDIN_REDIRECTS:
                        out.append(([target], False))
                continue
            if depth == 0 and re.search('[<>]', word) and not word.endswith('('):
                raise ValueError('unreadable operator')
            close()
            continue
        nested(word)
        if not current and ASSIGNMENT.fullmatch(word):
            assigned = True
            continue
        if not current and word in KEYWORDS:
            continue
        current.append(word)
    close()
    for seg, seg_assigned in raw:
        out.append((seg, not seg_assigned))
        starts = {0}
        for j, word in enumerate(seg):
            if word in FIND_EXEC or word == '{':
                starts.add(j + 1)
            if j in starts and base(word) in WRAPPERS:
                starts.update(range(j + 1, len(seg)))
        out.extend((seg[j:], False) for j in sorted(starts) if 0 < j < len(seg))
    return out


def base(word):
    name = word.replace('\\', '/').rsplit('/', 1)[-1].lower()
    return name[:-4] if name.endswith('.exe') else name


def effective_dir(path, cwd):
    """The directory a -C value names, with a relative value resolved against the directory before it."""
    p = path.replace('\\', '/')
    if not (p.startswith('/') or re.match(r'[A-Za-z]:/', p)) and cwd:
        p = cwd.replace('\\', '/') + '/' + p
    return os.path.normpath(p).replace('\\', '/')


def git_read_only(seg, cwd):
    """A git segment that only reads.

    Accepted: git [-C <dir>] [--no-pager|-P] followed by a read verb; git branch [--list]; git remote [-v];
    git worktree list; git --version. Refused: any argument starting with --out, --ext, --op or -O, any argument
    holding $, a -C that resolves under a ticket folder or scratch export, and a grep with a bundled -O.
    """
    if seg[1:] == ['--version']:
        return True
    if any('$' in a for a in seg[1:]):
        return False                          # a variable can carry any option
    i = 1
    directory = cwd or ''
    while i < len(seg) and seg[i].startswith('-'):
        if seg[i] == '-C' and i + 1 < len(seg):
            directory = effective_dir(seg[i + 1], directory)
            if re.match(r'(?i)(/root/orch/her/|c:/her-orch/)', directory):
                return False                  # a ticket folder or scratch export is not a repository to read here
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
    if verb == 'grep' and any(re.fullmatch(r'-[^-].*', a, re.S) and 'O' in a for a in args):
        return False                          # -O<pager> in any bundle, e.g. -1Osh
    if verb == 'worktree':
        return args[:1] == ['list'] and all(a in ('--porcelain', '-v', '--verbose') for a in args[1:])
    if verb == 'branch':
        if '--list' in args:
            return all(a == '--list' or a in BRANCH_LIST_FLAGS or not a.startswith('-') for a in args)
        return all(a in BRANCH_LIST_FLAGS for a in args)   # a bare name would create a branch
    if verb == 'remote':
        return args in ([], ['-v'], ['--verbose'])
    return verb in GIT_READ_VERBS


def find_read_only(seg):
    """find with tests, operators and -print/-print0 only: no action that runs, deletes or writes."""
    return all(not a.startswith('-') or a in FIND_TESTS or re.fullmatch(r'-newer[aBcmt][aBcmt]', a)
               or re.fullmatch(r'-[0-9].*', a, re.S) for a in seg[1:])


def date_read_only(args):
    """date that only prints: +FORMAT, -u, -R, -I..., --iso-8601..., --rfc..., -d/--date, -r/--reference; never a new time."""
    i = 0
    while i < len(args):
        a = args[i]
        if a in ('-d', '--date', '-r', '--reference'):
            i += 2
            continue
        if not (a.startswith(('+', '--date=', '--reference=', '-I', '--iso-8601', '--rfc'))
                or a in ('-u', '--utc', '--universal', '-R')):
            return False
        i += 1
    return True


def read_only_reader_allowed(words, role, cwd):
    """One command of a read-only pipeline: an allowed reader by its bare name, without options that write or run."""
    name, args = words[0], words[1:]
    if name not in READ_ONLY_COMMANDS:
        return False
    if name == 'git':
        return git_read_only(words, cwd)
    if name == 'gh':
        return gh_read(words, role)
    if name == 'find':
        return find_read_only(words)
    if name == 'sort':
        return not any(re.fullmatch(r'-[^-]*o.*|--o.*|--co.*', a, re.S) for a in args)
    if name == 'uniq':
        operands = [a for a in args if a == '-' or not a.startswith('-')]
        return '--' not in args and len(operands) <= 1          # a second file is an output
    if name == 'rg':
        return not any(a.startswith(('--pre', '--hos')) for a in args)
    if name == 'file':
        return not any(re.fullmatch(r'-[^-]*C.*|--co.*', a, re.S) for a in args)
    if name == 'date':
        return date_read_only(args)
    return True


def read_only_pipeline(command):
    """The commands (word lists) of a read-only call, or None unless it is one simple pipeline.

    Outside quotes only the characters of READ_ONLY_BARE, spaces, tabs and `|` may appear, plus the whole words
    2>/dev/null and 2>&1. Inside single quotes anything may appear. Inside double quotes anything but $, a backtick
    and a backslash, which bash would expand there. Control characters other than tab are refused anywhere.
    """
    if any(ord(c) < 0x20 and c != '\t' for c in command):
        return None
    bare, quote = [], None                    # the command with every quoted character replaced by 'x'
    for c in command:
        if quote is not None:
            if c == quote:
                quote = None
                bare.append(c)
            elif quote == '"' and c in '$`\\':
                return None
            else:
                bare.append('x')
        else:
            if c in '\'"':
                quote = c
            bare.append(c)
    bare_text = ''.join(bare)
    if quote is not None or re.search(r'[\n\r]', bare_text):
        return None
    bare_words = bare_text.replace('|', ' | ').split()
    lexer = shlex.shlex(command, posix=True, punctuation_chars='|')
    lexer.whitespace_split = True
    lexer.commenters = ''
    try:
        tokens = list(lexer)
    except ValueError:
        return None
    if len(tokens) != len(bare_words):
        return None                           # `||` or a token the two splits disagree on
    pipeline, current = [], []
    for bare_word, token in zip(bare_words, tokens):
        if bare_word == '|':
            if token != '|' or not current:
                return None
            pipeline.append(current)
            current = []
        elif bare_word in READ_ONLY_STDERR:
            continue
        elif READ_ONLY_BARE.fullmatch(bare_word):
            current.append(token)
        else:
            return None
    if not current:
        return None
    pipeline.append(current)
    return pipeline


def same_path(a, b, cwd):
    norm = lambda p: os.path.normcase(os.path.normpath(os.path.join(cwd or '', p.replace('\\', '/'))))
    return bool(a and b) and norm(a) == norm(b)


def path_has_link(path):
    """True when any part of an absolute path (POSIX, or a drive path with forward slashes) is a link or reparse point."""
    head, rest = ('/', path[1:]) if path.startswith('/') else (path[:3], path[3:])
    parts = rest.split('/')
    return any(is_link(head + '/'.join(parts[:i + 1])) for i in range(len(parts)))


def codex_target_allowed(workdir, prompt, cwd, branch):
    """The self-review reads the implementer's own worktree, with a prompt (not a link) in its own ticket's review folder."""
    prompt = prompt.replace('\\', '/')
    match = CODEX_PROMPT.fullmatch(prompt)
    ticket = re.fullmatch(r'her/([0-9]+)-.+', branch or '')
    return bool(match and ticket and match.group(1) == ticket.group(1) and not path_has_link(prompt)
                and same_path(workdir, cwd, cwd))


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


def codex_allowed(found, cwd, branch):
    """The implementer's read-only Codex self-review through the integration checkout's codex-task script."""
    if found[:1] == ['bash']:
        found = found[1:]
    if found and found[0] == CODEX_SH:
        return (len(found) in (7, 8) and bool(CODEX_NAME.fullmatch(found[1]))
                and found[4:7] == ['gpt-6-astra', 'high', 'read-only']
                and (len(found) == 7 or bool(CODEX_MINUTES.fullmatch(found[7])))
                and codex_target_allowed(found[2], found[3], cwd, branch))
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
                and ('-timeoutminutes' not in pairs or bool(CODEX_MINUTES.fullmatch(pairs['-timeoutminutes'])))
                and codex_target_allowed(pairs['-workdir'], pairs['-promptfile'], cwd, branch))
    return False


def gh_read(found, role):
    rest = without_repo(found)
    if any(w == '--watch' or w.startswith('--watch=') for w in rest):
        return False                            # never poll GitHub
    if rest[:3] == ['gh', 'api', 'rate_limit']:
        return rest[3:] == [] or (len(rest) == 5 and rest[3] in ('--jq', '-q'))
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
    return not path_has_link(path)


def check_bash(role, command, hook_cwd):
    if "$'" in command or re.search(r'\\\r?\n', command):
        raise Deny('no ANSI-C quoting ($\'...\') and no backslash-newline continuations; write one plain command.')
    if role in READ_ONLY_ROLES:
        pipeline = read_only_pipeline(command)
        if not pipeline or not all(read_only_reader_allowed(words, role, hook_cwd) for words in pipeline):
            raise Deny(READ_ONLY_REFUSAL)
        return
    plain = simple(command, hook_cwd)
    words, cwd = plain if plain else (None, hook_cwd)
    branch = branch_of(cwd)
    on_ticket = bool(branch and TICKET_BRANCH.fullmatch(branch))
    # Exact forms first: each is one plain command, so its message or values cannot run anything else.
    if role == 'implementer' and words and on_ticket and (
            push_allowed(words, branch) or commit_allowed(words) or quick_allowed(words, branch)
            or codex_allowed(words, cwd, branch)):
        return
    try:
        flat = flatten(command)
    except ValueError:
        raise Deny('cannot parse this command; write it as one plain command.')
    text = command + '\n' + ' '.join(flat)
    try:
        segs = segments(command)
    except ValueError:
        raise Deny('cannot parse this command; write it as one plain command.')
    commands = {base(words[0]) for words, _ in segs}
    indirect = '$' in command or '`' in command or bool(commands & WRAPPERS) or bool(FIND_EXEC & set(flat))
    names = {base(word) for word in flat}
    wordset = set(flat)
    has_git = 'git' in names or bool(GIT_WORD.search(text))
    if '--no-verify' in text or 'hookspath' in text.lower():
        raise Deny('hooks stay on: no --no-verify and no core.hooksPath.')
    if commands & {'gh-public.sh', 'gh-public.ps1'} or (indirect and 'gh-public' in text):
        raise Deny('only the coordinator posts (scripts/gh-public); put it in your report. To read the script, use the Read tool.')
    if commands & {'handoff.sh', 'handoff.ps1'} or (indirect and re.search(r'handoff\.(?:sh|ps1)', text)):
        raise Deny('only the coordinator hands off (scripts/handoff); put it in your report. To read the script, use the Read tool.')
    if 'build-slot.lock' in text:
        raise Deny('use the build slot (scripts/cargo-slot.ps1 on Windows, scripts/cargo-slot.sh on Linux); never create or remove its lock by hand.')
    if commands & CODEX_NAMES or (indirect and 'codex' in text.lower()):
        raise Deny('Codex runs only as the implementer\'s read-only self-review, from its own worktree: [cd <worktree> &&] '
                   '/root/her/scripts/codex-task.sh <name> <worktree = cwd> /root/orch/her/<n>/review/codex-self-<k>.md '
                   'gpt-6-astra high read-only [minutes] (Windows: powershell -NoProfile -File C:/her/scripts/codex-task.ps1 '
                   '-Name <name> -WorkDir <worktree> -PromptFile C:/her-orch/<n>/review/codex-self-<k>.md -Model gpt-6-astra '
                   '-Effort high -Sandbox read-only); <n> is your branch\'s ticket and the prompt is no link. '
                   'To read the script, use the Read tool.')
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
                   'view|list|diff|checks|status|download ...` (never watch or --watch) or `gh api rate_limit [--jq <x>]`; '
                   'implementers may also run exactly '
                   '`gh workflow run quick.yml --ref <your branch> -f target=<name> [-f filter=<test>] [-f os=linux|windows]`.')
    if role == 'implementer' and re.search(r'(^|[;&|\s])git\s+(merge|rebase|cherry-pick|revert|am|tag)(\s|$)', command):
        raise Deny('workers never merge, rebase or tag; the coordinator merges through a pull request.')


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

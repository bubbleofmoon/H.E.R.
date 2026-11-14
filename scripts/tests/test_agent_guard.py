#!/usr/bin/env python3
"""Table tests for .claude/hooks/agent-guard.sh (the worker guard; rules in agent_guard.py).

Each case feeds one PreToolUse JSON call to `bash agent-guard.sh <role>` and checks the exit code:
0 allows, 2 denies. No command is ever executed. A throwaway repository on `main` has a worktree on
`her/1-x` and a detached worktree, so the current-branch checks see real repository state.
Run: python -I scripts/tests/test_agent_guard.py
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import types
from unittest import mock
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
GUARD = ROOT / '.claude' / 'hooks' / 'agent-guard.sh'
BASH = shutil.which('bash')
ROLES = ('implementer', 'reviewer', 'gate', 'planner')


def clean_env():
    """This process's environment without any GIT_* variable, so no git call here reaches another repository."""
    return {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}


def load_guard():
    spec = importlib.util.spec_from_file_location('agent_guard', GUARD.with_name('agent_guard.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

READS = [
    'gh pr view 5 -R bubbleofmoon/H.E.R. --json headRefOid,statusCheckRollup',
    'gh -R bubbleofmoon/H.E.R. pr checks 5',
    'gh run view 123 --log --job 456',
    'gh run watch 1 --exit-status',
    'gh issue view 4 --comments',
    'gh run list --workflow quick.yml --branch her/1-x -L 1',
    "gh pr view 5 --json title --jq '.title | ascii_downcase'",
    'gh workflow view quick.yml',
    'git status',
    'git log --oneline -3',
    'ls -la',
    # codex-task reads and the Codex reads of the self-review (S4: a file named like a tool stays readable)
    'cat scripts/codex-task.sh',
    'git diff -- scripts/codex-task.sh',
    'cat scripts/cargo-slot.sh',
    'grep -n make x',
    'grep -rn install docs/',
    'git worktree list',
    'git --no-pager diff HEAD~1',
    'git -C /root/her log --oneline -3',
    'git show HEAD:AGENTS.md',
    # v4 read-only git and text reads
    'git rev-list --count HEAD',
    'git show-ref --heads',
    'git shortlog -sn HEAD',
    'git diff-tree -r HEAD',
    'git name-rev HEAD',
    'git --version',
    'git branch',
    'git branch --list her/*',
    'git branch -a',
    'git remote -v',
    'sed -n 1,5p x',
    'find . -name x.md',
]
IMPLEMENTER_ALLOWED = [
    ('wt', 'git push origin her/1-x'),
    ('wt', 'git push -u origin her/1-x'),
    ('wt', 'git push --set-upstream origin her/1-x'),
    ('wt', 'HER_GITLEAKS=/opt/gl/gitleaks git push -u origin her/1-x'),
    ('repo', 'cd {wt} && git push -u origin her/1-x'),
    ('wt', 'git commit -m "ci: push the ticket branch and run gh"'),
    ('wt', 'git commit -am x'),
    ('wt', 'git commit -F /root/orch/her/1/tmp/msg.txt'),
    ('repo', 'cd {wt} && git commit -m x'),
    ('wt', 'gh workflow run quick.yml --ref her/1-x -f target=unit -f filter=lifecycle::tests::a -f os=windows'),
    ('wt', 'gh workflow run quick.yml -R bubbleofmoon/H.E.R. --ref her/1-x -f target=harness_e2e'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} /root/orch/her/1/review/codex-self-1.md gpt-6-astra high read-only'),
    ('wt', 'bash /root/her/scripts/codex-task.sh 1-self-2 {wt} /root/orch/her/1/review/codex-self-2.md gpt-6-astra high read-only 120'),
    ('wt', 'powershell -NoProfile -ExecutionPolicy Bypass -File C:/her/scripts/codex-task.ps1 -Name 1-self-1 -WorkDir {wt} -PromptFile C:/her-orch/1/review/codex-self-1.md -Model gpt-6-astra -Effort high -Sandbox read-only'),
    ('wt', 'pwsh -NoProfile -File c:/her/scripts/codex-task.ps1 -name 1-self-1 -workdir {wt} -promptfile p.md -model gpt-6-astra -effort high -sandbox read-only -timeoutminutes 90'),
    ('wt', 'git add scripts/codex-task.sh'),
]
DENIED_EVERY_ROLE = [
    # R1: a second push after an allowed one, other refs, flags
    ('wt', 'git push origin her/1-x; git push origin HEAD:refs/heads/main'),
    ('wt', 'git push origin her/1-x && git push origin main'),
    ('wt', 'git push origin HEAD:refs/heads/main'),
    ('wt', 'git push origin main'),
    ('wt', 'git push -d origin her/1-x'),
    ('wt', 'git push -uf origin her/1-x'),
    ('wt', 'git push --no-verify origin her/1-x'),
    ('wt', 'git push --force origin her/1-x'),
    ('wt', 'git push origin +her/1-x'),
    ('wt', 'git push origin her/2-other'),
    ('wt', 'git push origin her/1-x 2>&1'),
    ('wt', 'x=1 git push origin her/1-x'),
    ('wt', 'HER_GITLEAKS=/bin/true git push origin her/1-x'),
    # R2: detection
    ('wt', 'git -c color.ui=false push origin HEAD:main'),
    ('wt', 'git --no-pager push origin her/1-x'),
    ('wt', 'git -C {wt} -C {wt} push origin her/1-x'),
    ('wt', '/usr/bin/git push origin her/1-x'),
    ('wt', '(git push origin her/1-x)'),
    ('wt', 'bash -c "git push origin her/1-x"'),
    ('wt', 'git "pu"sh origin main'),
    ('wt', 'git pu\\sh origin main'),
    ('wt', 'git push origin her/1-x #c'),
    ('wt', 'git push origin her/1-x#;git push origin main'),
    ('wt', 'git -c core.hooksPath=/dev/null commit -m x'),
    ('wt', 'git config core.hooksPath /dev/null'),
    ('wt', 'git config alias.p "pu""sh"'),
    # commits outside the exact form
    ('wt', 'git commit --no-verify -m x'),
    ('wt', 'git commit -nm x'),
    ('wt', 'git commit -m x; git push origin main'),
    ('wt', 'git commit -m "$(cat msg)"'),
    ('wt', 'git -C {wt} commit -m x'),
    ('wt', 'git add -A && git commit -m x'),
    ('repo', 'git commit -m x'),
    ('detached', 'git commit -m x'),
    ('wt', 'cd {repo} && git commit -m x'),
    ('repo', 'cd {wt} && git push origin main'),
    # moving main
    ('wt', 'git switch main'),
    ('wt', 'git checkout -B main'),
    ('wt', 'git switch -C main'),
    ('wt', 'git branch -f main HEAD'),
    ('wt', 'git update-ref refs/heads/main HEAD'),
    ('wt', 'git symbolic-ref HEAD refs/heads/main'),
    ('wt', 'git fetch origin her/1-x:main'),
    ('repo', 'git reset --hard HEAD'),
    # R3, R4, S3: gh writes and posting
    ('wt', 'gh -R bubbleofmoon/H.E.R. pr merge 68 --merge'),
    ('wt', 'gh pr -R bubbleofmoon/H.E.R. merge 5'),
    ('wt', 'gh issue -R bubbleofmoon/H.E.R. comment 5 --body hi'),
    ('wt', 'gh pr create --title t --body b'),
    ('wt', 'gh run rerun 5'),
    ('wt', 'gh run cancel 5'),
    ('wt', "gh 'api' repos/x -X PATCH"),
    ('wt', 'gh api repos/x'),
    ('wt', 'gh auth token'),
    ('wt', 'gh gist create f'),
    ('wt', 'gh alias set x y'),
    ('wt', 'gh label create x'),
    ('wt', 'gh cache delete x'),
    ('wt', 'gh extension install x'),
    ('wt', 'gh release create v1'),
    ('wt', 'gh repo edit'),
    ('wt', 'gh secret set X'),
    # the one-step handoff is the coordinator's alone
    ('wt', 'scripts/handoff.sh 80 windows /root/orch/her/80/handoff.md'),
    ('wt', 'pwsh -NoProfile -File scripts/handoff.ps1 80 windows b.md'),
    ('wt', 'gh variable set X'),
    ('wt', 'gh pr view 5; gh pr merge 5'),
    ('wt', 'gh pr view 5 | sh'),
    ('wt', 'echo $(gh pr merge 5)'),
    ('wt', 'echo `gh pr merge 5`'),
    ('wt', '/usr/bin/gh pr merge 5'),
    ('wt', 'gh --hostname github.com pr view 5'),
    ('wt', 'scripts/gh-public.sh --body-file b.md pr create --repo bubbleofmoon/H.E.R. --title t'),
    ('wt', 'bash scripts/gh-public.sh --body-file b.md issue comment 5 --repo bubbleofmoon/H.E.R.'),
    # quick-run variants
    ('wt', 'gh workflow run quick.yml --ref her/1-x -f target=unit; gh workflow run checks.yml --ref main'),
    ('wt', 'gh workflow run quick.yml --ref her/1-x --ref main -f target=unit'),
    ('wt', 'gh workflow run quick.yml --ref her/2-other -f target=unit'),
    ('wt', 'gh workflow run checks.yml --ref her/1-x'),
    ('wt', 'gh workflow run quick.yml --ref her/1-x -f target=unit -f os=mac'),
    ('wt', 'gh workflow run quick.yml --ref her/1-x -f target=unit -f target=x'),
    ('wt', 'gh workflow run quick.yml --ref her/1-x'),
    ('repo', 'gh workflow run quick.yml --ref main -f target=unit'),
    # fail closed: unparseable commands, ANSI-C quoting and backslash-newline continuations
    ('wt', 'echo "unterminated'),
    ('wt', "git $'pu\\x73h' origin HEAD:refs/heads/main"),
    ('wt', 'git pu\\\nsh origin main'),
    # Codex runs only as the implementer's pinned read-only self-review (every other form is refused)
    ('wt', 'scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only'),
    ('wt', '{wt}/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only'),
    ('wt', '/tmp/x/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only'),
    ('wt', '/root/her/scripts/../scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6.1-sol high read-only'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra max read-only'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high none'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high workspace-write'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only 0'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only 08'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only 1000'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only 5 extra'),
    ('wt', '/root/her/scripts/codex-task.sh ../x {wt} p.md gpt-6-astra high read-only'),
    ('wt', 'timeout 60 /root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only > log'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only; git push origin main'),
    ('wt', 'powershell -File C:/her/scripts/codex-task.ps1 -Name x -WorkDir {wt} -PromptFile p.md -Model gpt-6-astra -Effort high -sandbox none -Sandbox read-only'),
    ('wt', 'powershell -File C:/her/scripts/codex-task.ps1 -Name x -WorkDir {wt} -PromptFile p.md -Model gpt-6-astra -Effort high -Sandbox:none'),
    ('wt', 'powershell -File C:/her/scripts/codex-task.ps1 -Name x -WorkDir {wt} -PromptFile p.md -Model gpt-6-astra -Effort high -Sandbox read-only -Command x'),
    ('wt', 'powershell -File C:/her/scripts/codex-task.ps1 -Name x -WorkDir {wt} -PromptFile p.md -Model gpt-6-astra -Effort high -Sandbox read-only -AddDir C:/x'),
    ('wt', 'powershell -File C:/her/scripts/codex-task.ps1 -Name x -WorkDir {wt} -PromptFile p.md -Effort high -Sandbox read-only'),
    ('wt', 'powershell -File C:/her/scripts/codex-task.ps1 -Name x -WorkDir {wt} -PromptFile p.md -Model gpt-6-astra -Effort high -Sandbox read-only -TimeoutMinutes 0'),
    ('wt', 'powershell -File C:/her/scripts/codex-task.ps1 -NoProfile -Name x -WorkDir {wt} -PromptFile p.md -Model gpt-6-astra -Effort high -Sandbox read-only'),
    ('wt', 'powershell -File {wt}/scripts/codex-task.ps1 -Name x -WorkDir {wt} -PromptFile p.md -Model gpt-6-astra -Effort high -Sandbox read-only'),
    ('wt', 'powershell -Command "C:/her/scripts/codex-task.ps1 -Sandbox none"'),
    ('wt', 'codex exec x'),
    ('wt', 'codex.cmd exec x'),
    ('wt', 'npx @openai/codex exec x'),
    ('wt', 'node /usr/lib/node_modules/@openai/codex/bin/codex.js exec x'),
    ('wt', 'bash -c "codex exec x"'),
    ('detached', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only'),
    ('repo', '/root/her/scripts/codex-task.sh 1-self-1 {wt} p.md gpt-6-astra high read-only'),
    # v4: command positions after newlines, keywords, assignments-free wrappers and find -exec
    ('wt', 'ls\ncodex exec x'),
    ('wt', 'if true; then codex exec x; fi'),
    ('wt', '{{ codex exec x; }}'),
    ('wt', 'eval codex exec x'),
    ('wt', 'eval "codex exec x"'),
    ('wt', 'setsid codex exec x'),
    ('wt', 'ionice -c3 codex exec x'),
    ('wt', 'while true; do codex exec x; done'),
    ('wt', 'cat <<EOF\ncodex exec x\nEOF'),
    # fix round, Run 4: quoted and escaped break characters, redirections, comments (v2 and amendment 1)
    ('wt', '</dev/null codex exec x'),
    ('wt', '2>/dev/null codex exec x'),
    ('wt', 'eval -- codex exec x'),
    ('wt', "echo a # it's\ncodex exec x # it's"),
    ('wt', 'ls \\ #;codex exec x'),
    ('wt', 'ls \\;#;codex exec x'),
    ('wt', 'ls \\ #;scripts/handoff.sh 80 linux b.md'),
    ('wt', 'bash -s 1-self-1 {wt} p.md gpt-6-astra high none < /root/her/scripts/codex-task.sh'),   # regression (R3)
    ('wt', 'cd scripts && bash -s 80 linux b.md < handoff.sh'),                                      # regression (R3)
]
DENIED_REVIEW_AND_GATE = [
    ('wt', 'git push origin her/1-x'),
    ('wt', 'git commit -m x'),
    ('wt', 'gh workflow run quick.yml --ref her/1-x -f target=unit'),
    ('wt', '/root/her/scripts/codex-task.sh 1-self-1 {wt} /root/orch/her/1/review/codex-self-1.md gpt-6-astra high read-only'),
]
DENIED_REVIEWER = [
    # write rules see through paths and quoting
    ('wt', '/usr/bin/git reset --hard HEAD'),
    ('wt', "'g''it' reset --hard HEAD"),
    ('wt', 'git --no-pager reset --hard HEAD'),
    ('wt', 'gh run download 5 --dir /tmp/x'),
]
PLANNER_WRITES_ALLOWED = ['/root/orch/her/80/plan.md', 'C:/her-orch/80/plan.md', 'C:\\her-orch\\80\\plan.md', 'c:/her-orch/7/plan.md',
                          '/root/orch/her/80/scratch/her/src/lib.rs', '/root/orch/her/80/scratch/scripts/tests/test_x.py',
                          'C:/her-orch/80/scratch/her/tests/x.rs', '/root/orch/her/80/scratch/.gitignore']
PLANNER_WRITES_DENIED = ['/root/orch/her/80/report.md', '/root/orch/her/80/plan.md.bak', '/root/orch/her/80/tmp/plan.md',
                         '/root/orch/her/80/../81/plan.md', '/root/orch/her/x/plan.md', '/root/orch/her/080/plan.md',
                         '/root/orch/her/80/Plan.md', '/root/orch/her/80/PROGRESS.md', '/root/orch/her/80/scratch-evidence/x',
                         '/root/her/AGENTS.md', 'plan.md', '', 'D:/her-orch/80/plan.md', 'C:/her/AGENTS.md',
                         '/root/orch/her/80/plan.md/x', '/root/orch/her/80/scratch', '/root/orch/her/80/scratch2/x.rs',
                         '/root/her-wt/80/her/src/lib.rs', '/root/orch/her/80/scratch/.git',
                         '/root/orch/her/80/scratch/her/.git/config', '/root/orch/her/80/scratch/../plan2.md',
                         '/root/orch/her/80/scratch//x.rs', '/root/orch/her/80/scratch/./x.rs',
                         'C:/her-orch/80/scratch/.GIT', 'C:/her-orch/80/scratch/.git.', 'C:/her-orch/80/scratch/.git ',
                         'C:/her-orch/80/scratch/.git::$DATA', 'C:/her-orch/80/scratch/GIT~1', 'C:/her-orch/80/scratch/her/.Git/config',
                         'C:\\her-orch\\80\\scratch\\..\\plan.md', 'C:/her-orch/80/scratch/x.rs:stream',
                         '/root/orch/her/80/scratch/x\n', '//?/C:/her-orch/80/plan.md', 'C:/HER-ORCH/80/plan.md']
PLANNER_CANNOT_BUILD_OR_FORMAT = [
    ('wt', '/root/her/scripts/cargo-slot.sh 80p /root/orch/her/80/scratch /root/orch/her/80/scratch-evidence r check --tests'),
    ('wt', 'bash scripts/cargo-slot.sh 80p /root/orch/her/80/scratch /root/orch/her/80/scratch-evidence r check'),
    ('wt', 'powershell.exe -NoProfile -File C:/her/scripts/cargo-slot.ps1 80p C:/her-orch/80/scratch C:/her-orch/80/scratch-evidence r "check --tests"'),
    ('wt', 'cargo check'),
    ('wt', '/root/.cargo/bin/cargo check --tests'),
    ('wt', 'timeout 600 cargo check'),
    ('wt', 'env RUSTC_WRAPPER= cargo check'),
    ('wt', 'bash -c "cargo check"'),
    ('wt', 'rustfmt /root/orch/her/80/scratch/her/src/lib.rs'),
    ('wt', '/root/.cargo/bin/rustfmt --check /root/orch/her/80/scratch/her/src/lib.rs'),
    ('wt', 'cargo fmt'),
    ('wt', 'find . -name x.rs -exec rustfmt {{}} ;'),
]
READ_ONLY_GIT_DENIED = [
    ('wt', 'git -C /root/her config core.fsmonitor x'),
    ('wt', 'git remote set-url origin x'),
    ('wt', 'git branch -D her/1-x'),
    ('wt', 'git pull'),
    ('wt', 'git fetch origin'),
    ('wt', 'git add -A'),
    ('wt', 'git rm x'),
    ('wt', 'git gc'),
    ('wt', 'git stash'),
    ('wt', 'git -c core.pager=x log'),
    ('wt', 'git --git-dir=/x log'),
    ('wt', 'git --work-tree=/x status'),
    ('wt', 'git diff --output=/root/orch/her/80/x'),
    ('wt', 'git diff --ext-diff'),
    ('wt', 'git grep -O vi x'),
    ('wt', 'git worktree add /root/orch/her/80/scratch'),
    ('wt', 'git worktree remove x'),
    ('wt', 'xargs git config x'),
    # v4 (and amendments): command positions, assignments, wrappers, listing forms of branch and remote
    ('wt', 'ls\ngit config x y'),
    ('wt', 'if true; then git gc; fi'),
    ('wt', '{{ git add -A; }}'),
    ('wt', 'GIT_EXTERNAL_DIFF=/x git diff'),
    ('wt', 'GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.pager GIT_CONFIG_VALUE_0=x git log'),
    ('wt', 'env GIT_DIR=/x git log'),
    ('wt', 'timeout 5 git log'),
    ('wt', 'git diff --outp=/x'),
    ('wt', 'git diff --ext-d'),
    ('wt', 'git grep --open-f=vi x'),
    ('wt', 'git branch new-branch'),
    ('wt', 'git branch -D her/1-x'),
    ('wt', 'git branch --list -D her/1-x'),
    ('wt', 'git remote add x y'),
    ('wt', 'git --version --exec-path=/x'),
    ('wt', 'git grep -lOsh x'),                                  # amendment 3: bundled -O
    ('wt', "git diff '' --output=/root/orch/her/80/x"),         # an empty quoted word is an argument, not a break
]
READ_ONLY_FILE_WRITES_DENIED = [
    ('wt', 'ls\nrm x'),
    ('wt', 'for f in a; do rm x; done'),
    ('wt', '! cargo check'),
    ('wt', 'while true; do touch x; done'),
    ('wt', 'sed -i s/a/b/ x'),
    ('wt', 'sed -ni p x'),
    ('wt', 'sed -i.bak s/a/b/ x'),
    ('wt', 'sed --in-place s/a/b/ x'),
    ('wt', 'find . -name x -delete'),
    ('wt', 'find . -fprint /tmp/x'),
    ('wt', 'rmdir x'),
    ('wt', 'unlink x'),
    ('wt', 'patch -p1 -i x.diff'),
    ('wt', 'tar -xf x.tar'),
    ('wt', 'unzip x.zip'),
    ('wt', 'curl -o x https://example.invalid/'),
    ('wt', 'wget https://example.invalid/'),
    ('wt', 'rsync a b'),
    ('wt', 'setsid rm x'),
    ('wt', 'ionice -c3 rm x'),
    ('wt', 'taskset 1 touch x'),
    ('wt', 'flock /tmp/l rm x'),
    ('wt', 'strace -o x ls'),
    ('wt', 'script -c "rm x" log'),
    ('wt', 'doas rm x'),
    ('wt', 'su -c "rm x"'),
    ('wt', 'runuser -u x rm y'),
    ('wt', 'winpty rm x'),
    ('wt', 'cmd /c start rm x'),
    ('wt', 'call rm x'),
    ('wt', 'grep -rn "cargo check" docs/'),
    ('wt', 'function f {{ rm x; }}'),
    ('wt', 'cat <<EOF\nrm x\nEOF'),                             # a heredoc body line is a command
]


WRITERS_ALLOWED = [                     # implementers and gates keep ordinary commands with quotes and redirections
    ('wt', 'grep -n "Result<()>" x'),
    ('wt', 'grep -n "<<<<<<<" x'),
    ('wt', 'ls >&2'),
    ('wt', 'ls 2>&1 | head'),
    ('wt', 'git status\ngit log --oneline -3'),       # moved from READS: read-only roles now refuse a newline
    ('wt', 'ls >/dev/null 2>&1'),                     # moved from READS: read-only roles allow only 2>/dev/null and 2>&1
]


@unittest.skipUnless(BASH, 'bash is not available')
class AgentGuard(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls._tmp = tempfile.TemporaryDirectory()
        base = Path(cls._tmp.name)
        cls.places = {'repo': base / 'repo', 'wt': base / 'wt', 'detached': base / 'detached'}
        git = ['git', '-c', 'user.name=guard-test', '-c', 'user.email=guard-test@example.invalid',
               '-c', 'init.defaultBranch=main']
        repo = str(cls.places['repo'])
        env = clean_env()
        subprocess.run([*git, 'init', '-q', repo], check=True, env=env)
        subprocess.run([*git, '-C', repo, 'commit', '-q', '--allow-empty', '-m', 'init'], check=True, env=env)
        subprocess.run([*git, '-C', repo, 'worktree', 'add', '-q', '-b', 'her/1-x', str(cls.places['wt'])], check=True,
                       env=env)
        subprocess.run([*git, '-C', repo, 'worktree', 'add', '-q', '--detach', str(cls.places['detached'])], check=True,
                       env=env)

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    def guard(self, role, where, command):
        posix = {name: path.as_posix() for name, path in self.places.items()}
        call = {'tool_name': 'Bash', 'tool_input': {'command': command.format(**posix)}, 'cwd': str(self.places[where])}
        result = subprocess.run([BASH, str(GUARD), role], input=json.dumps(call), capture_output=True,
                                text=True, encoding='utf-8', timeout=60, env=clean_env())
        return result.returncode, result.stderr

    def tool_call(self, role, tool, path):
        call = {'tool_name': tool, 'tool_input': {'file_path': path}, 'cwd': str(self.places['wt'])}
        result = subprocess.run([BASH, str(GUARD), role], input=json.dumps(call), capture_output=True,
                                text=True, encoding='utf-8', timeout=60, env=clean_env())
        return result.returncode

    def expect(self, cases, roles, allowed):
        for role in roles:
            for where, command in cases:
                with self.subTest(role=role, where=where, command=command):
                    code, err = self.guard(role, where, command)
                    self.assertEqual(code, 0 if allowed else 2, err)

    def test_reads_are_allowed_for_every_role(self):
        self.expect([('wt', command) for command in READS], ROLES, True)

    def test_implementer_exact_forms_are_allowed(self):
        self.expect(IMPLEMENTER_ALLOWED, ('implementer',), True)

    def test_bypasses_are_denied_for_every_role(self):
        self.expect(DENIED_EVERY_ROLE, ROLES, False)

    def test_review_and_gate_never_push_commit_or_trigger(self):
        self.expect(DENIED_REVIEW_AND_GATE, ('reviewer', 'gate', 'planner'), False)

    def test_reviewer_write_rules_see_through_paths_and_quoting(self):
        self.expect(DENIED_REVIEWER[:3], ('reviewer', 'planner'), False)

    def test_planner_writes_only_its_plan_file_and_scratch(self):          # first red
        for path in PLANNER_WRITES_ALLOWED:
            for tool in ('Write', 'Edit'):
                with self.subTest(tool=tool, path=path):
                    self.assertEqual(self.tool_call('planner', tool, path), 0)
        for path in PLANNER_WRITES_DENIED:
            for tool in ('Write', 'Edit'):
                with self.subTest(tool=tool, path=path):
                    self.assertEqual(self.tool_call('planner', tool, path), 2)
        for path in ('/root/orch/her/80/plan.md', '/root/orch/her/80/scratch/x.ipynb'):
            self.assertEqual(self.tool_call('planner', 'NotebookEdit', path), 2)

    def test_a_planner_cannot_build_or_format(self):
        # Gate and implementer build by design; this pins the read-only roles only.
        self.expect(PLANNER_CANNOT_BUILD_OR_FORMAT, ('planner', 'reviewer'), False)

    def test_read_only_roles_run_git_with_read_verbs_only(self):
        self.expect(READ_ONLY_GIT_DENIED, ('planner', 'reviewer'), False)

    def test_read_only_roles_change_no_files(self):
        self.expect(READ_ONLY_FILE_WRITES_DENIED, ('planner', 'reviewer'), False)

    def test_a_quoted_pattern_naming_a_tool_points_to_the_grep_tool(self):
        for role in ('planner', 'reviewer'):
            code, err = self.guard(role, 'wt', 'grep -rn "cargo check" docs/')
            self.assertEqual(code, 2, err)
            self.assertIn('Grep tool', err)

    def test_other_roles_keep_their_write_rules(self):
        self.assertEqual(self.tool_call('implementer', 'Write', '/root/her-wt/1/her/src/x.rs'), 0)
        self.assertEqual(self.tool_call('reviewer', 'Write', '/root/orch/her/80/plan.md'), 2)
        self.assertEqual(self.tool_call('gate', 'Write', '/root/orch/her/80/plan.md'), 0)

    def test_links_junctions_and_reparse_points_on_the_way_are_refused(self):
        guard = load_guard()

        def call(path):
            return {'tool_name': 'Write', 'tool_input': {'file_path': path}}

        def lstat_with_reparse(target):
            def fake(path):
                if path == target:
                    return types.SimpleNamespace(st_file_attributes=0x400)
                raise FileNotFoundError(path)
            return fake
        cases = [   # (path written, the part that is a link, how it is a link)
            ('/root/orch/her/80/scratch/her/src/x.rs', '/root/orch/her/80/scratch/her', 'islink'),
            ('/root/orch/her/80/plan.md', '/root/orch/her/80/plan.md', 'islink'),                 # the last part
            ('C:/her-orch/80/scratch/x.rs', 'C:/her-orch/80/scratch', 'islink'),                    # Windows head
            ('C:/her-orch/80/scratch/x/config.toml', 'C:/her-orch/80/scratch/x', 'isjunction'),
            ('C:/her-orch/80/scratch/x/config.toml', 'C:/her-orch/80/scratch/x', 'reparse'),
        ]
        for path, link, how in cases:
            with self.subTest(path=path, how=how), \
                    mock.patch.object(guard.os.path, 'islink', side_effect=lambda p: how == 'islink' and p == link), \
                    mock.patch.object(guard.os.path, 'isjunction', side_effect=lambda p: how == 'isjunction' and p == link, create=True), \
                    mock.patch.object(guard.os, 'lstat', side_effect=lstat_with_reparse(link if how == 'reparse' else None)):
                with self.assertRaises(guard.Deny):
                    guard.check('planner', call(path))
        with mock.patch.object(guard.os.path, 'islink', return_value=False), \
                mock.patch.object(guard.os.path, 'isjunction', return_value=False, create=True), \
                mock.patch.object(guard.os, 'lstat', side_effect=FileNotFoundError):
            guard.check('planner', call('/root/orch/her/80/scratch/her/src/x.rs'))   # no link: allowed

    def test_an_unreadable_part_is_treated_as_a_link(self):
        guard = load_guard()
        with mock.patch.object(guard.os.path, 'islink', return_value=False), \
                mock.patch.object(guard.os.path, 'isjunction', return_value=False, create=True), \
                mock.patch.object(guard.os, 'lstat', side_effect=PermissionError):
            with self.assertRaises(guard.Deny):
                guard.check('planner', {'tool_name': 'Write', 'tool_input': {'file_path': '/root/orch/her/80/plan.md'}})

    # --- fix round, Run 4: the lexer for implementers and gates -----------------------------
    def test_segments_keep_arguments_after_quoted_operators_and_redirections(self):   # first red (fix round)
        guard = load_guard()
        words = lambda command: [w for w, _ in guard.segments(command)]
        self.assertIn(['git', 'log', '--output=/x'], words('git log &>/dev/null --output=/x'))
        self.assertIn(['git', 'log', '--output=/x'], words('git log &>>/dev/null --output=/x'))
        self.assertIn(['git', 'log', '--grep', ';', '--output=/x'], words("git log --grep ';' --output=/x"))
        self.assertIn(['git', 'log', '--grep', ';', '--output=/x'], words('git log --grep \\; --output=/x'))
        self.assertIn(['git', 'log', '--grep', ';', '--output=/x'], words('git log --grep ";" --output=/x'))
        self.assertIn(['rm', 'victim'], words('</dev/null rm victim'))
        self.assertIn(['rm', 'victim'], words('eval -- rm victim'))
        self.assertIn(['/root/her/scripts/codex-task.sh'],
                      words('bash -s x {wt} p.md gpt-6-astra high none < /root/her/scripts/codex-task.sh'))   # regression (R3)
        self.assertIn(['handoff.sh'], words('bash -s 80 linux b.md <<< handoff.sh'))                    # regression (R3)
        self.assertIn(['rm', 'x'], words("echo a # it's\nrm x # it's"))                                 # comments
        self.assertIn(['codex', 'exec', 'x'], words('ls \\ #;codex exec x'))                            # amendment 1

    def test_segments_never_raise_inside_quoted_patterns(self):                                         # regression
        guard = load_guard()
        for command in ('grep -rn "Result<()>" her/src', 'grep -n "Result<(), Error>" x', 'grep -n "Vec<u8>)" x',
                        'grep -n "Vec<u8>;" x', 'grep -n "<<<<<<<" x', 'grep -n ">>>>>>>" x',
                        'grep -n "Arc<Mutex<Vec<u8>>>" x'):
            guard.segments(command)
        with self.assertRaises(ValueError):
            guard.segments('ls >; x')                                                                   # depth 0 still refuses

    def test_an_unreadable_nested_string_counts_every_word(self):                                       # S6
        guard = load_guard()
        self.assertIn(['codex'], [w for w, _ in guard.segments('bash -c "x \'y codex"')])

    def test_writers_keep_ordinary_commands(self):
        self.expect(WRITERS_ALLOWED, ('implementer', 'gate'), True)

    def test_reviewer_cannot_download_and_gate_can(self):
        self.expect([DENIED_REVIEWER[3]], ('reviewer',), False)
        self.expect([DENIED_REVIEWER[3]], ('gate',), True)
        self.expect([('wt', 'gh run download 5 -n ci-evidence-ubuntu-24.04-1')], ('implementer', 'gate'), True)

    def test_reviewer_reads_git_log(self):
        self.expect([('wt', 'git log --oneline')], ('reviewer',), True)


if __name__ == '__main__':
    unittest.main()

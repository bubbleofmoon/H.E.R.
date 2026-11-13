#!/usr/bin/env python3
"""Table tests for .claude/hooks/agent-guard.sh (the worker guard; rules in agent_guard.py).

Each case feeds one PreToolUse JSON call to `bash agent-guard.sh <role>` and checks the exit code:
0 allows, 2 denies. No command is ever executed. A throwaway repository on `main` has a worktree on
`her/1-x` and a detached worktree, so the current-branch checks see real repository state.
Run: python -I scripts/tests/test_agent_guard.py
"""
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
GUARD = ROOT / '.claude' / 'hooks' / 'agent-guard.sh'
BASH = shutil.which('bash')
ROLES = ('implementer', 'reviewer', 'gate')

READS = [
    'gh pr view 5 -R bubbleofmoon/H.E.R. --json headRefOid,statusCheckRollup',
    'gh -R bubbleofmoon/H.E.R. pr checks 5',
    'gh run view 123 --log --job 456',
    'gh run watch 1 --exit-status',
    'gh issue view 4 --comments',
    'gh run list --workflow quick.yml --branch her/1-x -L 1',
    "gh pr view 5 --json title --jq '.title | ascii_downcase'",
    'gh run download 5 -n ci-evidence-ubuntu-24.04-1',
    'gh workflow view quick.yml',
    'git status',
    'git log --oneline -3',
    'ls -la',
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
]
DENIED_REVIEW_AND_GATE = [
    ('wt', 'git push origin her/1-x'),
    ('wt', 'git commit -m x'),
    ('wt', 'gh workflow run quick.yml --ref her/1-x -f target=unit'),
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
        subprocess.run([*git, 'init', '-q', repo], check=True)
        subprocess.run([*git, '-C', repo, 'commit', '-q', '--allow-empty', '-m', 'init'], check=True)
        subprocess.run([*git, '-C', repo, 'worktree', 'add', '-q', '-b', 'her/1-x', str(cls.places['wt'])], check=True)
        subprocess.run([*git, '-C', repo, 'worktree', 'add', '-q', '--detach', str(cls.places['detached'])], check=True)

    @classmethod
    def tearDownClass(cls):
        cls._tmp.cleanup()

    def guard(self, role, where, command):
        posix = {name: path.as_posix() for name, path in self.places.items()}
        call = {'tool_name': 'Bash', 'tool_input': {'command': command.format(**posix)}, 'cwd': str(self.places[where])}
        result = subprocess.run([BASH, str(GUARD), role], input=json.dumps(call), capture_output=True,
                                text=True, encoding='utf-8', timeout=60)
        return result.returncode, result.stderr

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
        self.expect(DENIED_REVIEW_AND_GATE, ('reviewer', 'gate'), False)


if __name__ == '__main__':
    unittest.main()

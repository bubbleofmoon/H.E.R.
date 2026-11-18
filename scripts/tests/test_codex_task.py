#!/usr/bin/env python3
"""Fake-codex tests for scripts/codex-task and scripts/codex-work (sh and ps1).

Receipts: each case runs a launcher against a fake `codex` that writes (or does not write) the report
named by `-o`, and reads the exit receipt. A run that produced no report must never read as a pass:
a missing or empty report gives `no-report`, a usage-limit message in the error log gives `quota`.
Work modes: `implement` runs Codex in a ticket worktree (a throwaway repository with a local bare remote);
after a clean exit with a report the wrapper, not Codex, commits and pushes the ticket branch. `plan` runs
Codex in the ticket's scratch export and installs the plan the run left in its plan-out folder.
The sh cases put the fake first on PATH; the ps1 cases run under pwsh with APPDATA pointing at a
fake package folder that holds the fake as codex.exe. POSIX only: the fake is a shell script.
Run: python -I scripts/tests/test_codex_task.py
"""
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
SH = ROOT / 'scripts' / 'codex-task.sh'
PS1 = ROOT / 'scripts' / 'codex-task.ps1'
WORK_SH = ROOT / 'scripts' / 'codex-work.sh'
WORK_PS1 = ROOT / 'scripts' / 'codex-work.ps1'
BASH = shutil.which('bash')
PWSH = shutil.which('pwsh')
GIT = shutil.which('git')
POSIX = sys.platform != 'win32'
LIMIT = "ERROR: You've hit your usage limit. Try again later."

FAKE = r'''#!/usr/bin/env bash
# Fake codex: FAKE_MODE chooses what the run leaves behind; FAKE_ARGS names a file that records the call.
if [ -n "${FAKE_ARGS:-}" ]; then
  { printf '%s\n' "$@"; echo "GIT_CONFIG_KEY_0=${GIT_CONFIG_KEY_0:-}"; echo "PWD=$PWD"; } > "$FAKE_ARGS"
fi
report=""
workdir=""
adds=()
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then report=$2; shift; fi
  if [ "$1" = "-C" ]; then workdir=$2; shift; fi
  if [ "$1" = "--add-dir" ]; then adds+=("$2"); shift; fi
  shift
done
cat > /dev/null
case "$FAKE_MODE" in
  change) printf 'new\n' > "$workdir/new.txt"; printf 'done\n' > "$report" ;;
  change-fail) printf 'new\n' > "$workdir/new.txt"; printf 'half done\n' > "$report"; exit 3 ;;
  change-quota) printf 'new\n' > "$workdir/new.txt"; echo "$FAKE_LIMIT" >&2; exit 1 ;;
  nochange) printf 'nothing to do\n' > "$report" ;;
  plan) printf '# plan\nstep 1\n' > "${adds[0]}/plan.md"; printf 'planned\n' > "$report" ;;
  plan-fail) printf '# plan\n' > "${adds[0]}/plan.md"; printf 'half\n' > "$report"; exit 3 ;;
  plan-none) printf 'planned nothing\n' > "$report" ;;
  report) printf 'looks fine\nPASS\n' > "$report" ;;
  report-fail) printf 'half done\n' > "$report"; exit 3 ;;
  report-limit) printf 'looks fine\n' > "$report"; echo "$FAKE_LIMIT" >&2 ;;
  empty) : > "$report" ;;
  quota) echo "$FAKE_LIMIT" >&2; exit 1 ;;
  quota-empty) : > "$report"; echo "$FAKE_LIMIT" >&2; exit 1 ;;
  crash) echo "boom" >&2; exit 2 ;;
esac
exit 0
'''


def clean_env():
    """This process's environment without any GIT_* variable, so no git call here reaches another repository."""
    return {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}


def git(*args, cwd):
    env = dict(clean_env(), GIT_AUTHOR_NAME='t', GIT_AUTHOR_EMAIL='t@example.invalid',
               GIT_COMMITTER_NAME='t', GIT_COMMITTER_EMAIL='t@example.invalid')
    return subprocess.run([GIT, *args], cwd=cwd, env=env, check=True, capture_output=True, text=True)


def install_fake(out, windows):
    """Put the fake codex where the launcher finds it; returns the extra environment."""
    if windows:
        package = out / 'appdata' / 'npm' / 'node_modules' / '@openai' / 'codex' / 'node_modules' / '@openai' / \
            'codex-win32-x64' / 'vendor' / 'x86_64-pc-windows-msvc' / 'bin'
        package.mkdir(parents=True)
        fake = package / 'codex.exe'
        fake.write_text(FAKE, encoding='utf-8')
        fake.chmod(0o755)
        return {'APPDATA': str(out / 'appdata'), 'USERPROFILE': str(out / 'home')}
    bin_dir = out / 'bin'
    bin_dir.mkdir()
    fake = bin_dir / 'codex'
    fake.write_text(FAKE, encoding='utf-8')
    fake.chmod(0o755)
    return {'PATH': f'{bin_dir}{os.pathsep}{os.environ["PATH"]}'}


class Receipts:
    """Shared cases; a subclass supplies launch(mode, out) -> (return code, name, output)."""

    def run_case(self, mode):
        out = Path(tempfile.mkdtemp(prefix='codex-task-test-'))
        self.addCleanup(shutil.rmtree, out, ignore_errors=True)
        rc, name, said = self.launch(mode, out)
        exit_file = out / f'{name}.exit.txt'
        self.assertTrue(exit_file.is_file(), f'no exit receipt written: {said}')
        return rc, exit_file.read_text(encoding='ascii').strip(), out / f'{name}.report.md'

    def test_report_present_passes(self):
        rc, receipt, report = self.run_case('report')
        self.assertEqual((rc, receipt), (0, '0'))
        self.assertIn('PASS', report.read_text())

    def test_empty_report_is_no_report(self):
        rc, receipt, _ = self.run_case('empty')
        self.assertEqual(receipt, 'no-report')
        self.assertNotEqual(rc, 0)

    def test_missing_report_is_no_report(self):
        rc, receipt, _ = self.run_case('crash')
        self.assertEqual(receipt, 'no-report')
        self.assertNotEqual(rc, 0)

    def test_missing_report_with_exit_zero_is_no_report(self):
        rc, receipt, _ = self.run_case('none')
        self.assertEqual(receipt, 'no-report')
        self.assertNotEqual(rc, 0)

    def test_usage_limit_without_report_is_quota(self):
        rc, receipt, _ = self.run_case('quota')
        self.assertEqual(receipt, 'quota')
        self.assertNotEqual(rc, 0)

    def test_usage_limit_with_empty_report_is_quota(self):
        rc, receipt, _ = self.run_case('quota-empty')
        self.assertEqual(receipt, 'quota')
        self.assertNotEqual(rc, 0)

    def test_report_with_usage_limit_text_keeps_the_numeric_code(self):
        rc, receipt, _ = self.run_case('report-limit')
        self.assertEqual((rc, receipt), (0, '0'))

    def test_report_with_failing_exit_keeps_the_numeric_code(self):
        rc, receipt, _ = self.run_case('report-fail')
        self.assertEqual((rc, receipt), (3, '3'))

    def test_defaults_are_sol_high_read_only(self):
        out = Path(tempfile.mkdtemp(prefix='codex-task-test-'))
        self.addCleanup(shutil.rmtree, out, ignore_errors=True)
        prompt, work, env = prepare(out, 'report')
        env.update(FAKE_ARGS=str(out / 'args.txt'))
        done = self.launch_defaults(out, prompt, work, env)
        self.assertEqual(done.returncode, 0, done.stderr)
        args = (out / 'args.txt').read_text().splitlines()
        self.assertEqual(args[args.index('-m') + 1], 'gpt-6.2-sol')
        self.assertIn('model_reasoning_effort="high"', args)
        self.assertEqual(args[args.index('-s') + 1], 'read-only')
        self.assertNotIn('--dangerously-bypass-approvals-and-sandbox', args)


def prepare(out, mode):
    prompt = out / 'prompt.md'
    prompt.write_text('review\n', encoding='utf-8')
    work = out / 'work'
    work.mkdir()
    env = dict(os.environ, FAKE_MODE=mode, FAKE_LIMIT=LIMIT)
    return prompt, work, env


@unittest.skipUnless(POSIX and BASH, 'needs bash on a POSIX host')
class ShReceipts(Receipts, unittest.TestCase):
    def launch(self, mode, out):
        prompt, work, env = prepare(out, mode)
        env.update(install_fake(out, False))
        done = subprocess.run([BASH, str(SH), 'case', str(work), str(prompt), 'gpt-6.2-astra', 'high', 'read-only', '1'],
                              env=env, capture_output=True, text=True, timeout=120)
        return done.returncode, 'case', done.stdout + done.stderr

    def launch_defaults(self, out, prompt, work, env):
        env.update(install_fake(out, False))
        return subprocess.run([BASH, str(SH), 'case', str(work), str(prompt)], env=env, capture_output=True,
                              text=True, timeout=120)


@unittest.skipUnless(POSIX and PWSH, 'needs pwsh on a POSIX host')
class Ps1Receipts(Receipts, unittest.TestCase):
    def launch(self, mode, out):
        prompt, work, env = prepare(out, mode)
        env.update(install_fake(out, True))
        done = subprocess.run([PWSH, '-NoProfile', '-File', str(PS1), '-Name', 'case', '-WorkDir', str(work),
                               '-PromptFile', str(prompt), '-Sandbox', 'read-only', '-TimeoutMinutes', '1'],
                              env=env, capture_output=True, text=True, timeout=120)
        return done.returncode, 'case', done.stdout + done.stderr

    def launch_defaults(self, out, prompt, work, env):
        env.update(install_fake(out, True))
        return subprocess.run([PWSH, '-NoProfile', '-File', str(PS1), '-Name', 'case', '-WorkDir', str(work),
                               '-PromptFile', str(prompt)], env=env, capture_output=True, text=True, timeout=120)


class Work:
    """Shared cases of codex-work implement and plan; a subclass supplies command(kind, **fields) and WINDOWS."""
    WINDOWS = False
    TICKET = '7'
    BRANCH = 'her/7-demo'

    def setUp(self):
        self.out = Path(tempfile.mkdtemp(prefix='codex-work-test-')).resolve()
        self.addCleanup(shutil.rmtree, self.out, ignore_errors=True)
        out = self.out
        self.orch = out / 'orch'
        self.ticket_dir = self.orch / self.TICKET
        self.ticket_dir.mkdir(parents=True)
        self.remote = out / 'remote.git'
        git('init', '-q', '--bare', str(self.remote), cwd=out)
        self.repo = out / 'repo'
        git('init', '-q', '-b', 'main', str(self.repo), cwd=out)
        (self.repo / 'a.txt').write_text('a\n')
        git('add', '-A', cwd=self.repo)
        git('commit', '-q', '-m', 'init', cwd=self.repo)
        git('remote', 'add', 'origin', str(self.remote), cwd=self.repo)
        git('push', '-q', 'origin', 'main', cwd=self.repo)
        self.wt = out / 'wt'
        git('worktree', 'add', '-q', '-b', self.BRANCH, str(self.wt), cwd=self.repo)
        self.prompt = self.ticket_dir / 'prompt.md'
        self.prompt.write_text('do it\n')
        self.message = self.ticket_dir / 'message.txt'
        self.message.write_text('#7 implement the thing\n')
        self.gitleaks = out / 'gitleaks'
        self.gitleaks.write_text('#!/bin/sh\nexit 0\n')
        self.gitleaks.chmod(0o755)
        self.fake_env = install_fake(out, self.WINDOWS)
        self.args_file = out / 'args.txt'

    def env(self, mode, **extra):
        env = dict(clean_env(), FAKE_MODE=mode, FAKE_LIMIT=LIMIT, FAKE_ARGS=str(self.args_file),
                   HER_ORCH=str(self.orch), HER_GITLEAKS=str(self.gitleaks),
                   GIT_AUTHOR_NAME='t', GIT_AUTHOR_EMAIL='t@example.invalid',
                   GIT_COMMITTER_NAME='t', GIT_COMMITTER_EMAIL='t@example.invalid')
        env.update(self.fake_env)
        env.update(extra)
        return {k: v for k, v in env.items() if v is not None}

    def run_work(self, kind, mode, env_extra=None, **fields):
        return subprocess.run(self.command(kind, **fields), env=self.env(mode, **(env_extra or {})),
                              capture_output=True, text=True, timeout=180)

    def implement(self, mode, env_extra=None, **over):
        fields = dict(name='run', workdir=self.wt, prompt=self.prompt, message=self.message)
        fields.update(over)
        return self.run_work('implement', mode, env_extra, **fields)

    def plan(self, mode, env_extra=None, **over):
        fields = dict(ticket=self.TICKET, name='plan', prompt=self.prompt)
        fields.update(over)
        return self.run_work('plan', mode, env_extra, **fields)

    def head(self, path):
        return git('rev-parse', 'HEAD', cwd=path).stdout.strip()

    def remote_head(self, branch):
        found = git('ls-remote', str(self.remote), f'refs/heads/{branch}', cwd=self.out).stdout.split()
        return found[0] if found else None

    def codex_args(self):
        return self.args_file.read_text().splitlines()

    # --- implement ----------------------------------------------------------------------------
    def test_implement_commits_and_pushes_the_ticket_branch_after_a_clean_run(self):
        before = self.head(self.wt)
        done = self.implement('change')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        after = self.head(self.wt)
        self.assertNotEqual(before, after)
        self.assertEqual(self.remote_head(self.BRANCH), after)
        self.assertEqual(git('log', '-1', '--format=%s', cwd=self.wt).stdout.strip(), '#7 implement the thing')
        self.assertEqual(git('show', '--name-only', '--format=', 'HEAD', cwd=self.wt).stdout.split(), ['new.txt'])
        self.assertIn(after, done.stdout)
        self.assertEqual((self.ticket_dir / 'run.push.txt').read_text().strip(), f'pushed {after}')
        self.assertEqual(git('status', '--porcelain', cwd=self.wt).stdout, '')

    def test_implement_runs_codex_confined_to_the_worktree_with_the_pinned_defaults(self):
        self.assertEqual(self.implement('change').returncode, 0)
        args = self.codex_args()
        self.assertEqual(args[args.index('-m') + 1], 'gpt-6.2-sol')
        self.assertIn('model_reasoning_effort="medium"', args)
        self.assertEqual(args[args.index('-s') + 1], 'workspace-write')
        self.assertEqual(args[args.index('-C') + 1], str(self.wt))
        self.assertNotIn('--dangerously-bypass-approvals-and-sandbox', args)
        added = [args[i + 1] for i, a in enumerate(args) if a == '--add-dir']
        self.assertEqual(added, [str(self.ticket_dir / 'tmp')])
        self.assertIn('GIT_CONFIG_KEY_0=remote.origin.pushurl', args)   # a run that reaches git cannot publish

    def test_implement_takes_model_effort_and_minutes_from_the_caller(self):
        done = self.implement('change', model='gpt-6.2-astra', effort='high', minutes='5')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        args = self.codex_args()
        self.assertEqual(args[args.index('-m') + 1], 'gpt-6.2-astra')
        self.assertIn('model_reasoning_effort="high"', args)

    def test_implement_refuses_main_and_runs_nothing(self):
        before = self.head(self.repo)
        done = self.implement('change', workdir=self.repo)
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists(), 'codex ran on main')
        self.assertEqual(self.head(self.repo), before)
        self.assertEqual(self.remote_head('main'), before)
        self.assertFalse((self.repo / 'new.txt').exists())

    def test_implement_refuses_a_branch_that_is_not_a_ticket_branch(self):
        other = self.out / 'other'
        git('worktree', 'add', '-q', '-b', 'feature/x', str(other), cwd=self.repo)
        done = self.implement('change', workdir=other)
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists())
        self.assertIsNone(self.remote_head('feature/x'))

    def test_implement_refuses_a_detached_head(self):
        other = self.out / 'detached'
        git('worktree', 'add', '-q', '--detach', str(other), cwd=self.repo)
        done = self.implement('change', workdir=other)
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists())

    def test_implement_needs_the_gitleaks_path_before_it_runs_codex(self):
        for value in (None, str(self.out / 'missing-gitleaks')):
            with self.subTest(gitleaks=value):
                done = self.implement('change', env_extra={'HER_GITLEAKS': value})
                self.assertNotEqual(done.returncode, 0)
                self.assertFalse(self.args_file.exists())
                self.assertFalse((self.wt / 'new.txt').exists())

    def test_implement_needs_a_commit_message(self):
        empty = self.ticket_dir / 'empty.txt'
        empty.write_text('')
        for message in (empty, self.ticket_dir / 'absent.txt'):
            with self.subTest(message=message.name):
                done = self.implement('change', message=message)
                self.assertNotEqual(done.returncode, 0)
                self.assertFalse(self.args_file.exists())

    def test_a_dirty_but_failed_run_commits_nothing(self):
        before = self.head(self.wt)
        done = self.implement('change-fail')
        self.assertEqual(done.returncode, 3, done.stdout + done.stderr)
        self.assertEqual((self.ticket_dir / 'run.exit.txt').read_text().strip(), '3')
        self.assertEqual(self.head(self.wt), before)
        self.assertIsNone(self.remote_head(self.BRANCH))
        self.assertEqual(git('status', '--porcelain', cwd=self.wt).stdout.split(), ['??', 'new.txt'])   # left for the coordinator
        self.assertFalse((self.ticket_dir / 'run.push.txt').exists())

    def test_a_quota_run_commits_nothing(self):
        before = self.head(self.wt)
        done = self.implement('change-quota')
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual((self.ticket_dir / 'run.exit.txt').read_text().strip(), 'quota')
        self.assertEqual(self.head(self.wt), before)
        self.assertIsNone(self.remote_head(self.BRANCH))

    def test_a_run_without_a_report_commits_nothing(self):
        before = self.head(self.wt)
        done = self.implement('crash')
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual((self.ticket_dir / 'run.exit.txt').read_text().strip(), 'no-report')
        self.assertEqual(self.head(self.wt), before)

    def test_a_clean_run_that_changed_nothing_commits_nothing(self):
        before = self.head(self.wt)
        done = self.implement('nochange')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.head(self.wt), before)
        self.assertEqual((self.ticket_dir / 'run.push.txt').read_text().strip(), 'no-changes')
        self.assertIsNone(self.remote_head(self.BRANCH))

    def test_a_failed_push_is_reported_and_keeps_the_commit(self):
        git('remote', 'set-url', 'origin', str(self.out / 'gone.git'), cwd=self.repo)
        done = self.implement('change')
        self.assertEqual(done.returncode, 7, done.stdout + done.stderr)
        self.assertEqual((self.ticket_dir / 'run.push.txt').read_text().strip(), 'push-failed')
        self.assertEqual(git('log', '-1', '--format=%s', cwd=self.wt).stdout.strip(), '#7 implement the thing')

    # --- plan ---------------------------------------------------------------------------------
    def make_scratch(self):
        scratch = self.ticket_dir / 'scratch'
        scratch.mkdir()
        (scratch / 'a.txt').write_text('a\n')
        return scratch

    def test_plan_runs_in_the_scratch_export_and_installs_the_plan(self):
        scratch = self.make_scratch()
        done = self.plan('plan')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        plan = self.ticket_dir / 'plan.md'
        self.assertEqual(plan.read_text(), '# plan\nstep 1\n')
        digest = hashlib.sha256(plan.read_bytes()).hexdigest()
        self.assertIn(f'{digest}  plan.md', done.stdout)
        self.assertEqual((self.ticket_dir / 'plan.sha256').read_text().strip(), f'{digest}  plan.md')
        self.assertFalse((self.ticket_dir / 'plan-out').exists())
        args = self.codex_args()
        self.assertEqual(args[args.index('-m') + 1], 'gpt-6.2-sol')
        self.assertIn('model_reasoning_effort="high"', args)
        self.assertEqual(args[args.index('-s') + 1], 'workspace-write')
        self.assertEqual(args[args.index('-C') + 1], str(scratch))
        added = [args[i + 1] for i, a in enumerate(args) if a == '--add-dir']
        self.assertEqual(added, [str(self.ticket_dir / 'plan-out')])
        self.assertNotIn('--dangerously-bypass-approvals-and-sandbox', args)

    def test_plan_without_a_plan_file_keeps_the_old_plan_and_fails(self):
        self.make_scratch()
        (self.ticket_dir / 'plan.md').write_text('old\n')
        done = self.plan('plan-none')
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual((self.ticket_dir / 'plan.md').read_text(), 'old\n')

    def test_a_failed_plan_run_installs_nothing(self):
        self.make_scratch()
        done = self.plan('plan-fail')
        self.assertEqual(done.returncode, 3, done.stdout + done.stderr)
        self.assertFalse((self.ticket_dir / 'plan.md').exists())

    def test_plan_needs_the_scratch_export_and_a_numeric_ticket(self):
        done = self.plan('plan')                                            # no scratch folder
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists())
        self.make_scratch()
        for ticket in ('x', '7/../8', '07x'):
            with self.subTest(ticket=ticket):
                done = self.plan('plan', ticket=ticket)
                self.assertNotEqual(done.returncode, 0)
                self.assertFalse(self.args_file.exists())

    def test_plan_refuses_a_scratch_export_that_is_a_link(self):
        real = self.out / 'elsewhere'
        real.mkdir()
        (self.ticket_dir / 'scratch').symlink_to(real, target_is_directory=True)
        done = self.plan('plan')
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists())


@unittest.skipUnless(POSIX and BASH and GIT, 'needs bash and git on a POSIX host')
class ShWork(Work, unittest.TestCase):
    def command(self, kind, **f):
        if kind == 'implement':
            cmd = [BASH, str(WORK_SH), 'implement', f['name'], str(f['workdir']), str(f['prompt']), str(f['message'])]
            return cmd + [f[k] for k in ('model', 'effort', 'minutes') if k in f]
        return [BASH, str(WORK_SH), 'plan', f['ticket'], f['name'], str(f['prompt'])]


@unittest.skipUnless(POSIX and PWSH and GIT, 'needs pwsh and git on a POSIX host')
class Ps1Work(Work, unittest.TestCase):
    WINDOWS = True

    def command(self, kind, **f):
        base = [PWSH, '-NoProfile', '-File', str(WORK_PS1), '-Mode', kind, '-Name', f['name'], '-PromptFile', str(f['prompt'])]
        if kind == 'implement':
            base += ['-WorkDir', str(f['workdir']), '-MessageFile', str(f['message'])]
        else:
            base += ['-Ticket', f['ticket']]
        for key, param in (('model', '-Model'), ('effort', '-Effort'), ('minutes', '-TimeoutMinutes')):
            if key in f:
                base += [param, f[key]]
        return base


if __name__ == '__main__':
    unittest.main()

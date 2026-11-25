#!/usr/bin/env python3
"""Fake-codex tests for scripts/codex-task and scripts/codex-work (sh and ps1).

Receipts: each case runs a launcher against a fake `codex` that writes (or does not write) the report
named by `-o`, and reads the exit receipt. A run that produced no report must never read as a pass:
a missing or empty report gives `no-report`, a usage-limit message in the error log gives `quota`.
Work modes: `implement` runs Codex in a ticket worktree (a throwaway repository with a local bare remote);
the fake commits and pushes like the real run, and the wrapper only verifies what it left. `plan` runs
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
# Paths whose change in a pushed commit the receipt flags: the hooks, the scan scripts, its config and exceptions.
PROTECTED = ['.githooks/pre-commit', '.githooks/post-commit', 'scripts/check-repo.py', 'scripts/scan-public.py',
             'scripts/tests/test_scan_public.py', '.gitleaks.toml', '.public-scan.json']

FAKE = r'''#!/usr/bin/env bash
# Fake codex: FAKE_MODE chooses what the run leaves behind; FAKE_ARGS names a file that records the call.
if [ -n "${FAKE_ARGS:-}" ]; then
  {
    printf '%s\n' "$@"
    for v in GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 HER_GITLEAKS TMPDIR TEMP TMP; do
      echo "$v=${!v:-}"
    done
    echo "PWD=$PWD"
  } > "$FAKE_ARGS"
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
# A run that edits FAKE_PATH (default new.txt), commits it and pushes the branch, as the real run does.
edit() { f=${FAKE_PATH:-new.txt}; mkdir -p "$(dirname "$workdir/$f")"; echo changed >> "$workdir/$f"; }
commit() { git -C "$workdir" add -A && git -C "$workdir" commit -q -m "${1:-fake commit}"; }
push() { git -C "$workdir" push -q "$@" origin "$(git -C "$workdir" symbolic-ref --short HEAD)"; }
case "$FAKE_MODE" in
  commit-push) edit; commit; push; printf 'done\n' > "$report" ;;
  commit-push-digits) f=${FAKE_PATH:-new.txt}; echo "id $(seq -s '' 1 9)" >> "$workdir/$f"; commit; push; printf 'done\n' > "$report" ;;
  rename) git -C "$workdir" mv "$FAKE_PATH" "$FAKE_PATH.moved"; commit; push; printf 'done\n' > "$report" ;;
  delete) git -C "$workdir" rm -q "$FAKE_PATH"; commit; push; printf 'done\n' > "$report" ;;
  symlink) rm "$workdir/$FAKE_PATH"; ln -s /etc/hostname "$workdir/$FAKE_PATH"; commit; push; printf 'done\n' > "$report" ;;
  amend-force) git -C "$workdir" commit -q --amend --allow-empty -m amended; push --force; printf 'done\n' > "$report" ;;
  reset-force) git -C "$workdir" reset -q --hard HEAD~1; push --force; printf 'done\n' > "$report" ;;
  push-reset) edit; commit; push; git -C "$workdir" reset -q --hard HEAD~1; printf 'done\n' > "$report" ;;
  common-config) git -C "$workdir" config demo.key value; edit; commit; push; printf 'done\n' > "$report" ;;
  common-hook)
    hooks="$(git -C "$workdir" rev-parse --path-format=absolute --git-common-dir)/hooks"
    mkdir -p "$hooks"; echo x > "$hooks/post-commit"; edit; commit; push; printf 'done\n' > "$report" ;;
  worktree-config)
    git -C "$workdir" config --file "$(git -C "$workdir" rev-parse --absolute-git-dir)/config.worktree" demo.key value
    edit; commit; push; printf 'done\n' > "$report" ;;
  config-entry)
    git -C "$workdir" config --file "$FAKE_CONFIG_FILE" "$FAKE_CONFIG_KEY" "$FAKE_CONFIG_VALUE" || exit 1
    printf 'done\n' > "$report" ;;
  config-fail) git -C "$workdir" config demo.key value; printf 'half\n' > "$report"; exit 3 ;;
  commit-twice) edit; commit 'first'; echo more >> "$workdir/second.txt"; commit 'second'; push; printf 'done\n' > "$report" ;;
  commit-nopush) edit; commit; printf 'done\n' > "$report" ;;
  commit-push-dirty) edit; commit; push; echo stray > "$workdir/stray.txt"; printf 'done\n' > "$report" ;;
  dirty) edit; printf 'done\n' > "$report" ;;
  move-head) edit; commit; git -C "$workdir" checkout -q --detach; printf 'done\n' > "$report" ;;
  change-fail) edit; printf 'half done\n' > "$report"; exit 3 ;;
  change-quota) edit; echo "$FAKE_LIMIT" >&2; exit 1 ;;
  nochange) printf 'nothing to do\n' > "$report" ;;
  plan-dir) mkdir "${adds[0]}/plan.md"; printf 'planned\n' > "$report" ;;
  plan-hardlink) printf '# plan\n' > "${adds[0]}/other"; ln "${adds[0]}/other" "${adds[0]}/plan.md"; printf 'planned\n' > "$report" ;;
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


def git_env():
    return dict(clean_env(), GIT_AUTHOR_NAME='t', GIT_AUTHOR_EMAIL='t@example.invalid',
                GIT_COMMITTER_NAME='t', GIT_COMMITTER_EMAIL='t@example.invalid')


def git(*args, cwd):
    return subprocess.run([GIT, *args], cwd=cwd, env=git_env(), check=True, capture_output=True, text=True)


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
        done = self.launch_sandbox(out, prompt, work, env, None)
        self.assertEqual(done.returncode, 0, done.stderr)
        args = (out / 'args.txt').read_text().splitlines()
        self.assertEqual(args[args.index('-m') + 1], 'gpt-6.2-sol')
        self.assertIn('model_reasoning_effort="high"', args)
        self.assertEqual(args[args.index('-s') + 1], 'read-only')
        self.assertNotIn('--dangerously-bypass-approvals-and-sandbox', args)
        self.assertFalse(any('exclude_slash_tmp' in a for a in args))     # workspace-write settings only

    def test_workspace_write_excludes_the_shared_temp_roots_and_adds_the_dirs(self):
        out = Path(tempfile.mkdtemp(prefix='codex-task-test-'))
        self.addCleanup(shutil.rmtree, out, ignore_errors=True)
        prompt, work, env = prepare(out, 'report')
        env.update(FAKE_ARGS=str(out / 'args.txt'))
        done = self.launch_sandbox(out, prompt, work, env, 'workspace-write', add=[out / 'a'])
        self.assertEqual(done.returncode, 0, done.stderr)
        args = (out / 'args.txt').read_text().splitlines()
        self.assertEqual(args[args.index('-s') + 1], 'workspace-write')
        for setting in ('sandbox_workspace_write.network_access=true', 'sandbox_workspace_write.exclude_slash_tmp=true',
                        'sandbox_workspace_write.exclude_tmpdir_env_var=true'):
            self.assertIn(setting, args)
        self.assertEqual([args[i + 1] for i, a in enumerate(args) if a == '--add-dir'], [str(out / 'a')])
        self.assertIn('GIT_CONFIG_COUNT=', args)                          # no push-disabling git config

    def test_the_none_sandbox_is_refused(self):
        out = Path(tempfile.mkdtemp(prefix='codex-task-test-'))
        self.addCleanup(shutil.rmtree, out, ignore_errors=True)
        prompt, work, env = prepare(out, 'report')
        env.update(FAKE_ARGS=str(out / 'args.txt'))
        done = self.launch_sandbox(out, prompt, work, env, 'none')
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse((out / 'args.txt').exists(), 'codex ran without a sandbox')


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

    def launch_sandbox(self, out, prompt, work, env, sandbox, add=()):
        env.update(install_fake(out, False))
        tail = [] if sandbox is None else ['gpt-6.2-sol', 'high', sandbox, '5', *map(str, add)]
        return subprocess.run([BASH, str(SH), 'case', str(work), str(prompt), *tail], env=env, capture_output=True,
                              text=True, timeout=120)


@unittest.skipUnless(POSIX and PWSH, 'needs pwsh on a POSIX host')
class Ps1Receipts(Receipts, unittest.TestCase):
    def test_read_only_ignores_cleanup_list(self):
        out = Path(tempfile.mkdtemp(prefix='codex-task-test-'))
        self.addCleanup(shutil.rmtree, out, ignore_errors=True)
        prompt, work, env = prepare(out, 'report')
        victim = out / 'keep.txt'
        victim.write_text('keep\n', encoding='utf-8')
        (out / 'case.cleanup.txt').write_text(f'{victim}\n', encoding='utf-8')

        done = self.launch_sandbox(out, prompt, work, env, 'read-only')

        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue(victim.is_file(), 'read-only run deleted the listed file')
        # On POSIX the Windows cleanup roots reject the path, but processing it still writes a log.
        self.assertFalse((out / 'case.cleanup.log').exists(), 'read-only run processed the cleanup list')

    def launch(self, mode, out):
        prompt, work, env = prepare(out, mode)
        env.update(install_fake(out, True))
        done = subprocess.run([PWSH, '-NoProfile', '-File', str(PS1), '-Name', 'case', '-WorkDir', str(work),
                               '-PromptFile', str(prompt), '-Sandbox', 'read-only', '-TimeoutMinutes', '1'],
                              env=env, capture_output=True, text=True, timeout=120)
        return done.returncode, 'case', done.stdout + done.stderr

    def launch_sandbox(self, out, prompt, work, env, sandbox, add=()):
        env.update(install_fake(out, True))
        cmd = [PWSH, '-NoProfile', '-File', str(PS1), '-Name', 'case', '-WorkDir', str(work), '-PromptFile', str(prompt)]
        if sandbox is not None:
            cmd += ['-Sandbox', sandbox]
        if add:
            cmd += ['-AddDir', ','.join(map(str, add))]
        return subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=120)


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
        for path in ['a.txt', *PROTECTED]:
            (self.repo / path).parent.mkdir(parents=True, exist_ok=True)
            (self.repo / path).write_text('{}\n' if path == '.public-scan.json' else 'a\n')
        git('add', '-A', cwd=self.repo)
        git('commit', '-q', '-m', 'init', cwd=self.repo)
        git('remote', 'add', 'origin', str(self.remote), cwd=self.repo)
        git('push', '-q', 'origin', 'main', cwd=self.repo)
        self.wt = out / 'wt'
        git('worktree', 'add', '-q', '-b', self.BRANCH, str(self.wt), cwd=self.repo)
        self.common = git('rev-parse', '--path-format=absolute', '--git-common-dir', cwd=self.wt).stdout.strip()
        self.prompt = self.ticket_dir / 'prompt.md'
        self.prompt.write_text('do it\n')
        self.gitleaks = out / 'gitleaks'
        self.gitleaks.write_text('#!/bin/sh\nexit 0\n')
        self.gitleaks.chmod(0o755)
        self.fake_env = install_fake(out, self.WINDOWS)
        self.args_file = out / 'args.txt'

    def env(self, mode, **extra):
        env = dict(git_env(), FAKE_MODE=mode, FAKE_LIMIT=LIMIT, FAKE_ARGS=str(self.args_file),
                   HER_ORCH=str(self.orch), HER_GITLEAKS=str(self.gitleaks), HER_REMOTE_URL=str(self.remote))
        env.update(self.fake_env)
        env.update(extra)
        return {k: v for k, v in env.items() if v is not None}

    def run_work(self, kind, mode, env_extra=None, **fields):
        return subprocess.run(self.command(kind, **fields), env=self.env(mode, **(env_extra or {})),
                              capture_output=True, text=True, timeout=180)

    def implement(self, mode, env_extra=None, **over):
        fields = dict(name='run', workdir=self.wt, prompt=self.prompt)
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

    def receipt(self, suffix='push', name='run'):
        return (self.ticket_dir / f'{name}.{suffix}.txt').read_text().splitlines()

    # --- implement ----------------------------------------------------------------------------
    def test_implement_verifies_what_codex_committed_and_pushed(self):
        before = self.head(self.wt)
        done = self.implement('commit-push')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        after = self.head(self.wt)
        self.assertNotEqual(before, after)
        self.assertEqual(self.remote_head(self.BRANCH), after)
        self.assertEqual(self.receipt(), [f'pushed {after}'])
        self.assertIn(f'pushed {after}', done.stdout)
        self.assertEqual(self.receipt('staged'), [f'commit {after} fake commit', 'A\tnew.txt'])
        args = self.codex_args()
        self.assertIn('--dangerously-bypass-approvals-and-sandbox', args)
        self.assertNotIn('-s', args)
        self.assertNotIn('--add-dir', args)
        self.assertEqual(args[args.index('-m') + 1], 'gpt-6.2-sol')
        self.assertIn('model_reasoning_effort="medium"', args)
        self.assertEqual(args[args.index('-C') + 1], str(self.wt))

    def test_the_staged_receipt_lists_every_commit_and_the_name_status_of_the_range(self):
        pre = self.head(self.wt)
        done = self.implement('commit-twice')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        log = git('log', '--format=commit %H %s', f'{pre}..HEAD', cwd=self.wt).stdout.splitlines()
        self.assertEqual(len(log), 2)
        self.assertEqual(self.receipt('staged'), [*log, 'A\tnew.txt', 'A\tsecond.txt'])

    def test_implement_gives_codex_gitleaks_a_temp_dir_and_no_push_blocking_git_config(self):
        self.assertEqual(self.implement('commit-push').returncode, 0)
        args = self.codex_args()
        self.assertIn(f'HER_GITLEAKS={self.gitleaks}', args)
        self.assertIn(f'TMPDIR={self.ticket_dir / "tmp"}', args)
        self.assertIn('GIT_CONFIG_COUNT=', args)
        self.assertIn('GIT_CONFIG_KEY_0=', args)
        self.assertIn('GIT_CONFIG_VALUE_0=', args)
        if self.WINDOWS:
            self.assertIn(f'TEMP={self.ticket_dir / "tmp"}', args)
            self.assertIn(f'TMP={self.ticket_dir / "tmp"}', args)

    def test_implement_takes_model_effort_and_minutes_from_the_caller(self):
        done = self.implement('commit-push', model='gpt-6.2-astra', effort='high', minutes='5')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        args = self.codex_args()
        self.assertEqual(args[args.index('-m') + 1], 'gpt-6.2-astra')
        self.assertIn('model_reasoning_effort="high"', args)

    def test_implement_refuses_main_and_runs_nothing(self):
        before = self.head(self.repo)
        done = self.implement('commit-push', workdir=self.repo)
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists(), 'codex ran on main')
        self.assertEqual(self.head(self.repo), before)
        self.assertEqual(self.remote_head('main'), before)

    def test_implement_refuses_a_branch_that_is_not_a_ticket_branch(self):
        other = self.out / 'other'
        git('worktree', 'add', '-q', '-b', 'feature/x', str(other), cwd=self.repo)
        done = self.implement('commit-push', workdir=other)
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists())
        self.assertIsNone(self.remote_head('feature/x'))

    def test_implement_refuses_a_detached_head(self):
        other = self.out / 'detached'
        git('worktree', 'add', '-q', '--detach', str(other), cwd=self.repo)
        done = self.implement('commit-push', workdir=other)
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists())

    def test_implement_needs_the_gitleaks_path_before_it_runs_codex(self):
        for value in (None, str(self.out / 'missing-gitleaks')):
            with self.subTest(gitleaks=value):
                done = self.implement('commit-push', env_extra={'HER_GITLEAKS': value})
                self.assertNotEqual(done.returncode, 0)
                self.assertFalse(self.args_file.exists())

    def test_implement_refuses_a_dirty_worktree_before_it_runs_codex(self):
        (self.wt / 'a.txt').write_text('edited\n')                      # a tracked change
        for stray in (None, 'untracked.txt'):
            with self.subTest(untracked=stray):
                if stray:
                    git('checkout', '--', 'a.txt', cwd=self.wt)
                    (self.wt / stray).write_text('x\n')
                done = self.implement('commit-push')
                self.assertNotEqual(done.returncode, 0)
                self.assertIn('not clean', done.stderr)
                self.assertFalse(self.args_file.exists())

    def test_implement_refuses_a_linked_ticket_folder_or_tmp(self):
        real = self.out / 'elsewhere'
        real.mkdir()
        (self.ticket_dir / 'tmp').symlink_to(real, target_is_directory=True)
        done = self.implement('commit-push')
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists())
        (self.ticket_dir / 'tmp').unlink()
        linked = self.out / 'orch-linked'
        linked.mkdir()
        (linked / self.TICKET).symlink_to(self.ticket_dir, target_is_directory=True)
        done = self.implement('commit-push', env_extra={'HER_ORCH': str(linked)})
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists())

    def test_a_failed_run_is_not_verified(self):
        before = self.head(self.wt)
        done = self.implement('change-fail')
        self.assertEqual(done.returncode, 3, done.stdout + done.stderr)
        self.assertEqual(self.receipt('exit'), ['3'])
        self.assertEqual(self.head(self.wt), before)
        self.assertFalse((self.ticket_dir / 'run.push.txt').exists())
        self.assertFalse((self.ticket_dir / 'run.staged.txt').exists())

    def test_a_quota_run_is_not_verified(self):
        done = self.implement('change-quota')
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual(self.receipt('exit'), ['quota'])
        self.assertFalse((self.ticket_dir / 'run.push.txt').exists())

    def test_a_run_without_a_report_is_not_verified(self):
        done = self.implement('crash')
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual(self.receipt('exit'), ['no-report'])
        self.assertFalse((self.ticket_dir / 'run.push.txt').exists())

    def test_a_clean_run_with_no_commit_is_no_changes(self):
        before = self.head(self.wt)
        done = self.implement('nochange')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.head(self.wt), before)
        self.assertEqual(self.receipt(), ['no-changes'])
        self.assertEqual(self.receipt('staged'), [])
        self.assertIsNone(self.remote_head(self.BRANCH))

    def test_uncommitted_changes_after_the_run_are_dirty(self):
        done = self.implement('dirty')
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), ['dirty new.txt', 'remote unchanged'])

    def test_a_stray_file_after_a_pushed_commit_is_dirty_and_the_remote_moved(self):
        done = self.implement('commit-push-dirty')
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), ['dirty stray.txt', 'remote moved'])

    def test_a_dirty_run_that_touched_a_protected_path_still_says_so(self):
        done = self.implement('commit-push-dirty', env_extra={'FAKE_PATH': '.githooks/pre-commit'})
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), ['dirty stray.txt', 'remote moved', 'protected .githooks/pre-commit'])

    def test_an_unpushed_run_that_touched_a_protected_path_still_says_so(self):
        done = self.implement('commit-nopush', env_extra={'FAKE_PATH': 'scripts/scan-public.py'})
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), ['unpushed', 'protected scripts/scan-public.py'])

    def test_a_remote_that_is_not_the_fixed_url_does_not_count(self):
        other = self.out / 'other.git'
        git('init', '-q', '--bare', str(other), cwd=self.out)
        git('remote', 'set-url', 'origin', str(other), cwd=self.repo)     # the run pushes to origin, not to the fixed URL
        done = self.implement('commit-push')
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), ['unpushed'])
        self.assertIsNone(self.remote_head(self.BRANCH))

    def test_implement_refuses_a_remote_that_does_not_answer(self):
        done = self.implement('commit-push', env_extra={'HER_REMOTE_URL': str(self.out / 'gone.git')})
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse(self.args_file.exists())

    def branch_commit(self):
        """A pushed commit on the ticket branch before the run, so the run starts above main."""
        (self.wt / 'b1.txt').write_text('b1\n')
        git('add', '-A', cwd=self.wt)
        git('commit', '-q', '-m', 'b1', cwd=self.wt)
        git('push', '-q', 'origin', self.BRANCH, cwd=self.wt)

    def test_an_amend_with_a_force_push_is_rewritten(self):
        self.branch_commit()
        done = self.implement('amend-force')
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), ['rewritten'])

    def test_a_reset_with_a_force_push_is_rewritten(self):
        self.branch_commit()
        done = self.implement('reset-force')
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), ['rewritten'])

    def test_a_push_followed_by_a_reset_to_the_start_is_remote_moved(self):
        self.branch_commit()
        before = self.head(self.wt)
        done = self.implement('push-reset')
        self.assertEqual(self.head(self.wt), before)
        self.assertNotEqual(self.remote_head(self.BRANCH), before)
        self.assertEqual(done.returncode, 8, self.receipt())
        self.assertEqual(self.receipt(), ['remote-moved'])

    def test_implement_refuses_a_branch_ahead_of_or_behind_the_remote(self):
        self.branch_commit()
        remote = self.remote_head(self.BRANCH)
        for relation in ('ahead', 'behind'):
            with self.subTest(relation=relation):
                git('reset', '-q', '--hard', remote, cwd=self.wt)
                if relation == 'ahead':
                    git('commit', '-q', '--allow-empty', '-m', 'local only', cwd=self.wt)
                else:
                    git('reset', '-q', '--hard', 'HEAD~1', cwd=self.wt)
                self.args_file.unlink(missing_ok=True)
                done = self.implement('nochange')
                self.assertEqual(done.returncode, 8, self.receipt())
                self.assertEqual(self.receipt(), ['pre-remote-mismatch'])
                self.assertFalse(self.args_file.exists())

    def test_a_new_commit_on_an_existing_remote_branch_passes_the_added_scan(self):
        self.branch_commit()
        done = self.implement('commit-push')
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), [f'pushed {self.head(self.wt)}'])
        self.assertIn('Public scan (added)', (self.ticket_dir / 'run.scan.txt').read_text())

    def test_a_commit_the_public_scan_refuses_is_flagged(self):
        done = self.implement('commit-push-digits')
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), [f'pushed {self.head(self.wt)}', 'scan-refused'])
        self.assertIn('digit', (self.ticket_dir / 'run.scan.txt').read_text())

    def config_changed(self, mode, expected, env_extra=None):
        done = self.implement(mode, env_extra)
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        lines = self.receipt()
        self.assertEqual(len(lines), 1, lines)                       # nothing else is checked
        self.assertTrue(lines[0].startswith('config-changed '), lines)
        self.assertIn(expected, lines[0])

    def test_a_changed_common_git_config_is_reported(self):
        self.config_changed('common-config', f'{self.common}/config')

    def test_branch_tracking_changes_are_ignored_in_both_configs(self):
        wtgit = git('rev-parse', '--absolute-git-dir', cwd=self.wt).stdout.strip()
        for path in (f'{self.common}/config', f'{wtgit}/config.worktree'):
            with self.subTest(path=path):
                done = self.implement('config-entry', dict(
                    FAKE_CONFIG_FILE=path, FAKE_CONFIG_KEY='branch.other.merge', FAKE_CONFIG_VALUE='refs/heads/main'))
                self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
                self.assertEqual(self.receipt(), ['no-changes'])

    def test_a_changed_fsmonitor_setting_is_reported(self):
        path = f'{self.common}/config'
        self.config_changed('config-entry', path, dict(
            FAKE_CONFIG_FILE=path, FAKE_CONFIG_KEY='core.fsmonitor', FAKE_CONFIG_VALUE=str(self.out / 'absent')))

    def test_a_changed_include_path_is_reported(self):
        path = f'{self.common}/config'
        self.config_changed('config-entry', path, dict(
            FAKE_CONFIG_FILE=path, FAKE_CONFIG_KEY='include.path', FAKE_CONFIG_VALUE=str(self.out / 'absent')))

    def test_a_new_hook_in_the_common_git_dir_is_reported(self):
        self.config_changed('common-hook', f'{self.common}/hooks/post-commit')

    def test_a_new_worktree_config_is_reported(self):
        self.config_changed('worktree-config', 'config.worktree')

    def test_a_changed_git_config_is_reported_even_when_the_run_failed(self):
        self.config_changed('config-fail', f'{self.common}/config')

    def test_an_unchanged_config_does_not_mask_a_failed_run(self):
        done = self.implement('change-fail')
        self.assertEqual(done.returncode, 3, done.stdout + done.stderr)
        self.assertFalse((self.ticket_dir / 'run.push.txt').exists())

    def test_a_commit_that_was_not_pushed_is_unpushed(self):
        done = self.implement('commit-nopush')
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), ['unpushed'])
        self.assertIsNone(self.remote_head(self.BRANCH))

    def test_a_head_that_moved_during_the_run_is_a_branch_change(self):
        done = self.implement('move-head')
        self.assertEqual(done.returncode, 8, done.stdout + done.stderr)
        self.assertEqual(self.receipt(), ['branch-changed detached'])

    def test_a_pushed_range_that_touches_a_protected_path_is_flagged(self):
        for path in [*PROTECTED, '.githooks/new-hook', '.GitHooks/x', 'scripts/Scan-Public.py']:
            with self.subTest(path=path):
                git('checkout', '-q', '-B', self.BRANCH, 'main', cwd=self.wt)
                if self.remote_head(self.BRANCH):
                    git('push', '-q', 'origin', f':refs/heads/{self.BRANCH}', cwd=self.wt)
                done = self.implement('commit-push', env_extra={'FAKE_PATH': path})
                self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
                sha = self.head(self.wt)
                self.assertEqual(self.receipt(), [f'pushed {sha}', f'protected {path}'])

    def test_a_renamed_deleted_or_symlinked_protected_path_is_flagged(self):
        cases = [('rename', 'scripts/check-repo.py', 'scripts/check-repo.py'),
                 ('delete', '.gitleaks.toml', '.gitleaks.toml'),
                 ('symlink', '.githooks/pre-commit', '.githooks/pre-commit')]
        for mode, path, flagged in cases:
            with self.subTest(mode=mode):
                git('checkout', '-q', '-B', self.BRANCH, 'main', cwd=self.wt)
                if self.remote_head(self.BRANCH):
                    git('push', '-q', 'origin', f':refs/heads/{self.BRANCH}', cwd=self.wt)
                done = self.implement(mode, env_extra={'FAKE_PATH': path})
                self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
                self.assertEqual(self.receipt(), [f'pushed {self.head(self.wt)}', f'protected {flagged}'])

    def test_an_unprotected_path_is_not_flagged(self):
        done = self.implement('commit-push', env_extra={'FAKE_PATH': 'scripts/other.py'})
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertEqual(len(self.receipt()), 1)

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
        self.assertEqual(plan.stat().st_nlink, 1)
        digest = hashlib.sha256(plan.read_bytes()).hexdigest()
        self.assertIn(f'{digest}  plan.md', done.stdout)
        self.assertEqual((self.ticket_dir / 'plan.sha256').read_text().strip(), f'{digest}  plan.md')
        self.assertEqual(self.receipt('plan', 'plan'), ['installed'])
        self.assertFalse((self.ticket_dir / 'plan-out').exists())
        self.assertEqual([p.name for p in self.ticket_dir.iterdir() if p.name.startswith('.plan')], [])
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
        self.assertEqual(self.receipt('plan', 'plan'), ['no-plan'])

    def old_plan(self):
        self.make_scratch()
        (self.ticket_dir / 'plan.md').write_text('old\n')
        (self.ticket_dir / 'plan.sha256').write_text('oldhash  plan.md\n')

    def assert_old_plan_kept(self):
        self.assertEqual((self.ticket_dir / 'plan.md').read_text(), 'old\n')
        self.assertEqual((self.ticket_dir / 'plan.sha256').read_text(), 'oldhash  plan.md\n')
        self.assertEqual([p.name for p in self.ticket_dir.iterdir() if p.name.startswith('.plan')], [])

    def test_a_plan_that_is_a_directory_is_not_installed(self):
        self.old_plan()
        done = self.plan('plan-dir')
        self.assertNotEqual(done.returncode, 0)
        self.assert_old_plan_kept()
        self.assertEqual(self.receipt('plan', 'plan'), ['no-plan'])

    def test_a_plan_with_a_second_hard_link_is_not_installed(self):
        self.old_plan()
        done = self.plan('plan-hardlink')
        self.assertNotEqual(done.returncode, 0)
        self.assert_old_plan_kept()
        self.assertEqual(self.receipt('plan', 'plan'), ['no-plan'])

    def test_a_half_failed_install_puts_the_old_plan_back(self):
        self.make_scratch()
        (self.ticket_dir / 'plan.md').write_text('old\n')
        (self.ticket_dir / 'plan.sha256').mkdir()                        # the second step of the install cannot succeed
        done = self.plan('plan')
        self.assertNotEqual(done.returncode, 0)
        self.assertEqual((self.ticket_dir / 'plan.md').read_text(), 'old\n')
        self.assertEqual([p.name for p in self.ticket_dir.iterdir() if p.name.startswith('.plan')], [])
        self.assertEqual(self.receipt('plan', 'plan'), ['no-plan'])

    def test_a_half_failed_first_install_leaves_no_plan(self):
        self.make_scratch()
        (self.ticket_dir / 'plan.sha256').mkdir()
        done = self.plan('plan')
        self.assertNotEqual(done.returncode, 0)
        self.assertFalse((self.ticket_dir / 'plan.md').exists())
        self.assertEqual(self.receipt('plan', 'plan'), ['no-plan'])

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
            cmd = [BASH, str(WORK_SH), 'implement', f['name'], str(f['workdir']), str(f['prompt'])]
            return cmd + [f[k] for k in ('model', 'effort', 'minutes') if k in f]
        return [BASH, str(WORK_SH), 'plan', f['ticket'], f['name'], str(f['prompt'])]


@unittest.skipUnless(POSIX and PWSH and GIT, 'needs pwsh and git on a POSIX host')
class Ps1Work(Work, unittest.TestCase):
    WINDOWS = True

    def command(self, kind, **f):
        base = [PWSH, '-NoProfile', '-File', str(WORK_PS1), '-Mode', kind, '-Name', f['name'], '-PromptFile', str(f['prompt'])]
        base += ['-WorkDir', str(f['workdir'])] if kind == 'implement' else ['-Ticket', f['ticket']]
        for key, param in (('model', '-Model'), ('effort', '-Effort'), ('minutes', '-TimeoutMinutes')):
            if key in f:
                base += [param, f[key]]
        return base


class DefaultRemoteUrl(unittest.TestCase):
    """The repository is named `H.E.R.`: git strips a trailing `.git`, so `.../H.E.R.git` names `H.E.R`."""

    def test_both_wrappers_default_to_the_real_repository(self):
        for name in ('codex-work.sh', 'codex-work.ps1'):
            text = (ROOT / 'scripts' / name).read_text(encoding='utf-8')
            self.assertIn('https://github.com/bubbleofmoon/H.E.R..git', text, name)
            self.assertNotIn('bubbleofmoon/H.E.R.git', text, name)


if __name__ == '__main__':
    unittest.main()

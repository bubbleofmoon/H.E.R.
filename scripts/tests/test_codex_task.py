#!/usr/bin/env python3
"""Fake-codex tests for scripts/codex-task (sh and ps1).

Receipts: each case runs a launcher against a fake `codex` that writes (or does not write) the report
named by `-o`, and reads the exit receipt. A run that produced no report must never read as a pass:
a missing or empty report gives `no-report`, a usage-limit message in the error log gives `quota`.
The sh cases put the fake first on PATH; the ps1 cases run under pwsh with APPDATA pointing at a
fake package folder that holds the fake as codex.exe. POSIX only: the fake is a shell script.
Run: python -I scripts/tests/test_codex_task.py
"""
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
BASH = shutil.which('bash')
PWSH = shutil.which('pwsh')
POSIX = sys.platform != 'win32'
LIMIT = "ERROR: You've hit your usage limit. Try again later."

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


if __name__ == '__main__':
    unittest.main()

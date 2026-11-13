#!/usr/bin/env python3
"""Run every scripts/test-*.py that is not already a cargo test target, one result line each.

Linux only: the scripts use fcntl, inotify, prctl, /bin/sh shims and a C compiler, and the four
that exercise PowerShell entries drive them through pwsh. Run it after run-tests.py has built the
bins; the executable defaults to $CARGO_TARGET_DIR/debug/her. A script file that is neither in
RUN nor in EXEMPT fails the run, so a new test script cannot stay unrun.
"""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import time

sys.stdout.reconfigure(encoding='utf-8', errors='replace')

root = Path(__file__).resolve().parents[2]
scripts = root / 'scripts'
TIMEOUT = 300

# (script, extra args after the executable, takes the executable). One entry per scenario.
RUN = [
    ('test-candidate-build.py', [], False),
    ('test-console-restart.py', [], True),
    ('test-cross-root-owner.py', [], True),
    ('test-lifecycle-fake-resident.py', [], False),
    ('test-lifecycle-fixture-encoding.py', [], False),
    ('test-lifecycle-fixture-pinning.py', [], False),
    ('test-lifecycle-guard.py', [], True),
    ('test-lifecycle-revision.py', [], True),
    *[('test-orphan-job-recovery.py', [s], True) for s in ('crash', 'close-fds', 'normal', 'threaded-normal', 'threaded-crash')],
    ('test-session-reset-cli.py', [], True),
    ('test-start-stop-race.py', [], True),
    ('test-stop-broken-config.py', [], True),
    ('test-stop-entry.py', [], False),
    *[('test-stop-review.py', [s], True) for s in ('incomplete-config', 'no-supervisor', 'no-supervisor-web', 'supervised-web')],
    ('test-stop-running-job.py', [], True),
    ('test-stop-task-gate-order.py', [], True),
    ('test-task-lifecycle-revocation.py', [], True),
    ('test-task-spawn-record-race.py', [], True),
    ('test-task-spawn-record-race.py', ['close-fds'], True),
    *[('test-task-store-redirect.py', [s], True) for s in ('locks', 'logs')],
    ('test-task-store-swap-race.py', [], True),
    ('test-watchdog-resume-race.py', [], True),
    *[('test-watchdog-start-result.py', [s], True) for s in ('invalid-groups', 'early-exit')],
]

# Scripts this runner does not run, with the reason and where they run instead.
EXEMPT = {
    'test-release-proof.py': 'native Windows lifecycle proof, local gate (docs/agents/rust-validation.md); her/tests/install_root.rs runs its evidence guard',
    'test-release-chains.py': 'native Windows lifecycle proof, local gate; her/tests/install_root.rs runs its evidence guard',
    'test-release-timeout.py': 'native Windows job tree; her/tests/install_root.rs affected_native_script_checks_use_disposable_roots',
    'test-lifecycle-window-observer.py': 'native Windows observer; her/tests/install_root.rs affected_native_script_checks_use_disposable_roots',
}

ap = argparse.ArgumentParser()
ap.add_argument('--exe', type=Path, help='built her executable (default: $CARGO_TARGET_DIR/debug/her)')
ap.add_argument('--evidence', type=Path, default=Path(os.environ.get('CI_EVIDENCE', 'ci-evidence')))
args = ap.parse_args()

if sys.platform != 'linux':
    print('run-script-tests.py runs on Linux only', file=sys.stderr)
    sys.exit(2)
target = Path(os.environ.get('CARGO_TARGET_DIR', root.parent / 'her-ci-target'))
exe = (args.exe or target / 'debug/her').resolve()
cli = exe.with_name('her-cli')
for binary in (exe, cli):
    if not binary.is_file():
        print(f'missing built binary {binary}; run scripts/ci/run-tests.py first', file=sys.stderr)
        sys.exit(2)

listed = {name for name, _, _ in RUN} | set(EXEMPT)
on_disk = {path.name for path in scripts.glob('test-*.py')}
problems = [f'{name}: not in RUN or EXEMPT' for name in sorted(on_disk - listed)]
problems += [f'{name}: listed but missing' for name in sorted(listed - on_disk)]
if problems:
    print('\n'.join(problems), file=sys.stderr)
    sys.exit(2)

ev = args.evidence.resolve() / 'script-tests'
ev.mkdir(parents=True, exist_ok=True)
env = os.environ.copy()
env.setdefault('HER_INSTALL_ROOT', str(ev / 'unused-install-root'))
env.setdefault('HER_TASK_SCHEDULER_FILE', str(ev / 'task-scheduler.xml'))
env['HER_TEST_CLI'] = str(cli)
# her/build.ps1 reads USERPROFILE, which Linux does not set.
env.setdefault('USERPROFILE', str(ev / 'userprofile'))

failed = []
for name, extra, takes_exe in RUN:
    label = ' '.join([name, *extra])
    log = ev / (label.replace(' ', '.').removeprefix('test-').removesuffix('.py') + '.log')
    # No -I: the scripts import their sibling fixture_paths.py from the script directory.
    command = [sys.executable, str(scripts / name), *([str(exe)] if takes_exe else []), *extra]
    started = time.monotonic()
    try:
        result = subprocess.run(command, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=TIMEOUT)
        code, output = result.returncode, result.stdout + result.stderr
    except subprocess.TimeoutExpired as timeout:
        code, output = 124, (timeout.stdout or b'') + (timeout.stderr or b'') + f'\ntimed out after {TIMEOUT}s\n'.encode()
    log.write_bytes(output)
    seconds = time.monotonic() - started
    lines = output.decode('utf-8', errors='replace').splitlines()
    print(f'{"PASS" if code == 0 else "FAIL"} {label} ({seconds:.1f}s, exit {code}): {lines[-1] if lines else ""}', flush=True)
    if code:
        failed.append(label)
        for line in lines[-40:]:
            print('    ' + line, flush=True)
print(f'Ran {len(RUN)} script tests; failures: ' + (', '.join(failed) or 'none'), flush=True)
sys.exit(1 if failed else 0)

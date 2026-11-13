#!/usr/bin/env python3
"""Run the build, unit suite and all integration targets, retaining every exit.

Shared Windows host: --slot-ticket 03 --evidence <external-directory>.
Hosted runners omit --slot-ticket; the Cargo commands and test flags are identical.
On Linux without a slot, a last target `scripts` runs scripts/ci/run-script-tests.py against the
`her` and `her-cli` that the `build` step produced, copied aside because later `cargo test`
invocations with other features rebuild target/debug/her.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

# Failure logs can contain Unicode even when the Windows console uses an ANSI code page.
sys.stdout.reconfigure(encoding='utf-8', errors='replace')
sys.stderr.reconfigure(encoding='utf-8', errors='replace')

root = Path(__file__).resolve().parents[2]
ap = argparse.ArgumentParser()
ap.add_argument('--slot-ticket')
ap.add_argument('--slot-script', type=Path)
ap.add_argument('--slot-worktree', type=Path)
ap.add_argument('--evidence', type=Path, default=Path(os.environ.get('CI_EVIDENCE', 'ci-evidence')))
args = ap.parse_args()
if (args.slot_script or args.slot_worktree) and not args.slot_ticket:
    ap.error('--slot-script and --slot-worktree require --slot-ticket')
ev = args.evidence.resolve()
ev.mkdir(parents=True, exist_ok=True)
env = os.environ.copy()
if os.name == 'nt':
    actual = subprocess.check_output(['whoami'], text=True).strip().split('\\')[-1]
    print(f'USERNAME inherited={env.get("USERNAME")} actual={actual}', flush=True)
    env['USERNAME'] = actual
env.setdefault('CARGO_TARGET_DIR', str(root.parent / 'her-ci-target'))
env.setdefault('HER_INSTALL_ROOT', str(ev / 'unused-install-root'))
env.setdefault('HER_TASK_SCHEDULER_FILE', str(ev / 'task-scheduler.xml'))
commands = [('build', 'build --locked --features parity-clock --bins'), ('unit', 'test --locked --features parity-clock --bin her -- --test-threads=1')]
for path in sorted((root / 'her/tests').glob('*.rs')):
    # Cargo skips a test target whose required-features are absent, and a skip is not a pass:
    # release_install needs both features so the signed fake update feed compiles and runs.
    feature = ' --features parity-clock,release-test-keys' if path.stem == 'release_install' else ' --features parity-clock'
    commands.append((path.stem, f'test --locked{feature} --test {path.stem} -- --test-threads=1'))
run_scripts = sys.platform == 'linux' and not args.slot_ticket
if run_scripts:
    commands.append(('scripts', 'scripts/ci/run-script-tests.py'))
script_bins = ev / 'script-bins'
# Only binaries copied by this invocation's build count; an earlier run's copy must never be used.
snapshot_ok = False
if run_scripts:
    shutil.rmtree(script_bins, ignore_errors=True)
results = []
for name, command in commands:
    print(f'RUN {name}: ' + ('python ' if name == 'scripts' else 'cargo ') + command, flush=True)
    if args.slot_ticket:
        slot_command = command
        if args.slot_worktree:
            subcommand, rest = command.split(' ', 1)
            slot_command = subcommand + ' --manifest-path "' + str(root / 'her/Cargo.toml') + '" ' + rest
        cmd = ['powershell.exe', '-NoProfile', '-File', str(args.slot_script or root / 'scripts/cargo-slot.ps1'),
               '-Ticket', args.slot_ticket, '-WorkTree', str(args.slot_worktree or root), '-EvidenceDir', str(ev),
               '-RunName', name, '-Cargo', slot_command]
        result = subprocess.run(cmd, env=env, creationflags=subprocess.CREATE_NO_WINDOW)
        code = result.returncode
        logs = [ev / (name + '.stdout.log'), ev / (name + '.stderr.log')]
    else:
        logs = [ev / (name + '.stdout.log'), ev / (name + '.stderr.log')]
        cmd = ['cargo', *command.split()]
        if name == 'scripts':
            cmd = [sys.executable, '-I', str(root / command), '--exe', str(script_bins / 'her'), '--evidence', str(ev)]
        with logs[0].open('wb') as out, logs[1].open('wb') as err:
            if name == 'scripts' and not snapshot_ok:
                err.write(b'scripts: unavailable: no snapshot from this run\n')
                code = 1
            else:
                code = subprocess.run(cmd, cwd=root / 'her', env=env, stdout=out, stderr=err,
                                      creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0).returncode
        (ev / (name + '.exit.txt')).write_text(str(code) + '\n')
        if name == 'build' and not code and run_scripts:
            try:
                # Cargo runs in her/, so a relative CARGO_TARGET_DIR is relative to that directory.
                target = root / 'her' / env['CARGO_TARGET_DIR'] / 'debug'
                script_bins.mkdir(parents=True, exist_ok=True)
                for binary in ('her', 'her-cli'):
                    shutil.copy2(target / binary, script_bins / binary)
                snapshot_ok = True
            except OSError as error:
                message = f'scripts-snapshot: cannot copy the built binaries from {target}: {error}'
                print(message, flush=True)
                try:
                    (ev / 'scripts-snapshot.stderr.log').write_text(message + '\n', encoding='utf-8')
                except OSError:
                    pass
                results.append({'target': 'scripts-snapshot', 'command': 'snapshot of her and her-cli', 'invocation': [], 'exit': 1})
                print('EXIT scripts-snapshot: 1', flush=True)
    results.append({'target': name, 'command': command, 'invocation': cmd, 'exit': code})
    failure_lines = []
    fallback_lines = []
    for log in logs:
        if log.exists():
            lines = log.read_text(encoding='utf-8', errors='replace').splitlines()
            for line in lines:
                if line.startswith('test result:') or (name == 'scripts' and line.startswith(('PASS ', 'FAIL ', 'Ran '))):
                    print(name + ': ' + line, flush=True)
            if code:
                fallback_lines.extend([f'--- {log.name} ---', *lines])
                for index, line in enumerate(lines):
                    if line.strip() == 'failures:':
                        failure_lines.extend([f'--- {log.name} ---', *lines[index:]])
                        break
    if code:
        print(f'FAILURE DETAILS {name} (last 200 lines):', flush=True)
        for line in (failure_lines or fallback_lines)[-200:]:
            print(line, flush=True)
        print(f'END FAILURE DETAILS {name}', flush=True)
    print(f'EXIT {name}: {code}', flush=True)
    (ev / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
print(f'Completed {len(results)} commands; failures: ' + ', '.join(r['target'] for r in results if r['exit']), flush=True)
sys.exit(any(r['exit'] for r in results))

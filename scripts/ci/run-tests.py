#!/usr/bin/env python3
"""Run the build, unit suite and all integration targets, retaining every exit.

Shared Windows host: --slot-ticket 03 --evidence <external-directory>.
Hosted runners omit --slot-ticket; the Cargo commands and test flags are identical.
"""
import argparse
import json
import os
from pathlib import Path
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
results = []
for name, command in commands:
    print(f'RUN {name}: cargo {command}', flush=True)
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
        with logs[0].open('wb') as out, logs[1].open('wb') as err:
            result = subprocess.run(['cargo', *command.split()], cwd=root / 'her', env=env,
                                    stdout=out, stderr=err,
                                    creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
        code = result.returncode
        (ev / (name + '.exit.txt')).write_text(str(code) + '\n')
    results.append({'target': name, 'command': command, 'invocation': cmd if args.slot_ticket else ['cargo', *command.split()], 'exit': code})
    failure_lines = []
    fallback_lines = []
    for log in logs:
        if log.exists():
            lines = log.read_text(encoding='utf-8', errors='replace').splitlines()
            for line in lines:
                if line.startswith('test result:'):
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

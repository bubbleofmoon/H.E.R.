#!/usr/bin/env python3
"""Exercise the local rollback guard with disposable gates and a synthetic child."""
import fcntl
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import fixture_paths as tempfile
import time

exe = Path(sys.argv[1]).resolve()
with tempfile.TemporaryDirectory(prefix='ticket27-guard-') as tmp:
    root = Path(tmp)
    for name in ('data/lifecycle', 'data/logs'):
        (root / name).mkdir(parents=True)
    desired = root / 'data/lifecycle/desired-state.json'
    def state(value):
        desired.write_text(json.dumps({'v':1, 'state':value, 'by':'fixture', 'reason':'', 'at':1, 'revision':1}))
    marker = root / 'ran'
    command = [str(exe), 'lifecycle', 'guard', '--runtime-root', str(root), '--',
               '/bin/sh', '-c', f'touch {shlex.quote(str(marker))}; sleep 8; exit 17']
    state('running')
    refused = subprocess.run(command, capture_output=True, text=True, timeout=12)
    assert refused.returncode != 0 and not marker.exists(), 'running state must refuse the guarded action'
    state('stopped')
    held_path = root / 'data/logs/rbridge-live.lock'
    with held_path.open('w') as held:
        fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
        refused = subprocess.run(command, capture_output=True, text=True, timeout=12)
        assert refused.returncode != 0 and not marker.exists(), 'held owner must refuse the guarded action'
    logs = root / 'data/logs'
    saved_logs = root / 'data/saved-logs'
    outside = root / 'outside'
    outside.mkdir()
    logs.rename(saved_logs)
    logs.symlink_to(outside, target_is_directory=True)
    refused = subprocess.run(command, capture_output=True, text=True, timeout=12)
    assert refused.returncode != 0 and not marker.exists() and not list(outside.iterdir()), 'redirected owner gates must be refused'
    logs.unlink()
    saved_logs.rename(logs)
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline and not marker.exists() and process.poll() is None:
        time.sleep(0.02)
    assert marker.exists(), f'stopped/free guard did not run its child: {process.communicate(timeout=10)}'
    for path in (root / 'data/lifecycle/lifecycle.lock', root / 'data/logs/rbridge-live.lock',
                 root / 'data/logs/rbridge-console.lock', root / 'data/logs/rbridge-supervisor.lock'):
        with path.open('r+') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                pass
            else:
                raise AssertionError(f'guard did not retain {path.name}')
    stdout, stderr = process.communicate(timeout=20)
    assert process.returncode == 17, (process.returncode, stdout, stderr)
    print('guard refuses running/held owners, retains all gates during the child, and propagates its exit')

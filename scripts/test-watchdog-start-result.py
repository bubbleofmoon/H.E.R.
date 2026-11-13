#!/usr/bin/env python3
"""A rejected synthetic live start must not produce a successful watchdog run."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import fixture_paths as tempfile
import time

exe = str(Path(sys.argv[1]).resolve())
case = sys.argv[2] if len(sys.argv) > 2 else 'invalid-groups'
assert case in ('invalid-groups', 'early-exit')
with tempfile.TemporaryDirectory(prefix='ticket27-watchdog-result-') as tmp:
    root = Path(tmp)
    for name in ('config','data','resources'):
        (root/name).mkdir()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    groups = ['not-a-canonical-key'] if case == 'invalid-groups' else ['group:1']
    (root/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
        'backends':{'mcpRustBridge':True},'rollout':{'rustGroups':groups}}))
    (root/'data/social-v2.json').write_text('{}')
    (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    process = subprocess.Popen([exe,'supervise','--runtime-root',str(root)],stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL,start_new_session=True)
    try:
        history = root/'data/tasks/runs.jsonl'
        deadline = time.monotonic()+35
        while time.monotonic()<deadline:
            events = [json.loads(line) for line in history.read_text().splitlines()] if history.exists() else []
            runs = [event['runId'] for event in events if event.get('taskId') == 'watchdog']
            terminal = [event for event in events if event.get('runId') in runs and event.get('state') in ('succeeded','failed','unknown')]
            if terminal:
                break
            time.sleep(.1)
        else:
            raise AssertionError('fixture watchdog did not complete')
        assert terminal[-1]['state'] == 'failed', f'watchdog recorded success for a rejected live start: {terminal[-1]}'
        print(f'a rejected live start produces a failed watchdog run: {case}')
    finally:
        try:
            os.killpg(process.pid,signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=10)

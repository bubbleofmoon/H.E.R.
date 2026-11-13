#!/usr/bin/env python3
"""A synthetic supervisor must stop its console even after config becomes unreadable."""
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
with tempfile.TemporaryDirectory(prefix='ticket27-broken-config-') as tmp:
    root = Path(tmp)
    for name in ('config', 'data', 'resources'):
        (root / name).mkdir()
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        port = s.getsockname()[1]
    cfg = root / 'config/config.json'
    cfg.write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,'backends':{'mcpRustBridge':True},'rollout':{'rustGroups':[]}}))
    (root / 'data/social-v2.json').write_text('{}')
    (root / 'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    def listening():
        try:
            with socket.create_connection(('127.0.0.1', port), timeout=.3):
                return True
        except OSError:
            return False
    def wait(predicate, description):
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            if predicate(): return
            time.sleep(.1)
        raise AssertionError(description)
    supervisor = subprocess.Popen([exe,'supervise','--runtime-root',str(root)], stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        wait(listening, 'console did not start')
        cfg.write_text('{broken')
        state = root / 'data/lifecycle/desired-state.json'
        state.parent.mkdir(exist_ok=True)
        pending = state.with_suffix('.tmp')
        pending.write_text(json.dumps({'v':1,'state':'stopped','by':'fixture','reason':'','at':1,'revision':1}))
        os.replace(pending, state)
        wait(lambda: not listening(), 'console stayed running after stop with unreadable config')
        print('stopped state retires the verified console even when configuration cannot be read')
    finally:
        try: os.killpg(supervisor.pid, signal.SIGKILL)
        except ProcessLookupError: pass
        supervisor.wait(timeout=10)

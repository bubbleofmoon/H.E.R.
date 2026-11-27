#!/usr/bin/env python3
"""Revoke a console credential while a lifecycle request waits for the config gate."""
import ctypes
import fcntl
import http.client
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import fixture_paths as tempfile
import threading
import time

exe = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ticket27-task-revoke-') as tmp:
    root = Path(tmp)
    for folder in ('config', 'data', 'resources', 'data/locks'):
        (root / folder).mkdir(parents=True, exist_ok=True)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    cfg = root / 'config/config.json'
    config = {'consoleToken':'fixture-token','consolePort':port,
              'backends':{'mcpRustBridge':True},'rollout':{'rustGroups':[]}}
    cfg.write_text(json.dumps(config))
    (root / 'data/social-v2.json').write_text('{}')
    (root / 'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')

    def request(method, path, token, body=None):
        conn = http.client.HTTPConnection('127.0.0.1', port, timeout=12)
        headers = {'x-console-token':token, 'Content-Type':'application/json', 'Origin':f'http://127.0.0.1:{port}'}
        try:
            conn.request(method, path, json.dumps(body) if body is not None else None, headers)
            resp = conn.getresponse()
            return resp.status, json.loads(resp.read())
        finally:
            conn.close()

    process = subprocess.Popen([exe,'--console-serve','--console-port',str(port),'--runtime-root',str(root)],
                               stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
    worker = None
    notify = -1
    try:
        deadline = time.monotonic() + 20
        while True:
            try:
                if request('GET','/api/health','fixture-token')[0] == 200:
                    break
            except OSError:
                pass
            assert time.monotonic() < deadline, 'fixture console did not start'
            time.sleep(.05)
        gate = open(root / 'data/locks/social-v2.lock', 'a+')
        with gate:
            fcntl.flock(gate, fcntl.LOCK_EX)
            libc = ctypes.CDLL(None, use_errno=True)
            notify = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
            assert notify >= 0
            assert libc.inotify_add_watch(notify, os.fsencode(gate.name), 0x20) >= 0
            result = []
            def submit():
                try:
                    result.append(request('POST','/api/tasks/lifecycle','fixture-token',{'state':'stopped','confirm':True}))
                except Exception as error:
                    result.append(error)
            worker = threading.Thread(target=submit, daemon=True)
            worker.start()
            deadline = time.monotonic() + 8
            gate_opened = False
            while time.monotonic() < deadline and not result:
                ready, _, _ = select.select([notify], [], [], .05)
                if ready:
                    os.read(notify, 4096)
                    gate_opened = True
                    break
            assert gate_opened and not result, f'lifecycle request did not wait at the config gate: {result}'
            config['consoleToken'] = 'replacement-token'
            cfg.write_text(json.dumps(config))
            fcntl.flock(gate, fcntl.LOCK_UN)
        worker.join(timeout=10)
        assert len(result) == 1 and isinstance(result[0], tuple) and result[0][0] == 401, result
        assert not (root / 'data/lifecycle/desired-state.json').exists(), 'revoked request changed lifecycle state'
        print('revoked console credential cannot mutate lifecycle state after waiting for the config gate')
    finally:
        if notify >= 0:
            os.close(notify)
        if worker is not None:
            worker.join(timeout=12)
        try:
            os.killpg(process.pid,signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=10)

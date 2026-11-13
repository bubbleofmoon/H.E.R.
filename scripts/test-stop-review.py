#!/usr/bin/env python3
"""Exercise stop with synthetic owners after config damage or supervisor loss."""
import json
import http.client
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import fixture_paths as tempfile
import time

exe = str(Path(sys.argv[1]).resolve())
case = sys.argv[2]
assert case in ('incomplete-config', 'no-supervisor', 'no-supervisor-web', 'supervised-web')
with tempfile.TemporaryDirectory(prefix='ticket27-stop-review-') as tmp:
    root = Path(tmp)
    for name in ('config', 'data', 'resources'):
        (root / name).mkdir()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    cfg = root / 'config/config.json'
    cfg.write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
                               'backends':{'mcpRustBridge':True},'rollout':{'rustGroups':[]}}))
    (root / 'data/social-v2.json').write_text('{}')
    (root / 'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')

    def listening():
        try:
            with socket.create_connection(('127.0.0.1', port), timeout=.3):
                return True
        except OSError:
            return False

    def web_stop():
        conn=http.client.HTTPConnection('127.0.0.1',port,timeout=3)
        try:
            conn.request('POST','/api/tasks/lifecycle',json.dumps({'state':'stopped','confirm':True}),
                         {'x-console-token':'fixture-token','Content-Type':'application/json','Origin':f'http://127.0.0.1:{port}'})
            response=conn.getresponse()
            return response.status,json.loads(response.read())
        finally: conn.close()

    def wait(predicate, description):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(.1)
        raise AssertionError(description)

    supervisor = subprocess.Popen([exe, 'supervise', '--runtime-root', str(root)],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        wait(listening, 'fixture console did not start')
        if case == 'incomplete-config':
            cfg.write_text(json.dumps({'consoleToken':'fixture-token', 'consolePort':port,
                                       'backends':{'mcpRustBridge':True}}))
            state = root / 'data/lifecycle/desired-state.json'
            state.parent.mkdir(exist_ok=True)
            pending = state.with_suffix('.tmp')
            pending.write_text(json.dumps({'v':1,'state':'stopped','by':'fixture','reason':'','at':1,'revision':1}))
            os.replace(pending, state)
        elif case == 'supervised-web':
            status, body = web_stop()
            assert status == 202 and body.get('applied') is False and body.get('state') == 'pending', (status,body)
        else:
            supervisor.kill()
            supervisor.wait(timeout=10)
            assert listening(), 'fixture console exited with the supervisor'
            if case == 'no-supervisor-web':
                status,body=web_stop()
                if status == 200:
                    wait(lambda: not listening(), 'web stop returned 200 without stopping the surviving console')
                else:
                    assert status == 503 and body.get('applied') is False,(status,body)
                    assert listening(), 'not-applied response lost the console'
                    assert not (root/'data/lifecycle/desired-state.json').exists(), 'not-applied stop changed desired state'
                print('web stop is enforced or explicitly not applied after supervisor loss')
            else:
                result = subprocess.run([exe, 'lifecycle', 'set', 'stopped', '--runtime-root', str(root),
                                         '--confirm', '--by', 'fixture'], capture_output=True, text=True, timeout=15)
                assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
        if case != 'no-supervisor-web':
            wait(lambda: not listening(), f'console stayed running after stop: {case}')
            print(f'stopped state retires the verified console: {case}')
    finally:
        try:
            os.killpg(supervisor.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        supervisor.wait(timeout=10)

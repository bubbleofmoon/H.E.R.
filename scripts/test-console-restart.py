#!/usr/bin/env python3
"""Run the public restart route against a synthetic supervisor and loopback-only runtime."""
import http.client
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
with tempfile.TemporaryDirectory(prefix='ticket27-restart-') as tmp:
    root = Path(tmp)
    for folder in ('config', 'data', 'resources'):
        (root / folder).mkdir()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    (root / 'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
        'backends':{'mcpRustBridge':True},'rollout':{'rustGroups':[]}}))
    (root / 'data/social-v2.json').write_text('{}')
    (root / 'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    def request(method, path, body=None, extra=None):
        conn = http.client.HTTPConnection('127.0.0.1', port, timeout=2)
        headers = {'x-console-token':'fixture-token','Content-Type':'application/json','Origin':f'http://127.0.0.1:{port}'}
        if extra:
            for name, value in extra.items():
                if value is None: headers.pop(name, None)
                else: headers[name] = value
        try:
            conn.request(method, path, json.dumps(body) if body is not None else None, headers)
            response = conn.getresponse()
            return response.status, json.loads(response.read())
        finally:
            conn.close()
    def wait_for(predicate, message):
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            try:
                value = predicate()
                if value:
                    return value
            except (OSError, ValueError, http.client.HTTPException):
                pass
            time.sleep(.1)
        raise AssertionError(message)
    process = subprocess.Popen([exe, 'supervise', '--runtime-root', str(root)], stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        old = wait_for(lambda: request('GET','/api/health')[1]['pid'], 'supervisor did not start fixture console')
        body = {'requestId':'restart-fixture-0001','confirm':True}
        assert request('POST', '/api/restart', body, {'Host':'evil.example'})[0] == 403
        assert request('POST', '/api/restart', body, {'x-console-token':None})[0] == 401
        assert request('POST', '/api/restart', body, {'Content-Type':'text/plain'})[0] == 415
        assert request('POST', '/api/restart', body, {'Origin':'https://evil.example'})[0] == 403
        assert request('POST', '/api/restart', body, {'x-agent-token':'fixture-agent'})[0] == 403
        assert request('POST', '/api/restart', {'requestId':'restart-fixture-0001'})[0] == 400
        status, response = request('POST', '/api/restart', {'requestId':'restart-fixture-0001','confirm':True})
        assert status == 202, f'restart must queue with status 202: {status} {response}'
        duplicate = request('POST', '/api/restart', body)
        assert duplicate[0] == 202 and duplicate[1].get('duplicate') is True, duplicate
        assert request('POST', '/api/restart', {'requestId':'restart-fixture-0002','confirm':True})[0] == 409
        new = wait_for(lambda: (pid if (pid := request('GET','/api/health')[1]['pid']) != old else None), 'console was not restarted')
        assert new != old
        def completed_history():
            history = request('GET','/api/tasks/runs')[1]
            return history if any(r['taskId']=='console-restart' and r['state']=='succeeded' for r in history['runs']) else None
        history = wait_for(completed_history, 'restart has no successful run record')
        assert any(r['taskId']=='console-restart' and r['source']=='manual' for r in history['runs'])
        audit = request('GET', '/api/audit/recent')[1]
        assert any(entry.get('path') == '/api/restart' for entry in audit['entries']), audit
        print('supervisor restarts exactly the selected console and records the completed manual run')
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=10)

#!/usr/bin/env python3
"""Exercise the fake residents used by the isolated Windows lifecycle fixture."""
import http.client
import json
from pathlib import Path
import socket
import subprocess
import fixture_paths as tempfile
import time

source = Path(__file__).resolve().parent / 'lifecycle-fake-resident.mjs'
assert source.is_file(), 'the Windows fixture must have a standalone synthetic resident'
with tempfile.TemporaryDirectory(prefix='ticket27-fake-residents-') as tmp:
    base = Path(tmp)
    processes = []
    try:
        for role in ('onebot', 'dsh'):
            with socket.socket() as sock:
                sock.bind(('127.0.0.1', 0)); port = sock.getsockname()[1]
            folder = base / role; folder.mkdir()
            script = folder / ('index.mjs' if role == 'onebot' else 'bin.mjs')
            script.write_bytes(source.read_bytes())
            (folder / 'fixture-resident.json').write_text(json.dumps({'role': role, 'port': port}))
            child = subprocess.Popen(['node', str(script)], stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL, start_new_session=True)
            processes.append(child)
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                try:
                    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=1)
                    connection.request('POST', '/', '{}', {'Content-Type': 'application/json'})
                    response = connection.getresponse(); body = json.loads(response.read()); connection.close()
                    if response.status == 200: break
                except OSError: time.sleep(.05)
            else: raise AssertionError(f'{role} fake did not start')
            if role == 'onebot':
                assert body['status'] == 'ok' and body['retcode'] == 0
                with socket.create_connection(('127.0.0.1', port), timeout=2) as conn:
                    conn.sendall(b'GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n')
                    assert b'101 Switching Protocols' in conn.recv(4096)
    finally:
        for process in processes:
            process.terminate()
            try: process.wait(timeout=5)
            except subprocess.TimeoutExpired: process.kill(); process.wait(timeout=5)
print('synthetic DSH and OneBot HTTP/WebSocket residents start on selected loopback ports')

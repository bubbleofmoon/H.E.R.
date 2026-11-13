#!/usr/bin/env python3
"""Two selected roots must not claim the same synthetic gateway and conversation."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import fixture_paths as tempfile
import threading
import time

exe=str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ticket27-source-owner-') as tmp:
    base=Path(tmp)
    stop=threading.Event()
    class Fake(BaseHTTPRequestHandler):
        protocol_version='HTTP/1.1'
        def log_message(self,*_args): pass
        def do_GET(self):
            key=self.headers.get('Sec-WebSocket-Key')
            if not key:
                self.send_response(503); self.send_header('Content-Length','0'); self.end_headers(); return
            accept=base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
            self.send_response(101); self.send_header('Upgrade','websocket'); self.send_header('Connection','Upgrade')
            self.send_header('Sec-WebSocket-Accept',accept); self.end_headers()
            self.connection.settimeout(.2)
            while not stop.is_set():
                try:
                    if not self.connection.recv(4096): break
                except socket.timeout: pass
                except OSError: break
        def do_POST(self):
            length=int(self.headers.get('Content-Length','0')); self.rfile.read(length)
            payload=b'{"status":"ok","retcode":0,"data":{"good":true}}'
            self.send_response(200); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(payload)))
            self.end_headers(); self.wfile.write(payload)
    server=ThreadingHTTPServer(('127.0.0.1',0),Fake)
    thread=threading.Thread(target=server.serve_forever,daemon=True); thread.start()
    port=server.server_port; endpoint=f'http://127.0.0.1:{port}'; ws=f'ws://127.0.0.1:{port}'
    processes=[]; output_files=[]
    try:
        for name in ('one','two','other'):
            root=base/name
            group='group:2' if name=='other' else 'group:1'
            for directory in ('config','data','resources','user/dsh'): (root/directory).mkdir(parents=True,exist_ok=True)
            (root/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','ownerQQ':1,
                'allow':{'groups':[1,2],'private':[]},'backends':{'mcpRustBridge':False},
                'rollout':{'rustGroups':[group]}}))
            (root/'data/social-v2.json').write_text('{}')
            (root/'user/dsh/.credentials.yaml').write_text('secret: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"\n')
            (root/'dependencies.json').write_text(json.dumps({'dsh':endpoint,'model':endpoint,'onebot':endpoint,'onebot_ws':ws}))
            out=open(root/'output.txt','w+'); output_files.append(out)
            process=subprocess.Popen([exe,'--live','--live-groups',group,'--console-port',str(port),
                                      '--runtime-root',str(root)],stdout=out,stderr=out,start_new_session=True)
            processes.append(process)
            heartbeat=root/'data/rust-heartbeat-live.json'; deadline=time.monotonic()+20
            while time.monotonic()<deadline and not heartbeat.exists() and process.poll() is None: time.sleep(.05)
            if name=='one' and not heartbeat.exists():
                out.flush(); raise AssertionError(f'first synthetic owner did not become live: {(root/"output.txt").read_text()}')
            if name=='two':
                assert not heartbeat.exists(),'two selected roots both claimed the same gateway and conversation'
            if name=='other': assert heartbeat.exists(),'a distinct conversation must remain available'
        processes[0].kill(); processes[0].wait(timeout=10)
        root=base/'two'; out=open(root/'retry-output.txt','w+'); output_files.append(out)
        retry=subprocess.Popen([exe,'--live','--live-groups','group:1','--console-port',str(port),'--runtime-root',str(root)],
                               stdout=out,stderr=out,start_new_session=True)
        processes.append(retry); deadline=time.monotonic()+20
        while time.monotonic()<deadline and not (root/'data/rust-heartbeat-live.json').exists() and retry.poll() is None: time.sleep(.05)
        assert (root/'data/rust-heartbeat-live.json').exists(),'a crashed owner did not release its source gate'
        print('one selected root owns the synthetic gateway and conversation')
    finally:
        stop.set()
        for process in processes:
            if process.poll() is None: process.kill()
            process.wait(timeout=10)
        for out in output_files: out.close()
        server.shutdown(); server.server_close(); thread.join(timeout=5)

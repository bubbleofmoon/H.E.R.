#!/usr/bin/env python3
"""Task execution must not follow a redirected task-store descendant."""
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
case = sys.argv[2]
assert case in ('locks','logs')
with tempfile.TemporaryDirectory(prefix='ticket27-task-redirect-') as tmp:
    base=Path(tmp); root=base/'runtime'; decoy=base/'outside'
    for path in (root/'config',root/'data/tasks',root/'resources',root/'install/memory',decoy):
        path.mkdir(parents=True)
    marker=decoy/'sentinel'; marker.write_text('unchanged')
    (root/'data/tasks'/case).symlink_to(decoy,target_is_directory=True)
    fake=base/'fake-node'; fake.write_text('#!/bin/sh\nprintf fixture-output\nexit 0\n'); fake.chmod(0o755)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    (root/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
        'tasks':{'nodePath':str(fake),'installRoot':str(root/'install')}}))
    (root/'data/social-v2.json').write_text('{}')
    (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    process=subprocess.Popen([exe,'--console-serve','--console-port',str(port),'--runtime-root',str(root)],
                             stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
    def request(method,path,body=None):
        conn=http.client.HTTPConnection('127.0.0.1',port,timeout=2)
        try:
            conn.request(method,path,json.dumps(body) if body is not None else None,
                         {'x-console-token':'fixture-token','Content-Type':'application/json','Origin':f'http://127.0.0.1:{port}'})
            response=conn.getresponse()
            return response.status,json.loads(response.read())
        finally: conn.close()
    try:
        deadline=time.monotonic()+25
        while True:
            try:
                if request('GET','/api/health')[0]==200: break
            except OSError: pass
            assert time.monotonic()<deadline,'fixture console did not start'
            time.sleep(.1)
        status,body=request('POST','/api/tasks/backup/run',{'requestId':f'redirect-{case}-001'})
        if status==202:
            run_id=body['runId']; deadline=time.monotonic()+20
            while time.monotonic()<deadline:
                status,view=request('GET',f'/api/tasks/runs/{run_id}')
                if status!=200 or view['run']['state'] not in ('queued','running'): break
                time.sleep(.1)
        assert list(decoy.iterdir())==[marker] and marker.read_text()=='unchanged',f'task store wrote outside the root through {case}'
        print(f'task store refuses redirected {case} without touching the decoy')
    finally:
        try: os.killpg(process.pid,signal.SIGKILL)
        except ProcessLookupError: pass
        process.wait(timeout=10)

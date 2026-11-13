#!/usr/bin/env python3
"""A desired stop cancels a synthetic job already in progress."""
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

exe=str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ticket27-running-job-') as tmp:
    base=Path(tmp); root=base/'runtime'
    for name in ('config','data','resources','install/memory'):
        (root/name).mkdir(parents=True,exist_ok=True)
    started=base/'started'; completed=base/'completed'
    fake=base/'fake-node'
    fake.write_text('#!/usr/bin/env python3\nimport os,time\nfrom pathlib import Path\nPath(os.environ["TICKET27_STARTED"]).write_text("started")\ntime.sleep(4)\nPath(os.environ["TICKET27_COMPLETED"]).write_text("completed")\n')
    fake.chmod(0o755)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    (root/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
        'tasks':{'nodePath':str(fake),'installRoot':str(root/'install')}}))
    (root/'data/social-v2.json').write_text('{}')
    (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    env=os.environ.copy(); env.update({'TICKET27_STARTED':str(started),'TICKET27_COMPLETED':str(completed)})
    process=subprocess.Popen([exe,'--console-serve','--console-port',str(port),'--runtime-root',str(root)],
                             stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True,env=env)
    child_pid=None; child_handle=None
    def request(method,path,body=None):
        conn=http.client.HTTPConnection('127.0.0.1',port,timeout=2)
        try:
            conn.request(method,path,json.dumps(body) if body is not None else None,
                         {'x-console-token':'fixture-token','Content-Type':'application/json','Origin':f'http://127.0.0.1:{port}'})
            response=conn.getresponse(); return response.status,json.loads(response.read())
        finally: conn.close()
    try:
        deadline=time.monotonic()+25
        while True:
            try:
                if request('GET','/api/health')[0]==200: break
            except OSError: pass
            assert time.monotonic()<deadline,'fixture console did not start'
            time.sleep(.1)
        status,body=request('POST','/api/tasks/upstream-check/run',{'requestId':'stop-running-job-001'})
        assert status==202,(status,body)
        run_id=body['runId']; deadline=time.monotonic()+15
        while time.monotonic()<deadline:
            view=request('GET',f'/api/tasks/runs/{run_id}')[1]['run']
            if started.exists() and view.get('pid'):
                child_pid=int(view['pid']); break
            time.sleep(.05)
        assert child_pid,'fixture job did not start'
        child_handle=os.pidfd_open(child_pid)
        stop=subprocess.run([exe,'lifecycle','set','stopped','--runtime-root',str(root),'--confirm','--by','fixture'],
                            stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=10)
        assert stop.returncode==0,'fixture stop request failed'
        deadline=time.monotonic()+5
        while time.monotonic()<deadline and not completed.exists(): time.sleep(.05)
        assert not completed.exists(),'running job continued after stop'
        print('desired stop cancels an already running synthetic job')
    finally:
        if child_handle is not None:
            try: signal.pidfd_send_signal(child_handle,signal.SIGKILL)
            except ProcessLookupError: pass
            os.close(child_handle)
        try: os.killpg(process.pid,signal.SIGKILL)
        except ProcessLookupError: pass
        process.wait(timeout=10)

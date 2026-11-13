#!/usr/bin/env python3
"""A recovered task must not overlap a child surviving its synthetic parent crash."""
import ctypes
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
scenario = sys.argv[2] if len(sys.argv) > 2 else 'crash'
threaded = scenario in ('threaded-normal', 'threaded-crash')
close_fds = scenario == 'close-fds' or threaded
normal = scenario in ('normal', 'threaded-normal')
assert ctypes.CDLL(None,use_errno=True).prctl(36,1,0,0,0)==0  # PR_SET_CHILD_SUBREAPER
with tempfile.TemporaryDirectory(prefix='ticket27-orphan-job-') as tmp:
    base=Path(tmp); root=base/'runtime'
    for name in ('config','data','resources','install/memory'): (root/name).mkdir(parents=True,exist_ok=True)
    marker=base/'started'; parent_marker=base/'parent'; release=base/'release'; fake=base/'fake-node'
    # The direct child may be killed when its runner dies; its same-group
    # descendant deliberately remains alive and must keep the task gate held.
    fake.write_text(
        '#!/usr/bin/env python3\nimport os,time\n'
        + ('os.closerange(3,256)\n' if close_fds or normal else '')
        + 'with open(os.environ["TICKET27_ORPHAN_PARENT"],"w") as f: f.write(str(os.getpid()))\n'
        + 'if os.fork()==0:\n'
        + (' import ctypes,threading\n os.closerange(0,256)\n'
           ' threading.Thread(target=time.sleep,args=(60,)).start()\n' if threaded else '')
        + ' with open(os.environ["TICKET27_ORPHAN_MARKER"],"a") as f: f.write(str(os.getpid())+"\\n")\n'
        + (' ctypes.CDLL(None).pthread_exit(None)\n' if threaded else '')
        + ('else:\n' + (
            ' deadline=time.monotonic()+15\n'
            ' while not os.path.exists(os.environ["TICKET27_ORPHAN_RELEASE"]) and time.monotonic()<deadline: time.sleep(.01)\n'
            ' if not os.path.exists(os.environ["TICKET27_ORPHAN_RELEASE"]): os._exit(2)\n'
            if threaded else '') + ' os._exit(0)\n' if normal else '')
        + 'time.sleep(30)\n')
    fake.chmod(0o755)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    (root/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
        'tasks':{'nodePath':str(fake),'installRoot':str(root/'install')}}))
    (root/'data/social-v2.json').write_text('{}')
    (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    env=os.environ.copy(); env['TICKET27_ORPHAN_MARKER']=str(marker); env['TICKET27_ORPHAN_PARENT']=str(parent_marker)
    env['TICKET27_ORPHAN_RELEASE']=str(release)
    consoles=[]; handles={}
    def console():
        p=subprocess.Popen([exe,'--console-serve','--console-port',str(port),'--runtime-root',str(root)],
                           stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True,env=env)
        consoles.append(p); deadline=time.monotonic()+20
        while time.monotonic()<deadline:
            try:
                if request('GET','/api/health')[0]==200: return p
            except OSError: pass
            time.sleep(.05)
        raise AssertionError('fixture console did not start')
    def request(method,path,body=None):
        conn=http.client.HTTPConnection('127.0.0.1',port,timeout=2)
        try:
            conn.request(method,path,json.dumps(body) if body is not None else None,
                {'x-console-token':'fixture-token','Content-Type':'application/json','Origin':f'http://127.0.0.1:{port}'})
            response=conn.getresponse(); return response.status,json.loads(response.read())
        finally: conn.close()
    def pids():
        return [int(line) for line in marker.read_text().splitlines()] if marker.exists() else []
    try:
        first=console()
        status, started = request('POST','/api/tasks/upstream-check/run',{'requestId':'orphan-first-001'})
        assert status==202
        deadline=time.monotonic()+15
        while not pids() and time.monotonic()<deadline: time.sleep(.05)
        assert pids(),'fixture task did not start'
        child=pids()[0]; parent=int(parent_marker.read_text()); handles[child]=os.pidfd_open(child)
        if threaded:
            # Hold the normal task leader until its descendant has a zombie
            # main thread and a live worker, so completion sees that exact state.
            deadline=time.monotonic()+10; states={}
            while time.monotonic()<deadline:
                states={stat.parent.name:stat.read_text().rsplit(')',1)[1].split()[0]
                        for stat in Path(f'/proc/{child}/task').glob('*/stat')}
                if states.get(str(child))=='Z' and any(state not in ('Z','X') for state in states.values()): break
                time.sleep(.05)
            assert states.get(str(child))=='Z' and any(state not in ('Z','X') for state in states.values()), (
                'fixture descendant did not retain a live worker after main-thread exit',states)
            print('fixture descendant main thread is Z with a live worker',flush=True)
            release.touch()
        if normal:
            deadline=time.monotonic()+10
            while time.monotonic()<deadline:
                run=request('GET',f'/api/tasks/runs/{started["runId"]}')[1]['run']
                if run['state'] not in ('queued','running'): break
                time.sleep(.05)
            assert run['state']=='unknown', ('a normal leader exit falsely completed a surviving tree',run)
            print('normal leader exit is unknown while a descendant survives',flush=True)
        else:
            handles[parent]=os.pidfd_open(parent)
            first.kill(); first.wait(timeout=5)
            console()
        status,body=request('POST','/api/tasks/upstream-check/run',{'requestId':'orphan-second-001'})
        if status==202:
            deadline=time.monotonic()+10
            while len(pids())<2 and time.monotonic()<deadline: time.sleep(.05)
        assert status==409,('a live orphan must block a second task',status,body)
        assert pids()==[child],'a second task overlapped the orphan'
        print('surviving descendant blocks overlap with HTTP 409',flush=True)
        stopped=subprocess.run([exe,'lifecycle','set','stopped','--confirm','--runtime-root',str(root)],
                               capture_output=True,text=True,timeout=20)
        assert stopped.returncode != 0, ('stop succeeded while a recorded task group survived',stopped.stdout,stopped.stderr)
        assert json.loads((root/'data/lifecycle/desired-state.json').read_text())['state']=='stopped'
        print('stop remains unresolved while a descendant survives',flush=True)
        for pid in handles:
            try: signal.pidfd_send_signal(handles[pid],signal.SIGKILL)
            except ProcessLookupError: pass
        for pid in handles:
            deadline=time.monotonic()+5
            while time.monotonic()<deadline:
                if os.waitpid(pid,os.WNOHANG)[0]==pid: break
                time.sleep(.05)
            else: raise AssertionError('synthetic orphan did not exit')
        resumed=subprocess.run([exe,'lifecycle','set','running','--runtime-root',str(root)],capture_output=True,text=True,timeout=10)
        assert resumed.returncode==0,resumed.stderr
        deadline=time.monotonic()+10
        while time.monotonic()<deadline:
            status,body=request('POST','/api/tasks/upstream-check/run',{'requestId':'orphan-after-exit-001'})
            if status==202: break
            time.sleep(.1)
        assert status==202,('task did not become available after the orphan exited',status,body)
        deadline=time.monotonic()+10
        while len(pids())<2 and time.monotonic()<deadline: time.sleep(.05)
        assert len(pids())==2,'replacement task did not start'
        print('a surviving tree blocks overlap and successful stop until it has exited: '+scenario)
    finally:
        for pid in pids():
            if pid not in handles:
                try: handles[pid]=os.pidfd_open(pid)
                except ProcessLookupError: pass
        for pid,handle in handles.items():
            try: signal.pidfd_send_signal(handle,signal.SIGKILL)
            except ProcessLookupError: pass
            os.close(handle)
        for process in consoles:
            if process.poll() is None: process.kill()
            process.wait(timeout=5)
        for pid in handles:
            deadline=time.monotonic()+5
            while time.monotonic()<deadline:
                try:
                    if os.waitpid(pid,os.WNOHANG)[0]==pid: break
                except ChildProcessError: break
                time.sleep(.05)
            else: raise AssertionError('synthetic orphan cleanup did not finish')

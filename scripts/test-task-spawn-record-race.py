#!/usr/bin/env python3
"""A job cannot execute before its PID record commits, even if it closes inherited FDs."""
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
import threading
import time

exe=str(Path(sys.argv[1]).resolve())
close_fds = len(sys.argv) > 2 and sys.argv[2] == 'close-fds'
assert ctypes.CDLL(None,use_errno=True).prctl(36,1,0,0,0)==0
with tempfile.TemporaryDirectory(prefix='ticket27-spawn-record-') as tmp:
    base=Path(tmp); root=base/'runtime'
    for folder in ('config','data','resources','install/memory'): (root/folder).mkdir(parents=True,exist_ok=True)
    marker=base/'children'; fake=base/'fake-node'
    fake.write_text('#!/usr/bin/env python3\nimport os,time\n'+('os.closerange(3,256)\n' if close_fds else '')+'with open(os.environ["TICKET27_CHILDREN"],"a") as f: f.write(str(os.getpid())+"\\n")\nif os.fork()==0:\n with open(os.environ["TICKET27_CHILDREN"],"a") as f: f.write(str(os.getpid())+"\\n")\ntime.sleep(30)\n'); fake.chmod(0o755)
    with socket.socket() as sock: sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    (root/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
        'tasks':{'nodePath':str(fake),'installRoot':str(root/'install')}}))
    (root/'data/social-v2.json').write_text('{}')
    (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    source=base/'barrier.c'; source.write_text(r'''
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
ssize_t write(int fd,const void *data,size_t count) {
 static ssize_t (*real_write)(int,const void*,size_t);
 static int once;
 if(!real_write) real_write=dlsym(RTLD_NEXT,"write");
 const char *needle="\"state\":\"running\"";
 if(memmem(data,count,needle,strlen(needle)) && __sync_bool_compare_and_swap(&once,0,1)) {
  int marker=open(getenv("TICKET27_RECORD_NOTIFY"),O_CREAT|O_WRONLY|O_TRUNC,0600);
  if(marker>=0) close(marker);
  const char *release=getenv("TICKET27_RECORD_RELEASE");
  for(int i=0;i<1000 && access(release,F_OK)!=0;i++) usleep(10000);
 }
 return real_write(fd,data,count);
}
''')
    shim=base/'barrier.so'; subprocess.run(['cc','-shared','-fPIC','-o',str(shim),str(source),'-ldl'],check=True,capture_output=True,timeout=30)
    notify=base/'notify'; release=base/'release'; env=os.environ.copy(); env['TICKET27_CHILDREN']=str(marker)
    env.update({'LD_PRELOAD':str(shim),'TICKET27_RECORD_NOTIFY':str(notify),'TICKET27_RECORD_RELEASE':str(release)})
    consoles=[]; worker=None; handles={}
    def request(method,path,body=None):
        conn=http.client.HTTPConnection('127.0.0.1',port,timeout=12)
        try:
            conn.request(method,path,json.dumps(body) if body is not None else None,
                {'x-console-token':'fixture-token','Content-Type':'application/json','Origin':f'http://127.0.0.1:{port}'})
            response=conn.getresponse(); return response.status,json.loads(response.read())
        finally: conn.close()
    def console(environment):
        p=subprocess.Popen([exe,'--console-serve','--console-port',str(port),'--runtime-root',str(root)],
                           stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True,env=environment)
        consoles.append(p); deadline=time.monotonic()+20
        while time.monotonic()<deadline:
            try:
                if request('GET','/api/health')[0]==200: return p
            except OSError: pass
            time.sleep(.05)
        raise AssertionError('fixture console did not start')
    def children(): return [int(line) for line in marker.read_text().splitlines()] if marker.exists() else []
    try:
        first=console(env)
        worker=threading.Thread(target=lambda: request('POST','/api/tasks/upstream-check/run',{'requestId':'before-record-001'}),daemon=True)
        worker.start(); deadline=time.monotonic()+15
        while not notify.exists() and time.monotonic()<deadline: time.sleep(.02)
        assert notify.exists(),'fixture did not reach PID recording'
        # The record is blocked before its write. User code must still be held,
        # so no descendant can outlive an unrecorded process group.
        time.sleep(1)
        assert not children(),'the job executed before its PID record committed'
        first.kill(); first.wait(timeout=5); worker.join(timeout=15)
        clean_env=env.copy(); clean_env.pop('LD_PRELOAD',None)
        console(clean_env)
        status,body=request('POST','/api/tasks/upstream-check/run',{'requestId':'after-record-001'})
        assert status==202,('an unstarted lost job must permit a new run',status,body)
        deadline=time.monotonic()+15
        while len(children())<2 and time.monotonic()<deadline: time.sleep(.02)
        assert len(children())==2,'the replacement job did not execute exactly once'
        run=request('GET',f'/api/tasks/runs/{body["runId"]}')[1]['run']
        assert run['pid']==children()[0] and run['state']=='running',run
        print('jobs wait for their PID record; a pre-record crash starts no orphan and a replacement runs once')
    finally:
        release.touch()
        for child in children():
            if child not in handles:
                try: handles[child]=os.pidfd_open(child)
                except ProcessLookupError: pass
        for handle in handles.values():
            try: signal.pidfd_send_signal(handle,signal.SIGKILL)
            except ProcessLookupError: pass
            os.close(handle)
        for p in consoles:
            if p.poll() is None: p.kill()
            p.wait(timeout=5)
        for child in handles:
            try: os.waitpid(child,os.WNOHANG)
            except ChildProcessError: pass
        if worker is not None: worker.join(timeout=3)

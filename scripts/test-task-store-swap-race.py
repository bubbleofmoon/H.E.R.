#!/usr/bin/env python3
"""Interleave a task lock open with replacement of its checked parent directory."""
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

exe = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ticket27-task-swap-') as tmp:
    base=Path(tmp); root=base/'runtime'; outside=base/'outside'
    for folder in (root/'config', root/'data', root/'resources', root/'install/memory', outside):
        folder.mkdir(parents=True)
    fake=base/'fake-node'; fake.write_text('#!/bin/sh\nexit 0\n'); fake.chmod(0o755)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    (root/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
        'tasks':{'nodePath':str(fake),'installRoot':str(root/'install')}}))
    (root/'data/social-v2.json').write_text('{}')
    (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    source=base/'barrier.c'
    source.write_text(r'''
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static int once;
static void barrier(const char *path) {
 if(path && strstr(path,"/backup.lock") && __sync_bool_compare_and_swap(&once,0,1)) {
  const char *notify=getenv("TICKET27_SWAP_NOTIFY"), *release=getenv("TICKET27_SWAP_RELEASE");
  int (*real_open)(const char*,int,...)=dlsym(RTLD_NEXT,"open");
  int fd=real_open(notify,O_WRONLY|O_CREAT|O_TRUNC,0600); if(fd>=0) close(fd);
  for(int i=0;i<1000 && access(release,F_OK)!=0;i++) usleep(10000);
 }
}
int open64(const char *path,int flags,...) {
 int (*real_open)(const char*,int,...)=dlsym(RTLD_NEXT,"open64");
 va_list ap; va_start(ap,flags); int mode=(flags&O_CREAT)?va_arg(ap,int):0; va_end(ap);
 barrier(path); return real_open(path,flags,mode);
}
int open(const char *path,int flags,...) {
 int (*real_open)(const char*,int,...)=dlsym(RTLD_NEXT,"open");
 va_list ap; va_start(ap,flags); int mode=(flags&O_CREAT)?va_arg(ap,int):0; va_end(ap);
 barrier(path); return real_open(path,flags,mode);
}
''')
    shim=base/'barrier.so'
    subprocess.run(['cc','-shared','-fPIC','-o',str(shim),str(source),'-ldl'],check=True,capture_output=True,timeout=30)
    notify=base/'notify'; release=base/'release'; env=os.environ.copy()
    env.update({'LD_PRELOAD':str(shim),'TICKET27_SWAP_NOTIFY':str(notify),'TICKET27_SWAP_RELEASE':str(release)})
    process=subprocess.Popen([exe,'--console-serve','--console-port',str(port),'--runtime-root',str(root)],
                             stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True,env=env)
    def request(method,path,body=None):
        conn=http.client.HTTPConnection('127.0.0.1',port,timeout=15)
        try:
            conn.request(method,path,json.dumps(body) if body is not None else None,
                         {'x-console-token':'fixture-token','Content-Type':'application/json','Origin':f'http://127.0.0.1:{port}'})
            response=conn.getresponse(); response.read(); return response.status
        finally: conn.close()
    worker=None
    try:
        deadline=time.monotonic()+25
        while time.monotonic()<deadline:
            try:
                if request('GET','/api/health')==200: break
            except OSError: pass
            time.sleep(.05)
        else: raise AssertionError('fixture console did not start')
        worker=threading.Thread(target=lambda: request('POST','/api/tasks/backup/run',{'requestId':'swap-race-001'}),daemon=True)
        worker.start(); deadline=time.monotonic()+15
        while not notify.exists() and time.monotonic()<deadline: time.sleep(.01)
        assert notify.exists(),'fixture did not reach the task lock open'
        locks=root/'data/tasks/locks'; locks.rename(locks.with_name('locks-original'))
        locks.symlink_to(outside,target_is_directory=True)
        release.touch(); worker.join(timeout=20)
        assert not worker.is_alive(),'fixture request did not finish'
        assert not list(outside.iterdir()), 'task lock was opened through a swapped directory'
        print('task lock open remains inside the selected runtime during a parent-directory swap')
    finally:
        release.touch()
        try: os.killpg(process.pid,signal.SIGKILL)
        except ProcessLookupError: pass
        process.wait(timeout=10)
        if worker is not None: worker.join(timeout=3)

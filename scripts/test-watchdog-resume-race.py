#!/usr/bin/env python3
"""Interleave watchdog live-owner termination with a concurrent resume on Linux."""
import fcntl
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
with tempfile.TemporaryDirectory(prefix='ticket27-watchdog-resume-') as tmp:
    base=Path(tmp); runtime=base/'runtime'; other=base/'other'
    for root in (runtime,other):
        for name in ('config','data/logs','resources'): (root/name).mkdir(parents=True,exist_ok=True)
        (root/'data/social-v2.json').write_text('{}')
        (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); supervisor_port=sock.getsockname()[1]
    (other/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port}))
    (runtime/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':supervisor_port,
        'backends':{'mcpRustBridge':False},'rollout':{'rustGroups':['group:1']}}))
    (runtime/'data/lifecycle').mkdir()
    (runtime/'data/lifecycle/desired-state.json').write_text(json.dumps({'v':1,'state':'stopped','by':'fixture','reason':'','at':1,'revision':1}))
    target=subprocess.Popen([exe,'--console-serve','--console-port',str(port),'--runtime-root',str(other)],
                            stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
    supervisor=resume=None
    try:
        deadline=time.monotonic()+15
        while time.monotonic()<deadline:
            try:
                with socket.create_connection(('127.0.0.1',port),timeout=.2): break
            except OSError: time.sleep(.05)
        else: raise AssertionError('synthetic target did not start')
        stat=Path(f'/proc/{target.pid}/stat').read_text()
        start=int(stat[stat.rfind(')')+1:].split()[19])
        identity={'pid':target.pid,'start':start,'exe':str(Path(f'/proc/{target.pid}/exe').resolve())}
        gate_path=runtime/'data/logs/rbridge-live.lock'; gate=open(gate_path,'a+')
        with gate:
            fcntl.flock(gate,fcntl.LOCK_EX)
            Path(str(gate_path)+'.owner.json').write_text(json.dumps({'v':1,'role':'live','identity':identity,
                'acquiredAt':1,'refreshedAt':1,'groups':['group:1']}))
            source=base/'barrier.c'
            source.write_text(r'''
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>
long syscall(long n,...) {
 static long (*real_call)(long,...);
 if (!real_call) real_call=dlsym(RTLD_NEXT,"syscall");
 va_list ap; va_start(ap,n); long a[6]; for(int i=0;i<6;i++) a[i]=va_arg(ap,long); va_end(ap);
 const char *pid=getenv("TICKET27_TARGET_PID");
 if (n==SYS_pidfd_open && pid && a[0]==strtol(pid,0,10)) {
  const char *notify=getenv("TICKET27_WATCH_NOTIFY"), *resume=getenv("TICKET27_WATCH_RESUME");
  int fd=open(notify,O_WRONLY|O_CREAT|O_TRUNC,0600); if(fd>=0) close(fd);
  for(int i=0;i<1000 && access(resume,F_OK)!=0;i++) usleep(10000);
 }
 return real_call(n,a[0],a[1],a[2],a[3],a[4],a[5]);
}
''')
            shim=base/'barrier.so'
            subprocess.run(['cc','-shared','-fPIC','-o',str(shim),str(source),'-ldl'],check=True,timeout=30,capture_output=True)
            notify=base/'notify'; release=base/'release'
            env=os.environ.copy(); env.update({'LD_PRELOAD':str(shim),'TICKET27_TARGET_PID':str(target.pid),
                'TICKET27_WATCH_NOTIFY':str(notify),'TICKET27_WATCH_RESUME':str(release)})
            supervisor=subprocess.Popen([exe,'supervise','--runtime-root',str(runtime)],stdout=subprocess.DEVNULL,
                                         stderr=subprocess.DEVNULL,start_new_session=True,env=env)
            deadline=time.monotonic()+20
            while not notify.exists() and supervisor.poll() is None and time.monotonic()<deadline: time.sleep(.01)
            assert notify.exists(),'watchdog did not reach the synthetic termination boundary'
            resume=subprocess.Popen([exe,'lifecycle','set','running','--runtime-root',str(runtime),'--by','fixture'],
                                    stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
            try:
                completed_before=resume.wait(timeout=.4)==0
            except subprocess.TimeoutExpired:
                completed_before=False
            release.touch()
            assert resume.wait(timeout=10)==0,'resume failed'
            time.sleep(.5)
            if completed_before:
                assert target.poll() is None,'watchdog stopped the owner after resume completed'
            print('watchdog stop and independent resume have a consistent transition order')
    finally:
        if resume is not None and resume.poll() is None: resume.kill(); resume.wait(timeout=5)
        if supervisor is not None and supervisor.poll() is None:
            try: os.killpg(supervisor.pid,signal.SIGKILL)
            except ProcessLookupError: pass
            supervisor.wait(timeout=5)
        if target.poll() is None: target.kill()
        target.wait(timeout=5)

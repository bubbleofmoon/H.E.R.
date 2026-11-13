#!/usr/bin/env python3
"""Interleave a real console start and stop at the owner gate on a synthetic root."""
import ctypes
import fcntl
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import fixture_paths as tempfile
import time

exe = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ticket27-start-stop-') as tmp:
    root=Path(tmp)/'runtime'; other=Path(tmp)/'other'
    for base in (root,other):
        for name in ('config','data/logs','resources'):
            (base/name).mkdir(parents=True,exist_ok=True)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    (root/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
        'backends':{'mcpRustBridge':True},'rollout':{'rustGroups':[]}}))
    (root/'data/social-v2.json').write_text('{}')
    (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    # The gate is held the way a real console holds it: the OS lock plus an owner marker naming a live
    # her process (a console on another synthetic root). A bare lock is an unidentifiable owner, which
    # stop refuses by design.
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); other_port=sock.getsockname()[1]
    (other/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':other_port}))
    (other/'data/social-v2.json').write_text('{}')
    (other/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    holder=starter=stopper=gate=None
    notify=-1
    def kill_group(process):
        if process is None: return
        try: os.killpg(process.pid,signal.SIGKILL)
        except ProcessLookupError: pass
        try: process.wait(timeout=10)
        except subprocess.TimeoutExpired: pass
    try:
        holder=subprocess.Popen([exe,'--console-serve','--console-port',str(other_port),'--runtime-root',str(other)],
                                stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
        deadline=time.monotonic()+15
        while True:
            try:
                with socket.create_connection(('127.0.0.1',other_port),timeout=.2): break
            except OSError:
                assert time.monotonic()<deadline and holder.poll() is None,'synthetic gate holder did not start'
                time.sleep(.05)
        stat=Path(f'/proc/{holder.pid}/stat').read_text()
        identity={'pid':holder.pid,'start':int(stat[stat.rfind(')')+1:].split()[19]),
                  'exe':str(Path(f'/proc/{holder.pid}/exe').resolve())}
        gate_path=root/'data/logs/rbridge-console.lock'; gate_path.touch()
        gate=open(gate_path,'r+')
        fcntl.flock(gate,fcntl.LOCK_EX)
        Path(str(gate_path)+'.owner.json').write_text(json.dumps({'v':1,'role':'console','identity':identity,
            'acquiredAt':1,'refreshedAt':1}))
        libc=ctypes.CDLL(None,use_errno=True)
        notify=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
        assert notify>=0 and libc.inotify_add_watch(notify,os.fsencode(gate_path),0x20)>=0
        starter=subprocess.Popen([exe,'--console-serve','--console-port',str(port),'--runtime-root',str(root)],
                                 stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
        ready,_,_=select.select([notify],[],[],15)
        assert ready,'console did not reach the held owner gate'
        os.read(notify,4096)
        stopper=subprocess.Popen([exe,'lifecycle','set','stopped','--runtime-root',str(root),'--confirm','--by','fixture'],
                                  stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
        time.sleep(.15)
        fcntl.flock(gate,fcntl.LOCK_UN)
        assert stopper.wait(timeout=15)==0,'stop transition failed'
        deadline=time.monotonic()+3
        while time.monotonic()<deadline:
            try:
                with socket.create_connection(('127.0.0.1',port),timeout=.2):
                    raise AssertionError('console started after the stop transition')
            except OSError:
                time.sleep(.05)
        print('stop and console startup serialize at the owner transition')
    finally:
        if gate is not None: gate.close()
        if notify>=0: os.close(notify)
        if stopper is not None and stopper.poll() is None: kill_group(stopper)
        kill_group(starter)
        kill_group(holder)

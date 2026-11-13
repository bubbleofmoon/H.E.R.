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
    root=Path(tmp)
    for name in ('config','data/logs','resources'):
        (root/name).mkdir(parents=True,exist_ok=True)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
    (root/'config/config.json').write_text(json.dumps({'consoleToken':'fixture-token','consolePort':port,
        'backends':{'mcpRustBridge':True},'rollout':{'rustGroups':[]}}))
    (root/'data/social-v2.json').write_text('{}')
    (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    gate_path=root/'data/logs/rbridge-console.lock'; gate_path.touch()
    gate=open(gate_path,'r+')
    fcntl.flock(gate,fcntl.LOCK_EX)
    libc=ctypes.CDLL(None,use_errno=True)
    notify=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
    assert notify>=0 and libc.inotify_add_watch(notify,os.fsencode(gate_path),0x20)>=0
    starter=subprocess.Popen([exe,'--console-serve','--console-port',str(port),'--runtime-root',str(root)],
                             stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
    stopper=None
    try:
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
        gate.close(); os.close(notify)
        if stopper is not None and stopper.poll() is None:
            stopper.kill(); stopper.wait(timeout=5)
        try: os.killpg(starter.pid,signal.SIGKILL)
        except ProcessLookupError: pass
        starter.wait(timeout=10)

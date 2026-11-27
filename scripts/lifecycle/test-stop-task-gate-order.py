#!/usr/bin/env python3
"""Stop must not hold the lifecycle gate while waiting for a running task."""
import fcntl
import json
from pathlib import Path
import subprocess
import sys
import fixture_paths as tempfile
import time

exe=str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ticket27-stop-gate-order-') as tmp:
    root=Path(tmp); locks=root/'data/tasks/locks'; locks.mkdir(parents=True)
    task=open(locks/'backup.lock','a+'); fcntl.flock(task,fcntl.LOCK_EX)
    process=subprocess.Popen([exe,'lifecycle','set','stopped','--runtime-root',str(root),'--confirm','--by','fixture'],
                             stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
    try:
        desired=root/'data/lifecycle/desired-state.json'; gate_path=root/'data/lifecycle/lifecycle.lock'
        deadline=time.monotonic()+10
        while time.monotonic()<deadline:
            if desired.exists() and json.loads(desired.read_text())['state']=='stopped' and gate_path.exists(): break
            time.sleep(.01)
        else: raise AssertionError('stop did not record its desired state')
        time.sleep(.2)
        with open(gate_path,'r+') as gate:
            try: fcntl.flock(gate,fcntl.LOCK_EX|fcntl.LOCK_NB)
            except BlockingIOError: raise AssertionError('stop held lifecycle gate while waiting for a task') from None
            fcntl.flock(gate,fcntl.LOCK_UN)
        print('stop releases the lifecycle gate while waiting for task cancellation')
    finally:
        fcntl.flock(task,fcntl.LOCK_UN); task.close()
        try: process.wait(timeout=15)
        except subprocess.TimeoutExpired: process.kill(); process.wait(timeout=5)

#!/usr/bin/env python3
"""A resume must not overwrite a newer independently recorded stop."""
import json
from pathlib import Path
import subprocess
import sys
import fixture_paths as tempfile

exe=str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ticket27-revision-') as tmp:
    root=Path(tmp); (root/'data').mkdir()
    def call(*args):
        return subprocess.run([exe,'lifecycle',*args,'--runtime-root',str(root)],capture_output=True,text=True,timeout=20)
    first=call('set','stopped','--confirm')
    assert first.returncode==0,(first.stdout,first.stderr)
    revision=json.loads(first.stdout)['record']['revision']
    resume=call('set','running','--expected-revision',str(revision))
    assert resume.returncode==0,('matching revision must resume',resume.stdout,resume.stderr)
    stop=call('set','stopped','--confirm')
    assert stop.returncode==0,(stop.stdout,stop.stderr)
    stale=json.loads(stop.stdout)['record']['revision']
    independent=call('set','stopped','--confirm')
    assert independent.returncode==0,(independent.stdout,independent.stderr)
    resume=call('set','running','--expected-revision',str(stale))
    assert resume.returncode!=0,'a stale resume overwrote an independent stop'
    status=call('status')
    assert json.loads(status.stdout)['desired']['state']=='stopped'
    print('a lifecycle resume applies only to its expected revision')

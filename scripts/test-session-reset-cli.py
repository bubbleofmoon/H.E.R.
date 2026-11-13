#!/usr/bin/env python3
"""Reset the selected session map through the stopped-runtime lifecycle CLI."""
import json
from pathlib import Path
import subprocess
import sys
import fixture_paths as tempfile

exe=str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix='ticket27-session-reset-') as tmp:
    root=Path(tmp)
    for name in ('config','data','resources'): (root/name).mkdir()
    (root/'config/config.json').write_text('{"consoleToken":"fixture-token"}')
    (root/'data/social-v2.json').write_text('{}')
    (root/'dependencies.json').write_text('{"dsh":"http://127.0.0.1:1","model":"http://127.0.0.1:1","onebot":"http://127.0.0.1:1"}')
    original={'sessions':{'group:1':'session'},'sessionPolicies':{'group:1':'policy'},
              'workspaceId':'workspace','extra':{'nested':[1,{'kept':True}]}}
    state=root/'data/sessions.json'; state.write_text(json.dumps(original))
    def run(*args):
        return subprocess.run([exe,'lifecycle',*args,'--runtime-root',str(root)],text=True,capture_output=True,timeout=20)
    assert run('reset-sessions','--confirm').returncode!=0,'reset must refuse a running runtime'
    assert json.loads(state.read_text())==original
    assert run('set','stopped','--confirm').returncode==0
    result=run('reset-sessions','--confirm')
    assert result.returncode==0,(result.returncode,result.stdout,result.stderr)
    assert json.loads(state.read_text())=={'sessions':{},'sessionPolicies':{},'workspaceId':'workspace','extra':{'nested':[1,{'kept':True}]}}
    backups=list((root/'data/lifecycle/backups').glob('sessions-*.json'))
    assert len(backups)==1 and json.loads(backups[0].read_text())==original
    audit=(root/'data/lifecycle/audit.jsonl').read_text()
    assert 'sessions-reset' in audit and 'applied' in audit
    state.write_text('{broken')
    assert run('reset-sessions','--confirm').returncode!=0,'invalid session JSON must be refused'
    assert state.read_text()=='{broken'
    print('guarded session reset preserves metadata, keeps a backup, and refuses damaged input')

"""Observe the MCP/curl/digest chains independently of supervisor startup readiness."""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import fixture_paths as tempfile
import time
from release_fixture import ReleaseFixture

parser = argparse.ArgumentParser()
for name in ('candidate', 'support', 'evidence'):
    parser.add_argument(name, type=lambda arg: Path(arg).resolve())
parser.add_argument('--timeout', type=float, help='override each command timeout in seconds')
args = parser.parse_args()
if args.timeout is not None and args.timeout <= 0:
    parser.error('--timeout must be positive')
candidate, support, evidence = args.candidate, args.support, args.evidence
tempfile.development_root(evidence)
tempfile.gettempdir()  # Check redirected temp before even creating evidence.
evidence.mkdir(parents=True, exist_ok=True)
repo = Path(__file__).resolve().parent.parent
root = Path(tempfile.mkdtemp(prefix='relproof-chains-'))
fixture = ReleaseFixture(root, evidence, timeout=args.timeout)
observer = None
try:
    runtime = root / 'runtime'
    for directory in ['config', 'data', 'resources', 'user/dsh']:
        (runtime / directory).mkdir(parents=True, exist_ok=True)
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0)); port = str(listener.getsockname()[1])
    assert int(port) not in [3000, 3001, 3080, 3100, 5099]
    (runtime / 'config/config.json').write_text(json.dumps({'consoleToken': 'synthetic-release-token', 'consolePort': int(port)}))
    (runtime / 'data/social-v2.json').write_text('{}')
    (runtime / 'user/dsh/.credentials.yaml').write_text('secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\nrecords:\n  client-connection/browser-session:\n    kind: grant\n    payload:\n      version: 1\n      secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n')
    (root / 'fake-curl').mkdir()
    local_candidate = root / 'fake-curl/her.exe'
    shutil.copyfile(candidate, local_candidate)
    compile_script = root / 'compile.ps1'
    compile_script.write_text("$ErrorActionPreference='Stop'\nAdd-Type -OutputType ConsoleApplication -OutputAssembly '" + str(root / 'fake-curl/curl.exe') + "' -Path '" + str(repo / 'scripts/lifecycle-curl-adapter.cs') + "'\n")
    code = fixture.run(['powershell.exe', '-NoProfile', '-File', str(compile_script)], 'compile')
    assert code == 0, ('compile exit', code)
    observer_out = (root / 'observer.stdout.log').open('w')
    observer_err = (root / 'observer.stderr.log').open('w')
    observer = fixture.start(['powershell.exe', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File',
                                 str(repo / 'scripts/lifecycle-window-observer.ps1'), '-FixtureRoot', str(root),
                                 '-Ready', str(root / 'ready'), '-Done', str(root / 'done'), '-Output', str(root / 'observer.json')],
                                stdout=observer_out, stderr=observer_err)
    deadline = time.monotonic() + 15
    while not (root / 'ready').exists():
        assert observer.poll() is None and time.monotonic() < deadline, 'observer did not become ready'
        time.sleep(.05)
    command = [shutil.which('node'), str(repo / 'scripts/lifecycle-extra-chains.mjs'), str(local_candidate), str(runtime), str(root), str(support), port, '--fake-console']
    (evidence / 'command.json').write_text(json.dumps(command))
    code = fixture.run(command, 'chains', timeout=90,
                       env=dict(os.environ, MEMORY_DB_PATH=str(root / 'digest-runtime/data/memory.db')))
    assert code == 0, ('chains exit', code, (evidence / 'chains.stderr.log').read_text())
    (root / 'done').write_text('done')
    assert observer.wait(timeout=15) == 0, 'native window observation failed'
    observer_out.close(); observer_err.close()
    observed = json.loads((root / 'observer.json').read_text())
    assert observed['samples'] > 0 and observed['observed'] and observed['done'] and not observed['violations'], observed
    curl_images = [value for value in observed['identities'].values() if value.lower().endswith('curl.exe')]
    assert len(curl_images) >= 2, 'both the adapter and actual system curl must be observed'
    print('PASS native enumeration; generated MCP initialize/list/call; actual curl to fake loopback; digest prompt; no sampled visible/focus windows')
    print('Sampling interval: 250 ms plus enumeration time; shorter windows may be missed.')
finally:
    try:
        if observer is not None:
            if observer.poll() is None:
                (root / 'done').write_text('done')
                try: observer.wait(timeout=15)
                except subprocess.TimeoutExpired: pass  # finish terminates and verifies the entire job
    finally:
        try:
            for tree in fixture.trees:
                tree.stop()
        finally:
            if observer is not None:
                observer_out.close(); observer_err.close()
                (evidence / 'observer.exit.txt').write_text(str(observer.returncode))
            fixture.finish()

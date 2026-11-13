"""Native release acceptance at the PowerShell fixture/process observation boundary."""
import argparse
import ctypes
import json
import os
from pathlib import Path
import shutil
import fixture_paths as tempfile
import uuid
from release_fixture import ReleaseFixture

parser = argparse.ArgumentParser()
parser.add_argument('candidate')
parser.add_argument('support')
parser.add_argument('evidence')
parser.add_argument('--fixture', type=Path)
parser.add_argument('--baseline', action='store_true', help='diagnostic: run the unchanged fixture without chain assertions')
parser.add_argument('--highest', action='store_true', help='run the fixture task as the interactive user at Highest (requires elevation)')
parser.add_argument('--timeout', type=float, default=240, help='fixture timeout in seconds (124 receipt on timeout)')
args = parser.parse_args()
if args.timeout <= 0:
    parser.error('--timeout must be positive')
repo = Path(__file__).resolve().parent.parent
evidence = tempfile.development_root(Path(args.evidence).resolve())
root = tempfile.development_root(Path(tempfile.gettempdir()) / ('relproof-' + uuid.uuid4().hex))
evidence.mkdir(parents=True, exist_ok=True)
task_name = 'QQBot-Fixture-' + uuid.uuid4().hex
caller_elevated = bool(ctypes.windll.shell32.IsUserAnAdmin()) if os.name == 'nt' else False
(evidence / 'execution-context.json').write_text(json.dumps({
    'callerElevated': caller_elevated, 'requestedHighest': args.highest,
    'taskName': task_name, 'fixtureRoot': str(root),
    'registeredPrincipalEvidence': 'artifacts/task-principal.json',
}), encoding='utf-8')
if args.highest and not caller_elevated:
    parser.error('--highest requires an elevated caller (Run as administrator); no fixture task was registered')
fixture = ReleaseFixture(root, evidence, [task_name])
command = ['powershell.exe', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
           '-File', str(args.fixture or repo / 'scripts/test-lifecycle-fixture.ps1'), '-Candidate', str(Path(args.candidate).resolve()),
           '-FixtureRoot', str(root), '-NodePath', shutil.which('node'), '-FixtureTaskName', task_name]
if args.highest:
    command.append('-RunLevelHighest')
if not args.baseline:
    command.extend(['-DigestSupportRoot', str(Path(args.support).resolve())])
(evidence / 'command.json').write_text(json.dumps(command), encoding='utf-8')
try:
    code = fixture.run(command, 'fixture', timeout=args.timeout,
                       env=dict(os.environ, MEMORY_DB_PATH=str(root / 'digest-runtime/data/memory.db')))
    assert code == 0, ('fixture exit', code, (evidence / 'fixture.stderr.log').read_text(encoding='utf-8'))
    principal = json.loads((root / 'task-principal.json').read_text(encoding='utf-8-sig'))
    assert principal['callerElevated'] == caller_elevated and principal['readBeforeTrigger'], principal
    if args.highest:
        assert principal['principal']['runLevel'] == 'Highest' and principal['principal']['logonType'] == 'Interactive', principal
    if not args.baseline:
        chains = json.loads((root / 'chains.json').read_text(encoding='utf-8-sig'))
        assert chains['mcp'] == ['snowluma', 'snowluma-host', 'web-search-safe'], chains
        assert chains['digestPrompts'] == 1 and chains['manual'] and chains['recovery'], chains
    observer = json.loads((root / 'observer.json').read_text(encoding='utf-8-sig'))
    assert observer['samples'] > 0 and observer['observed'] and observer['done'] and not observer['violations'], observer
    print('PASS native scheduled/manual/recovery, generated MCP initialize/list/call, curl, digest prompt, stop gates and sampled windows')
    print('Window observation samples every 250 ms plus enumeration time; shorter windows can be missed.')
finally:
    try:
        fixture.finish()
    finally:
        # Preserve the original public log names used by release evidence indexes.
        for name in ('stdout.log', 'stderr.log'):
            source = evidence / ('fixture.' + name)
            if source.exists():
                shutil.copyfile(source, evidence / name)

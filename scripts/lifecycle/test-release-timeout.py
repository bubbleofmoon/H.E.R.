"""Force the public release runner to time out; no task is registered or started."""
import ctypes
import json
from pathlib import Path
import subprocess
import sys
import fixture_paths as tempfile
import time

from fixture_process import hidden


with tempfile.TemporaryDirectory(prefix='release-timeout-selftest-') as temporary:
    base = Path(temporary)
    fixture = base / 'hang.ps1'
    fixture.write_text(r'''
param($Candidate,$FixtureRoot,$NodePath,$DigestSupportRoot,$FixtureTaskName)
$ErrorActionPreference='Stop'
[IO.Directory]::CreateDirectory($FixtureRoot) | Out-Null
[IO.File]::WriteAllText((Join-Path $FixtureRoot 'diagnostic.json'), '{"beforeTimeout":true}')
$child = Start-Process -FilePath $Candidate -ArgumentList '-NoProfile -NonInteractive -Command "Start-Sleep -Seconds 120"' -WindowStyle Hidden -PassThru
[IO.File]::WriteAllText((Join-Path $FixtureRoot 'child.txt'), [string]$child.Id)
Start-Sleep -Seconds 120
''', encoding='utf-8')
    evidence = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else base / 'evidence'
    process = subprocess.Popen([sys.executable, str(Path(__file__).with_name('test-release-proof.py')),
                             'C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe', str(base), str(evidence),
                             '--fixture', str(fixture), '--timeout', '5'],
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, **hidden())
    deadline = time.monotonic() + 10
    while not (evidence / 'command.json').exists():
        assert process.poll() is None and time.monotonic() < deadline
        time.sleep(.02)
    command = json.loads((evidence / 'command.json').read_text())
    root = Path(command[command.index('-FixtureRoot') + 1])
    while not (root / 'child.txt').exists():
        assert process.poll() is None and time.monotonic() < deadline
        time.sleep(.02)
    # Emulate a Scheduler-launched tree outside the release runner's job. Only
    # this parent's args name the root; its existing child must also be reaped.
    escape = base / 'escape.py'
    escape.write_text('''import subprocess,sys,time,json
from pathlib import Path
child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(120)'],creationflags=subprocess.CREATE_NO_WINDOW)
Path(sys.argv[1],'escaped.json').write_text(json.dumps({'parent':__import__('os').getpid(),'child':child.pid}))
time.sleep(120)
''')
    escaped = subprocess.Popen([sys.executable, str(escape), str(root)], **hidden())
    stdout, stderr = process.communicate(timeout=60)
    assert process.returncode != 0, 'a timed-out proof must fail'
    assert (evidence / 'fixture.exit.txt').read_text() == '124', (stdout, stderr)
    assert json.loads((evidence / 'artifacts/diagnostic.json').read_text()) == {'beforeTimeout': True}
    assert not root.exists(), root
    child_pid = int((evidence / 'artifacts/child.txt').read_text())
    escaped_ids = json.loads((evidence / 'artifacts/escaped.json').read_text())
    api = ctypes.WinDLL('kernel32', use_last_error=True)
    api.OpenProcess.restype = ctypes.c_void_p
    api.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    api.CloseHandle.argtypes = [ctypes.c_void_p]
    for child_pid in [child_pid, *escaped_ids.values()]:
        handle = api.OpenProcess(0x100000, False, child_pid)
        if handle:
            try:
                assert api.WaitForSingleObject(handle, 0) == 0, 'timeout left a live child'
            finally:
                api.CloseHandle(handle)
    assert escaped.wait(timeout=5) == 124
    cleanup = json.loads((evidence / 'cleanup.json').read_text(encoding='utf-8-sig'))
    assert cleanup['remainingTasks'] == [] and cleanup['remainingProcesses'] == [], cleanup
    print('PASS timeout exit 124; diagnostics archived; owned and external descendants exited; zero fixture tasks; temporary root removed')
    # A leader can exit before its descendants. The job must remain owned until
    # finalization, and a non-timeout failure must archive diagnostics as well.
    fixture.write_text(fixture.read_text().replace('\nStart-Sleep -Seconds 120\n', '\nexit 7\n'))
    failure_evidence = evidence / 'nonzero'
    failed = subprocess.run([sys.executable, str(Path(__file__).with_name('test-release-proof.py')),
                             'C:/Windows/System32/WindowsPowerShell/v1.0/powershell.exe', str(base), str(failure_evidence),
                             '--fixture', str(fixture), '--timeout', '5'],
                            capture_output=True, text=True, timeout=60, **hidden())
    assert failed.returncode != 0
    assert (failure_evidence / 'fixture.exit.txt').read_text() == '7', (failed.stdout, failed.stderr)
    assert json.loads((failure_evidence / 'artifacts/diagnostic.json').read_text()) == {'beforeTimeout': True}
    child_pid = int((failure_evidence / 'artifacts/child.txt').read_text())
    handle = api.OpenProcess(0x100000, False, child_pid)
    if handle:
        try:
            assert api.WaitForSingleObject(handle, 0) == 0, 'nonzero leader left a live descendant'
        finally:
            api.CloseHandle(handle)
    assert json.loads((failure_evidence / 'finalize.json').read_text())['errors'] == []
    print('PASS nonzero exit 7; diagnostics archived; descendant reaped after leader exit')

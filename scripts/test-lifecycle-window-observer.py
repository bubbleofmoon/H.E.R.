#!/usr/bin/env python3
"""Prove the Windows observer detects a visible descendant using synthetic CIM/window data."""
from pathlib import Path
from fixture_process import hidden
import os
import subprocess
import sys
import fixture_paths as tempfile

observer = Path(__file__).resolve().parent / 'lifecycle-window-observer.ps1'
assert observer.is_file(), 'the lifecycle fixture must include a descendant window observer'
with tempfile.TemporaryDirectory(prefix='ticket27-observer-') as tmp:
    root = Path(tmp)
    harness = root / 'harness.ps1'
    harness.write_text(r'''
param($Observer,$Root)
$ErrorActionPreference='Stop'
if (-not $env:TICKET27_NATIVE_FAILURE -or $env:OS -eq 'Windows_NT') {
Add-Type -TypeDefinition @'
using System;
public static class QQBotFixtureWindows {
 public static int Samples=0;
 public static string[] VisibleFor(int[] ids) {
  if (Environment.GetEnvironmentVariable("TICKET27_NATIVE_FAILURE") == "1") throw new System.InvalidOperationException("EnumWindows observation failed");
  Samples++;
  if (Environment.GetEnvironmentVariable("TICKET27_VISIBLE") == "1" &&
      (Environment.GetEnvironmentVariable("TICKET27_ORPHAN") != "1" || Samples >= 2) && Array.IndexOf(ids, 101) >= 0) return new string[]{"window|101"};
  return new string[0];
 }
 public static int ForegroundOwner() { return 0; }
}
'@
}
Add-Type -TypeDefinition @'
using System;
public static class QQBotFixtureIdentity {
 static int ChildReads=0;
 public static string Read(int id) {
  if (id == 101) ChildReads++;
  string mode=Environment.GetEnvironmentVariable("TICKET27_IDENTITY");
  if (id == 101 && mode == "exited") return null;
  if (id == 101 && mode == "unknown") throw new InvalidOperationException("live descendant identity unavailable");
  return id == 101 ? ((Environment.GetEnvironmentVariable("TICKET27_REUSED") == "1" && ChildReads > 1) ? "201|/elsewhere/node" : "200|/elsewhere/node") : "100|root";
 }
}
'@
$script:calls=0
function Get-CimInstance {
 [CmdletBinding()] param([string]$ClassName)
 $script:calls++
 if ($script:calls -ge 2) { [IO.File]::WriteAllText((Join-Path $Root 'done'),'done') }
 if (-not $env:TICKET27_ORPHAN -or $script:calls -lt 2) {
   [pscustomobject]@{ProcessId=100;ParentProcessId=1;CreationDate=[DateTime]::FromFileTimeUtc(100);ExecutablePath=(Join-Path $Root 'her.exe');CommandLine='root'}
 }
 $start=if ($env:TICKET27_REUSED -and $script:calls -ge 2) { 201 } else { 200 }
 [pscustomobject]@{ProcessId=101;ParentProcessId=$(if ($script:calls -ge 2 -and $env:TICKET27_ORPHAN) {1} else {100});CreationDate=[DateTime]::FromFileTimeUtc($start);ExecutablePath=$(if ($env:TICKET27_IDENTITY) { $null } else { '/elsewhere/node' });CommandLine='child'}
}
& $Observer -FixtureRoot $Root -Ready (Join-Path $Root 'ready') -Done (Join-Path $Root 'done') -Output (Join-Path $Root 'result.json') -MaxSeconds 5
if ($LASTEXITCODE) { exit $LASTEXITCODE }
''')
    native = root/'libuser32.dll.so'
    if os.name != 'nt': subprocess.run(['cc','-shared','-fPIC','-x','c','-o',str(native),'-'],input='''
int EnumWindows(void *callback, void *data) { return 0; }
int IsWindowVisible(void *window) { return 0; }
unsigned int GetWindowThreadProcessId(void *window, unsigned int *pid) { *pid=0; return 0; }
void *GetForegroundWindow(void) { return 0; }
''',text=True,check=True,capture_output=True,timeout=20)
    for case, visible, expected in (('hidden','0',0),('visible','1',1),('orphan','1',1),('reused','1',0),('native-failure','0',1),('exited','0',0),('verified-live','0',0),('unknown','0',1),('verified-live-visible','1',1)):
        if len(sys.argv) > 1 and case != sys.argv[1]: continue
        for name in ('ready', 'done', 'result.json'):
            (root/name).unlink(missing_ok=True)
        env = os.environ.copy(); env['TICKET27_VISIBLE'] = visible
        if case in ('exited','verified-live','unknown','verified-live-visible'): env['TICKET27_IDENTITY']=case
        if case in ('orphan','reused'): env['TICKET27_ORPHAN']='1'
        if case == 'reused': env['TICKET27_REUSED']='1'
        if case == 'native-failure':
            env['TICKET27_NATIVE_FAILURE']='1'
            env['LD_LIBRARY_PATH']=str(root)+os.pathsep+env.get('LD_LIBRARY_PATH','')
        result = subprocess.run(['powershell.exe' if os.name == 'nt' else 'pwsh', '-NoProfile', '-File', str(harness), str(observer), str(root)],
                                text=True, capture_output=True, env=env, timeout=15, **hidden())
        report = (root/'result.json').read_text() if (root/'result.json').exists() else '<no report>'
        assert (result.returncode == 0) == (expected == 0), (case, result.returncode, result.stdout, result.stderr, report)
        if case == 'native-failure': assert 'EnumWindows' in result.stderr and 'DllNotFoundException' not in result.stderr, result.stderr
        print(case+': observer result verified')
# Identity read retry: compile the observer's own identity source with a fault-injecting probe.
source = observer.read_text(encoding='utf-8')
begin = source.index('# QQBOT-IDENTITY-SOURCE-BEGIN'); end = source.index('# QQBOT-IDENTITY-SOURCE-END')
csharp = source[begin:end].split("@'\n", 1)[1].rsplit("\n'@", 1)[0]
assert 'QQBotFixtureIdentity' in csharp and 'ReadWith' in csharp, 'observer identity source markers must wrap the C# type'
with tempfile.TemporaryDirectory(prefix='observer-identity-') as tmp:
    root = Path(tmp)
    (root/'identity.cs').write_text(csharp + r'''
public sealed class FaultProbe : IQQBotProcessProbe {
  public int Failures, ExitAfter = -1, OpenCalls, QueryCalls, CloseCalls, OpenError, QueryError = 5;
  public IntPtr Open(int pid, out int error) { OpenCalls++; error = OpenError; return OpenError == 0 ? new IntPtr(77) : IntPtr.Zero; }
  public uint Wait(IntPtr h, out int error) { error = 0; return ExitAfter >= 0 && QueryCalls >= ExitAfter ? 0u : 258u; }
  public bool Query(IntPtr h, out string identity, out int error) {
    QueryCalls++; identity = null; error = QueryError;
    if (QueryCalls <= Failures) return false;
    identity = "123|node.exe"; error = 0; return true;
  }
  public void Close(IntPtr h) { CloseCalls++; }
}
''', encoding='utf-8')
    (root/'identity.ps1').write_text(r'''
param($Source)
$ErrorActionPreference='Stop'
Add-Type -TypeDefinition (Get-Content -LiteralPath $Source -Raw)
function Probe($failures,$exitAfter,$openError){ $p=New-Object FaultProbe; $p.Failures=$failures; $p.ExitAfter=$exitAfter; $p.OpenError=$openError; $p }
$results=[ordered]@{}
$p=Probe 0 -1 0; $results.immediate=[QQBotFixtureIdentity]::ReadWith($p,1,1000,10); $results.immediateQueries=$p.QueryCalls
$p=Probe 4 -1 0; $results.transient=[QQBotFixtureIdentity]::ReadWith($p,1,1000,10); $results.transientQueries=$p.QueryCalls; $results.transientOpens=$p.OpenCalls; $results.transientCloses=$p.CloseCalls
$p=Probe 3 3 0; $results.exited=[string][QQBotFixtureIdentity]::ReadWith($p,1,1000,10); $results.exitedCloses=$p.CloseCalls
$p=Probe 999 -1 0; $watch=[Diagnostics.Stopwatch]::StartNew(); $message=''
try { [void][QQBotFixtureIdentity]::ReadWith($p,4242,300,10) } catch { $message=$_.Exception.GetBaseException().Message }
$results.neverMessage=$message; $results.neverMs=$watch.ElapsedMilliseconds; $results.neverCloses=$p.CloseCalls; $results.neverQueries=$p.QueryCalls
$p=Probe 0 -1 87; $results.absent=[string][QQBotFixtureIdentity]::ReadWith($p,1,1000,10)
$p=Probe 0 -1 5; $message=''
try { [void][QQBotFixtureIdentity]::ReadWith($p,1,1000,10) } catch { $message=$_.Exception.GetBaseException().Message }
$results.denied=$message
$results | ConvertTo-Json | Write-Output
''', encoding='utf-8')
    run = subprocess.run(['powershell.exe' if os.name == 'nt' else 'pwsh', '-NoProfile', '-File', str(root/'identity.ps1'), str(root/'identity.cs')],
                         text=True, capture_output=True, timeout=60, **hidden())
    assert run.returncode == 0, (run.returncode, run.stdout, run.stderr)
    import json
    got = json.loads(run.stdout)
    assert got['immediate'] == '123|node.exe' and got['immediateQueries'] == 1, got
    assert got['transient'] == '123|node.exe' and got['transientQueries'] == 5 and got['transientOpens'] == 1 and got['transientCloses'] == 1, got
    assert got['exited'] == '' and got['exitedCloses'] == 1, got
    assert 'identity unavailable: pid=4242' in got['neverMessage'] and 'win32=5' in got['neverMessage'], got
    assert 280 <= got['neverMs'] < 5000 and got['neverQueries'] > 2 and got['neverCloses'] == 1, got
    assert got['absent'] == '', got
    assert 'cannot be opened' in got['denied'] and 'win32=5' in got['denied'], got
    print('identity read: transient query failures retry on one handle, exit returns null, persistent failure throws with the Win32 code')

# A failing observer still writes its diagnostics, including the error.
with tempfile.TemporaryDirectory(prefix='observer-error-') as tmp:
    root = Path(tmp)
    harness = root/'harness.ps1'
    harness.write_text(r'''
param($Observer,$Root)
$ErrorActionPreference='Stop'
Add-Type -TypeDefinition @'
using System;
public static class QQBotFixtureWindows {
 public static string[] VisibleFor(int[] ids) { return new string[0]; }
 public static int ForegroundOwner() { return 0; }
}
public static class QQBotFixtureIdentity {
 public static string Read(int id) { if (id == 101) throw new InvalidOperationException("Live fixture descendant identity unavailable: pid=101 win32=5"); return "100|root"; }
}
'@
function Get-CimInstance {
 [CmdletBinding()] param([string]$ClassName)
 [pscustomobject]@{ProcessId=100;ParentProcessId=1;CreationDate=[DateTime]::FromFileTimeUtc(100);ExecutablePath=(Join-Path $Root 'her.exe');CommandLine='root'}
 [pscustomobject]@{ProcessId=101;ParentProcessId=100;CreationDate=[DateTime]::FromFileTimeUtc(200);ExecutablePath='/elsewhere/node';CommandLine='child'}
}
& $Observer -FixtureRoot $Root -Ready (Join-Path $Root 'ready') -Done (Join-Path $Root 'done') -Output (Join-Path $Root 'result.json') -MaxSeconds 5
exit $LASTEXITCODE
''', encoding='utf-8')
    result = subprocess.run(['powershell.exe' if os.name == 'nt' else 'pwsh', '-NoProfile', '-File', str(harness), str(observer), str(root)],
                            text=True, capture_output=True, timeout=30, **hidden())
    assert result.returncode == 1 and 'win32=5' in result.stderr, (result.returncode, result.stdout, result.stderr)
    report = json.loads((root/'result.json').read_text(encoding='utf-8'))
    assert 'identity unavailable: pid=101' in report['error'] and report['done'] is False and 'samples' in report, report
    print('observer failure still writes result.json with the error field')

print('window observer retains descendant identities, rejects failed native enumeration and excludes reused PIDs')

#!/usr/bin/env python3
"""Run the Windows fixture cleanup against a synthetic PID-reuse interleaving."""
from pathlib import Path
import subprocess
import fixture_paths as tempfile

repo = Path(__file__).resolve().parent.parent
with tempfile.TemporaryDirectory(prefix='ticket27-fixture-pin-') as tmp:
    harness = Path(tmp) / 'pinning.ps1'
    harness.write_text(r'''
param($Fixture)
$ErrorActionPreference='Stop'
$target = [IO.Path]::GetFullPath((Join-Path ([IO.Path]::GetTempPath()) 'fixture-her.exe'))
Add-Type -TypeDefinition @'
using System;
public static class QQBotFixturePinnedProcess {
 public static bool Stopped=false;
 public static bool Called=false;
 public static string Stop(uint pid, ulong start, string exe) {
  Called=true;
  if (start != 456) return "mismatch";
  Stopped=true; return "stopped";
 }
}
'@
function Get-Process {
 [CmdletBinding()] param([int]$Id)
 return [pscustomobject]@{Id=$Id;Path=$target;StartTime=[DateTime]::FromFileTimeUtc(123)}
}
function Stop-Process {
 [CmdletBinding()] param([int]$Id,[switch]$Force)
 [QQBotFixturePinnedProcess]::Stopped=$true
}
$tokens=$null; $errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile($Fixture,[ref]$tokens,[ref]$errors)
if ($errors.Count) { throw 'Fixture must parse' }
$func=$ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq 'Stop-VerifiedOwner'}, $true)
Invoke-Expression $func.Extent.Text
$owner=[pscustomobject]@{state='held';owner=[pscustomobject]@{identity=[pscustomobject]@{pid=42;start=123;exe=$target}}}
try { Stop-VerifiedOwner $owner } catch { }
if ([QQBotFixturePinnedProcess]::Stopped) { throw 'Fixture cleanup stopped a process that reused the owner PID.' }
if (-not [QQBotFixturePinnedProcess]::Called) { throw 'Fixture cleanup did not try to pin the owner.' }
Write-Output 'fixture cleanup refuses a reused owner PID'
''')
    result = subprocess.run(['pwsh', '-NoProfile', '-File', str(harness), str(repo / 'scripts/test-lifecycle-fixture.ps1')],
                            text=True, capture_output=True, timeout=30)
    print(result.stdout, end='')
    print(result.stderr, end='')
    raise SystemExit(result.returncode)

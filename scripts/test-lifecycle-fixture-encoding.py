#!/usr/bin/env python3
"""Run fixture JSON initialization with Windows PowerShell 5.1 UTF8 semantics."""
from pathlib import Path
import subprocess
import fixture_paths as tempfile

repo = Path(__file__).resolve().parent.parent
with tempfile.TemporaryDirectory(prefix='ticket27-fixture-encoding-') as tmp:
    harness = Path(tmp) / 'encoding.ps1'
    harness.write_text(r'''
param($Fixture,$Root)
$ErrorActionPreference='Stop'
$runtime=Join-Path $Root 'runtime'
[IO.Directory]::CreateDirectory((Join-Path $runtime 'config')) | Out-Null
[IO.Directory]::CreateDirectory((Join-Path $runtime 'data')) | Out-Null
$port=41234
$snowPort=41235; $dshPort=41236
$FixtureRoot=$Root; $NodePath=Join-Path $Root 'node.exe'
$snowDirectory=Join-Path $Root 'snow'; $dshDirectory=Join-Path $Root 'dsh'; $dshScript=Join-Path $dshDirectory 'bin.mjs'
function Set-Content {
 [CmdletBinding()] param([Parameter(ValueFromPipeline=$true)]$Value,[string]$LiteralPath,[string]$Encoding)
 process { [IO.File]::WriteAllText($LiteralPath,[string]$Value,[Text.UTF8Encoding]::new($true)) }
}
$source=[IO.File]::ReadAllText($Fixture)
$start=$source.IndexOf('  @{ consoleToken=')
$end=$source.IndexOf('  [void](Invoke-Bridge', $start)
if ($start -lt 0 -or $end -lt 0) { throw 'Could not locate fixture initialization.' }
Invoke-Expression $source.Substring($start,$end-$start)
foreach ($name in @('config/config.json','dependencies.json','data/social-v2.json')) {
 $path=Join-Path $runtime $name
 $bytes=[IO.File]::ReadAllBytes($path)
 if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xef -and $bytes[1] -eq 0xbb -and $bytes[2] -eq 0xbf) { throw "Fixture JSON has a UTF-8 BOM: $name" }
 $null=[IO.File]::ReadAllText($path,[Text.Encoding]::UTF8) | ConvertFrom-Json
}
Write-Output 'fixture runtime JSON is valid UTF-8 without a BOM'
''')
    result = subprocess.run(['pwsh', '-NoProfile', '-File', str(harness),
                             str(repo / 'scripts/test-lifecycle-fixture.ps1'), tmp],
                            text=True, capture_output=True, timeout=30)
    print(result.stdout, end='')
    print(result.stderr, end='')
    raise SystemExit(result.returncode)

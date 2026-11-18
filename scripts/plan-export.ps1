[CmdletBinding()]
param(
  [Parameter(Mandatory)][ValidatePattern('^[0-9]+$')][string]$Ticket,
  [switch]$Replace,
  [string]$OrchRoot = 'C:/her-orch'
)

# Windows counterpart of scripts/plan-export.sh. Exports origin/main of this checkout (git archive, no .git) into
# <OrchRoot>/<Ticket>/scratch and prints the commit exported. A non-empty scratch or scratch-evidence folder is
# refused; -Replace deletes both first. It does not fetch: run `git fetch origin main` first when the tip may have moved.
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$dir = Join-Path $OrchRoot $Ticket
$scratch = Join-Path $dir 'scratch'
$evidence = Join-Path $dir 'scratch-evidence'
$commit = (& git -C $repo rev-parse --verify --quiet 'origin/main^{commit}')
if ($LASTEXITCODE -ne 0 -or -not $commit) { [Console]::Error.WriteLine("plan-export: origin/main not found in $repo"); exit 1 }
foreach ($folder in $scratch, $evidence) {
  if ((Test-Path -LiteralPath $folder) -and (Get-ChildItem -LiteralPath $folder -Force | Select-Object -First 1) -and -not $Replace) {
    [Console]::Error.WriteLine("plan-export: $folder is not empty; pass -Replace to delete scratch and scratch-evidence first")
    exit 1
  }
}
if ($Replace) { Remove-Item -LiteralPath $scratch, $evidence -Recurse -Force -ErrorAction SilentlyContinue }
$null = New-Item -ItemType Directory -Path $scratch -Force
$tar = Join-Path $dir 'scratch-export.tar'
try {
  & git -C $repo archive --format=tar -o $tar $commit
  if ($LASTEXITCODE -ne 0) { throw 'git archive failed' }
  & tar -xf $tar -C $scratch
  if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
} catch {
  Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
  [Console]::Error.WriteLine("plan-export: $($_.Exception.Message)")
  exit 1
} finally {
  Remove-Item -LiteralPath $tar -Force -ErrorAction SilentlyContinue
}
Write-Output "exported $commit to $scratch"

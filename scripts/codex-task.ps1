[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Name,
  [Parameter(Mandatory)][string]$WorkDir,
  [Parameter(Mandatory)][string]$PromptFile,
  [string]$Model = 'gpt-6-astra',
  [string]$Effort = 'high',
  [ValidateSet('workspace-write', 'read-only', 'none')][string]$Sandbox = 'none',
  [string[]]$AddDir = @('C:\her-orch', 'C:\her-targets', (Join-Path $env:USERPROFILE '.cargo')),
  [int]$TimeoutMinutes = 360
)

# Windows counterpart of scripts/codex-task.sh. Runs one Codex CLI task hidden at BelowNormal and writes receipts
# next to the prompt: <Name>.report.md (last message), <Name>.out.log, <Name>.err.log, <Name>.exit.txt (numeric code
# or "timeout"; <Name>.stopped.txt when the run was stopped after its report). workspace-write gets loopback/network access and write access to -AddDir. Sandbox `none`
# (the default, user decision 2026-11-03) runs without Codex's sandbox; pushes are made to fail through GIT_CONFIG_*
# for this process tree. Time spent only reconnecting during a network outage extends the timeout (at most 6 h).
# After the run, paths listed in <Name>.cleanup.txt (inside C:\her-orch or C:\her-targets only) are deleted.
$ErrorActionPreference = 'Stop'
# Start-Process cannot launch the npm shims (codex, codex.cmd), so use the native binary shipped inside the package.
$codex = Join-Path $env:APPDATA 'npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe'
if (-not (Test-Path -LiteralPath $codex -PathType Leaf)) { throw "Native codex.exe not found: $codex" }
$promptPath = (Resolve-Path -LiteralPath $PromptFile).Path
$outDir = Split-Path -Parent $promptPath
$report = Join-Path $outDir "$Name.report.md"
$outLog = Join-Path $outDir "$Name.out.log"
$errLog = Join-Path $outDir "$Name.err.log"
$exitFile = Join-Path $outDir "$Name.exit.txt"
Remove-Item -LiteralPath $report, $exitFile -ErrorAction SilentlyContinue

$codexArgs = @('exec', '--skip-git-repo-check', '-m', $Model, '-c', "model_reasoning_effort=`"$Effort`"", '-C', $WorkDir, '--color', 'never', '-o', $report)
if ($Sandbox -eq 'none') {
  $codexArgs += '--dangerously-bypass-approvals-and-sandbox'
  $env:GIT_CONFIG_COUNT = '2'
  $env:GIT_CONFIG_KEY_0 = 'remote.origin.pushurl'; $env:GIT_CONFIG_VALUE_0 = 'C:/nonexistent/push-disabled'
  $env:GIT_CONFIG_KEY_1 = 'push.default'; $env:GIT_CONFIG_VALUE_1 = 'nothing'
} else {
  $codexArgs += @('-s', $Sandbox)
}
if ($Sandbox -eq 'workspace-write') {
  $codexArgs += @('-c', 'sandbox_workspace_write.network_access=true')
  foreach ($dir in $AddDir) { [IO.Directory]::CreateDirectory($dir) | Out-Null; $codexArgs += @('--add-dir', $dir) }
}
$codexArgs += '-'
$quoted = $codexArgs | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }

# Workers cannot delete their own temporary folders (Codex's approval review refuses it), so they list them, one
# absolute path per line, in <Name>.cleanup.txt next to the prompt; after the run this deletes each listed path that
# lies strictly inside C:\her-orch or C:\her-targets and records what it did in <Name>.cleanup.log.
function Invoke-Cleanup {
  $list = Join-Path $outDir "$Name.cleanup.txt"
  if (-not (Test-Path -LiteralPath $list)) { return }
  $roots = @('C:\her-orch\', 'C:\her-targets\')
  $log = Join-Path $outDir "$Name.cleanup.log"
  foreach ($entry in Get-Content -LiteralPath $list -Encoding UTF8) {
    $raw = $entry.Trim()
    if (-not $raw) { continue }
    try { $full = [IO.Path]::GetFullPath($raw.Replace('/', '\')).TrimEnd('\') } catch { Add-Content -LiteralPath $log -Value "refused (bad path): $raw"; continue }
    $inside = $roots | Where-Object { $full.StartsWith($_, [StringComparison]::OrdinalIgnoreCase) -and $full.Length -gt $_.Length }
    if (-not $inside) { Add-Content -LiteralPath $log -Value "refused (outside C:\her-orch and C:\her-targets): $full"; continue }
    if (-not (Test-Path -LiteralPath $full)) { Add-Content -LiteralPath $log -Value "absent: $full"; continue }
    try { Remove-Item -LiteralPath $full -Recurse -Force -ErrorAction Stop; Add-Content -LiteralPath $log -Value "deleted: $full" }
    catch { Add-Content -LiteralPath $log -Value "failed: $full ($($_.Exception.Message))" }
  }
}

$process = Start-Process -FilePath $codex -ArgumentList $quoted -WindowStyle Hidden -PassThru `
  -RedirectStandardInput $promptPath -RedirectStandardOutput $outLog -RedirectStandardError $errLog
$null = $process.Handle  # keep the handle open so ExitCode is available after exit
try { $process.PriorityClass = [Diagnostics.ProcessPriorityClass]::BelowNormal } catch { }

# Codex sometimes stays alive after writing its final report (seen 2026-11-03: report at 22:45, process held until
# the 6 h timeout). Once the report exists and has not changed for 10 minutes, stop this run's own process tree.
$deadline = (Get-Date).AddMinutes($TimeoutMinutes)
$extendedUntil = (Get-Date).AddMinutes($TimeoutMinutes + 360)
# Tail of the (locked) err log, read with shared access.
function Get-ErrTail {
  try {
    $fs = [IO.File]::Open($errLog, 'Open', 'Read', 'ReadWrite')
    try { $len = [Math]::Min(4096, $fs.Length); $null = $fs.Seek(-$len, 'End'); $buf = New-Object byte[] $len; $null = $fs.Read($buf, 0, $len); [Text.Encoding]::UTF8.GetString($buf) }
    finally { $fs.Dispose() }
  } catch { '' }
}
while (-not $process.WaitForExit(30000)) {
  # A network outage leaves Codex only reconnecting; that time does not count against the run (at most 6 extra hours).
  $errItem = Get-Item -LiteralPath $errLog -ErrorAction SilentlyContinue
  if ($errItem -and ((Get-Date) - $errItem.LastWriteTime).TotalMinutes -lt 2 -and (Get-ErrTail) -match 'waiting for network|request timed out' -and $deadline -lt $extendedUntil) {
    $deadline = $deadline.AddSeconds(30)
  }
  $finished = Get-Item -LiteralPath $report -ErrorAction SilentlyContinue
  if ($finished -and $finished.Length -gt 0 -and ((Get-Date) - $finished.LastWriteTime).TotalMinutes -ge 10) {
    & taskkill.exe /PID $process.Id /T /F | Out-Null
    Set-Content -LiteralPath $exitFile -Value '0' -Encoding ASCII
    # The redirected err log stays locked by this process; record the stop next to the exit file instead.
    Set-Content -LiteralPath (Join-Path $outDir "$Name.stopped.txt") -Value "report written; process stopped after 10 idle minutes" -Encoding ASCII
    Invoke-Cleanup
    exit 0
  }
  if ((Get-Date) -ge $deadline) {
    & taskkill.exe /PID $process.Id /T /F | Out-Null
    Set-Content -LiteralPath $exitFile -Value 'timeout' -Encoding ASCII
    exit 124
  }
}
$process.WaitForExit()
Set-Content -LiteralPath $exitFile -Value ([string]$process.ExitCode) -Encoding ASCII
Invoke-Cleanup
exit $process.ExitCode

[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Name,
  [Parameter(Mandatory)][string]$WorkDir,
  [Parameter(Mandatory)][string]$PromptFile,
  [string]$Model = 'strata/swift-1.5-iq3_s',
  [ValidateSet('off', 'minimal', 'low', 'medium', 'high', 'xhigh')][string]$Thinking = 'medium',
  [int]$TimeoutMinutes = 240,
  # Continue the newest session in <Name>.session\ (after a config change or a stopped run) instead of starting fresh.
  [switch]$Continue
)

# Runs one pi coding-agent task on the rig's local model, hidden at BelowNormal, and writes receipts next to the
# prompt: <Name>.report.md (final message), <Name>.err.log, <Name>.exit.txt (numeric code or "timeout") and the pi
# session under <Name>.session\ (full transcript for review). The provider comes from ~/.pi/agent/models.json; its key
# is the STRATA_API_KEY user environment variable, never stored in the repo. Pushes to origin fail for this process
# tree; the key and the push block reach only the child, never the caller's environment.
$ErrorActionPreference = 'Stop'
# Start-Process cannot launch the npm shim, so run the package's entry script with node.
$cli = Join-Path $env:APPDATA 'npm\node_modules\@mariozechner\pi-coding-agent\dist\cli.js'
if (-not (Test-Path -LiteralPath $cli -PathType Leaf)) { throw "pi cli.js not found: $cli" }
$node = (Get-Command node.exe -ErrorAction Stop).Source
$childEnv = [ordered]@{
  STRATA_API_KEY = $(if ($env:STRATA_API_KEY) { $env:STRATA_API_KEY } else { [Environment]::GetEnvironmentVariable('STRATA_API_KEY', 'User') })
  GIT_CONFIG_COUNT = '2'
  GIT_CONFIG_KEY_0 = 'remote.origin.pushurl'; GIT_CONFIG_VALUE_0 = 'C:/nonexistent/push-disabled'
  GIT_CONFIG_KEY_1 = 'push.default'; GIT_CONFIG_VALUE_1 = 'nothing'
}
if (-not $childEnv.STRATA_API_KEY) { throw 'STRATA_API_KEY is not set' }

$promptPath = (Resolve-Path -LiteralPath $PromptFile).Path
$outDir = Split-Path -Parent $promptPath
$report = Join-Path $outDir "$Name.report.md"
$errLog = Join-Path $outDir "$Name.err.log"
$exitFile = Join-Path $outDir "$Name.exit.txt"
$sessionDir = Join-Path $outDir "$Name.session"
Remove-Item -LiteralPath $report, $exitFile -ErrorAction SilentlyContinue
[IO.Directory]::CreateDirectory($sessionDir) | Out-Null

# The prompt arrives on stdin; print mode merges it into the initial message (with -Continue, into the resumed session). A console stdin would block forever.
$task = if ($Continue) { 'You were stopped and are resumed in the same session. Check the state of the worktree and your evidence directory, then carry on with the task. End with the report it asks for.' } else { 'Carry out the task described above. End with the report it asks for.' }
$piArgs = @($cli, '-p', '--model', $Model, '--thinking', $Thinking, '--session-dir', $sessionDir) + $(if ($Continue) { @('--continue') } else { @() }) + @($task)
$quoted = $piArgs | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }
# The child inherits these at launch; restore the caller's values right after so an in-process caller keeps its
# own push access and never holds the key.
$saved = @{}
foreach ($k in $childEnv.Keys) { $saved[$k] = [Environment]::GetEnvironmentVariable($k, 'Process'); [Environment]::SetEnvironmentVariable($k, $childEnv[$k], 'Process') }
try {
  $process = Start-Process -FilePath $node -ArgumentList $quoted -WorkingDirectory $WorkDir -WindowStyle Hidden -PassThru `
    -RedirectStandardInput $promptPath -RedirectStandardOutput $report -RedirectStandardError $errLog
} finally {
  foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k], 'Process') }
}
$null = $process.Handle  # keep the handle open so ExitCode is available after exit
try { $process.PriorityClass = [Diagnostics.ProcessPriorityClass]::BelowNormal } catch { }

if (-not $process.WaitForExit($TimeoutMinutes * 60000)) {
  & taskkill.exe /PID $process.Id /T /F | Out-Null
  Set-Content -LiteralPath $exitFile -Value 'timeout' -Encoding ASCII
  exit 124
}
$process.WaitForExit()
Set-Content -LiteralPath $exitFile -Value ([string]$process.ExitCode) -Encoding ASCII
exit $process.ExitCode

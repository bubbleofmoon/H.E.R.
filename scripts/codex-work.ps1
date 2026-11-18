[CmdletBinding()]
param(
  [Parameter(Mandatory)][ValidateSet('implement', 'plan')][string]$Mode,
  [Parameter(Mandatory)][string]$Name,
  [Parameter(Mandatory)][string]$PromptFile,
  [string]$WorkDir,
  [string]$MessageFile,
  [string]$Ticket,
  [string]$Model = 'gpt-6.2-sol',
  [string]$Effort,
  [int]$TimeoutMinutes = 360
)

# Windows counterpart of scripts/codex-work.sh: runs a Codex plan or implement task for one ticket through
# scripts/codex-task.ps1; the wrapper, never Codex, touches git and installs the result.
#   -Mode implement -Name <n> -WorkDir <worktree> -PromptFile <f> -MessageFile <f> [-Model] [-Effort] [-TimeoutMinutes]
#   -Mode plan -Ticket <n> -Name <n> -PromptFile <f> [-Model] [-Effort] [-TimeoutMinutes]
# Defaults: gpt-6.2-sol; effort medium for implement, high for plan; 360 minutes. Receipts sit next to the prompt:
# codex-task's <Name>.report.md and <Name>.exit.txt, plus <Name>.push.txt (implement: `pushed <sha>`, `no-changes`,
# `commit-failed` or `push-failed`) or <Name>.plan.txt (plan: `installed` or `no-plan`).
# implement: HER_GITLEAKS must name the gitleaks binary; the worktree's HEAD must be her/<n>-<slug>, never main.
#   Codex (workspace-write) writes only the worktree and <orch>/<n>/tmp. After it exits 0 with a report, this script
#   stages the worktree, commits with the message file and pushes the branch; it exits 7 when that fails.
# plan: Codex (workspace-write) writes only <orch>/<n>/scratch and <orch>/<n>/plan-out; the plan it leaves in
#   plan-out/plan.md becomes <orch>/<n>/plan.md, and plan.sha256 holds its sha256 line.
# <orch> is $env:HER_ORCH, default C:/her-orch.
$ErrorActionPreference = 'Stop'
$orch = if ($env:HER_ORCH) { $env:HER_ORCH.TrimEnd('/', '\') } else { 'C:/her-orch' }
$task = Join-Path $PSScriptRoot 'codex-task.ps1'
if (-not $Effort) { $Effort = if ($Mode -eq 'plan') { 'high' } else { 'medium' } }

function Stop-Refused([string]$Why) { [Console]::Error.WriteLine("codex-work: $Why"); exit 64 }
function Test-Link([string]$Path) { $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue; [bool]($item -and $item.LinkType) }
function Invoke-Task([string]$Dir, [string[]]$Extra) {
  # The child script ends with `exit`, which leaves this script's flow alone and sets $LASTEXITCODE.
  $null = & $task -Name $Name -WorkDir $Dir -PromptFile $PromptFile -Model $Model -Effort $Effort -Sandbox workspace-write `
    -TimeoutMinutes $TimeoutMinutes -AddDir $Extra
  return [int]$LASTEXITCODE
}

if ($Name -notmatch '^[A-Za-z0-9][A-Za-z0-9_-]*$') { Stop-Refused "bad name: $Name" }
if ($TimeoutMinutes -lt 1 -or $TimeoutMinutes -gt 999) { Stop-Refused "bad minutes: $TimeoutMinutes" }
if (-not (Test-Path -LiteralPath $PromptFile -PathType Leaf)) { Stop-Refused "no prompt file: $PromptFile" }
$outDir = Split-Path -Parent (Resolve-Path -LiteralPath $PromptFile).Path

if ($Mode -eq 'implement') {
  if (-not $WorkDir -or -not $MessageFile) { Stop-Refused 'implement needs -WorkDir and -MessageFile' }
  $message = Get-Item -LiteralPath $MessageFile -ErrorAction SilentlyContinue
  if (-not $message -or $message.Length -eq 0) { Stop-Refused "no commit message: $MessageFile" }
  if (-not $env:HER_GITLEAKS -or -not (Test-Path -LiteralPath $env:HER_GITLEAKS -PathType Leaf)) { Stop-Refused 'HER_GITLEAKS must name the gitleaks binary' }
  if (-not (Test-Path -LiteralPath $WorkDir -PathType Container)) { Stop-Refused "not a worktree root: $WorkDir" }
  function Get-Branch { $b = & git -C $WorkDir symbolic-ref --short -q HEAD 2> $null; if ($LASTEXITCODE -eq 0) { "$b".Trim() } else { '' } }
  $top = (& git -C $WorkDir rev-parse --show-toplevel 2> $null)
  if ($LASTEXITCODE -ne 0 -or (Resolve-Path -LiteralPath "$top").Path -ne (Resolve-Path -LiteralPath $WorkDir).Path) { Stop-Refused "not a worktree root: $WorkDir" }
  $branch = Get-Branch
  if ($branch -notmatch '^her/([0-9]+)-[A-Za-z0-9._-]+$') { Stop-Refused "HEAD is not a ticket branch (her/<n>-<slug>): $(if ($branch) { $branch } else { 'detached' })" }
  $ticketNumber = $Matches[1]
  $pushFile = Join-Path $outDir "$Name.push.txt"
  Remove-Item -LiteralPath $pushFile -ErrorAction SilentlyContinue
  $tmp = "$orch/$ticketNumber/tmp"
  [IO.Directory]::CreateDirectory($tmp) | Out-Null
  $code = Invoke-Task $WorkDir @($tmp)
  if ($code -ne 0) { exit $code }
  if ((Get-Branch) -ne $branch) { Stop-Refused "HEAD moved during the run: $(Get-Branch)" }
  & git -C $WorkDir add -A
  if ($LASTEXITCODE -ne 0) { exit 7 }
  & git -C $WorkDir diff --cached --quiet
  if ($LASTEXITCODE -eq 0) {
    Set-Content -LiteralPath $pushFile -Value 'no-changes' -Encoding ASCII
    [Console]::Error.WriteLine('no-changes: the run left nothing to commit')
    exit 0
  }
  & git -C $WorkDir commit -q -F $MessageFile
  if ($LASTEXITCODE -ne 0) { Set-Content -LiteralPath $pushFile -Value 'commit-failed' -Encoding ASCII; exit 7 }
  $sha = (& git -C $WorkDir rev-parse HEAD).Trim()
  & git -C $WorkDir push -q -u origin $branch
  $pushed = $LASTEXITCODE
  $remote = ((& git -C $WorkDir ls-remote origin "refs/heads/$branch" 2> $null) -split "`t")[0]
  if ($pushed -ne 0 -or $remote -ne $sha) { Set-Content -LiteralPath $pushFile -Value 'push-failed' -Encoding ASCII; exit 7 }
  Set-Content -LiteralPath $pushFile -Value "pushed $sha" -Encoding ASCII
  Write-Output "pushed $sha"
  exit 0
}

if ($Ticket -notmatch '^[1-9][0-9]*$') { Stop-Refused "bad ticket: $Ticket" }
$dir = "$orch/$Ticket"
$scratch = "$dir/scratch"
$stage = "$dir/plan-out"
if ((Test-Link $dir) -or -not (Test-Path -LiteralPath $scratch -PathType Container) -or (Test-Link $scratch)) {
  Stop-Refused "no scratch export (scripts/plan-export.ps1 -Ticket $Ticket -Replace): $scratch"
}
if (Test-Link $stage) { Stop-Refused "plan-out is a link: $stage" }
$planFile = Join-Path $outDir "$Name.plan.txt"
Remove-Item -LiteralPath $planFile -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
[IO.Directory]::CreateDirectory($stage) | Out-Null
$code = Invoke-Task $scratch @($stage)
$staged = Get-Item -LiteralPath "$stage/plan.md" -Force -ErrorAction SilentlyContinue
if ($code -eq 0 -and $staged -and $staged.Length -gt 0 -and -not $staged.LinkType) {
  Move-Item -LiteralPath "$stage/plan.md" -Destination "$dir/plan.md" -Force
  Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
  Set-Content -LiteralPath $planFile -Value 'installed' -Encoding ASCII
  $hash = (Get-FileHash -LiteralPath "$dir/plan.md" -Algorithm SHA256).Hash.ToLowerInvariant()
  $line = "$hash  plan.md"
  [IO.File]::WriteAllText("$dir/plan.sha256", "$line`n")
  Write-Output $line
  exit 0
}
Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
if ($code -eq 0) {
  Set-Content -LiteralPath $planFile -Value 'no-plan' -Encoding ASCII
  [Console]::Error.WriteLine('codex-work: the run left no plan-out/plan.md')
  $code = 1
}
exit $code

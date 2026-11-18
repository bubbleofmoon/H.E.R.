[CmdletBinding()]
param(
  [Parameter(Mandatory)][ValidateSet('implement', 'plan')][string]$Mode,
  [Parameter(Mandatory)][string]$Name,
  [Parameter(Mandatory)][string]$PromptFile,
  [string]$WorkDir,
  [string]$Ticket,
  [string]$Model = 'gpt-6.2-sol',
  [string]$Effort,
  [int]$TimeoutMinutes = 360
)

# Windows counterpart of scripts/codex-work.sh: runs a Codex plan or implement task for one ticket through
# scripts/codex-task.ps1 and verifies what it left.
#   -Mode implement -Name <n> -WorkDir <worktree> -PromptFile <f> [-Model] [-Effort] [-TimeoutMinutes]
#   -Mode plan -Ticket <n> -Name <n> -PromptFile <f> [-Model] [-Effort] [-TimeoutMinutes]
# Defaults: gpt-6.2-sol; effort medium for implement, high for plan; 360 minutes. Receipts sit next to the prompt:
# codex-task's <Name>.report.md and <Name>.exit.txt, plus <Name>.push.txt and <Name>.staged.txt (implement) or
# <Name>.plan.txt (plan: `installed` or `no-plan`).
# implement: HER_GITLEAKS must name the gitleaks binary (Codex's own push runs the pre-push hook with it). Before the run
#   the worktree must be clean, its HEAD on her/<n>-<slug> (never main), and <orch>/<n> and its tmp must not be links.
#   Codex (workspace-write, network) writes the worktree, <orch>/<n>/tmp and the repository's common git dir, and
#   commits and pushes its own ticket branch. After it exits 0 with a report this script checks, in order, and writes
#   the first line of <Name>.push.txt: `branch-changed <branch>`, `dirty <paths>` (the worktree is not clean),
#   `no-changes` (no new commit), `unpushed` (the remote head is not the local HEAD) or `pushed <sha>`. The first,
#   second and fourth exit 8; the others exit 0. A second line `protected <paths>` names the commits' paths under
#   .githooks/ or in the scan scripts, scan config and scan exceptions, which the gate must review.
#   <Name>.staged.txt lists the commits since the pre-run HEAD (`commit <sha> <subject>`) and the name-status of
#   every path they touched (`<status><TAB><path>`).
# plan: Codex (workspace-write) writes only <orch>/<n>/scratch and <orch>/<n>/plan-out; the plan it leaves there as a
#   regular, single-link plan.md is copied to <orch>/<n>/plan.md, and plan.sha256 holds its sha256 line.
# <orch> is $env:HER_ORCH, default C:/her-orch.
$ErrorActionPreference = 'Stop'
$orch = if ($env:HER_ORCH) { $env:HER_ORCH.TrimEnd('/', '\') } else { 'C:/her-orch' }
$task = Join-Path $PSScriptRoot 'codex-task.ps1'
if (-not $Effort) { $Effort = if ($Mode -eq 'plan') { 'high' } else { 'medium' } }

function Stop-Refused([string]$Why) { [Console]::Error.WriteLine("codex-work: $Why"); exit 64 }
function Test-Link([string]$Path) { $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue; [bool]($item -and $item.LinkType) }
# Native git: stderr text is not an error here, the exit code is.
function Invoke-Git { $ErrorActionPreference = 'Continue'; & git @args }
function Invoke-Task([string]$Dir, [string[]]$Extra, [string]$Temp) {
  # The caller's temp variables point at a writable folder for the Codex process only; they are restored after the run.
  $names = 'TMPDIR', 'TEMP', 'TMP'
  $saved = @{}
  foreach ($n in $names) { $saved[$n] = [Environment]::GetEnvironmentVariable($n); [Environment]::SetEnvironmentVariable($n, $Temp) }
  try {
    # The child script ends with `exit`, which leaves this script's flow alone and sets $LASTEXITCODE.
    $null = & $task -Name $Name -WorkDir $Dir -PromptFile $PromptFile -Model $Model -Effort $Effort -Sandbox workspace-write `
      -TimeoutMinutes $TimeoutMinutes -AddDir $Extra
    return [int]$LASTEXITCODE
  } finally { foreach ($n in $names) { [Environment]::SetEnvironmentVariable($n, $saved[$n]) } }
}

if ($Name -notmatch '^[A-Za-z0-9][A-Za-z0-9_-]*$') { Stop-Refused "bad name: $Name" }
if ($TimeoutMinutes -lt 1 -or $TimeoutMinutes -gt 999) { Stop-Refused "bad minutes: $TimeoutMinutes" }
if (-not (Test-Path -LiteralPath $PromptFile -PathType Leaf)) { Stop-Refused "no prompt file: $PromptFile" }
$outDir = Split-Path -Parent (Resolve-Path -LiteralPath $PromptFile).Path

if ($Mode -eq 'implement') {
  if (-not $WorkDir) { Stop-Refused 'implement needs -WorkDir' }
  if (-not $env:HER_GITLEAKS -or -not (Test-Path -LiteralPath $env:HER_GITLEAKS -PathType Leaf)) { Stop-Refused 'HER_GITLEAKS must name the gitleaks binary' }
  if (-not (Test-Path -LiteralPath $WorkDir -PathType Container)) { Stop-Refused "not a worktree root: $WorkDir" }
  function Get-Branch { $b = Invoke-Git -C $WorkDir symbolic-ref --short -q HEAD 2> $null; if ($LASTEXITCODE -eq 0) { "$b".Trim() } else { '' } }
  $top = Invoke-Git -C $WorkDir rev-parse --show-toplevel 2> $null
  if ($LASTEXITCODE -ne 0 -or (Resolve-Path -LiteralPath "$top").Path -ne (Resolve-Path -LiteralPath $WorkDir).Path) { Stop-Refused "not a worktree root: $WorkDir" }
  $branch = Get-Branch
  if ($branch -notmatch '^her/([0-9]+)-[A-Za-z0-9._-]+$') { Stop-Refused "HEAD is not a ticket branch (her/<n>-<slug>): $(if ($branch) { $branch } else { 'detached' })" }
  $ticketNumber = $Matches[1]
  if (Invoke-Git -C $WorkDir status --porcelain --untracked-files=all) { Stop-Refused "the worktree is not clean: $WorkDir" }
  $tmp = "$orch/$ticketNumber/tmp"
  if ((Test-Link "$orch/$ticketNumber") -or (Test-Link $tmp)) { Stop-Refused "a link in the ticket folder: $orch/$ticketNumber" }
  $common = Invoke-Git -C $WorkDir rev-parse --path-format=absolute --git-common-dir 2> $null
  if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath "$common" -PathType Container)) { Stop-Refused "no git common dir for $WorkDir" }
  $pre = (Invoke-Git -C $WorkDir rev-parse HEAD).Trim()
  [IO.Directory]::CreateDirectory($tmp) | Out-Null
  if (Test-Link $tmp) { Stop-Refused "a link in the ticket folder: $tmp" }
  $pushFile = Join-Path $outDir "$Name.push.txt"
  $stagedFile = Join-Path $outDir "$Name.staged.txt"
  Remove-Item -LiteralPath $pushFile, $stagedFile -ErrorAction SilentlyContinue
  $code = Invoke-Task $WorkDir @($tmp, "$common") $tmp
  if ($code -ne 0) { exit $code }
  function Set-Receipt([string[]]$Lines) { Set-Content -LiteralPath $pushFile -Value $Lines -Encoding ASCII }
  $now = Get-Branch
  if ($now -ne $branch) { Set-Receipt "branch-changed $(if ($now) { $now } else { 'detached' })"; exit 8 }
  $range = "$pre..HEAD"
  $commits = @(Invoke-Git -C $WorkDir log --format='commit %H %s' $range)
  $changes = @(Invoke-Git -C $WorkDir -c core.quotepath=off log -m --no-renames --name-status --format= $range | Where-Object { $_ })
  Set-Content -LiteralPath $stagedFile -Value (@($commits) + @($changes | Sort-Object -Unique -CaseSensitive)) -Encoding ASCII
  $stray = @(Invoke-Git -C $WorkDir -c core.quotepath=off status --porcelain --untracked-files=all --no-renames | ForEach-Object { $_.Substring(3) })
  if ($stray.Count -gt 0) { Set-Receipt "dirty $($stray -join ' ')"; exit 8 }
  $sha = (Invoke-Git -C $WorkDir rev-parse HEAD).Trim()
  if ($sha -eq $pre) { Set-Receipt 'no-changes'; [Console]::Error.WriteLine('no-changes: the run left no commit'); exit 0 }
  $remote = ((Invoke-Git -C $WorkDir ls-remote origin "refs/heads/$branch" 2> $null) -split "`t")[0]
  if ($remote -ne $sha) { Set-Receipt 'unpushed'; [Console]::Error.WriteLine("unpushed: the remote head is not $sha"); exit 8 }
  $paths = @(Invoke-Git -C $WorkDir -c core.quotepath=off log -m --no-renames --name-only --format= $range | Where-Object { $_ } | Sort-Object -Unique -CaseSensitive)
  $held = @($paths | Where-Object {
      $l = $_.ToLowerInvariant()
      $l.StartsWith('.githooks/') -or $l -in 'scripts/check-repo.py', 'scripts/scan-public.py', 'scripts/tests/test_scan_public.py', '.gitleaks.toml', '.public-scan.json'
    })
  if ($held.Count -gt 0) { Set-Receipt "pushed $sha", "protected $($held -join ' ')"; [Console]::Error.WriteLine("protected $($held -join ' ')") }
  else { Set-Receipt "pushed $sha" }
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
$code = Invoke-Task $scratch @($stage) $stage
if ($code -ne 0) { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue; exit $code }
# The plan must be a regular file with no link: it is copied into a fresh file, never moved. Every step is checked and
# `installed` follows the last one.
$installed = $false
$tmpPlan = "$dir/.plan.md.$PID.tmp"
try {
  $staged = Get-Item -LiteralPath "$stage/plan.md" -Force -ErrorAction Stop
  if (-not $staged.PSIsContainer -and -not $staged.LinkType -and $staged.Length -gt 0 -and -not (Test-Path -LiteralPath "$dir/plan.md" -PathType Container)) {
    [IO.File]::Copy($staged.FullName, $tmpPlan, $true)
    Move-Item -LiteralPath $tmpPlan -Destination "$dir/plan.md" -Force
    $hash = (Get-FileHash -LiteralPath "$dir/plan.md" -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$dir/plan.sha256", "$hash  plan.md`n")
    $installed = $true
  }
} catch { $installed = $false }
Remove-Item -LiteralPath $tmpPlan -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
if (-not $installed) {
  Set-Content -LiteralPath $planFile -Value 'no-plan' -Encoding ASCII
  [Console]::Error.WriteLine('codex-work: the run left no regular single-link plan-out/plan.md that could be installed')
  exit 1
}
Set-Content -LiteralPath $planFile -Value 'installed' -Encoding ASCII
Write-Output "$hash  plan.md"
exit 0

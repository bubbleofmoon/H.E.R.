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
# codex-task's <Name>.report.md and <Name>.exit.txt, plus <Name>.push.txt, <Name>.staged.txt and <Name>.scan.txt
# (implement) or <Name>.plan.txt (plan: `installed` or `no-plan`).
# implement: HER_GITLEAKS must name the gitleaks binary (Codex's own push runs the pre-push hook with it). Before the run
#   the worktree must be clean, its HEAD on her/<n>-<slug> (never main), <orch>/<n> and its tmp must not be links, and
#   the remote must answer. An existing remote branch must match local HEAD, or the receipt says `pre-remote-mismatch`.
#   Codex runs unsandboxed with network access and commits and pushes its own ticket branch.
#   Before and after the run, the wrapper hashes the listed entries from config and config.worktree with branch.*
#   dropped, and hashes commondir, the .git file and hooks/ by bytes. A difference gives `config-changed <files>`
#   and stops verification.
#   Every later git call ignores them anyway: it uses the git dir found before the run, core.fsmonitor=false and
#   core.hooksPath off, and asks $env:HER_REMOTE_URL (default https://github.com/bubbleofmoon/H.E.R..git) for the
#   remote head, never `origin`.
#   <Name>.push.txt line 1 is the first that applies: `rewritten` (the pre-run HEAD is not an ancestor of HEAD),
#   `branch-changed <branch>`, `git-failed`, `dirty <paths>` plus `remote moved|unchanged|unknown`, `remote-moved`
#   (local HEAD unchanged but remote head moved), `no-changes` (both heads unchanged), `unpushed` or `pushed <sha>`.
#   A line `protected <paths>` follows whenever pre-run HEAD..HEAD holds commits touching .githooks/ or the scan scripts,
#   scan config or scan exceptions; a line `scan-refused` follows when scripts/scan-public.py of this checkout, run on
#   the repository's main checkout with --added-from-stdin (--revs-from-stdin when the remote had no such branch) and
#   --objects-from-stdin over those commits, refuses or cannot run (<Name>.scan.txt has its output). That scan is
#   detection after the push, never a barrier. Exit 8 for `config-changed`, `rewritten`, `branch-changed`, `git-failed`,
#   `dirty`, `pre-remote-mismatch`, `remote-moved`, `unpushed` and `scan-refused`, else 0.
#   <Name>.staged.txt lists the commits since the pre-run HEAD (`commit <sha> <subject>`) and the name-status of
#   every path they touched (`<status><TAB><path>`).
# plan: Codex (workspace-write) writes only <orch>/<n>/scratch and <orch>/<n>/plan-out; the plan it leaves there as a
#   regular, single-link plan.md is copied to <orch>/<n>/plan.md, and plan.sha256 holds its sha256 line. A failed
#   install restores the old plan.md and plan.sha256.
# <orch> is $env:HER_ORCH, default C:/her-orch.
$ErrorActionPreference = 'Stop'
$orch = if ($env:HER_ORCH) { $env:HER_ORCH.TrimEnd('/', '\') } else { 'C:/her-orch' }
$task = Join-Path $PSScriptRoot 'codex-task.ps1'
if (-not $Effort) { $Effort = if ($Mode -eq 'plan') { 'high' } else { 'medium' } }
$nullDevice = if ($IsWindows -ne $false) { 'NUL' } else { '/dev/null' }

function Stop-Refused([string]$Why) { [Console]::Error.WriteLine("codex-work: $Why"); exit 64 }
function Test-Link([string]$Path) { $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue; [bool]($item -and $item.LinkType) }
# Native git: stderr text is not an error here, the exit code is.
function Invoke-Git { $ErrorActionPreference = 'Continue'; & git @args }
function Invoke-Task([string]$Dir, [string]$Sandbox, [string]$Temp, [string[]]$Extra = @()) {
  # The caller's temp variables point at a writable folder for the Codex process only; they are restored after the run.
  $names = 'TMPDIR', 'TEMP', 'TMP'
  $saved = @{}
  foreach ($n in $names) { $saved[$n] = [Environment]::GetEnvironmentVariable($n); [Environment]::SetEnvironmentVariable($n, $Temp) }
  try {
    # The child script ends with `exit`, which leaves this script's flow alone and sets $LASTEXITCODE.
    $null = & $task -Name $Name -WorkDir $Dir -PromptFile $PromptFile -Model $Model -Effort $Effort -Sandbox $Sandbox `
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
  $remoteUrl = if ($env:HER_REMOTE_URL) { $env:HER_REMOTE_URL } else { 'https://github.com/bubbleofmoon/H.E.R..git' }
  if (-not (Test-Path -LiteralPath $WorkDir -PathType Container)) { Stop-Refused "not a worktree root: $WorkDir" }
  $top = Invoke-Git -C $WorkDir rev-parse --show-toplevel 2> $null
  if ($LASTEXITCODE -ne 0 -or (Resolve-Path -LiteralPath "$top").Path -ne (Resolve-Path -LiteralPath $WorkDir).Path) { Stop-Refused "not a worktree root: $WorkDir" }
  $wtgit = "$(Invoke-Git -C $WorkDir rev-parse --absolute-git-dir 2> $null)".Trim()
  if ($LASTEXITCODE -ne 0 -or -not $wtgit) { Stop-Refused "no git dir for $WorkDir" }
  $common = "$(Invoke-Git -C $WorkDir rev-parse --path-format=absolute --git-common-dir 2> $null)".Trim()
  if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $common -PathType Container)) { Stop-Refused "no git common dir for $WorkDir" }
  if ((Split-Path -Leaf $common) -ne '.git') { Stop-Refused "the common git dir is not a checkout's .git: $common" }
  $mainRoot = Split-Path -Parent $common
  # Every git call from here on names the git dir found now, and turns off the settings that run programs.
  function G { Invoke-Git "--git-dir=$wtgit" "--work-tree=$WorkDir" -c core.fsmonitor=false -c "core.hooksPath=$nullDevice" @args }
  function Get-Branch { $b = G symbolic-ref --short -q HEAD 2> $null; if ($LASTEXITCODE -eq 0) { "$b".Trim() } else { '' } }
  $branch = Get-Branch
  if ($branch -notmatch '^her/([0-9]+)-[A-Za-z0-9._-]+$') { Stop-Refused "HEAD is not a ticket branch (her/<n>-<slug>): $(if ($branch) { $branch } else { 'detached' })" }
  $ticketNumber = $Matches[1]
  if (G status --porcelain --untracked-files=all) { Stop-Refused "the worktree is not clean: $WorkDir" }
  $tmp = "$orch/$ticketNumber/tmp"
  if ((Test-Link "$orch/$ticketNumber") -or (Test-Link $tmp)) { Stop-Refused "a link in the ticket folder: $orch/$ticketNumber" }
  $pre = (G rev-parse HEAD).Trim()
  $preRemote = ((G ls-remote $remoteUrl "refs/heads/$branch" 2> $null) -split "`t")[0]
  if ($LASTEXITCODE -ne 0) { Stop-Refused "the remote does not answer: $remoteUrl" }
  $pushFile = Join-Path $outDir "$Name.push.txt"
  $stagedFile = Join-Path $outDir "$Name.staged.txt"
  $scanFile = Join-Path $outDir "$Name.scan.txt"
  Remove-Item -LiteralPath $pushFile, $stagedFile, $scanFile -ErrorAction SilentlyContinue
  function Set-Receipt([string[]]$Lines) { Set-Content -LiteralPath $pushFile -Value $Lines -Encoding ASCII }
  if ($preRemote -and $preRemote -ne $pre) {
    Set-Receipt 'pre-remote-mismatch'
    [Console]::Error.WriteLine('pre-remote-mismatch')
    exit 8
  }
  [IO.Directory]::CreateDirectory($tmp) | Out-Null
  if (Test-Link $tmp) { Stop-Refused "a link in the ticket folder: $tmp" }
  # What git, its hooks and later sessions read: hashed before and after the run.
  function Get-ConfigSnap([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if ($item.LinkType) { return "link:$($item.Target -join ',')  $Path" }
    if ($item.PSIsContainer) { return "other  $Path" }
    $content = ''
    if ($item) {
      $entries = @(G config --file $Path --no-includes --list)
      if ($LASTEXITCODE -ne 0) {
        return "invalid:$((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant())  $Path"
      }
      $entries = @($entries | Where-Object { -not $_.StartsWith('branch.', [StringComparison]::Ordinal) })
      if ($entries.Count) { $content = ($entries -join "`n") + "`n" }
    }
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
      $hash = [BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($content))).Replace('-', '').ToLowerInvariant()
      "$hash  $Path"
    } finally { $sha.Dispose() }
  }
  function Get-Snap {
    $items = @("$wtgit/commondir", "$WorkDir/.git")
    if (Test-Path -LiteralPath "$common/hooks" -PathType Container) {
      $items += @(Get-ChildItem -LiteralPath "$common/hooks" -Recurse -Force | Where-Object { -not $_.PSIsContainer -or $_.LinkType } | ForEach-Object { $_.FullName })
    }
    $lines = @(
      Get-ConfigSnap "$common/config"
      Get-ConfigSnap "$wtgit/config.worktree"
      foreach ($f in $items) {
        $i = Get-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue
        if (-not $i) { continue }
        if ($i.LinkType) { "link:$($i.Target -join ',')  $f" }
        elseif ($i.PSIsContainer) { "other  $f" }
        else { "$((Get-FileHash -LiteralPath $f -Algorithm SHA256).Hash.ToLowerInvariant())  $f" }
      }
    )
    @($lines | Sort-Object -CaseSensitive)
  }
  $preSnap = Get-Snap
  $code = Invoke-Task $WorkDir bypass $tmp
  $postSnap = Get-Snap
  if (($preSnap -join "`n") -ne ($postSnap -join "`n")) {
    $changed = @(Compare-Object $preSnap $postSnap -CaseSensitive | ForEach-Object { ($_.InputObject -split '  ', 2)[1] } | Sort-Object -Unique) -join ' '
    Set-Receipt "config-changed $changed"
    [Console]::Error.WriteLine("config-changed $changed")
    exit 8
  }
  if ($code -ne 0) { exit $code }
  $lines = @()
  $rc = 0
  function Stop-GitFailed {
    if ($lines.Count -gt 0) {
      Set-Receipt $lines[0]
      [Console]::Error.WriteLine($lines[0])
    } else {
      Set-Receipt 'git-failed'
      [Console]::Error.WriteLine('codex-work: a git call failed')
    }
    exit 8
  }
  $now = Get-Branch
  $head = "$(G rev-parse HEAD)".Trim()
  if ($LASTEXITCODE -ne 0) { Stop-GitFailed }
  $null = G merge-base --is-ancestor $pre $head 2> $null
  if ($LASTEXITCODE -ne 0) { $lines = @('rewritten'); $rc = 8 }
  elseif ($now -ne $branch) { $lines = @("branch-changed $(if ($now) { $now } else { 'detached' })"); $rc = 8 }
  $range = "$pre..$head"
  $list = @(G rev-list $range) -join "`n"
  if ($LASTEXITCODE -ne 0) { Stop-GitFailed }
  $commits = @(G log --format='commit %H %s' $range)
  if ($LASTEXITCODE -ne 0) { Stop-GitFailed }
  $changes = @(G -c core.quotepath=off log -m --no-renames --name-status --format= $range)
  if ($LASTEXITCODE -ne 0) { Stop-GitFailed }
  Set-Content -LiteralPath $stagedFile -Value (@($commits | Where-Object { $_ }) + @($changes | Where-Object { $_ } | Sort-Object -Unique -CaseSensitive)) -Encoding ASCII
  $rawPaths = (G log -m --no-renames --name-only -z --format= $range) -join "`n"
  if ($LASTEXITCODE -ne 0) { Stop-GitFailed }
  $paths = @(($rawPaths -split "`0") | ForEach-Object { $_.Trim("`n", "`r") } | Where-Object { $_ } | Sort-Object -Unique -CaseSensitive)
  $held = @($paths | Where-Object {
      $l = $_.ToLowerInvariant()
      $l.StartsWith('.githooks/') -or $l -in 'scripts/check-repo.py', 'scripts/scan-public.py', 'scripts/tests/test_scan_public.py', '.gitleaks.toml', '.public-scan.json'
    })
  if ($lines.Count -eq 0) {
    $status = @(G -c core.quotepath=off status --porcelain --untracked-files=all --no-renames)
    if ($LASTEXITCODE -ne 0) { Stop-GitFailed }
    $stray = @($status | Where-Object { $_ } | ForEach-Object { $_.Substring(3) })
    $remote = ((G ls-remote $remoteUrl "refs/heads/$branch" 2> $null) -split "`t")[0]
    $remoteOk = $LASTEXITCODE -eq 0
    if ($stray.Count -gt 0) {
      $moved = if (-not $remoteOk) { 'unknown' } elseif ($remote -ne $preRemote) { 'moved' } else { 'unchanged' }
      $lines = @("dirty $($stray -join ' ')", "remote $moved"); $rc = 8
    }
    elseif ($head -eq $pre -and -not $remoteOk) { Stop-GitFailed }
    elseif ($head -eq $pre -and $remote -ne $preRemote) { $lines = @('remote-moved'); $rc = 8 }
    elseif ($head -eq $pre) { $lines = @('no-changes') }
    elseif (-not $remoteOk -or $remote -ne $head) { $lines = @('unpushed'); $rc = 8 }
    else { $lines = @("pushed $head") }
  }
  if ($held.Count -gt 0) { $lines += "protected $($held -join ' ')" }
  if ($list) {
    $scanMode = if ($preRemote) { '--added-from-stdin' } else { '--revs-from-stdin' }
    $py = (Get-Command python3, python -ErrorAction SilentlyContinue | Select-Object -First 1).Source
    $scanOk = $false
    if ($py) {
      $scanner = Join-Path $PSScriptRoot 'scan-public.py'
      $gitEnv = [ordered]@{ GIT_CONFIG_COUNT = '2'; GIT_CONFIG_KEY_0 = 'core.fsmonitor'; GIT_CONFIG_VALUE_0 = 'false'
                            GIT_CONFIG_KEY_1 = 'core.hooksPath'; GIT_CONFIG_VALUE_1 = $nullDevice }
      $savedEnv = @{}
      foreach ($k in $gitEnv.Keys) { $savedEnv[$k] = [Environment]::GetEnvironmentVariable($k); [Environment]::SetEnvironmentVariable($k, $gitEnv[$k]) }
      Push-Location -LiteralPath $mainRoot
      try {
        $ErrorActionPreference = 'Continue'
        $first = $list | & $py -I $scanner --root $mainRoot $scanMode 2>&1
        $firstCode = $LASTEXITCODE
        $second = $list | & $py -I $scanner --root $mainRoot --objects-from-stdin 2>&1
        $secondCode = $LASTEXITCODE
        Set-Content -LiteralPath $scanFile -Value (@($first) + @($second)) -Encoding UTF8
        $scanOk = ($firstCode -eq 0 -and $secondCode -eq 0)
      } finally {
        $ErrorActionPreference = 'Stop'
        Pop-Location
        foreach ($k in $gitEnv.Keys) { [Environment]::SetEnvironmentVariable($k, $savedEnv[$k]) }
      }
    }
    if (-not $scanOk) { $lines += 'scan-refused'; $rc = 8 }
  }
  Set-Receipt $lines
  foreach ($line in $lines | Select-Object -Skip 1) { [Console]::Error.WriteLine($line) }
  if ($lines[0] -like 'pushed*') { Write-Output $lines[0] } else { [Console]::Error.WriteLine($lines[0]) }
  exit $rc
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
$code = Invoke-Task $scratch workspace-write $stage @($stage)
if ($code -ne 0) { Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue; exit $code }
# The plan must be a regular file with no link: it is copied into a fresh file, never moved. Every step is checked,
# `installed` follows the last one, and a failed install puts the old plan.md and plan.sha256 back (or removes both
# when there was no pair).
$installed = $false
$bakPlan = "$dir/.plan.md.bak"; $bakSha = "$dir/.plan.sha256.bak"; $tmpPlan = "$dir/.plan.md.new"; $tmpSha = "$dir/.plan.sha256.new"
Remove-Item -LiteralPath $bakPlan, $bakSha, $tmpPlan, $tmpSha -Force -ErrorAction SilentlyContinue
$staged = Get-Item -LiteralPath "$stage/plan.md" -Force -ErrorAction SilentlyContinue
if ($staged -and -not $staged.PSIsContainer -and -not $staged.LinkType -and $staged.Length -gt 0 -and -not (Test-Path -LiteralPath "$dir/plan.md" -PathType Container)) {
  $prepared = $false
  try {
    if (Test-Path -LiteralPath "$dir/plan.md" -PathType Leaf) { Copy-Item -LiteralPath "$dir/plan.md" -Destination $bakPlan -Force }
    if (Test-Path -LiteralPath "$dir/plan.sha256" -PathType Leaf) { Copy-Item -LiteralPath "$dir/plan.sha256" -Destination $bakSha -Force }
    [IO.File]::Copy($staged.FullName, $tmpPlan, $true)
    $hash = (Get-FileHash -LiteralPath $tmpPlan -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText($tmpSha, "$hash  plan.md`n")
    $prepared = $true
  } catch { $prepared = $false }
  if ($prepared) {
    try {
      Move-Item -LiteralPath $tmpPlan -Destination "$dir/plan.md" -Force
      if (Test-Path -LiteralPath "$dir/plan.sha256" -PathType Container) { throw 'plan.sha256 is a directory' }
      Move-Item -LiteralPath $tmpSha -Destination "$dir/plan.sha256" -Force
      $installed = $true
    } catch {
      if (Test-Path -LiteralPath $bakPlan) { Move-Item -LiteralPath $bakPlan -Destination "$dir/plan.md" -Force } else { Remove-Item -LiteralPath "$dir/plan.md" -Force -ErrorAction SilentlyContinue }
      if (Test-Path -LiteralPath $bakSha) { Move-Item -LiteralPath $bakSha -Destination "$dir/plan.sha256" -Force } else { Remove-Item -LiteralPath "$dir/plan.sha256" -Force -ErrorAction SilentlyContinue }
    }
  }
}
Remove-Item -LiteralPath $bakPlan, $bakSha, $tmpPlan, $tmpSha -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
if (-not $installed) {
  Set-Content -LiteralPath $planFile -Value 'no-plan' -Encoding ASCII
  [Console]::Error.WriteLine('codex-work: the run left no regular single-link plan-out/plan.md that could be installed')
  exit 1
}
Set-Content -LiteralPath $planFile -Value 'installed' -Encoding ASCII
Write-Output "$hash  plan.md"
exit 0

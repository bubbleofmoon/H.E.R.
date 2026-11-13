[CmdletBinding()]
param(
  [Parameter(Mandatory)][string]$Ticket,
  [Parameter(Mandatory)][string]$WorkTree,
  [Parameter(Mandatory)][string]$EvidenceDir,
  [Parameter(Mandatory)][string]$RunName,
  [Parameter(Mandatory)][string]$Cargo
)

# Windows counterpart of scripts/cargo-slot.sh. Runs one cargo command in <WorkTree>\her, offline, hidden, with MSVC set
# up through vcvars64.bat.
# usage: scripts\cargo-slot.ps1 <ticket> <worktree> <evidence-dir> <run-name> "<cargo subcommand> [args...]"
# The cargo command is one quoted string (PowerShell would otherwise eat `--` and `--flags`); it is passed to cmd.exe
# as written, so quote any argument that contains spaces inside it.
# Receipts: <evidence-dir>\<run-name>.stdout.log, .stderr.log, .exit.txt (numeric cargo exit code); for `test` and
# `run` also <run-name>.build.log (the compile step's output).
# Compiling holds one of QQBOT_BUILD_SLOTS (default 2) build slots, pinned to CPUs QQBOT_BUILD_CPUS (hex affinity mask,
# default 0F) at BelowNormal with `-j <QQBOT_CARGO_JOBS, default 2>` right after the subcommand. `test` and `run` compile
# first (`--no-run` / `build`), release the build slot, then run in one of QQBOT_TEST_SLOTS (default 2) test slots at
# Normal: slot 1 on CPUs QQBOT_TEST_CPUS (default F0), alone; slot 2 on QQBOT_TEST2_CPUS (default 0F), shared with the
# BelowNormal compilers, which yield to it. No two suites share CPUs. <run-name>.slot.txt names the test slot used.
# Production processes are not pinned. A test run past QQBOT_TEST_TIMEOUT_MIN (default 60) or a compile past
# QQBOT_BUILD_TIMEOUT_MIN (default 90) has its own process tree stopped, exits 124 and writes <run-name>.timeout.txt.
# Disk refusal exits 123 with <run-name>.disk.txt; QQBOT_MIN_FREE_GB defaults to 40 (GiB).
# Below that minimum, prune targets older than QQBOT_TARGET_MAX_AGE_DAYS (default 2), oldest first, toward
# minimum + 20 GiB free. <run-name>.prune.txt records removals, protected targets and nonfatal failures.
# Run the integration checkout's copy, C:\her\scripts\cargo-slot.ps1 -WorkTree <worktree>; another copy that differs
# writes <run-name>.stale-slot-script.txt.
# Overrides: QQBOT_BUILD_SLOT (first build lock, default C:\her-orch\build-slot.lock; slot N>1 adds "-N" before
# ".lock"), QQBOT_TEST_SLOT (first test lock, default C:\her-orch\test-slot.lock; slot 2 adds "-2"), HER_TARGET_ROOT
# (default C:\her-targets).
# Each lock holds owner.txt "<ticket> <run> <time> pid=<pid>"; a waiter breaks it only when that pid no longer exists,
# or when owner.txt is missing or has no pid for over 60 s. Release removes the lock only if it is still ours.
$ErrorActionPreference = 'Stop'
$firstLock = if ($env:QQBOT_BUILD_SLOT) { $env:QQBOT_BUILD_SLOT } else { 'C:\her-orch\build-slot.lock' }
$slotCount = if ($env:QQBOT_BUILD_SLOTS) { [int]$env:QQBOT_BUILD_SLOTS } else { 2 }
$buildLocks = @(1..$slotCount | ForEach-Object { if ($_ -eq 1) { $firstLock } else { $firstLock -replace '\.lock$', "-$_.lock" } })
$firstTestLock = if ($env:QQBOT_TEST_SLOT) { $env:QQBOT_TEST_SLOT } else { 'C:\her-orch\test-slot.lock' }
$testSlotCount = if ($env:QQBOT_TEST_SLOTS) { [int]$env:QQBOT_TEST_SLOTS } else { 2 }
if ($testSlotCount -lt 1 -or $testSlotCount -gt 2) { throw 'QQBOT_TEST_SLOTS must be 1 or 2' }
$testLocks = @(1..$testSlotCount | ForEach-Object { if ($_ -eq 1) { $firstTestLock } else { $firstTestLock -replace '\.lock$', "-$_.lock" } })
$buildCpus = [Convert]::ToInt64($(if ($env:QQBOT_BUILD_CPUS) { $env:QQBOT_BUILD_CPUS } else { '0F' }), 16)
$testCpus = @(
  [Convert]::ToInt64($(if ($env:QQBOT_TEST_CPUS) { $env:QQBOT_TEST_CPUS } else { 'F0' }), 16),
  [Convert]::ToInt64($(if ($env:QQBOT_TEST2_CPUS) { $env:QQBOT_TEST2_CPUS } else { '0F' }), 16)
)[0..($testSlotCount - 1)]
$targetRoot = if ($env:HER_TARGET_ROOT) { $env:HER_TARGET_ROOT } else { 'C:\her-targets' }
$jobs = if ($env:QQBOT_CARGO_JOBS) { $env:QQBOT_CARGO_JOBS } else { '2' }
$testTimeout = if ($env:QQBOT_TEST_TIMEOUT_MIN) { [int]$env:QQBOT_TEST_TIMEOUT_MIN } else { 60 }
$buildTimeout = if ($env:QQBOT_BUILD_TIMEOUT_MIN) { [int]$env:QQBOT_BUILD_TIMEOUT_MIN } else { 90 }
# Every run should use the integration checkout's copy (`C:\her\scripts\cargo-slot.ps1 -WorkTree <wt>`); a stale
# worktree copy still works but misses newer slot rules, so say so in the evidence.
$central = 'C:\her\scripts\cargo-slot.ps1'
$staleCopy = ($PSCommandPath -ne $central) -and (Test-Path -LiteralPath $central) -and
  ((Get-FileHash -LiteralPath $PSCommandPath).Hash -ne (Get-FileHash -LiteralPath $central).Hash)
[IO.Directory]::CreateDirectory($EvidenceDir) | Out-Null
[IO.Directory]::CreateDirectory((Split-Path -Parent $firstLock)) | Out-Null
$stdout = Join-Path $EvidenceDir "$RunName.stdout.log"
$stderr = Join-Path $EvidenceDir "$RunName.stderr.log"
$buildLog = Join-Path $EvidenceDir "$RunName.build.log"
$exitFile = Join-Path $EvidenceDir "$RunName.exit.txt"
Remove-Item -LiteralPath $exitFile -ErrorAction SilentlyContinue
if ($staleCopy) {
  Set-Content -LiteralPath (Join-Path $EvidenceDir "$RunName.stale-slot-script.txt") -Encoding ASCII `
    -Value "ran $PSCommandPath, which differs from $central; use $central -WorkTree $WorkTree"
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path -LiteralPath $vcvars)) { throw "vcvars64.bat not found under '$vsPath'" }

$me = "$Ticket $RunName $(Get-Date -Format o) pid=$PID"
# Serialize lock publication with pruning, so a new slot owner cannot appear during deletion.
$targetMaintenance = New-Object Threading.Mutex($false, 'Global\HER.CargoSlot.TargetMaintenance')
function Enter-TargetMaintenance {
  try { $null = $targetMaintenance.WaitOne() } catch [Threading.AbandonedMutexException] { }
}
function Test-Stale([string]$Candidate) {
  if (-not (Test-Path -LiteralPath $Candidate)) { return $false }
  $owner = Join-Path $Candidate 'owner.txt'
  if (Test-Path -LiteralPath $owner) {
    $text = Get-Content -LiteralPath $owner -Raw -ErrorAction SilentlyContinue
    if ($text -match 'pid=(\d+)') { return -not (Get-Process -Id ([int]$Matches[1]) -ErrorAction SilentlyContinue) }
    # An empty owner.txt means the owner was stopped between creating the lock and writing it.
    return ((Get-Date) - (Get-Item -LiteralPath $owner).LastWriteTime).TotalSeconds -gt 60
  }
  return ((Get-Date) - (Get-Item -LiteralPath $Candidate).LastWriteTime).TotalSeconds -gt 60
}
function Enter-Slot([string[]]$Candidates) {
  while ($true) {
    foreach ($candidate in $Candidates) {
      Enter-TargetMaintenance
      try {
        New-Item -ItemType Directory -Path $candidate -ErrorAction Stop | Out-Null
        Set-Content -LiteralPath (Join-Path $candidate 'owner.txt') -Value $me -Encoding ASCII
        return $candidate
      } catch {
        if (Test-Stale $candidate) { Remove-Item -LiteralPath $candidate -Recurse -Force -ErrorAction SilentlyContinue }
      } finally { $targetMaintenance.ReleaseMutex() }
    }
    Start-Sleep -Seconds 15
  }
}
function Exit-Slot([string]$Lock) {
  $text = Get-Content -LiteralPath (Join-Path $Lock 'owner.txt') -Raw -ErrorAction SilentlyContinue
  if ($text -and $text.Trim() -eq $me) { Remove-Item -LiteralPath $Lock -Recurse -Force -ErrorAction SilentlyContinue }
}
function Get-TargetFreeBytes {
  # DriveInfo is recreated for each probe: free space can change while waiting for a slot or pruning.
  $drive = New-Object IO.DriveInfo ([IO.Path]::GetPathRoot([IO.Path]::GetFullPath($targetRoot)))
  return $drive.AvailableFreeSpace
}
function Assert-PlainTargetPath([string]$Path) {
  $root = [IO.Path]::GetFullPath($targetRoot).TrimEnd([char[]]'\/')
  $full = [IO.Path]::GetFullPath($Path)
  if ($full -ne $root -and -not $full.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw "path outside target root: $full"
  }
  # Inspect ancestors too: a plain child inside a junction is still outside the intended tree.
  $part = $full
  while ($part) {
    if (Test-Path -LiteralPath $part) {
      if ((Get-Item -LiteralPath $part -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "reparse point: $part"
      }
    }
    $part = Split-Path -Parent $part
  }
}
function Get-TargetTree([string]$Path) {
  $pending = New-Object 'Collections.Generic.Stack[string]'
  $directories = New-Object 'Collections.Generic.List[string]'
  $files = New-Object 'Collections.Generic.List[string]'
  $pending.Push($Path)
  $latest = [DateTime]::MinValue
  $size = [long]0
  while ($pending.Count -gt 0) {
    $directory = $pending.Pop()
    Assert-PlainTargetPath $directory
    $item = Get-Item -LiteralPath $directory -Force
    if ($item.LastWriteTimeUtc -gt $latest) { $latest = $item.LastWriteTimeUtc }
    $directories.Add($directory)
    foreach ($entry in Get-ChildItem -LiteralPath $directory -Force) {
      if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "reparse point: $($entry.FullName)" }
      if ($entry.LastWriteTimeUtc -gt $latest) { $latest = $entry.LastWriteTimeUtc }
      if ($entry.PSIsContainer) { $pending.Push($entry.FullName) } else {
        $files.Add($entry.FullName)
        $size += $entry.Length
      }
    }
  }
  return [pscustomobject]@{ Path = $Path; LastWrite = $latest; Size = $size; Files = $files; Directories = $directories }
}
function Test-TargetLocked([string]$Name) {
  # Discover higher-numbered build slots too, if another runner configured more than this one.
  $lockDirectory = Split-Path -Parent $firstLock
  $lockPattern = '^' + [regex]::Escape([IO.Path]::GetFileNameWithoutExtension($firstLock)) +
    '(?:-\d+)?' + [regex]::Escape([IO.Path]::GetExtension($firstLock)) + '$'
  $locks = @($buildLocks) + @($testLocks) + @(Get-ChildItem -LiteralPath $lockDirectory -Directory -Force |
    Where-Object { $_.Name -match $lockPattern } | ForEach-Object { $_.FullName })
  foreach ($slot in $locks | Select-Object -Unique) {
    if (-not (Test-Path -LiteralPath $slot)) { continue }
    try {
      $owner = Get-Content -LiteralPath (Join-Path $slot 'owner.txt') -Raw -ErrorAction Stop
      if ($owner -match '^(\S+)\s+.*\bpid=(\d+)') {
        $ownerTicket = $Matches[1]
        $ownerPid = [int]$Matches[2]
        if ($ownerTicket -eq $Name -and (Get-Process -Id $ownerPid -ErrorAction SilentlyContinue)) { return $true }
      } else {
        # Owner publication is not atomic with mkdir. An unknown owner protects every target.
        return $true
      }
    } catch {
      if (Test-Path -LiteralPath $slot) { return $true }
    }
  }
  return $false
}
function Invoke-TargetPrune([double]$MinimumBytes) {
  $receipt = Join-Path $EvidenceDir "$RunName.prune.txt"
  $maxAge = if ($env:QQBOT_TARGET_MAX_AGE_DAYS) { [double]$env:QQBOT_TARGET_MAX_AGE_DAYS } else { 2.0 }
  if ([double]::IsNaN($maxAge) -or [double]::IsInfinity($maxAge) -or $maxAge -lt 0) {
    throw 'QQBOT_TARGET_MAX_AGE_DAYS must be a finite, nonnegative number'
  }
  $root = [IO.Path]::GetFullPath($targetRoot).TrimEnd([char[]]'\/')
  Assert-PlainTargetPath $root
  $ownTarget = [IO.Path]::GetFullPath((Join-Path $root $Ticket))
  $goal = $MinimumBytes + 20GB
  Add-Content -LiteralPath $receipt -Encoding UTF8 -Value "prune root=$root max_age_days=$maxAge goal_bytes=$goal"
  if (-not (Test-Path -LiteralPath $root)) { return }
  $candidates = @(
    foreach ($directory in Get-ChildItem -LiteralPath $root -Directory -Force) {
      if ($ownTarget -eq $directory.FullName -or $ownTarget.StartsWith($directory.FullName + '\', [StringComparison]::OrdinalIgnoreCase)) { continue }
      try {
        if (Test-TargetLocked $directory.Name) {
          Add-Content -LiteralPath $receipt -Encoding UTF8 -Value "skipped locked path=$($directory.FullName)"
          continue
        }
        $tree = Get-TargetTree $directory.FullName
        if (([DateTime]::UtcNow - $tree.LastWrite).TotalDays -gt $maxAge) { $tree }
      } catch {
        Add-Content -LiteralPath $receipt -Encoding UTF8 -Value "skipped path=$($directory.FullName) error=$($_.Exception.Message)"
      }
    }
  )
  foreach ($candidate in $candidates | Sort-Object LastWrite) {
    if ((Get-TargetFreeBytes) -ge $goal) { break }
    try {
      $name = Split-Path -Leaf $candidate.Path
      # Recheck after scanning/sorting: the tree may have been written or claimed in the meantime.
      if (Test-TargetLocked $name) {
        Add-Content -LiteralPath $receipt -Encoding UTF8 -Value "skipped locked path=$($candidate.Path)"
        continue
      }
      $tree = Get-TargetTree $candidate.Path
      $age = ([DateTime]::UtcNow - $tree.LastWrite).TotalDays
      if ($age -le $maxAge) { continue }
      # Never use recursive deletion: PS 5.1 can traverse junctions. Delete checked leaves then empty parents.
      foreach ($file in $tree.Files) {
        Assert-PlainTargetPath $file
        [IO.File]::SetAttributes($file, [IO.FileAttributes]::Normal)
        [IO.File]::Delete($file)
      }
      for ($index = $tree.Directories.Count - 1; $index -ge 0; $index--) {
        $directory = $tree.Directories[$index]
        Assert-PlainTargetPath $directory
        [IO.Directory]::Delete($directory, $false)
      }
      Add-Content -LiteralPath $receipt -Encoding UTF8 -Value "removed path=$($tree.Path) size_bytes=$($tree.Size) age_days=$([math]::Round($age, 3))"
    } catch {
      Add-Content -LiteralPath $receipt -Encoding UTF8 -Value "failed path=$($candidate.Path) size_bytes=$($candidate.Size) age_days=$([math]::Round(([DateTime]::UtcNow - $candidate.LastWrite).TotalDays, 3)) error=$($_.Exception.Message)"
    }
  }
}
function Test-TargetDisk {
  $diskReceipt = Join-Path $EvidenceDir "$RunName.disk.txt"
  Enter-TargetMaintenance
  try {
    $minimum = if ($env:QQBOT_MIN_FREE_GB) { [double]$env:QQBOT_MIN_FREE_GB } else { 40.0 }
    if ([double]::IsNaN($minimum) -or [double]::IsInfinity($minimum) -or $minimum -lt 0) {
      throw 'QQBOT_MIN_FREE_GB must be a finite, nonnegative number'
    }
    Assert-PlainTargetPath ([IO.Path]::GetFullPath($targetRoot))
    $free = Get-TargetFreeBytes
    if ($free -ge ($minimum * 1GB)) { return $true }
    Invoke-TargetPrune ($minimum * 1GB)
    $free = Get-TargetFreeBytes
    if ($free -ge ($minimum * 1GB)) { return $true }
    $message = "Cargo refused: target root '$targetRoot' has $([math]::Round($free / 1GB, 2)) GiB free; requires $minimum GiB (QQBOT_MIN_FREE_GB). Free space or allow old targets to be pruned. Exit 123."
  } catch {
    $message = "Cargo refused: cannot check target disk '$targetRoot': $($_.Exception.Message). Exit 123."
  } finally { $targetMaintenance.ReleaseMutex() }
  Add-Content -LiteralPath $diskReceipt -Value $message -Encoding UTF8
  [Console]::Error.WriteLine($message)
  return $false
}
# Runs one cargo command line inside a held slot; returns its exit code as a string ('124' when it ran past
# $TimeoutMinutes and its own process tree was stopped, so a hung test cannot hold a slot).
# $Cpus holds one affinity mask, or one per slot in $Slots order.
function Invoke-Cargo([string[]]$Slots, [string]$Line, [long[]]$Cpus, [Diagnostics.ProcessPriorityClass]$Priority, [string]$Out, [string]$Err, [int]$TimeoutMinutes, [string]$SlotReceipt) {
  $lock = Enter-Slot $Slots
  $mask = if ($Cpus.Count -gt 1) { $Cpus[[array]::IndexOf($Slots, $lock)] } else { $Cpus[0] }
  if ($SlotReceipt) { Set-Content -LiteralPath $SlotReceipt -Value ('{0} cpus=0x{1:X}' -f $lock, $mask) -Encoding ASCII }
  try {
    if (-not (Test-TargetDisk)) {
      Set-Content -LiteralPath $Out -Value '' -Encoding ASCII
      Set-Content -LiteralPath $Err -Value (Get-Content -LiteralPath (Join-Path $EvidenceDir "$RunName.disk.txt") -Raw) -Encoding UTF8
      return '123'
    }
    $env:CARGO_HOME = Join-Path $env:USERPROFILE '.cargo'
    $env:CARGO_TARGET_DIR = Join-Path $targetRoot $Ticket
    $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    $cmd = '"' + $vcvars + '" >nul && "' + $cargo + '" +stable-x86_64-pc-windows-msvc ' + $Line
    $p = Start-Process -FilePath 'cmd.exe' -ArgumentList @('/d', '/s', '/c', '"' + $cmd + '"') -WorkingDirectory (Join-Path $WorkTree 'her') `
      -WindowStyle Hidden -PassThru -RedirectStandardOutput $Out -RedirectStandardError $Err
    $null = $p.Handle
    # vcvars64.bat runs for about a second before cargo starts, so cargo and everything below it inherit these.
    try { $p.ProcessorAffinity = [IntPtr]$mask } catch { }
    try { $p.PriorityClass = $Priority } catch { }
    if (-not $p.WaitForExit($TimeoutMinutes * 60000)) {
      & taskkill.exe /PID $p.Id /T /F | Out-Null
      $p.WaitForExit()
      Add-Content -LiteralPath $exitFile.Replace('.exit.txt', '.timeout.txt') -Value "stopped after $TimeoutMinutes min: $Line" -Encoding ASCII
      return '124'
    }
    return [string]$p.ExitCode
  } finally { Exit-Slot $lock }
}

$code = 'unknown'
try {
  $sub, $rest = $Cargo.Trim() -split '\s+', 2
  if ($null -eq $rest) { $rest = '' }
  $full = "$sub --offline -j $jobs $rest".Trim()
  $compileOnly = $sub -eq 'test' -and (" $rest " -match '\s--no-run\s')
  if ($sub -in @('test', 'run') -and -not $compileOnly) {
    # Compile step: the same selection without the test binary's or program's own arguments (after `--`).
    $head = ($rest -split '(^|\s)--(\s|$)', 2)[0].Trim()
    $compile = if ($sub -eq 'test') { "test --offline -j $jobs --no-run $head" } else { "build --offline -j $jobs $head" }
    $code = Invoke-Cargo $buildLocks $compile.Trim() $buildCpus ([Diagnostics.ProcessPriorityClass]::BelowNormal) $buildLog "$buildLog.err" $buildTimeout
    if ($code -eq '0') {
      $code = Invoke-Cargo $testLocks $full $testCpus ([Diagnostics.ProcessPriorityClass]::Normal) $stdout $stderr $testTimeout (Join-Path $EvidenceDir "$RunName.slot.txt")
    } else {
      Copy-Item -LiteralPath "$buildLog.err" -Destination $stderr -Force
      Set-Content -LiteralPath $stdout -Value '' -Encoding ASCII
    }
  } else {
    $code = Invoke-Cargo $buildLocks $full $buildCpus ([Diagnostics.ProcessPriorityClass]::BelowNormal) $stdout $stderr $buildTimeout
  }
} finally {
  Set-Content -LiteralPath $exitFile -Value $code -Encoding ASCII
  $targetMaintenance.Dispose()
}
if ($code -match '^\d+$') { exit [int]$code } else { exit 1 }

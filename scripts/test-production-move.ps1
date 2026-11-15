# Native Windows rehearsal of scripts\production-move.ps1 (#67) on a synthetic copy of the production layout.
# Everything lives in a new folder under the temporary folder: an install root with a legacy SnowLuma folder, an old
# runtime and release, a profile DSH home, an npx-style DSH package, a relay folder with its login folder, loopback
# fakes (scripts\production-move-fake.mjs), two HER-Test-67-<guid> fixture tasks started through hidden VBS launchers
# that wait (so their instances stay Running, as in production) and a file scheduler for the HER task. It names no
# production path as a target and contacts no QQ, DSH, model or TTS service. Windows PowerShell 5.1; no elevation.
# The install root's qq-bridge folder holds two DSH bundles the profile DSH home links to, as in production.
# -NodePath names a node.exe to copy: the synthetic production runs only on a private copy inside the fixture (with an
# npm stub beside it), so no production process ever matches its configured program; the copy goes with the fixture.
# Ends with "PASS production move rehearsal" or exits 1.
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\test-production-move.ps1 -Candidate <folder with
#     her.exe, her-cli.exe and herupdate.exe> -NodePath <node.exe to copy> -FixtureRoot <new folder under the temporary folder>
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][string]$Candidate,
  [Parameter(Mandatory = $true)][string]$NodePath,
  [Parameter(Mandatory = $true)][string]$FixtureRoot,
  [switch]$KeepFixture
)
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -ne 5 -or $PSVersionTable.PSVersion.Minor -ne 1) { throw 'Run the rehearsal in Windows PowerShell 5.1.' }
$moveScript = Join-Path $PSScriptRoot 'production-move.ps1'
$fake = Join-Path $PSScriptRoot 'production-move-fake.mjs'
$powershell = Join-Path $env:WINDIR 'System32\WindowsPowerShell\v1.0\powershell.exe'
$Candidate = [IO.Path]::GetFullPath($Candidate).TrimEnd('\')
$NodePath = [IO.Path]::GetFullPath($NodePath)
$F = [IO.Path]::GetFullPath($FixtureRoot).TrimEnd('\')
$temp = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
if (-not $F.StartsWith($temp, [StringComparison]::OrdinalIgnoreCase) -or (Test-Path -LiteralPath $F)) { throw 'FixtureRoot must be a new folder under the temporary folder.' }
# Lexical containment is not enough: a junction or link between the temporary folder and the fixture would lead every
# fixture write and launch elsewhere, production included.
function Test-FixturePlace([string]$Path) {
  $step = Split-Path -Parent ([IO.Path]::GetFullPath($Path).TrimEnd('\'))
  while ($step.Length -gt $temp.TrimEnd('\').Length) {
    $item = Get-Item -LiteralPath $step -Force -ErrorAction SilentlyContinue
    if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { return $false }
    $step = Split-Path -Parent $step
  }
  return $true
}
if (-not (Test-FixturePlace $F)) { throw 'FixtureRoot must not lie behind a junction or link under the temporary folder.' }
foreach ($name in @('her.exe', 'her-cli.exe', 'herupdate.exe')) { if (-not (Test-Path -LiteralPath (Join-Path $Candidate $name) -PathType Leaf)) { throw ('Candidate has no ' + $name + '.') } }
if (-not (Test-Path -LiteralPath $NodePath -PathType Leaf)) { throw 'NodePath must name node.exe.' }
$cli = Join-Path $Candidate 'her-cli.exe'
$savedInstallRoot = $env:HER_INSTALL_ROOT
$savedScheduler = $env:HER_TASK_SCHEDULER_FILE
Remove-Item -Path Env:HER_INSTALL_ROOT -ErrorAction SilentlyContinue
Remove-Item -Path Env:HER_TASK_SCHEDULER_FILE -ErrorAction SilentlyContinue

if (-not ('HerMove67RehearsalNative' -as [type])) {
  Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;
public static class HerMove67RehearsalNative {
  [StructLayout(LayoutKind.Sequential)] struct FileTime { public uint Low; public uint High; }
  [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetProcessTimes(IntPtr process, out FileTime creation, out FileTime exit, out FileTime kernel, out FileTime user);
  [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder path, ref uint size);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr process, uint code);
  [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern SafeFileHandle CreateFile(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
  public static string Stop(uint pid, ulong start, string image) {
    IntPtr process = OpenProcess(0x1000 | 0x100000 | 0x1, false, pid);
    if (process == IntPtr.Zero) return Marshal.GetLastWin32Error() == 87 ? "gone" : "unknown";
    try {
      FileTime creation, exit, kernel, user;
      if (!GetProcessTimes(process, out creation, out exit, out kernel, out user)) return "unknown";
      StringBuilder path = new StringBuilder(32768);
      uint size = (uint)path.Capacity;
      if (!QueryFullProcessImageName(process, 0, path, ref size)) return "unknown";
      ulong actual = ((ulong)creation.High << 32) | creation.Low;
      if (actual != start || !String.Equals(Path.GetFullPath(path.ToString()), Path.GetFullPath(image), StringComparison.OrdinalIgnoreCase)) return "mismatch";
      if (!TerminateProcess(process, 1)) return "unknown";
      return WaitForSingleObject(process, 15000) == 0 ? "stopped" : "unknown";
    } finally { CloseHandle(process); }
  }
  // Opens a folder without delete sharing, so nobody can rename it while the handle is held.
  public static SafeFileHandle HoldDirectory(string path) {
    SafeFileHandle handle = CreateFile(path, 1u << 31, 1, IntPtr.Zero, 3, 1u << 25, IntPtr.Zero);
    if (handle.IsInvalid) throw new Win32Exception(Marshal.GetLastWin32Error());
    return handle;
  }
}
'@
}

$root = Join-Path $F 'qqbot'
$unused = Join-Path $F 'unused-root'
$oldRelease = Join-Path $F 'qqbot-rust\releases\fixture-old'
$oldCli = Join-Path $oldRelease 'her-cli.exe'
$oldRuntime = Join-Path $F 'qqbot-rust\runtimes\production'
$profileFolder = Join-Path $F 'profile'
$dshHome = Join-Path $profileFolder '.dsh'
$npx = Join-Path $F 'npm-cache\_npx\fixtureabc'
$dshPackage = Join-Path $npx 'node_modules\@deepseek-ai\dsh'
$relayFolder = Join-Path $F 'her-orch\relay\cliproxy-0.0.1'
$authFolder = Join-Path $profileFolder '.cli-proxy-api'
$snowFolder = Join-Path $root 'SnowLuma'
$releaseSource = Join-Path $F 'release-source'
$evidence = Join-Path $F 'evidence'
$herScheduler = Join-Path $F 'scheduler\her-task.xml'
$oldScheduler = Join-Path $F 'scheduler\old-task.xml'
$failMarker = Join-Path $F 'fail-new-dsh'
$decoy = Join-Path $F 'decoy-root'
$bridgeBundles = Join-Path $root 'qq-bridge\dsh'
$bundleNames = @('qq-mode-console', 'qq-agent-presets')
$boundary = Join-Path $temp ('her-67-boundary-' + [guid]::NewGuid().ToString('N'))
$registered = @()
$passed = $false
$scenarios = 0
$baselineSupervisor = 0
$consolePort = 0; $onebotPort = 0; $dshPort = 0; $relayPort = 0

function Write-Fixture([string]$Path, [string]$Text) {
  [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Path))
  [IO.File]::WriteAllText($Path, $Text, [Text.UTF8Encoding]::new($false))
}
function Write-FixtureJson([string]$Path, $Value) { Write-Fixture $Path ($Value | ConvertTo-Json -Depth 20) }
function Add-LedgerRecord($Record) {
  [IO.File]::AppendAllText((Join-Path $oldRuntime 'data\turn-ledger.jsonl'), (($Record | ConvertTo-Json -Compress -Depth 5) + "`n"), [Text.UTF8Encoding]::new($false))
}
function Invoke-Her([string[]]$Arguments, [int[]]$Allowed = @(0), [string]$Exe = $cli) {
  $previous = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try { $output = & $Exe @Arguments 2>$null | Out-String; $code = $LASTEXITCODE } finally { $ErrorActionPreference = $previous }
  if ($Allowed -notcontains $code) { throw ('her-cli ' + $Arguments[0] + ' exited ' + $code) }
  return $output
}
function Get-FileHashText([string]$Path) { return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant() }
# A tree's names, file hashes and times, without following a junction or link: a link is recorded with its target
# (the linked bundles are hashed once, through the qq-bridge entry of Get-Originals).
function Get-TreeHash([string]$Folder) {
  $base = [IO.Path]::GetFullPath($Folder).TrimEnd('\')
  $lines = [Collections.Generic.List[string]]::new()
  $pending = [Collections.Generic.Stack[string]]::new()
  $pending.Push($base)
  while ($pending.Count -gt 0) {
    foreach ($item in [IO.DirectoryInfo]::new($pending.Pop()).GetFileSystemInfos()) {
      $relative = $item.FullName.Substring($base.Length)
      if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { $lines.Add('L ' + $relative + ' ' + [string]@((Get-Item -LiteralPath $item.FullName -Force).Target)[0]); continue }
      if ($item -is [IO.DirectoryInfo]) { $lines.Add('D ' + $relative); $pending.Push($item.FullName); continue }
      $lines.Add('F ' + $relative + ' ' + (Get-FileHashText $item.FullName) + ' ' + $item.LastWriteTimeUtc.Ticks)
    }
  }
  $sorted = @($lines | Sort-Object)
  return ($sorted -join "`n")
}
function Get-TaskShape([string]$Name) {
  $tasks = @(Get-ScheduledTask -TaskPath '\' -ErrorAction Stop | Where-Object { $_.TaskName -ceq $Name })
  if ($tasks.Count -ne 1) { return '' }
  $task = $tasks[0]
  $actions = @($task.Actions | ForEach-Object { [string]$_.Execute + '|' + [string]$_.Arguments + '|' + [string]$_.WorkingDirectory }) -join ';'
  $triggers = @($task.Triggers | ForEach-Object { [string]$_.CimClass.CimClassName + '|' + [string]$_.Enabled }) -join ';'
  $principal = [string]$task.Principal.UserId + '|' + [string]$task.Principal.LogonType + '|' + [string]$task.Principal.RunLevel
  return ($actions + '#' + $triggers + '#' + $principal + '#' + [string]$task.Settings.Enabled)
}
function Get-Owners([string]$Runtime, [string]$InstallRoot, [string]$Exe) {
  $status = Invoke-Her @('lifecycle', 'status', '--runtime-root', $Runtime, '--install-root', $InstallRoot) @(0, 3) $Exe | ConvertFrom-Json
  return [pscustomobject]@{
    supervisor = [string]$status.owners.supervisor.state; live = [string]$status.owners.live.state; console = [string]$status.owners.console.state
    supervisorPid = [int]$status.owners.supervisor.owner.identity.pid; supervisorExe = [string]$status.owners.supervisor.owner.identity.exe
    consolePid = [int]$status.owners.console.owner.identity.pid
    livePid = [int]$status.owners.live.owner.identity.pid
  }
}
function Get-Listener([int]$Port) {
  $ids = @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue | ForEach-Object { [int]$_.OwningProcess } | Sort-Object -Unique)
  if ($ids.Count -ne 1) { return $null }
  $process = @(Get-CimInstance -ClassName Win32_Process -Filter ('ProcessId=' + $ids[0]))
  if ($process.Count -ne 1) { return $null }
  return [pscustomobject]@{ Pid = $ids[0]; Exe = [string]$process[0].ExecutablePath; CommandLine = [string]$process[0].CommandLine }
}
# get_status as HER's OneBot client sends and reads it (her/src/onebot.rs): Connection: close, the raw body read to
# the end of the connection, no chunked decoding. True when that body is JSON with status ok.
function Test-RawOneBot([int]$Port) {
  $client = [Net.Sockets.TcpClient]::new('127.0.0.1', $Port)
  try {
    $stream = $client.GetStream()
    $stream.ReadTimeout = 10000
    $request = 'POST /get_status HTTP/1.1' + "`r`n" + 'Host: 127.0.0.1:' + $Port + "`r`n" + 'Content-Type: application/json' + "`r`n" + 'Content-Length: 2' + "`r`n" + 'Connection: close' + "`r`n`r`n" + '{}'
    $bytes = [Text.Encoding]::ASCII.GetBytes($request)
    $stream.Write($bytes, 0, $bytes.Length)
    $raw = [IO.StreamReader]::new($stream, [Text.UTF8Encoding]::new($false)).ReadToEnd()
  } finally { $client.Close() }
  $split = $raw.IndexOf("`r`n`r`n")
  if ($split -lt 0) { return $false }
  try { $parsed = $raw.Substring($split + 4) | ConvertFrom-Json } catch { return $false }
  return ([string]$parsed.status -eq 'ok')
}
function Stop-FixtureListener([int]$Port) {
  if ($Port -le 0) { return }
  $listener = Get-Listener $Port
  if ($null -eq $listener) { return }
  $inside = $listener.Exe.StartsWith($F + '\', [StringComparison]::OrdinalIgnoreCase) -or ($listener.Exe -ieq $NodePath -and $listener.CommandLine.IndexOf($F, [StringComparison]::OrdinalIgnoreCase) -ge 0)
  if (-not $inside) { Write-Warning ('Port ' + $Port + ' is held by a process outside the fixture; it was left running.'); return }
  $process = Get-Process -Id $listener.Pid -ErrorAction SilentlyContinue
  if ($null -eq $process) { return }
  $start = [uint64]$process.StartTime.ToUniversalTime().ToFileTimeUtc()
  $result = [HerMove67RehearsalNative]::Stop([uint32]$listener.Pid, $start, $listener.Exe)
  if (@('stopped', 'gone', 'mismatch') -notcontains $result) { Write-Warning ('Fixture listener on port ' + $Port + ' could not be stopped safely: ' + $result) }
}
function Wait-For([scriptblock]$Condition, [string]$Description, [int]$Seconds) {
  $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
  while ([DateTime]::UtcNow -lt $deadline) { if (& $Condition) { return }; Start-Sleep -Milliseconds 500 }
  throw ('Timed out: ' + $Description)
}
function Assert-True([bool]$Condition, [string]$Message) { if (-not $Condition) { throw ('FAIL ' + $Message) } }
function Step([string]$Name) { $script:scenarios++; Write-Host ('scenario ' + $Name) }
function Invoke-MoveScript([string[]]$Arguments, [string]$Name, [string]$InputText = '') {
  $log = Join-Path $F ('run-' + $Name + '.log')
  $flags = @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $moveScript)
  $previous = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    # -RehearsalConfirm reads the typed word from the child's input; the pipeline feeds it.
    if ($InputText) { $InputText | & $powershell @flags @Arguments *> $log } else { & $powershell @flags @Arguments *> $log }
    $code = $LASTEXITCODE
  } finally { $ErrorActionPreference = $previous }
  Write-Host ('  ' + $Name + ' -> exit ' + $code)
  return [pscustomobject]@{ Code = $code; Text = [IO.File]::ReadAllText($log); Name = $Name }
}
function Assert-Run($Run, [int]$Code, [string]$Text, [string]$Scenario) {
  if ($Run.Code -ne $Code -or $Run.Text.IndexOf($Text, [StringComparison]::OrdinalIgnoreCase) -lt 0) {
    throw ('FAIL ' + $Scenario + ': exit ' + $Run.Code + ' (expected ' + $Code + ') or the output lacks "' + $Text + '"; see run-' + $Run.Name + '.log in the fixture')
  }
}
function Get-LatestState {
  $folder = @(Get-ChildItem -LiteralPath $evidence -Directory -Filter 'move-*' | Sort-Object -Property Name -Descending)[0]
  return (Join-Path $folder.FullName 'state.json')
}
function Remove-McpBlock([string]$Text) { return [regex]::Replace($Text, '(?ms)^# === qq-bridge MCP BEGIN ===.*?^# === qq-bridge MCP END ===\r?\n', '') }
# Everything except the keys the move changes on purpose, serialized the same way on both sides.
function Get-ComparableConfig([string]$Text) {
  $value = $Text | ConvertFrom-Json
  foreach ($name in @('dependencies', 'update')) { if ($null -ne $value.PSObject.Properties[$name]) { $value.PSObject.Properties.Remove($name) } }
  foreach ($pair in @(@('tasks', 'start-all'), @('tasks', 'installRoot'), @('snowluma', 'homeDir'), @('snowluma', 'launcherPath'))) {
    $parent = $value.PSObject.Properties[$pair[0]]
    if ($null -ne $parent -and $null -ne $parent.Value.PSObject.Properties[$pair[1]]) { $parent.Value.PSObject.Properties.Remove($pair[1]) }
  }
  # -DigestModel sets memory.digest.model on purpose; everything else under memory must survive.
  $memory = $value.PSObject.Properties['memory']
  if ($null -ne $memory -and $null -ne $memory.Value.PSObject.Properties['digest'] -and $null -ne $memory.Value.digest.PSObject.Properties['model']) { $memory.Value.digest.PSObject.Properties.Remove('model') }
  return ($value | ConvertTo-Json -Depth 100 -Compress)
}
function Test-OldUp {
  $owners = Get-Owners $oldRuntime $unused $cli
  if ($owners.supervisor -ne 'held' -or $owners.live -ne 'held' -or $owners.console -ne 'held') { return $false }
  foreach ($port in @($onebotPort, $dshPort, $relayPort, $consolePort)) { if ($null -eq (Get-Listener $port)) { return $false } }
  return $true
}
function Get-Originals {
  return [ordered]@{ dsh = (Get-TreeHash $dshHome); npx = (Get-TreeHash $npx); relay = (Get-TreeHash $relayFolder); auth = (Get-TreeHash $authFolder); snow = (Get-TreeHash $snowFolder); release = (Get-TreeHash $oldRelease); bridge = (Get-TreeHash (Join-Path $root 'qq-bridge')) }
}
function Assert-Originals([string]$Scenario) {
  $now = Get-Originals
  foreach ($key in @($originals.Keys)) { Assert-True ($now[$key] -eq $originals[$key]) ($Scenario + ': the original ' + $key + ' folder changed') }
}
function Assert-NoNewLayout([string]$Scenario) {
  foreach ($name in @('runtime', 'releases', 'deps', 'current', 'herupdate.exe')) { Assert-True (-not (Test-Path -LiteralPath (Join-Path $root $name))) ($Scenario + ': the install root still has ' + $name) }
}
function Get-RootSddl { return (Get-Acl -LiteralPath $root).GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::Access) }
# Before a finished move the install root access must be exactly what it was; after one it stays restricted.
function Assert-RootAcl([string]$Scenario, [switch]$Restricted) {
  if ($Restricted) {
    $acl = Get-Acl -LiteralPath $root
    Assert-True ($acl.AreAccessRulesProtected) ($Scenario + ': the install root is not restricted after a finished move')
  } else {
    Assert-True ((Get-RootSddl) -eq $rootSddl) ($Scenario + ': the install root access was not restored')
  }
}
# A folder that holds credential copies after a rollback: no inheritance, and no account but this one, SYSTEM and
# Administrators (the fixture grants Users read access from above, so an inheriting folder fails here).
function Assert-Protected([string]$Path, [string]$What) {
  $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  $acl = Get-Acl -LiteralPath $Path
  Assert-True ($acl.AreAccessRulesProtected) ($What + ' still inherits access')
  $foreign = @($acl.Access | Where-Object { @($sid, 'S-1-5-18', 'S-1-5-32-544') -notcontains $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value })
  Assert-True ($foreign.Count -eq 0) ($What + ' grants access to another account')
}
# Stops one fixture process by its verified identity (a reboot simulation); never a process outside the fixture.
function Stop-FixtureProcess([int]$ProcessId) {
  $process = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
  if ($null -eq $process) { return }
  $image = [string]$process.Path
  if (-not $image.StartsWith($F + '\', [StringComparison]::OrdinalIgnoreCase)) { throw ('FAIL fixture: process ' + $ProcessId + ' is not a fixture process') }
  $start = [uint64]$process.StartTime.ToUniversalTime().ToFileTimeUtc()
  $result = [HerMove67RehearsalNative]::Stop([uint32]$ProcessId, $start, $image)
  if (@('stopped', 'gone') -notcontains $result) { throw ('FAIL fixture: process ' + $ProcessId + ' could not be stopped: ' + $result) }
}
function Assert-OldUntouched([string]$Scenario) {
  $owners = Get-Owners $oldRuntime $unused $cli
  Assert-True ($owners.supervisor -eq 'held' -and $owners.supervisorPid -eq $script:baselineSupervisor) ($Scenario + ': the old supervisor was restarted or stopped')
  foreach ($name in @($supervisorTask, $relayTask)) { Assert-True ((Get-TaskShape $name) -eq $taskShapes[$name]) ($Scenario + ': the task ' + $name + ' changed') }
  Assert-NoNewLayout $Scenario
  Assert-RootAcl $Scenario
  Assert-Originals $Scenario
}
function Assert-OldRunning([string]$Scenario, [switch]$Restricted) {
  Wait-For { Test-OldUp } ($Scenario + ': the old production runs again') 60
  foreach ($name in @($supervisorTask, $relayTask)) { Assert-True ((Get-TaskShape $name) -eq $taskShapes[$name]) ($Scenario + ': the task ' + $name + ' was not restored') }
  Assert-True ((Get-Listener $onebotPort).Exe -ieq (Join-Path $snowFolder 'node.exe')) ($Scenario + ': OneBot is not the original SnowLuma')
  Assert-True ((Get-Listener $relayPort).Exe -ieq (Join-Path $relayFolder 'cli-proxy-api.exe')) ($Scenario + ': the relay is not the original')
  Assert-True ((Get-Listener $dshPort).CommandLine.IndexOf($dshPackage, [StringComparison]::OrdinalIgnoreCase) -ge 0) ($Scenario + ': DSH is not the original package')
  $task = Invoke-Her @('scheduled-task', 'read', '--install-root', $root) | ConvertFrom-Json
  Assert-True ($null -eq $task.xml) ($Scenario + ': the HER task is still registered')
  Assert-NoNewLayout $Scenario
  $attempts = @(Get-ChildItem -LiteralPath $root -Directory -Filter 'move-67-attempt-*')
  Assert-True ($attempts.Count -ge 1) ($Scenario + ': no moved-aside attempt folder')
  # A link kept in an attempt folder would reach live folders after a later move (and a recursive delete would follow it).
  foreach ($attempt in $attempts) {
    $attemptLinks = Get-FixtureLinks $attempt.FullName
    Assert-True ($attemptLinks.Count -eq 0) ($Scenario + ': a moved-aside attempt folder still holds a junction or link')
    # It keeps copied credentials, and the root's original (here wider) access is back: it must be closed on its own.
    Assert-Protected $attempt.FullName ($Scenario + ': a moved-aside attempt folder')
  }
  Assert-RootAcl $Scenario -Restricted:$Restricted
  Assert-Originals $Scenario
  $script:baselineSupervisor = (Get-Owners $oldRuntime $unused $cli).supervisorPid
}
# Every junction or link below a folder, found without following any (Get-ChildItem -Recurse would follow them).
function Get-FixtureLinks([string]$Folder) {
  $found = [Collections.Generic.List[string]]::new()
  $pending = [Collections.Generic.Stack[string]]::new()
  $pending.Push($Folder)
  while ($pending.Count -gt 0) {
    foreach ($item in [IO.DirectoryInfo]::new($pending.Pop()).GetFileSystemInfos()) {
      if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { $found.Add($item.FullName); continue }
      if ($item -is [IO.DirectoryInfo]) { $pending.Push($item.FullName) }
    }
  }
  return ,$found
}
# Deletes a fixture tree: its links first, as links, then the folder, but only when no link is left; a failed link
# deletion keeps the tree (a recursive delete would follow the link). $DeleteLink lets the self-check make it fail.
function Remove-FixtureTree([string]$Path, [scriptblock]$DeleteLink = { param($Link) if ((Get-Item -LiteralPath $Link -Force) -is [IO.DirectoryInfo]) { [IO.Directory]::Delete($Link) } else { [IO.File]::Delete($Link) } }) {
  try {
    foreach ($link in (Get-FixtureLinks $Path)) { & $DeleteLink $link }
    $remaining = Get-FixtureLinks $Path
    if ($remaining.Count -gt 0) { return $false }
  } catch { Write-Warning ('Cleanup of the fixture links: ' + $_.Exception.Message); return $false }
  Remove-Item -LiteralPath $Path -Recurse -Force -ErrorAction SilentlyContinue
  return $true
}
function Get-LateChildren {
  return @(Get-CimInstance -ClassName Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.IndexOf('her-67-late-child', [StringComparison]::OrdinalIgnoreCase) -ge 0 })
}
function Assert-Moved([string]$Scenario, [switch]$Digest) {
  $newCli = Join-Path $root 'current\her-cli.exe'
  $pin = Join-Path $root 'deps\relay\0.0.1'
  $current = Get-Item -LiteralPath (Join-Path $root 'current') -Force
  Assert-True ([bool]($current.Attributes -band [IO.FileAttributes]::ReparsePoint)) ($Scenario + ': current is not a junction')
  foreach ($name in @('her.exe', 'her-cli.exe', 'herupdate.exe')) { Assert-True ((Get-FileHashText (Join-Path $root ('current\' + $name))) -eq (Get-FileHashText (Join-Path $Candidate $name))) ($Scenario + ': current\' + $name + ' is not the candidate') }
  Assert-True ((Get-FileHashText (Join-Path $root 'herupdate.exe')) -eq (Get-FileHashText (Join-Path $Candidate 'herupdate.exe'))) ($Scenario + ': the root helper is not the candidate')
  $owners = Get-Owners (Join-Path $root 'runtime') $root $newCli
  Assert-True ($owners.supervisor -eq 'held' -and $owners.live -eq 'held' -and $owners.console -eq 'held') ($Scenario + ': the new owners are not all held')
  Assert-True ($owners.supervisorExe.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) ($Scenario + ': the new supervisor does not run from the install root')
  # The move waited for the new live: a heartbeat names the new live owner (the copied old heartbeat never does).
  $beats = @(@('rust-heartbeat-live.json', 'rust-heartbeat.json') | ForEach-Object { Join-Path $root ('runtime\data\' + $_) } | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | ForEach-Object { [int](Get-Content -LiteralPath $_ -Raw | ConvertFrom-Json).pid })
  Assert-True ($owners.livePid -gt 0 -and $beats -contains $owners.livePid) ($Scenario + ': no heartbeat names the new live owner')
  $old = Get-Owners $oldRuntime $unused $cli
  Assert-True ($old.supervisor -ne 'held' -and $old.live -ne 'held' -and $old.console -ne 'held') ($Scenario + ': an old owner still runs')
  Assert-True ((Get-Listener $onebotPort).Exe -ieq (Join-Path $root 'deps\snowluma\1.2.3\node.exe')) ($Scenario + ': OneBot is not served from deps\snowluma')
  Assert-True ((Get-Listener $relayPort).Exe -ieq (Join-Path $pin 'cli-proxy-api.exe')) ($Scenario + ': the relay is not served from deps\relay')
  Assert-True ((Get-Listener $dshPort).CommandLine.IndexOf((Join-Path $root 'deps\dsh\9.9.9\'), [StringComparison]::OrdinalIgnoreCase) -ge 0) ($Scenario + ': DSH does not run from deps\dsh')
  Assert-True ((Get-Listener $consolePort).Pid -eq $owners.consolePid) ($Scenario + ': the console port is not the new console')
  $newText = Get-Content -LiteralPath (Join-Path $root 'runtime\config\config.json') -Raw -Encoding UTF8
  $oldText = Get-Content -LiteralPath (Join-Path $oldRuntime 'config\config.json') -Raw -Encoding UTF8
  Assert-True ((Get-ComparableConfig $oldText) -ceq (Get-ComparableConfig $newText)) ($Scenario + ': the configuration outside the intended changes did not survive the rewrite')
  $config = $newText | ConvertFrom-Json
  Assert-True ($config.dependencies.snowluma.version -eq '1.2.3' -and $config.dependencies.snowluma.program -ieq (Join-Path $root 'deps\snowluma\1.2.3\node.exe')) ($Scenario + ': dependencies.snowluma')
  Assert-True ($config.dependencies.dsh.version -eq '9.9.9' -and $config.dependencies.dsh.program -ieq $NodePath -and $null -eq $config.dependencies.dsh.script) ($Scenario + ': dependencies.dsh')
  Assert-True ($config.dependencies.relay.version -eq '0.0.1' -and $config.dependencies.relay.program -ieq (Join-Path $pin 'cli-proxy-api.exe') -and $config.dependencies.relay.clientKeyFile -ieq (Join-Path $pin 'her-relay-client.key') -and $config.dependencies.relay.loginDirectory -ieq (Join-Path $pin 'auth')) ($Scenario + ': dependencies.relay')
  Assert-True ((@($config.dependencies.relay.arguments) -join '|') -ieq ((Join-Path $pin 'fake-relay.mjs') + '|-config|' + (Join-Path $pin 'config.yaml'))) ($Scenario + ': relay arguments')
  Assert-True ($config.update.policy -eq 'off') ($Scenario + ': update.policy is not off')
  Assert-True ($null -eq $config.tasks.'start-all' -and $null -eq $config.tasks.installRoot) ($Scenario + ': legacy task paths remain')
  Assert-True ($config.snowluma.homeDir -ieq (Join-Path $root 'deps\snowluma\1.2.3') -and $config.snowluma.launcherPath -ieq (Join-Path $root 'deps\snowluma\1.2.3\index.mjs')) ($Scenario + ': SnowLuma paths were not remapped')
  $dependencies = Get-Content -LiteralPath (Join-Path $root 'runtime\dependencies.json') -Raw | ConvertFrom-Json
  Assert-True ($dependencies.relay -eq ('http://127.0.0.1:' + $relayPort)) ($Scenario + ': the relay origin')
  $harnessKey = Join-Path $root 'runtime\config\harness-api-key'
  if ($Digest) {
    Assert-True ($config.memory.digest.model -ceq 'fixture-digest-model' -and $config.memory.digest.timeoutSecs -eq 90) ($Scenario + ': memory.digest.model or its sibling setting')
    Assert-True ($dependencies.model -eq ('http://127.0.0.1:' + $relayPort)) ($Scenario + ': the model origin is not the relay')
    Assert-True ((Test-Path -LiteralPath $harnessKey -PathType Leaf) -and [IO.File]::ReadAllText($harnessKey) -ceq 'fixture-relay-key-67') ($Scenario + ': config\harness-api-key is not the relay client key')
  } else {
    Assert-True ($null -eq $config.memory.digest.model -and $config.memory.digest.timeoutSecs -eq 90) ($Scenario + ': memory.digest changed without -DigestModel')
    Assert-True ($dependencies.model -eq ('http://127.0.0.1:' + $dshPort)) ($Scenario + ': the model origin changed without -DigestModel')
    Assert-True (-not (Test-Path -LiteralPath $harnessKey)) ($Scenario + ': config\harness-api-key was written without -DigestModel')
  }
  # Linked DSH bundles: links recreated in the new DSH home, bundles copied, texts rewritten, and DSH loads them there.
  $newHome = Join-Path $root 'runtime\user\dsh'
  foreach ($name in $bundleNames) {
    $linkPath = Join-Path $newHome ('plugins\node_modules\' + $name)
    $link = Get-Item -LiteralPath $linkPath -Force
    Assert-True ([bool]($link.Attributes -band [IO.FileAttributes]::ReparsePoint)) ($Scenario + ': the bundle ' + $name + ' is not linked in the new DSH home')
    $target = ([string]@($link.Target)[0]) -replace '^\\\\\?\\', ''
    Assert-True ($target.TrimEnd('\') -ieq (Join-Path $newHome ('bundles\' + $name))) ($Scenario + ': the bundle link ' + $name + ' does not point at the new bundles folder')
    Assert-True ((Get-FileHashText (Join-Path $newHome ('bundles\' + $name + '\package.json'))) -eq (Get-FileHashText (Join-Path $bridgeBundles ($name + '\package.json')))) ($Scenario + ': the bundle ' + $name + ' was not copied')
  }
  Assert-True ((Get-FileHashText (Join-Path $newHome 'bundles\qq-agent-presets\presets\qq-chat-v2.patch.yml')) -eq (Get-FileHashText (Join-Path $bridgeBundles 'qq-agent-presets\presets\qq-chat-v2.patch.yml'))) ($Scenario + ': the bundle preset was not copied')
  $pluginsText = [IO.File]::ReadAllText((Join-Path $newHome 'plugins\package.json'))
  Assert-True ($pluginsText.IndexOf('qq-bridge', [StringComparison]::OrdinalIgnoreCase) -lt 0) ($Scenario + ': the new plugins package.json still names qq-bridge')
  $plugins = $pluginsText | ConvertFrom-Json
  Assert-True ($plugins.dependencies.'qq-mode-console' -ceq ('link:' + (Join-Path $newHome 'bundles\qq-mode-console').Replace('\', '/'))) ($Scenario + ': the forward-slash link: path was not rewritten')
  Assert-True ($plugins.dependencies.'qq-agent-presets' -ceq ('link:' + (Join-Path $newHome 'bundles\qq-agent-presets'))) ($Scenario + ': the backslash link: path was not rewritten')
  $loaded = Invoke-RestMethod -UseBasicParsing -Uri ('http://127.0.0.1:' + $dshPort + '/fixture/bundles') -TimeoutSec 10
  $loadedNames = @(@($loaded.loaded) | ForEach-Object { [string]$_.name } | Sort-Object)
  Assert-True (($loadedNames -join ',') -ceq 'qq-agent-presets,qq-mode-console') ($Scenario + ': DSH did not load both bundles')
  foreach ($entry in @($loaded.loaded)) {
    $real = [string]$entry.real
    Assert-True ($real.EndsWith(('\runtime\user\dsh\bundles\' + [string]$entry.name), [StringComparison]::OrdinalIgnoreCase) -and $real.IndexOf('qq-bridge', [StringComparison]::OrdinalIgnoreCase) -lt 0) ($Scenario + ': DSH loaded ' + [string]$entry.name + ' from outside the new bundles folder')
  }
  foreach ($name in @('import-manifest.json', 'data\lifecycle\session-handover.json', 'data\lifecycle\session-handover-required.json')) { Assert-True (-not (Test-Path -LiteralPath (Join-Path $root ('runtime\' + $name)))) ($Scenario + ': the new runtime kept ' + $name) }
  Assert-True ((Get-FileHashText (Join-Path $root 'runtime\user\dsh\.credentials.yaml')) -eq (Get-FileHashText (Join-Path $dshHome '.credentials.yaml'))) ($Scenario + ': the DSH credentials')
  $newCordis = Join-Path $root 'runtime\user\dsh\profiles\web\cordis.patch.yml'
  $check = Invoke-Her @('dsh-mcp-config', '--patch-file', $newCordis, '--bridge-exe', (Join-Path $root 'current\her.exe'), '--runtime-root', (Join-Path $root 'runtime'), '--console-port', [string]$consolePort, '--check', '--install-root', $decoy)
  Assert-True ($check.Trim() -eq 'unchanged') ($Scenario + ': the MCP block does not name the new image and runtime')
  Assert-True ((Remove-McpBlock ([IO.File]::ReadAllText($newCordis))) -eq (Remove-McpBlock $originalCordis)) ($Scenario + ': the DSH profile outside the MCP block changed')
  $relayLines = [IO.File]::ReadAllLines((Join-Path $pin 'config.yaml'))
  Assert-True ($relayLines -contains ("auth-dir: '" + (Join-Path $pin 'auth') + "'")) ($Scenario + ': the relay auth-dir line')
  Assert-True ((Get-FileHashText (Join-Path $pin 'auth\antigravity-fixture.json')) -eq (Get-FileHashText (Join-Path $authFolder 'antigravity-fixture.json'))) ($Scenario + ': the relay login file')
  Assert-True ([IO.File]::ReadAllText((Join-Path $pin 'her-relay-client.key')) -eq 'fixture-relay-key-67') ($Scenario + ': the relay client key file')
  $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  Assert-True ((Get-Acl -LiteralPath $root).AreAccessRulesProtected) ($Scenario + ': the install root still inherits access')
  foreach ($path in @($root, (Join-Path $root 'runtime\user\dsh'))) {
    $acl = Get-Acl -LiteralPath $path
    $foreign = @($acl.Access | Where-Object { @($sid, 'S-1-5-18', 'S-1-5-32-544') -notcontains $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value })
    Assert-True ($foreign.Count -eq 0) ($Scenario + ': another account has access under the install root')
  }
  foreach ($name in @($supervisorTask, $relayTask)) { Assert-True ((Get-TaskShape $name).EndsWith('#False')) ($Scenario + ': the task ' + $name + ' is still enabled') }
  [void](Invoke-Her @('scheduled-task', 'compare', '--install-root', $root) @(0) $newCli)
  $copied = Get-Item -LiteralPath (Join-Path $root 'deps\snowluma\1.2.3\config\snowluma.json')
  Assert-True ($copied.LastWriteTimeUtc -eq [DateTime]::new(2020, 1, 2, 3, 4, 5, [DateTimeKind]::Utc)) ($Scenario + ': a copy lost its timestamp')
  Assert-True (Test-Path -LiteralPath (Join-Path $root 'deps\snowluma\1.2.3\logs') -PathType Container) ($Scenario + ': an empty folder was not copied')
  Assert-Originals $Scenario
}

try {
  [void][IO.Directory]::CreateDirectory($F)
  # Fixture guard against an unused boundary (rust-validation.md); this never stats the production root.
  [void](Invoke-Her @('install-root', 'check-development', '--runtime-root', $F, '--install-root', $boundary))
  # Users may read the fixture (an inherited entry, as a wider production root ACL would give), so a folder the move
  # leaves inheriting after a rollback is visible to another account and Assert-Protected catches it.
  $fixtureAcl = Get-Acl -LiteralPath $F
  $fixtureAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new('S-1-5-32-545'), [Security.AccessControl.FileSystemRights]::ReadAndExecute, [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit', [Security.AccessControl.PropagationFlags]::None, [Security.AccessControl.AccessControlType]::Allow))
  [IO.Directory]::SetAccessControl($F, [Security.AccessControl.DirectorySecurity]$fixtureAcl)

  # A private copy of Node inside the fixture runs the whole synthetic production, so a production node.exe (possibly
  # under another account) never matches the fixture's configured program. The managed DSH pin is used in place; the
  # supervisor only requires npm beside Node as a file, so a stub stands in and npm never runs.
  $nodeFolder = Join-Path $F 'node'
  [void][IO.Directory]::CreateDirectory($nodeFolder)
  Copy-Item -LiteralPath $NodePath -Destination (Join-Path $nodeFolder 'node.exe')
  $NodePath = Join-Path $nodeFolder 'node.exe'
  Write-Fixture (Join-Path $nodeFolder 'node_modules\npm\bin\npm-cli.js') "// fixture npm stub; the rehearsal never runs npm`n"

  # The comparator must notice a wrapped array, a changed number and a dropped key, or S5 cannot judge the rewrite.
  $sample = '{"a":[1,2],"b":[],"c":["x"],"d":0.12,"e":null,"f":{},"g":"t"}'
  $sampleBase = Get-ComparableConfig $sample
  foreach ($variant in @('{"a":[1,2],"b":[],"c":"x","d":0.12,"e":null,"f":{},"g":"t"}', '{"a":[1,2],"b":[],"c":["x"],"d":0.13,"e":null,"f":{},"g":"t"}', '{"a":[1,2],"b":[],"c":["x"],"d":0.12,"f":{},"g":"t"}', '{"a":{"value":[1,2],"Count":2},"b":[],"c":["x"],"d":0.12,"e":null,"f":{},"g":"t"}')) {
    if ((Get-ComparableConfig $variant) -ceq $sampleBase) { throw 'FAIL comparator self-check: a changed configuration compares equal.' }
  }

  # Link self-check inside the fixture: a place behind a junction is refused, and a tree hash does not follow a
  # junction (a change behind it leaves the hash alone).
  $probeTarget = Join-Path $F 'link-check\target'
  $probeLink = Join-Path $F 'link-check\tree\linked'
  Write-Fixture (Join-Path $probeTarget 'file.txt') 'one'
  [void][IO.Directory]::CreateDirectory((Split-Path -Parent $probeLink))
  [void](New-Item -ItemType Junction -Path $probeLink -Target $probeTarget)
  try {
    if (Test-FixturePlace (Join-Path $probeLink 'fixture')) { throw 'FAIL link self-check: a place behind a junction is accepted.' }
    $before = Get-TreeHash (Join-Path $F 'link-check\tree')
    Write-Fixture (Join-Path $probeTarget 'file.txt') 'two'
    if ((Get-TreeHash (Join-Path $F 'link-check\tree')) -cne $before) { throw 'FAIL link self-check: the tree hash follows a junction.' }
    $removed = Remove-FixtureTree (Join-Path $F 'link-check\tree') { param($Link) throw 'link deletion refused (self-check)' } 3> $null
    if ($removed -or -not (Test-Path -LiteralPath (Join-Path $probeTarget 'file.txt'))) { throw 'FAIL link self-check: the cleanup deleted a tree whose link could not be removed.' }
  } finally {
    if (Test-Path -LiteralPath $probeLink) { [IO.Directory]::Delete($probeLink) }
    Remove-Item -LiteralPath (Join-Path $F 'link-check') -Recurse -Force
  }

  # Four free loopback ports, none a production port.
  $listeners = @(1..4 | ForEach-Object { $item = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0); $item.Start(); $item })
  $ports = @($listeners | ForEach-Object { $_.LocalEndpoint.Port })
  $listeners | ForEach-Object { $_.Stop() }
  if (@($ports | Where-Object { @(3000, 3001, 3080, 3100, 8317) -contains $_ }).Count -gt 0) { throw 'A fixture port overlaps a production port; run the rehearsal again.' }
  $consolePort = $ports[0]; $onebotPort = $ports[1]; $dshPort = $ports[2]; $relayPort = $ports[3]

  # Releases: the candidate as the old release (with its console twin) and as the release source with its hash list.
  foreach ($folder in @($oldRelease, $releaseSource)) {
    [void][IO.Directory]::CreateDirectory($folder)
    foreach ($name in @('her.exe', 'her-cli.exe', 'herupdate.exe')) { Copy-Item -LiteralPath (Join-Path $Candidate $name) -Destination (Join-Path $folder $name) }
  }
  $sumLines = @(foreach ($name in @('her.exe', 'her-cli.exe', 'herupdate.exe')) { (Get-FileHashText (Join-Path $releaseSource $name)) + '  ' + $name })
  Write-Fixture (Join-Path $releaseSource 'SHA256SUMS') (($sumLines -join "`r`n") + "`r`n")

  # Legacy install root content: SnowLuma (its own node.exe, an old timestamp, a large file, an empty folder).
  [void][IO.Directory]::CreateDirectory((Join-Path $snowFolder 'logs'))
  Copy-Item -LiteralPath $NodePath -Destination (Join-Path $snowFolder 'node.exe')
  Copy-Item -LiteralPath $fake -Destination (Join-Path $snowFolder 'index.mjs')
  # The fakes append their start and every request to logs outside every source folder (diagnostics after a failure).
  [void][IO.Directory]::CreateDirectory((Join-Path $F 'fake-logs'))
  Write-FixtureJson (Join-Path $snowFolder 'fixture-resident.json') ([ordered]@{ role = 'onebot'; port = $onebotPort; requestLog = (Join-Path $F 'fake-logs\onebot.jsonl') })
  Write-FixtureJson (Join-Path $snowFolder 'package.json') ([ordered]@{ name = 'snowluma-fixture'; version = '1.2.3' })
  Write-Fixture (Join-Path $snowFolder 'config\snowluma.json') '{"note":"fixture"}'
  $snowConfig = Get-Item -LiteralPath (Join-Path $snowFolder 'config\snowluma.json')
  $snowConfig.LastWriteTimeUtc = [DateTime]::new(2020, 1, 2, 3, 4, 5, [DateTimeKind]::Utc)
  [void][IO.Directory]::CreateDirectory((Join-Path $snowFolder 'data\fixture'))
  [IO.File]::WriteAllBytes((Join-Path $snowFolder 'data\fixture\store.bin'), (New-Object byte[] (64MB)))
  [void][IO.Directory]::CreateDirectory((Join-Path $root 'qq-bridge'))
  Write-Fixture (Join-Path $root 'backups\notes.txt') 'legacy notes'

  # DSH package in an npx-style cache folder.
  Write-FixtureJson (Join-Path $npx 'package.json') ([ordered]@{ name = 'npx-fixture'; private = $true })
  Write-FixtureJson (Join-Path $dshPackage 'package.json') ([ordered]@{ name = '@deepseek-ai/dsh'; version = '9.9.9'; bin = [ordered]@{ dsh = 'bin/dsh.mjs' } })
  [void][IO.Directory]::CreateDirectory((Join-Path $dshPackage 'bin'))
  Copy-Item -LiteralPath $fake -Destination (Join-Path $dshPackage 'bin\dsh.mjs')
  Write-FixtureJson (Join-Path $dshPackage 'bin\fixture-resident.json') ([ordered]@{ role = 'dsh'; port = $dshPort; failMarker = $failMarker; bundles = $bundleNames; requestLog = (Join-Path $F 'fake-logs\dsh.jsonl') })
  [void][IO.Directory]::CreateDirectory((Join-Path $F 'dsh-work'))

  # Profile DSH home, and the runtime's own copy of its credentials.
  $secret = 'A' * 43
  $credentialText = 'secret: ' + $secret + "`nrecords:`n  client-connection/browser-session:`n    kind: grant`n    payload:`n      version: 1`n      secret: " + $secret + "`n"
  Write-Fixture (Join-Path $dshHome '.credentials.yaml') $credentialText
  Write-Fixture (Join-Path $oldRuntime 'user\dsh\.credentials.yaml') $credentialText
  # Two bundles in the install root's qq-bridge folder, linked from the profile's plugins folder and named by link:
  # dependencies in two spellings. Production uses directory symbolic links, which need a privilege (or developer
  # mode): qq-agent-presets gets one when this account may create it, otherwise a junction; the line says which.
  [void][IO.Directory]::CreateDirectory((Join-Path $dshHome 'plugins\node_modules'))
  foreach ($name in $bundleNames) {
    Write-FixtureJson (Join-Path $bridgeBundles ($name + '\package.json')) ([ordered]@{ name = $name; version = '0.0.1'; private = $true })
    $linkPath = Join-Path $dshHome ('plugins\node_modules\' + $name)
    $linkTarget = Join-Path $bridgeBundles $name
    $shape = 'junction'
    if ($name -eq 'qq-agent-presets') {
      try { [void](New-Item -ItemType SymbolicLink -Path $linkPath -Target $linkTarget -ErrorAction Stop); $shape = 'symbolic link' } catch { $shape = 'junction' }
    }
    if ($shape -eq 'junction') { [void](New-Item -ItemType Junction -Path $linkPath -Target $linkTarget) }
    Write-Host ('bundle link ' + $name + ': ' + $shape)
  }
  # A DSH-home file outside profiles and plugins that names a bundle's old folder: Inventory lists it for F3.
  Write-FixtureJson (Join-Path $dshHome 'state\fixture-index.json') ([ordered]@{ bundle = (Join-Path $bridgeBundles 'qq-mode-console') })
  Write-Fixture (Join-Path $bridgeBundles 'qq-agent-presets\presets\qq-chat-v2.patch.yml') "# fixture preset`n"
  Write-FixtureJson (Join-Path $dshHome 'plugins\package.json') ([ordered]@{ name = 'dsh-plugins-fixture'; private = $true; dependencies = [ordered]@{ 'qq-mode-console' = ('link:' + (Join-Path $bridgeBundles 'qq-mode-console').Replace('\', '/')); 'qq-agent-presets' = ('link:' + (Join-Path $bridgeBundles 'qq-agent-presets')) } })
  Write-Fixture (Join-Path $dshHome 'sessions\fixture-workdir\fixture.bin') ('fixture bytes ' * 50)
  $cordis = Join-Path $dshHome 'profiles\web\cordis.patch.yml'
  Write-Fixture $cordis "# fixture profile head`nbundles:`n  - qq-mode-console`n  - qq-agent-presets`n"
  [void](Invoke-Her @('dsh-mcp-config', '--patch-file', $cordis, '--bridge-exe', (Join-Path $oldRelease 'her.exe'), '--runtime-root', $oldRuntime, '--console-port', [string]$consolePort, '--install-root', $decoy))
  [IO.File]::AppendAllText($cordis, "# fixture profile tail`n", [Text.UTF8Encoding]::new($false))
  $originalCordis = [IO.File]::ReadAllText($cordis)

  # Relay folder (node.exe stands in for the relay program) and its login folder in the profile.
  [void][IO.Directory]::CreateDirectory($relayFolder)
  Copy-Item -LiteralPath $NodePath -Destination (Join-Path $relayFolder 'cli-proxy-api.exe')
  Copy-Item -LiteralPath $fake -Destination (Join-Path $relayFolder 'fake-relay.mjs')
  Write-Fixture (Join-Path $relayFolder 'config.yaml') ('port: ' + $relayPort + "`nauth-dir: `"~/.cli-proxy-api`"`napi-keys:`n  - `"fixture-relay-key-67`"`n")
  Write-Fixture (Join-Path $authFolder 'antigravity-fixture.json') '{"type":"fixture"}'

  # Old runtime in the fixed shape of today's production, with values PowerShell 5.1 must carry through a rewrite.
  $config = [ordered]@{
    consoleToken = 'fixture-token-67'; consolePort = $consolePort; ownerQQ = 1
    allow = [ordered]@{ groups = @(1); private = @() }
    backends = [ordered]@{ mcpRustBridge = $true }
    rollout = [ordered]@{ rustGroups = @('group:1') }
    snowluma = [ordered]@{ accessToken = 'fixture-snow'; homeDir = $snowFolder; launcherPath = (Join-Path $snowFolder 'index.mjs') }
    tasks = [ordered]@{ installRoot = $oldRelease; nodePath = $NodePath; 'start-all' = [ordered]@{ snowLumaDirectory = $snowFolder; dshScript = (Join-Path $dshPackage 'bin\dsh.mjs'); dshWorkingDirectory = (Join-Path $F 'dsh-work') } }
    backup = [ordered]@{ keepDays = 7 }
    memory = [ordered]@{ digest = [ordered]@{ timeoutSecs = 90 } }
    fixtureExtras = [ordered]@{
      many = @(1, 2, 3); one = @('only'); none = @(); objects = @([ordered]@{ a = 1 }, [ordered]@{ b = @() }); nested = @(@(1, 2), @('x'))
      big = [long]4 * [long][int]::MaxValue; ratio = 0.12; nothing = $null; empty = [ordered]@{}
      text = ('quote " backslash \ angle <tag> amp & apostrophe ' + "'" + ' ' + [string][char]0x5C0F + [string][char]0x9CB8)
    }
  }
  Write-FixtureJson (Join-Path $oldRuntime 'config\config.json') $config
  Write-FixtureJson (Join-Path $oldRuntime 'dependencies.json') ([ordered]@{ dsh = ('http://127.0.0.1:' + $dshPort); model = ('http://127.0.0.1:' + $dshPort); onebot = ('http://127.0.0.1:' + $onebotPort); onebot_ws = ('ws://127.0.0.1:' + $onebotPort) })
  Write-Fixture (Join-Path $oldRuntime 'data\social-v2.json') '{}'
  Write-FixtureJson (Join-Path $oldRuntime 'data\sessions.json') ([ordered]@{ sessions = [ordered]@{}; workspaceId = 'fixture-workspace' })
  [void][IO.Directory]::CreateDirectory((Join-Path $oldRuntime 'resources'))
  $originalConfig = [IO.File]::ReadAllBytes((Join-Path $oldRuntime 'config\config.json'))

  # Two fixture tasks start the old supervisor and the relay through hidden VBS launchers that wait, like production.
  $guid = [guid]::NewGuid().ToString('N')
  $supervisorTask = 'HER-Test-67-' + $guid + '-supervisor'
  $relayTask = 'HER-Test-67-' + $guid + '-relay'
  $vbsSupervisor = Join-Path $F 'launch\old-supervisor.vbs'
  $vbsRelay = Join-Path $F 'launch\old-relay.vbs'
  [void][IO.Directory]::CreateDirectory((Join-Path $F 'launch'))
  [IO.File]::WriteAllText($vbsSupervisor, ("Set shell = CreateObject(`"WScript.Shell`")`r`nshell.Environment(`"PROCESS`")(`"HER_TASK_SCHEDULER_FILE`") = `"" + $oldScheduler + "`"`r`nshell.Run `"`"`"" + (Join-Path $oldRelease 'her.exe') + "`"`" supervise --runtime-root `"`"" + $oldRuntime + "`"`" --install-root `"`"" + $unused + "`"`"`", 0, True`r`n"), [Text.Encoding]::Unicode)
  [IO.File]::WriteAllText($vbsRelay, ("Set shell = CreateObject(`"WScript.Shell`")`r`nshell.Run `"`"`"" + (Join-Path $relayFolder 'cli-proxy-api.exe') + "`"`" `"`"" + (Join-Path $relayFolder 'fake-relay.mjs') + "`"`" -config `"`"" + (Join-Path $relayFolder 'config.yaml') + "`"`"`", 0, True`r`n"), [Text.Encoding]::Unicode)
  $wscript = Join-Path $env:WINDIR 'System32\wscript.exe'
  $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
  foreach ($pair in @(@($supervisorTask, $vbsSupervisor), @($relayTask, $vbsRelay))) {
    if (Get-ScheduledTask -TaskName $pair[0] -ErrorAction SilentlyContinue) { throw 'A fixture task name unexpectedly exists.' }
    Register-ScheduledTask -TaskName $pair[0] -TaskPath '\' -Action (New-ScheduledTaskAction -Execute $wscript -Argument ('"' + $pair[1] + '"')) -Settings $settings -ErrorAction Stop | Out-Null
    $registered += $pair[0]
  }
  [void](Invoke-Her @('lifecycle', 'set', 'running', '--runtime-root', $oldRuntime, '--by', 'fixture', '--install-root', $unused))
  Start-ScheduledTask -TaskName $relayTask -TaskPath '\'
  Start-ScheduledTask -TaskName $supervisorTask -TaskPath '\'
  Wait-For { Test-OldUp } 'the synthetic old production starts' 120
  # The fake gateway must answer in the framing HER's client reads, or every health check fails at snowluma.
  if (-not (Test-RawOneBot $onebotPort)) { throw 'FAIL fixture: the fake OneBot get_status answer is not plain JSON that HER''s client can read.' }
  $baselineSupervisor = (Get-Owners $oldRuntime $unused $cli).supervisorPid
  $taskShapes = @{}
  foreach ($name in @($supervisorTask, $relayTask)) { $taskShapes[$name] = Get-TaskShape $name }
  $originals = Get-Originals
  $rootSddl = Get-RootSddl
  $opsSource = Join-Path $F 'her-orch\ops'
  Write-Fixture (Join-Path $opsSource 'fixture-op.ps1') "# fixture operator script`r`n"
  $env:HER_TASK_SCHEDULER_FILE = $herScheduler
  [void][IO.Directory]::CreateDirectory($evidence)
  $rehearsalArgs = @('-Rehearsal', '-InstallRoot', $root, '-EvidenceRoot', $evidence, '-OldSupervisorTask', $supervisorTask, '-OldRelayTask', $relayTask, '-ProfileFolder', $profileFolder)
  $sourceArgs = @('-OldRuntime', $oldRuntime, '-OldRelayFolder', $relayFolder, '-ReleaseSource', $releaseSource)
  $moveArgs = $rehearsalArgs + @('-Mode', 'Move') + $sourceArgs + @('-ReleaseName', 'fixture-new', '-HealthWaitSeconds', '180')

  Step 'S0 self-test'
  $run = Invoke-MoveScript @('-Mode', 'SelfTest') 'S0'
  Assert-Run $run 0 'PASS self-test: 55 checks' 'S0 self-test'

  Step 'S1a production guards'
  $run = Invoke-MoveScript @('-Mode', 'Inventory', '-InstallRoot', $root, '-EvidenceRoot', $evidence) 'S1a-no-switch'
  Assert-Run $run 1 'pass -Production for the live move or -Rehearsal' 'S1a no switch'
  $run = Invoke-MoveScript @('-Mode', 'Inventory', '-Production', '-InstallRoot', $root, '-EvidenceRoot', $evidence) 'S1a-production'
  Assert-Run $run 1 'the production install root is C:\qqbot' 'S1a production switch'
  Assert-OldUntouched 'S1a'

  Step 'S1b inventory'
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Inventory') + $sourceArgs + @('-ReleaseName', 'fixture-new')) 'S1b'
  Assert-Run $run 0 'INVENTORY: 0 blockers' 'S1b'
  $inventoryFolder = @(Get-ChildItem -LiteralPath $evidence -Directory -Filter 'inventory-*' | Sort-Object -Property Name -Descending)[0]
  $otherList = [IO.File]::ReadAllText((Join-Path $inventoryFolder.FullName 'dsh-other-bundle-references.txt'))
  Assert-True ($otherList.IndexOf('state\fixture-index.json', [StringComparison]::OrdinalIgnoreCase) -ge 0) 'S1b: Inventory does not list the other DSH-home file that names a bundle'
  # No account-home path on the console or in inventory.json (the fixture lives under the profile's temporary folder).
  Assert-True ($run.Text.IndexOf($env:USERPROFILE, [StringComparison]::OrdinalIgnoreCase) -lt 0) 'S1b: the console shows an account-home path'
  $inventoryText = [IO.File]::ReadAllText((Join-Path $inventoryFolder.FullName 'inventory.json'))
  foreach ($private in @($profileFolder, $env:USERPROFILE)) {
    Assert-True ($inventoryText.IndexOf($private.Replace('\', '\\'), [StringComparison]::OrdinalIgnoreCase) -lt 0) 'S1b: inventory.json holds an account-home path'
  }
  Assert-True ($inventoryText.IndexOf('<profile>', [StringComparison]::Ordinal) -ge 0) 'S1b: inventory.json has no profile placeholder'
  Assert-OldUntouched 'S1b'

  Step 'S1c busy turn'
  $now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
  Add-LedgerRecord ([ordered]@{ k = 'begun'; turn = 'turn-fixture-busy'; t = $now; key = 'group:1'; backend = 'dsh'; reason = 'atMention'; epoch = 'e1'; inbound = @('group:1|message:7'); resumedFrom = $null })
  $run = Invoke-MoveScript $moveArgs 'S1c'
  Add-LedgerRecord ([ordered]@{ k = 'ended'; turn = 'turn-fixture-busy'; t = $now + 1; outcome = 'cancelled'; reason = 'fixture' })
  Assert-Run $run 1 'conversation turns are in progress' 'S1c'
  Assert-OldUntouched 'S1c'

  Step 'S1d old archives'
  $archive = Join-Path $root 'backups\qqbot-2026-01-01T04-00-00.zip'
  Write-Fixture $archive 'not a real archive'
  try { $run = Invoke-MoveScript $moveArgs 'S1d' } finally { Remove-Item -LiteralPath $archive -Force }
  Assert-Run $run 1 'archives the new retention would prune' 'S1d'
  Assert-OldUntouched 'S1d'

  Step 'S1e earlier attempt'
  [void][IO.Directory]::CreateDirectory((Join-Path $root 'runtime'))
  try {
    $run = Invoke-MoveScript $moveArgs 'S1e'
    $inventory = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Inventory') + $sourceArgs + @('-ReleaseName', 'fixture-new')) 'S1e-inventory'
  } finally { Remove-Item -LiteralPath (Join-Path $root 'runtime') -Recurse -Force }
  Assert-Run $run 1 'the install root already has runtime' 'S1e'
  # Inventory records the finding and runs every other check (one blocker, then the summary line).
  Assert-Run $inventory 1 'INVENTORY: 1 blockers' 'S1e inventory'
  Assert-OldUntouched 'S1e'

  Step 'S1f disk space'
  $run = Invoke-MoveScript ($moveArgs + @('-FreeSpaceReserveBytes', [string]([long]::MaxValue - 1))) 'S1f'
  Assert-Run $run 1 'not enough free disk space' 'S1f'
  Assert-OldUntouched 'S1f'

  Step 'S1g concurrent run'
  $lock = [IO.File]::Open((Join-Path $evidence 'move.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
  try { $run = Invoke-MoveScript $moveArgs 'S1g' } finally { $lock.Dispose() }
  Assert-Run $run 1 'another run of this script is active' 'S1g'
  Assert-OldUntouched 'S1g'

  Step 'S1h junction in the DSH home'
  $link = Join-Path $dshHome 'linked'
  [void](New-Item -ItemType Junction -Path $link -Target (Join-Path $F 'npm-cache'))
  try { $run = Invoke-MoveScript $moveArgs 'S1h' } finally { [IO.Directory]::Delete($link) }
  Assert-Run $run 1 'a junction or link was found' 'S1h'
  Assert-OldUntouched 'S1h'

  Step 'S1i path too long for the target'
  $deep = Join-Path $dshHome 'deep'
  $path = $deep
  while ($path.Length + 41 -le 240) { $path = Join-Path $path ('d' * 40) }
  $pad = 240 - $path.Length - 1
  if ($pad -gt 0) { $path = Join-Path $path ('e' * $pad) }
  [void][IO.Directory]::CreateDirectory($path)
  Write-Fixture (Join-Path $path ('f' * (255 - $path.Length - 1))) 'deep'
  try { $run = Invoke-MoveScript $moveArgs 'S1i' } finally { Remove-Item -LiteralPath $deep -Recurse -Force }
  Assert-Run $run 1 'a path is longer than 259 characters' 'S1i'
  Assert-OldUntouched 'S1i'

  Step 'S1j tampered image list'
  $sumsPath = Join-Path $releaseSource 'SHA256SUMS'
  $sumsBytes = [IO.File]::ReadAllBytes($sumsPath)
  $sumsText = [IO.File]::ReadAllText($sumsPath)
  $first = '0'
  if ($sumsText[0] -eq '0') { $first = '1' }
  Write-Fixture $sumsPath ($first + $sumsText.Substring(1))
  try { $run = Invoke-MoveScript $moveArgs 'S1j' } finally { [IO.File]::WriteAllBytes($sumsPath, $sumsBytes) }
  Assert-Run $run 1 'differs from SHA256SUMS' 'S1j'
  Assert-OldUntouched 'S1j'

  Step 'S1k configuration path with a parent step'
  $configPath = Join-Path $oldRuntime 'config\config.json'
  $edited = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
  $edited.snowluma.launcherPath = Join-Path $snowFolder '..\..\elsewhere\x.mjs'
  Write-FixtureJson $configPath $edited
  try { $run = Invoke-MoveScript $moveArgs 'S1k' } finally { [IO.File]::WriteAllBytes($configPath, $originalConfig) }
  Assert-Run $run 1 'configuration paths need a decision' 'S1k'
  Assert-OldUntouched 'S1k'

  Step 'S1l versioned release name'
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Move') + $sourceArgs + @('-ReleaseName', 'v0.1.0')) 'S1l'
  Assert-Run $run 1 '-ReleaseName must be' 'S1l'
  Assert-OldUntouched 'S1l'

  Step 'S1m rehearsal confinement'
  $outside = @('-Rehearsal', '-InstallRoot', $root, '-EvidenceRoot', $evidence, '-OldSupervisorTask', $supervisorTask, '-OldRelayTask', $relayTask, '-ProfileFolder', $env:SystemRoot, '-Mode', 'Move') + $sourceArgs + @('-ReleaseName', 'fixture-new')
  $run = Invoke-MoveScript $outside 'S1m'
  Assert-Run $run 1 'rehearsal paths must lie inside the temporary folder' 'S1m'
  # Inside the temporary folder by name, but behind a junction: refused before anything is written there.
  $redirect = Join-Path $F 'redirect'
  [void][IO.Directory]::CreateDirectory((Join-Path $F 'redirect-target'))
  [void](New-Item -ItemType Junction -Path $redirect -Target (Join-Path $F 'redirect-target'))
  $behind = @('-Rehearsal', '-InstallRoot', $root, '-EvidenceRoot', (Join-Path $redirect 'evidence'), '-OldSupervisorTask', $supervisorTask, '-OldRelayTask', $relayTask, '-ProfileFolder', $profileFolder, '-Mode', 'Inventory') + $sourceArgs + @('-ReleaseName', 'fixture-new')
  try { $run = Invoke-MoveScript $behind 'S1m-junction' } finally { [IO.Directory]::Delete($redirect) }
  Assert-Run $run 1 'a rehearsal path passes through a junction or link' 'S1m junction'
  Assert-True (-not (Test-Path -LiteralPath (Join-Path $F 'redirect-target\evidence'))) 'S1m: the rehearsal wrote behind the junction'
  # A source reached through a junction above it: refused by the check production runs too, before the rehearsal rule.
  $aliasOrch = Join-Path $F 'alias-orch'
  [void](New-Item -ItemType Junction -Path $aliasOrch -Target (Join-Path $F 'her-orch'))
  $aliased = $rehearsalArgs + @('-Mode', 'Inventory', '-OldRuntime', $oldRuntime, '-OldRelayFolder', (Join-Path $aliasOrch 'relay\cliproxy-0.0.1'), '-ReleaseSource', $releaseSource, '-ReleaseName', 'fixture-new')
  try { $run = Invoke-MoveScript $aliased 'S1m-ancestor' } finally { [IO.Directory]::Delete($aliasOrch) }
  Assert-Run $run 1 'a junction or link lies on the path of an old production folder' 'S1m junction above a source'
  Assert-OldUntouched 'S1m'

  Step 'S1n process changed after the checks'
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalFault', 'StaleIdentity')) 'S1n'
  Assert-Run $run 1 'the relay process changed since the checks' 'S1n'
  Assert-OldUntouched 'S1n'

  Step 'S1o turn started after the tasks were disabled'
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalFault', 'DisturbAfterDisable')) 'S1o'
  Add-LedgerRecord ([ordered]@{ k = 'ended'; turn = 'turn-fault-67'; t = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(); outcome = 'cancelled'; reason = 'fixture' })
  Assert-Run $run 1 'conversation turns started during the checks' 'S1o'
  Assert-OldUntouched 'S1o'

  Step 'S1p the owner types a wrong word'
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalConfirm')) 'S1p' 'move'
  Assert-Run $run 1 'the owner did not confirm' 'S1p'
  Assert-OldUntouched 'S1p'

  Step 'S1q link inside a linked bundle'
  $nested = Join-Path $bridgeBundles 'qq-mode-console\nested'
  [void](New-Item -ItemType Junction -Path $nested -Target (Join-Path $F 'npm-cache'))
  try { $run = Invoke-MoveScript $moveArgs 'S1q' } finally { [IO.Directory]::Delete($nested) }
  Assert-Run $run 1 'a junction or link was found in dsh-bundle-qq-mode-console' 'S1q'
  Assert-OldUntouched 'S1q'

  Step 'S1r bundle placement and dependencies'
  $placeholder = Join-Path $dshHome 'bundles'
  [void][IO.Directory]::CreateDirectory($placeholder)
  try { $run = Invoke-MoveScript $moveArgs 'S1r-bundles' } finally { [IO.Directory]::Delete($placeholder) }
  Assert-Run $run 1 'the profile DSH home already has a bundles entry' 'S1r bundles entry'
  $twinTarget = Join-Path $root 'qq-bridge\other\qq-mode-console'
  $twinLink = Join-Path $dshHome 'plugins\extra\qq-mode-console'
  [void][IO.Directory]::CreateDirectory($twinTarget)
  [void][IO.Directory]::CreateDirectory((Split-Path -Parent $twinLink))
  [void](New-Item -ItemType Junction -Path $twinLink -Target $twinTarget)
  try { $run = Invoke-MoveScript $moveArgs 'S1r-twin' }
  finally { [IO.Directory]::Delete($twinLink); Remove-Item -LiteralPath (Join-Path $dshHome 'plugins\extra') -Recurse -Force; Remove-Item -LiteralPath (Join-Path $root 'qq-bridge\other') -Recurse -Force }
  Assert-Run $run 1 'two linked DSH bundles share a folder name' 'S1r same folder name'
  # The next three runs are Inventory runs: read-only, so a missing check shows as exit 0 and nothing moves.
  $inventoryArgs = $rehearsalArgs + @('-Mode', 'Inventory') + $sourceArgs + @('-ReleaseName', 'fixture-new')
  $bundleManifest = Join-Path $bridgeBundles 'qq-mode-console\package.json'
  $manifestBytes = [IO.File]::ReadAllBytes($bundleManifest)
  $manifestTime = (Get-Item -LiteralPath $bundleManifest).LastWriteTimeUtc
  Write-FixtureJson $bundleManifest ([ordered]@{ name = 'qq-mode-console'; version = '0.0.1'; peerDependencies = [ordered]@{ 'fixture-peer' = '1.0.0' } })
  try { $run = Invoke-MoveScript $inventoryArgs 'S1r-dependency' }
  finally { [IO.File]::WriteAllBytes($bundleManifest, $manifestBytes); (Get-Item -LiteralPath $bundleManifest).LastWriteTimeUtc = $manifestTime }
  Assert-Run $run 1 'depends on a package outside its own folder' 'S1r undeclared peer dependency'
  # An undeclared package in a node_modules above the bundle that the new place cannot reach.
  $ancestorModules = Join-Path $bridgeBundles 'node_modules'
  Write-FixtureJson (Join-Path $ancestorModules 'fixture-shared\package.json') ([ordered]@{ name = 'fixture-shared'; version = '1.0.0' })
  try { $run = Invoke-MoveScript $inventoryArgs 'S1r-ancestor' } finally { Remove-Item -LiteralPath $ancestorModules -Recurse -Force }
  Assert-Run $run 1 'depends on a package outside its own folder' 'S1r ancestor node_modules'
  # A bundle reached through a junction inside qq-bridge.
  $realFolder = Join-Path $root 'qq-bridge\real'
  $alias = Join-Path $root 'qq-bridge\alias'
  $thirdLink = Join-Path $dshHome 'plugins\extra\fixture-third'
  Write-FixtureJson (Join-Path $realFolder 'fixture-third\package.json') ([ordered]@{ name = 'fixture-third'; version = '0.0.1' })
  [void](New-Item -ItemType Junction -Path $alias -Target $realFolder)
  [void][IO.Directory]::CreateDirectory((Split-Path -Parent $thirdLink))
  [void](New-Item -ItemType Junction -Path $thirdLink -Target (Join-Path $alias 'fixture-third'))
  try { $run = Invoke-MoveScript $inventoryArgs 'S1r-chain' }
  finally { [IO.Directory]::Delete($thirdLink); Remove-Item -LiteralPath (Join-Path $dshHome 'plugins\extra') -Recurse -Force; [IO.Directory]::Delete($alias); Remove-Item -LiteralPath $realFolder -Recurse -Force }
  Assert-Run $run 1 'a junction or link was found on the way to a linked DSH bundle' 'S1r junction inside qq-bridge'
  Assert-OldUntouched 'S1r'

  Step 'S1s digest key conflict'
  $oldHarnessKey = Join-Path $oldRuntime 'config\harness-api-key'
  Write-Fixture $oldHarnessKey 'another-fixture-key'
  try { $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Inventory') + $sourceArgs + @('-ReleaseName', 'fixture-new', '-DigestModel', 'fixture-digest-model')) 'S1s' }
  finally { Remove-Item -LiteralPath $oldHarnessKey -Force }
  Assert-Run $run 1 'holds another key than the relay client key' 'S1s'
  Assert-OldUntouched 'S1s'

  Step 'S1t client key on the relay command line'
  # The relay runs once with the key in an argument: Inventory blocks, and inventory.json must not hold the key.
  Stop-FixtureListener $relayPort
  Wait-For { $null -eq (Get-Listener $relayPort) } 'S1t: the relay stops' 30
  Wait-For { (Get-ScheduledTask -TaskName $relayTask -TaskPath '\').State -ne 'Running' } 'S1t: the relay task instance ends' 60
  [void](Start-Process -FilePath (Join-Path $relayFolder 'cli-proxy-api.exe') -ArgumentList @(('"' + (Join-Path $relayFolder 'fake-relay.mjs') + '"'), '-config', ('"' + (Join-Path $relayFolder 'config.yaml') + '"'), '--note=fixture-relay-key-67') -WindowStyle Hidden -PassThru)
  Wait-For { $null -ne (Get-Listener $relayPort) } 'S1t: the keyed relay listens' 30
  try { $run = Invoke-MoveScript $inventoryArgs 'S1t' }
  finally {
    Stop-FixtureListener $relayPort
    Wait-For { $null -eq (Get-Listener $relayPort) } 'S1t: the keyed relay stops' 30
    Start-ScheduledTask -TaskName $relayTask -TaskPath '\'
    Wait-For { Test-OldUp } 'S1t: the relay runs from its task again' 60
  }
  Assert-Run $run 1 'the relay command line carries the client key' 'S1t'
  $keyedFolder = @(Get-ChildItem -LiteralPath $evidence -Directory -Filter 'inventory-*' | Sort-Object -Property Name -Descending)[0]
  Assert-True ([IO.File]::ReadAllText((Join-Path $keyedFolder.FullName 'inventory.json')).IndexOf('fixture-relay-key-67', [StringComparison]::Ordinal) -lt 0) 'S1t: inventory.json holds the relay client key'
  Assert-OldUntouched 'S1t'

  Step 'S2 health failure rolls back'
  Write-Fixture $failMarker 'fail'
  try { $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Move') + $sourceArgs + @('-ReleaseName', 'fixture-new', '-HealthWaitSeconds', '45')) 'S2' }
  finally { Remove-Item -LiteralPath $failMarker -Force }
  Assert-Run $run 2 'ROLLED BACK' 'S2'
  Assert-OldRunning 'S2'

  Step 'S2b corrupted copy rolls back'
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalFault', 'CorruptCopy')) 'S2b'
  Assert-Run $run 2 'differs from its source' 'S2b'
  Assert-OldRunning 'S2b'

  Step 'S3 interrupted after the stop'
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalAbortAfter', 'Stopped')) 'S3'
  Assert-Run $run 4 'REHEARSAL ABORT after Stopped' 'S3'
  $owners = Get-Owners $oldRuntime $unused $cli
  Assert-True ($owners.supervisor -ne 'held' -and $null -eq (Get-Listener $onebotPort)) 'S3: the old production still runs at the abort point'
  $state = Get-LatestState
  # Someone deleted a disabled old task meanwhile: the rollback recreates it from its saved definition.
  Wait-For { (Get-ScheduledTask -TaskName $relayTask -TaskPath '\').State -ne 'Running' } 'S3: the relay task instance ends' 60
  Unregister-ScheduledTask -TaskName $relayTask -TaskPath '\' -Confirm:$false
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Rollback', '-StateFile', $state)) 'S3-rollback'
  Assert-Run $run 0 'ROLLED BACK' 'S3 rollback'
  Assert-OldRunning 'S3'
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Rollback', '-StateFile', $state)) 'S3-rollback-again'
  Assert-Run $run 0 'nothing to roll back' 'S3 second rollback'

  Step 'S3b interrupted after staging'
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalAbortAfter', 'Staged')) 'S3b'
  Assert-Run $run 4 'REHEARSAL ABORT after Staged' 'S3b'
  # Someone put a junction where the new deps folder would be: the rollback deletes that junction as a link and never
  # enters it, so a link inside its target survives.
  $outside = Join-Path $F 'outside-deps'
  $outsideLink = Join-Path $outside 'inner'
  Write-Fixture (Join-Path $F 'outside-inner\keep.txt') 'keep'
  [void][IO.Directory]::CreateDirectory($outside)
  [void](New-Item -ItemType Junction -Path $outsideLink -Target (Join-Path $F 'outside-inner'))
  [void](New-Item -ItemType Junction -Path (Join-Path $root 'deps') -Target $outside)
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Rollback', '-StateFile', (Get-LatestState))) 'S3b-rollback'
  Assert-Run $run 0 'ROLLED BACK' 'S3b rollback'
  Assert-True ((Test-Path -LiteralPath $outsideLink) -and ((Get-Item -LiteralPath $outsideLink -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) 'S3b: the rollback entered a junction at deps and deleted a link behind it'
  [IO.Directory]::Delete($outsideLink)
  Remove-Item -LiteralPath $outside, (Join-Path $F 'outside-inner') -Recurse -Force
  Assert-OldUntouched 'S3b'

  Step 'S3c interrupted after the copy; a held folder blocks the move-aside'
  # The retired cutover files are left out of the copy. They exist only while this Move runs and are gone before any
  # rollback restarts the old production (with them, its live bridge would demand legacy fences and refuse to start).
  $handover = @('import-manifest.json', 'data\lifecycle\session-handover.json', 'data\lifecycle\session-handover-required.json')
  Write-FixtureJson (Join-Path $oldRuntime $handover[0]) ([ordered]@{ sourceLayout = 'legacy-deployment' })
  Write-FixtureJson (Join-Path $oldRuntime $handover[1]) ([ordered]@{ rustOwns = $true; legacyRoot = (Join-Path $F 'legacy') })
  Write-FixtureJson (Join-Path $oldRuntime $handover[2]) ([ordered]@{ v = 1 })
  try { $run = Invoke-MoveScript ($moveArgs + @('-RehearsalAbortAfter', 'Copied')) 'S3c' }
  finally { foreach ($name in $handover) { Remove-Item -LiteralPath (Join-Path $oldRuntime $name) -Force -ErrorAction SilentlyContinue } }
  Assert-Run $run 4 'REHEARSAL ABORT after Copied' 'S3c'
  foreach ($name in $handover) { Assert-True (-not (Test-Path -LiteralPath (Join-Path $root ('runtime\' + $name)))) ('S3c: the copied runtime kept ' + $name) }
  $state = Get-LatestState
  $hold = [HerMove67RehearsalNative]::HoldDirectory((Join-Path $root 'runtime'))
  try { $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Rollback', '-StateFile', $state)) 'S3c-held' } finally { $hold.Dispose() }
  Assert-Run $run 3 'THE OLD PRODUCTION RUNS AGAIN' 'S3c held folder'
  Assert-True ($run.Text.IndexOf('could not be moved aside', [StringComparison]::OrdinalIgnoreCase) -ge 0) 'S3c: the move-aside problem is not named'
  Assert-True ($run.Text.IndexOf('possibly QQ', [StringComparison]::OrdinalIgnoreCase) -ge 0) 'S3c: the move-aside hint is missing'
  # The copied runtime stayed in the root and the root's original access is back: the runtime must be closed on its own.
  Assert-Protected (Join-Path $root 'runtime') 'S3c: the leftover runtime'
  Assert-RootAcl 'S3c held'
  Wait-For { Test-OldUp } 'S3c: the old production runs while the rollback is incomplete' 60
  # The relay fails before the rerun while the old supervisor stays up: the rerun must start the relay task alone.
  Stop-FixtureListener $relayPort
  Wait-For { $null -eq (Get-Listener $relayPort) } 'S3c: the relay stops' 30
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Rollback', '-StateFile', $state)) 'S3c-rollback'
  Assert-Run $run 0 'ROLLED BACK' 'S3c rollback'
  Assert-OldRunning 'S3c'

  Step 'S3d reboot after the old tasks were disabled'
  # The move stops after disabling both old tasks; a reboot then ends every old process and nothing starts them.
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalAbortAfter', 'TasksDisabled')) 'S3d'
  Assert-Run $run 4 'REHEARSAL ABORT after TasksDisabled' 'S3d'
  $owners = Get-Owners $oldRuntime $unused $cli
  foreach ($processId in @($owners.supervisorPid, $owners.livePid)) { if ($processId -gt 0) { Stop-FixtureProcess $processId } }
  foreach ($port in @($consolePort, $onebotPort, $dshPort, $relayPort)) { Stop-FixtureListener $port }
  Wait-For { $null -eq (Get-Listener $onebotPort) -and $null -eq (Get-Listener $relayPort) -and $null -eq (Get-Listener $dshPort) -and $null -eq (Get-Listener $consolePort) } 'S3d: the simulated reboot ends the old production' 30
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Rollback', '-StateFile', (Get-LatestState))) 'S3d-rollback'
  Assert-Run $run 0 'ROLLED BACK' 'S3d rollback'
  Assert-Run $run 0 'the old production runs again' 'S3d old production started'
  Assert-OldRunning 'S3d'

  Step 'S4 interrupted after registration, then a logon starts HER'
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalAbortAfter', 'Registered')) 'S4'
  Assert-Run $run 4 'REHEARSAL ABORT after Registered' 'S4'
  $task = Invoke-Her @('scheduled-task', 'read', '--install-root', $root) | ConvertFrom-Json
  Assert-True ($null -ne $task.xml) 'S4: the HER task is not registered at the abort point'
  Start-Process -FilePath (Join-Path $root 'current\her.exe') -ArgumentList ('supervise --runtime-root "' + (Join-Path $root 'runtime') + '" --install-root "' + $root + '"') -WorkingDirectory (Join-Path $root 'current') -WindowStyle Hidden
  Wait-For { (Get-Owners (Join-Path $root 'runtime') $root (Join-Path $root 'current\her-cli.exe')).supervisor -eq 'held' } 'S4: the new supervisor a logon would start' 60
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Rollback', '-StateFile', (Get-LatestState))) 'S4-rollback'
  Assert-Run $run 0 'ROLLED BACK' 'S4 rollback'
  Assert-OldRunning 'S4'

  Step 'S5 move, confirmed with the right word'
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalConfirm', '-DigestModel', 'fixture-digest-model')) 'S5' 'MOVE'
  Assert-Run $run 0 'MOVE DONE' 'S5'
  Assert-Run $run 0 'Type MOVE to go on' 'S5 confirmation prompt'
  Assert-Run $run 0 'linked 2 DSH bundles in the new DSH home' 'S5 bundle links'
  Assert-Run $run 0 'live: every new owner is held and the new live bridge publishes its heartbeat' 'S5 live owner'
  Assert-Moved 'S5' -Digest
  $doneState = Get-LatestState

  Step 'S6 rollback after a finished move'
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Rollback', '-StateFile', $doneState)) 'S6-refused'
  Assert-Run $run 1 'pass -AfterDone' 'S6 refusal'
  # The new DSH starts a child the new runtime's own stop does not end; the rollback must find and stop it.
  [void](Invoke-WebRequest -UseBasicParsing -Uri ('http://127.0.0.1:' + $dshPort + '/fixture/spawn-child') -TimeoutSec 10)
  Wait-For { @(Get-LateChildren).Count -ge 1 } 'S6: the new DSH child starts' 30
  $run = Invoke-MoveScript ($rehearsalArgs + @('-Mode', 'Rollback', '-StateFile', $doneState, '-AfterDone')) 'S6'
  Assert-Run $run 0 'ROLLED BACK' 'S6'
  Assert-True (@(Get-LateChildren).Count -eq 0) 'S6: a child of the new DSH survived the rollback'
  Assert-OldRunning 'S6' -Restricted

  Step 'S7 move again after rollbacks; DSH starts a child after the checks'
  $run = Invoke-MoveScript ($moveArgs + @('-RehearsalFault', 'LateDshChild')) 'S7'
  Assert-Run $run 0 'MOVE DONE' 'S7'
  Assert-True (@(Get-LateChildren).Count -eq 0) 'S7: a DSH child started after the checks survived the stop'
  Assert-Moved 'S7'

  Step 'S8 operator folder'
  $operatorArgs = $rehearsalArgs + @('-Mode', 'OperatorFolders', '-OperatorSource', $opsSource)
  # A copy that fails its hash check leaves neither ops nor its staging folder, so the retry below can run.
  $run = Invoke-MoveScript ($operatorArgs + @('-RehearsalFault', 'CorruptCopy')) 'S8-corrupt'
  Assert-Run $run 1 'differs from its source' 'S8 failed copy'
  Assert-True (-not (Test-Path -LiteralPath (Join-Path $root 'ops')) -and -not (Test-Path -LiteralPath (Join-Path $root '.ops-copy'))) 'S8: a failed copy left an ops folder behind'
  $run = Invoke-MoveScript $operatorArgs 'S8'
  Assert-Run $run 0 'OPERATOR FOLDER COPIED' 'S8'
  Assert-True ((Get-FileHashText (Join-Path $root 'ops\fixture-op.ps1')) -eq (Get-FileHashText (Join-Path $opsSource 'fixture-op.ps1'))) 'S8: the ops copy differs'
  $run = Invoke-MoveScript $operatorArgs 'S8-again'
  Assert-Run $run 1 'the install root already has ops' 'S8 second copy'

  $passed = $true
  Write-Host ('PASS production move rehearsal: ' + $scenarios + ' scenarios')
} catch {
  Write-Host $_.Exception.Message
} finally {
  $newCli = Join-Path $root 'current\her-cli.exe'
  if (Test-Path -LiteralPath $newCli -PathType Leaf) {
    try {
      [void](Invoke-Her @('scheduled-task', 'remove', '--install-root', $root) @(0, 1, 2, 3) $newCli)
      [void](Invoke-Her @('stop', '--runtime-root', (Join-Path $root 'runtime'), '--confirm', '--with-dependencies', '--install-root', $root) @(0, 1, 2, 3) $newCli)
      [void](Invoke-Her @('lifecycle', 'stop-supervisor', '--runtime-root', (Join-Path $root 'runtime'), '--confirm', '--install-root', $root) @(0, 1, 2, 3) $newCli)
    } catch { Write-Warning ('Cleanup of the new runtime: ' + $_.Exception.Message) }
  }
  if (Test-Path -LiteralPath $oldCli -PathType Leaf) {
    try {
      # The old owners accept a stop only from their own image or its twin: the fixture's old her-cli.exe.
      [void](Invoke-Her @('stop', '--runtime-root', $oldRuntime, '--confirm', '--with-dependencies', '--install-root', $unused) @(0, 1, 2, 3) $oldCli)
      [void](Invoke-Her @('lifecycle', 'stop-supervisor', '--runtime-root', $oldRuntime, '--confirm', '--install-root', $unused) @(0, 1, 2, 3) $oldCli)
    } catch { Write-Warning ('Cleanup of the old runtime: ' + $_.Exception.Message) }
  }
  foreach ($name in $registered) {
    Stop-ScheduledTask -TaskName $name -TaskPath '\' -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $name -TaskPath '\' -Confirm:$false -ErrorAction SilentlyContinue
  }
  foreach ($port in @($relayPort, $onebotPort, $dshPort, $consolePort)) {
    try { Stop-FixtureListener $port } catch { Write-Warning ('Cleanup of a fixture port: ' + $_.Exception.Message) }
  }
  # A late DSH child the move missed: the fixture's own marker process, stopped by its verified identity.
  foreach ($child in @(Get-LateChildren)) {
    try {
      $process = Get-Process -Id ([int]$child.ProcessId) -ErrorAction SilentlyContinue
      if ($null -ne $process -and [string]$child.ExecutablePath -ieq $NodePath) {
        $start = [uint64]$process.StartTime.ToUniversalTime().ToFileTimeUtc()
        [void][HerMove67RehearsalNative]::Stop([uint32]$child.ProcessId, $start, [string]$child.ExecutablePath)
      }
    } catch { Write-Warning ('Cleanup of a late DSH child: ' + $_.Exception.Message) }
  }
  Remove-Item -Path Env:HER_TASK_SCHEDULER_FILE -ErrorAction SilentlyContinue
  if ($savedScheduler) { $env:HER_TASK_SCHEDULER_FILE = $savedScheduler }
  if ($savedInstallRoot) { $env:HER_INSTALL_ROOT = $savedInstallRoot }
  if ($passed -and -not $KeepFixture) {
    if (-not (Remove-FixtureTree $F)) { Write-Warning 'The fixture still holds a junction or link; it was kept. Delete its links as links, then the folder.' }
  }
}
if (-not $passed) { exit 1 }
exit 0

# Windows-only isolated lifecycle fixture. It never names an existing QQBot task or installation.
[CmdletBinding()]
param(
  [Parameter(Mandatory=$true)][string]$Candidate,
  [Parameter(Mandatory=$true)][string]$FixtureRoot,
  [Parameter(Mandatory=$true)][string]$NodePath,
  [string]$DigestSupportRoot = '',
  [string]$FixtureTaskName = '',
  [switch]$RunLevelHighest
)
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'This fixture requires Windows.' }
$callerIdentity = [Security.Principal.WindowsIdentity]::GetCurrent()
$callerElevated = ([Security.Principal.WindowsPrincipal]::new($callerIdentity)).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($RunLevelHighest -and -not $callerElevated) {
  throw '-RunLevelHighest requires an elevated PowerShell caller (Run as administrator); no fixture task was registered.'
}
$callerSession = (Get-Process -Id $PID).SessionId
if ($RunLevelHighest -and $callerSession -eq 0) { throw '-RunLevelHighest requires an interactive user session.' }
$inheritedUsername = $env:USERNAME
$Candidate = [IO.Path]::GetFullPath($Candidate)
$FixtureRoot = [IO.Path]::GetFullPath($FixtureRoot)
$NodePath = [IO.Path]::GetFullPath($NodePath)
$temp = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
if (-not $FixtureRoot.StartsWith($temp, [StringComparison]::OrdinalIgnoreCase) -or
    (Test-Path -LiteralPath $FixtureRoot) -or -not (Test-Path -LiteralPath $Candidate -PathType Leaf) -or
    -not (Test-Path -LiteralPath $NodePath -PathType Leaf)) {
  throw 'FixtureRoot must be a new directory beneath the current temporary directory; Candidate and NodePath must be files.'
}
$taskName = if ($FixtureTaskName) { $FixtureTaskName } else { 'QQBot-Fixture-' + [guid]::NewGuid().ToString('N') }
if ($taskName -cnotmatch '^QQBot-Fixture-[0-9a-f]{32}$') { throw 'Foreign fixture task name refused.' }
$runtime = Join-Path $FixtureRoot 'runtime'
. (Join-Path $PSScriptRoot 'fixture-paths.ps1')
Assert-HerDevelopmentRoot -Path $FixtureRoot
$target = Join-Path $FixtureRoot 'her\target\release\her.exe'
$source = Split-Path -Parent $PSScriptRoot
$registered = $false
$script:supervisorPid = 0
$observer = $null
$observerDone = Join-Path $FixtureRoot 'observer.done'
$observerReady = Join-Path $FixtureRoot 'observer.ready'
$observerResult = Join-Path $FixtureRoot 'observer.json'

function Invoke-Bridge([string[]]$Arguments) {
  $output = & $target @Arguments | Out-String
  if ($LASTEXITCODE -ne 0) { throw "her exited $LASTEXITCODE for $($Arguments[0])" }
  return ($output -join "`n")
}
function Get-Status {
  return (Invoke-Bridge @('lifecycle','status','--runtime-root',$runtime) | ConvertFrom-Json)
}
function Wait-For([scriptblock]$Predicate, [string]$Description, [int]$Seconds = 30) {
  $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
  while ([DateTime]::UtcNow -lt $deadline) {
    if (& $Predicate) { return }
    Start-Sleep -Milliseconds 100
  }
  throw "Timed out: $Description"
}
if (-not ('QQBotFixturePinnedProcess' -as [type])) {
  Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
public static class QQBotFixturePinnedProcess {
  [StructLayout(LayoutKind.Sequential)] struct FILETIME { public uint low, high; }
  [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetProcessTimes(IntPtr h, out FILETIME creation, out FILETIME exit, out FILETIME kernel, out FILETIME user);
  [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern bool QueryFullProcessImageName(IntPtr h, uint flags, StringBuilder path, ref uint size);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool TerminateProcess(IntPtr h, uint code);
  [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
  public static string Stop(uint pid, ulong expectedStart, string expectedExe) {
    IntPtr h=OpenProcess(0x1000 | 0x0001 | 0x00100000, false, pid);
    if (h==IntPtr.Zero) return Marshal.GetLastWin32Error()==87 ? "gone" : "unknown";
    try {
      FILETIME c,e,k,u;
      if (!GetProcessTimes(h,out c,out e,out k,out u)) return "unknown";
      ulong actual=((ulong)c.high<<32)|c.low;
      var path=new StringBuilder(32768); uint size=(uint)path.Capacity;
      if (!QueryFullProcessImageName(h,0,path,ref size)) return "unknown";
      if (actual!=expectedStart || !String.Equals(Path.GetFullPath(path.ToString()),Path.GetFullPath(expectedExe),StringComparison.OrdinalIgnoreCase)) return "mismatch";
      if (!TerminateProcess(h,1)) return "unknown";
      return WaitForSingleObject(h,10000)==0 ? "stopped" : "unknown";
    } finally { CloseHandle(h); }
  }
}
'@
}
function Stop-VerifiedOwner($Owner) {
  if ($Owner.state -cne 'held' -or -not $Owner.owner.identity) { return }
  $identity = $Owner.owner.identity
  if ([int]$identity.pid -le 0 -or [uint64]$identity.start -eq 0 -or
      [IO.Path]::GetFullPath([string]$identity.exe) -ine [IO.Path]::GetFullPath($target)) {
    throw 'Refusing an incomplete or foreign fixture identity.'
  }
  $result = [QQBotFixturePinnedProcess]::Stop([uint32]$identity.pid, [uint64]$identity.start, [string]$identity.exe)
  if ($result -notin @('stopped','gone')) { throw "Fixture owner could not be safely stopped: $result" }
}
function Stop-FixtureResident([int]$Port, [string]$Program, [string]$Script) {
  $connections = @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue)
  if ($connections.Count -eq 0) { return }
  $ids = @($connections | ForEach-Object { [int]$_.OwningProcess } | Sort-Object -Unique)
  if ($ids.Count -ne 1 -or $ids[0] -le 0) { throw 'Fixture listener ownership is not unique.' }
  $process = Get-Process -Id $ids[0] -ErrorAction Stop
  $start = [uint64]$process.StartTime.ToUniversalTime().ToFileTimeUtc()
  if ([IO.Path]::GetFullPath($process.Path) -ine [IO.Path]::GetFullPath($Program)) { throw 'Fixture listener executable differs.' }
  $classified = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId=$($ids[0])" -ErrorAction Stop
  if (-not $classified.CommandLine -or $classified.CommandLine.IndexOf($Script, [StringComparison]::OrdinalIgnoreCase) -lt 0 -or $process.HasExited) {
    throw 'Fixture listener command line cannot be verified.'
  }
  $result = [QQBotFixturePinnedProcess]::Stop([uint32]$ids[0], $start, $Program)
  if ($result -notin @('stopped','gone')) { throw "Fixture resident could not be safely stopped: $result" }
}

try {
  if ($RunLevelHighest) { $env:USERNAME = $callerIdentity.Name.Split('\')[-1] }
  foreach ($dir in @('runtime\config','runtime\data','runtime\resources','runtime\user\dsh','her\target\release','snow','dsh')) {
    New-Item -ItemType Directory -Path (Join-Path $FixtureRoot $dir) -Force | Out-Null
  }
  Copy-Item -LiteralPath $Candidate -Destination $target
  $consoleTwin = Join-Path ([IO.Path]::GetDirectoryName($Candidate)) 'her-cli.exe'
  if (-not (Test-Path -LiteralPath $consoleTwin -PathType Leaf)) { throw 'Fixture candidate requires its sibling her-cli.exe.' }
  Copy-Item -LiteralPath $consoleTwin -Destination (Join-Path ([IO.Path]::GetDirectoryName($target)) 'her-cli.exe')
  Copy-Item -LiteralPath (Join-Path $source 'external-path.ps1') -Destination $FixtureRoot
  Copy-Item -LiteralPath (Join-Path $source 'scripts\lifecycle-window-observer.ps1') -Destination $FixtureRoot
  Copy-Item -LiteralPath (Join-Path $source 'scripts\fixture-paths.ps1') -Destination $FixtureRoot
  $snowDirectory = Join-Path $FixtureRoot 'snow'
  $dshDirectory = Join-Path $FixtureRoot 'dsh'
  $snowExe = Join-Path $snowDirectory 'node.exe'
  $snowScript = Join-Path $snowDirectory 'index.mjs'
  $dshScript = Join-Path $dshDirectory 'bin.mjs'
  Copy-Item -LiteralPath $NodePath -Destination $snowExe
  Copy-Item -LiteralPath (Join-Path $source 'scripts\lifecycle-fake-resident.mjs') -Destination $snowScript
  Copy-Item -LiteralPath (Join-Path $source 'scripts\lifecycle-fake-resident.mjs') -Destination $dshScript
  $listeners = @(1..3 | ForEach-Object { $item = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0); $item.Start(); $item })
  $port = $listeners[0].LocalEndpoint.Port; $snowPort = $listeners[1].LocalEndpoint.Port; $dshPort = $listeners[2].LocalEndpoint.Port
  $listeners | ForEach-Object { $_.Stop() }
  if (@($port,$snowPort,$dshPort | Where-Object { $_ -in @(3000,3001,3080,3100,5099) }).Count -gt 0) { throw 'Fixture ports overlap a reserved production port.' }
  @{ role='onebot'; port=$snowPort } | ConvertTo-Json | ForEach-Object { [IO.File]::WriteAllText((Join-Path $snowDirectory 'fixture-resident.json'), $_, [Text.UTF8Encoding]::new($false)) }
  @{ role='dsh'; port=$dshPort } | ConvertTo-Json | ForEach-Object { [IO.File]::WriteAllText((Join-Path $dshDirectory 'fixture-resident.json'), $_, [Text.UTF8Encoding]::new($false)) }
  [IO.File]::WriteAllText((Join-Path $runtime 'user\dsh\.credentials.yaml'), "secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA`nrecords:`n  client-connection/browser-session:`n    kind: grant`n    payload:`n      version: 1`n      secret: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA`n", [Text.UTF8Encoding]::new($false))
  @{ consoleToken='fixture-token-27'; consolePort=$port; ownerQQ=1; allow=@{groups=@(1);private=@()}; backends=@{mcpRustBridge=$true}; rollout=@{rustGroups=@('group:1')};
     tasks=@{installRoot=$FixtureRoot;nodePath=$NodePath;'start-all'=@{snowLumaDirectory=$snowDirectory;dshScript=$dshScript;dshWorkingDirectory=$dshDirectory}} } |
    ConvertTo-Json -Depth 8 | ForEach-Object { [IO.File]::WriteAllText((Join-Path $runtime 'config\config.json'), $_, [Text.UTF8Encoding]::new($false)) }
  @{dsh="http://127.0.0.1:$dshPort";model="http://127.0.0.1:$dshPort";onebot="http://127.0.0.1:$snowPort";onebot_ws="ws://127.0.0.1:$snowPort"} | ConvertTo-Json |
    ForEach-Object { [IO.File]::WriteAllText((Join-Path $runtime 'dependencies.json'), $_, [Text.UTF8Encoding]::new($false)) }
  '{}' | ForEach-Object { [IO.File]::WriteAllText((Join-Path $runtime 'data\social-v2.json'), $_, [Text.UTF8Encoding]::new($false)) }
  [void](Invoke-Bridge @('lifecycle','set','stopped','--confirm','--runtime-root',$runtime,'--by','fixture'))
  if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) { throw 'Fixture task name unexpectedly exists.' }
  $arguments = 'supervise --runtime-root "' + $runtime + '"'
  $action = New-ScheduledTaskAction -Execute $target -Argument $arguments -WorkingDirectory $FixtureRoot
  $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
  $principalArguments = @{}
  if ($RunLevelHighest) {
    $principalArguments.Principal = New-ScheduledTaskPrincipal -UserId $callerIdentity.Name -LogonType Interactive -RunLevel Highest
  }
  # Install no trigger until the principal has been read back and observation is ready.
  Register-ScheduledTask -TaskName $taskName -Action $action -Settings $settings @principalArguments -ErrorAction Stop | Out-Null
  $registered = $true
  $registeredPrincipal = (Get-ScheduledTask -TaskName $taskName -ErrorAction Stop).Principal
  @{
    callerElevated=$callerElevated; callerAccount=$callerIdentity.Name; callerSid=$callerIdentity.User.Value
    callerSessionId=$callerSession; inheritedUsername=$inheritedUsername; username=$env:USERNAME
    requestedHighest=[bool]$RunLevelHighest; taskName=$taskName
    principal=@{userId=[string]$registeredPrincipal.UserId; logonType=[string]$registeredPrincipal.LogonType; runLevel=[string]$registeredPrincipal.RunLevel}
    readBeforeTrigger=$true
  } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $FixtureRoot 'task-principal.json') -Encoding UTF8
  if ($RunLevelHighest) {
    $registeredUser = [string]$registeredPrincipal.UserId
    $registeredSid = if ($registeredUser -match '^S-1-') { [Security.Principal.SecurityIdentifier]::new($registeredUser).Value } else {
      [Security.Principal.NTAccount]::new($registeredUser).Translate([Security.Principal.SecurityIdentifier]).Value
    }
    if ($registeredPrincipal.RunLevel -ne 'Highest' -or $registeredPrincipal.LogonType -ne 'Interactive' -or $registeredSid -ne $callerIdentity.User.Value) {
      throw 'Registered fixture principal does not match the elevated interactive caller.'
    }
  }
  $powershell = Join-Path $env:WINDIR 'System32\WindowsPowerShell\v1.0\powershell.exe'
  $observerArgs = '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File "' + (Join-Path $FixtureRoot 'lifecycle-window-observer.ps1') +
    '" -FixtureRoot "' + $FixtureRoot + '" -Ready "' + $observerReady + '" -Done "' + $observerDone + '" -Output "' + $observerResult + '"'
  $observer = Start-Process -FilePath $powershell -ArgumentList $observerArgs -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput (Join-Path $FixtureRoot 'observer.out.txt') -RedirectStandardError (Join-Path $FixtureRoot 'observer.err.txt')
  $null = $observer.Handle
  Wait-For { Test-Path -LiteralPath $observerReady } 'window observer readiness'
  if ((Get-Status).owners.supervisor.state -ceq 'held') { throw 'Scheduled supervisor started before observation was ready.' }
  $trigger = New-ScheduledTaskTrigger -Once -At ([DateTime]::Now.AddSeconds(10))
  Set-ScheduledTask -TaskName $taskName -Trigger $trigger -ErrorAction Stop | Out-Null
  Wait-For { (Get-Status).owners.supervisor.state -ceq 'held' } 'fixture supervisor ownership'
  Get-ScheduledTaskInfo -TaskName $taskName -ErrorAction Stop | Select-Object LastRunTime,LastTaskResult | ConvertTo-Json | Set-Content (Join-Path $FixtureRoot 'scheduled-task-info.json') -Encoding UTF8
  $initial = Get-Status
  $script:supervisorPid = [int]$initial.owners.supervisor.owner.identity.pid
  if ($initial.desired.state -cne 'stopped' -or $initial.owners.live.state -cne 'free' -or $initial.owners.console.state -cne 'free') {
    throw 'Stopped fixture unexpectedly started a live or console owner.'
  }
  $duplicate = Start-Process -FilePath $target -ArgumentList @('supervise','--runtime-root',('"' + $runtime + '"')) -WindowStyle Hidden -PassThru
  $null = $duplicate.Handle
  if (-not $duplicate.WaitForExit(10000)) { $duplicate.Kill(); throw 'Second supervisor did not exit.' }
  if ($duplicate.ExitCode -eq 0) { throw 'A second supervisor acquired the same runtime.' }
  [void](Invoke-Bridge @('lifecycle','set','running','--runtime-root',$runtime,'--by','fixture'))
  Wait-For { (Get-Status).owners.console.state -ceq 'held' } 'console starts after running'
  Wait-For { (Get-Status).owners.live.state -ceq 'held' } 'live owner starts for fixture group'
  Wait-For { @(Get-NetTCPConnection -State Listen -LocalPort $dshPort -ErrorAction SilentlyContinue).Count -eq 1 } 'synthetic DSH starts'
  if ($DigestSupportRoot) {
    $curlDirectory = Join-Path $FixtureRoot 'fake-curl'
    [void][IO.Directory]::CreateDirectory($curlDirectory)
    # The MCP policy accepts this public literal, but this transport adapter never
    # connects to it: the actual system curl receives only our loopback fake URL.
    Add-Type -OutputType ConsoleApplication -OutputAssembly (Join-Path $curlDirectory 'curl.exe') -Path (Join-Path $source 'scripts\lifecycle-curl-adapter.cs')
    Copy-Item -LiteralPath (Join-Path $curlDirectory 'curl.exe') -Destination (Join-Path (Split-Path -Parent $target) 'curl.exe')
    $extraArguments = '"' + (Join-Path $source 'scripts\lifecycle-extra-chains.mjs') + '" "' + $target + '" "' + $runtime + '" "' + $FixtureRoot + '" "' + $DigestSupportRoot + '" ' + $port
    $extra = Start-Process $NodePath -ArgumentList $extraArguments -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $FixtureRoot 'chains.out.log') -RedirectStandardError (Join-Path $FixtureRoot 'chains.err.log')
    $null=$extra.Handle
    if (-not $extra.WaitForExit(60000)) { $extra.Kill(); $extra.WaitForExit(); throw 'Extended chains timed out.' }
    if ($extra.ExitCode -ne 0) { throw 'Extended MCP/digest chains failed; inspect chains.err.log.' }
    $headers = @{'x-console-token'='fixture-token-27'}
    $manual = Invoke-RestMethod -Method Post -Uri ("http://127.0.0.1:$port/api/tasks/watchdog/run") -Headers $headers -ContentType 'application/json' -Body '{"requestId":"release-proof-manual"}'
    Wait-For { @((Invoke-Bridge @('tasks','runs','--runtime-root',$runtime) | ConvertFrom-Json).runs | Where-Object { $_.taskId -eq 'watchdog' -and $_.requestId -eq 'release-proof-manual' -and $_.source -eq 'manual' -and $_.state -eq 'succeeded' }).Count -gt 0 } 'manual watchdog success'
  }
  Start-Sleep -Seconds 3 # allow the supervisor to observe the healthy listener before fault injection
  $oldDsh = @(Get-NetTCPConnection -State Listen -LocalPort $dshPort -ErrorAction Stop)[0].OwningProcess
  Stop-FixtureResident $dshPort $NodePath $dshScript
  Wait-For {
    $listeners = @(Get-NetTCPConnection -State Listen -LocalPort $dshPort -ErrorAction SilentlyContinue)
    $listeners.Count -eq 1 -and $listeners[0].OwningProcess -ne $oldDsh
  } 'synthetic DSH recovers after failure' 90
  if ($DigestSupportRoot) {
    $runs = Invoke-Bridge @('tasks','runs','--runtime-root',$runtime) | ConvertFrom-Json
    $dependencyState = Get-Content -LiteralPath (Join-Path $runtime 'data/lifecycle/dependencies.json') -Raw | ConvertFrom-Json
    $replacementDsh = @(Get-NetTCPConnection -State Listen -LocalPort $dshPort -ErrorAction Stop)[0].OwningProcess
    if ($dependencyState.dsh.state -ne 'started' -or $dependencyState.dsh.identity.pid -ne $replacementDsh -or $replacementDsh -eq $oldDsh) { throw 'No successful native dependency recovery was recorded.' }
    $dependencyState | ConvertTo-Json -Depth 10 | Set-Content (Join-Path $FixtureRoot 'dependency-state.json') -Encoding UTF8
    $runs | ConvertTo-Json -Depth 15 | Set-Content (Join-Path $FixtureRoot 'task-runs.json') -Encoding UTF8
    $chains = Get-Content (Join-Path $FixtureRoot 'chains.json') -Raw | ConvertFrom-Json
    $chains | Add-Member -NotePropertyName manual -NotePropertyValue $true
    $chains | Add-Member -NotePropertyName recovery -NotePropertyValue $true
    $chains | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $FixtureRoot 'chains.json') -Encoding UTF8
  }
  [void](Invoke-Bridge @('lifecycle','set','stopped','--confirm','--runtime-root',$runtime,'--by','fixture'))
  Wait-For { (Get-Status).owners.console.state -ceq 'free' } 'console stops after stopped'
  Wait-For { (Get-Status).owners.live.state -ceq 'free' } 'live owner stops after stopped'
  $status = Get-Status
  $status | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $FixtureRoot 'result.json') -Encoding UTF8
  [IO.File]::WriteAllText($observerDone, 'done')
  if (-not $observer.WaitForExit(15000)) { throw 'Window observer did not finish.' }
  if ($observer.ExitCode -ne 0) { throw 'A fixture window or foreground owner was observed, or window sampling failed.' }
  Write-Host "PASS synthetic Task Scheduler, live owner, DSH recovery and sampled windows: $taskName ($FixtureRoot)"
} finally {
  if ($observer -and -not $observer.HasExited) {
    [IO.File]::WriteAllText($observerDone, 'done')
    if (-not $observer.WaitForExit(15000)) { $observer.Kill(); [void]$observer.WaitForExit(5000) }
  }
  if ($registered) {
    Stop-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
  }
  if (Test-Path -LiteralPath $target -PathType Leaf) {
    try {
      [void](Invoke-Bridge @('lifecycle','set','stopped','--confirm','--runtime-root',$runtime,'--by','fixture-cleanup'))
      $last = Get-Status
      Stop-VerifiedOwner $last.owners.console
      Stop-VerifiedOwner $last.owners.live
      Stop-VerifiedOwner $last.owners.supervisor
    } catch { Write-Warning "Fixture cleanup could not verify an owner: $($_.Exception.Message)" }
    try { if ($dshPort -and $dshScript) { Stop-FixtureResident $dshPort $NodePath $dshScript } }
    catch { Write-Warning "Fixture cleanup could not verify DSH: $($_.Exception.Message)" }
    try { if ($snowPort -and $snowScript) { Stop-FixtureResident $snowPort $snowExe $snowScript } }
    catch { Write-Warning "Fixture cleanup could not verify OneBot: $($_.Exception.Message)" }
  }
  if ($RunLevelHighest) { $env:USERNAME = $inheritedUsername }
}

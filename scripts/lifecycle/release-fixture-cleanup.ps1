# Only GUID fixture tasks and processes selected under this disposable root.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$FixtureRoot, [Parameter(Mandatory)][string]$Output, [string]$TaskNames = '')
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath($FixtureRoot).TrimEnd('\')
. (Join-Path $PSScriptRoot 'fixture-paths.ps1')
Assert-HerDevelopmentRoot -Path $root
Assert-HerDevelopmentRoot -Path $Output
if ([IO.Path]::GetDirectoryName($root) -ine [IO.Path]::GetTempPath().TrimEnd('\') -or
    [IO.Path]::GetFileName($root) -notlike 'relproof-*') { throw 'Not a disposable release fixture root.' }
$tasks = @($TaskNames -split ',' | Where-Object { $_ })
foreach ($name in $tasks) {
  if ($name -cnotmatch '^QQBot-Fixture-[0-9a-f]{32}$') { throw 'Foreign task name refused.' }
}
$remainingTasks = @(); $remainingProcesses = @(); $failures = @()
function Test-MissingTask($Failure) {
  $exception = $Failure.Exception
  while ($exception.InnerException) { $exception = $exception.InnerException }
  return $exception.HResult -eq -2147024894
}
try {
  if ($tasks.Count) {
    $service = New-Object -ComObject 'Schedule.Service'
    $service.Connect()
    $folder = $service.GetFolder('\')
    foreach ($name in $tasks) {
      try { $task = $folder.GetTask($name) }
      catch { if (Test-MissingTask $_) { continue }; throw }
      $owned = $false
      foreach ($action in $task.Definition.Actions) {
        if ($action.Arguments -match ([regex]::Escape($root) + '(?=[\\/"\s]|$)') -or
            $action.WorkingDirectory -ieq $root) { $owned = $true }
      }
      if (-not $owned) { throw 'Fixture task does not refer to the selected temporary root.' }
      if ($task.GetInstances(0).Count) { $task.Stop(0) }
      $folder.DeleteTask($name, 0)
      try { [void]$folder.GetTask($name); $remainingTasks += $name }
      catch { if (-not (Test-MissingTask $_)) { throw } }
    }
  }
  # Match only the unique root plus a path/argument boundary. Pin and compare
  # creation time before termination; a recycled PID never authorizes a stop.
  $filter = "CommandLine LIKE '%" + $root.Replace('\','\\').Replace("'","''") + "%'"
  $pattern = [regex]::Escape($root) + '(?=[\\/"\s]|$)'
  Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Threading;
public sealed class ReleaseCleanupJob : IDisposable {
  [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr CreateJobObject(IntPtr attributes, string name);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool TerminateJobObject(IntPtr job, uint code);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool SetInformationJobObject(IntPtr job, int kind, byte[] info, uint length);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool QueryInformationJobObject(IntPtr job, int kind, byte[] info, uint length, IntPtr returned);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  IntPtr job = CreateJobObject(IntPtr.Zero, null);
  public ReleaseCleanupJob() {
    if (job == IntPtr.Zero) throw new Win32Exception();
    byte[] info = new byte[IntPtr.Size == 8 ? 144 : 112];
    info[17] = 0x20; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE at LimitFlags byte 16
    if (!SetInformationJobObject(job, 9, info, (uint)info.Length)) { CloseHandle(job); throw new Win32Exception(); }
  }
  public void Add(IntPtr process) { if (!AssignProcessToJobObject(job, process)) throw new Win32Exception(); }
  public void Stop() {
    if (!TerminateJobObject(job, 124)) throw new Win32Exception();
    for (int i=0; i<500; i++) {
      byte[] info = new byte[48];
      if (!QueryInformationJobObject(job, 1, info, 48, IntPtr.Zero)) throw new Win32Exception();
      if (BitConverter.ToUInt32(info, 40) == 0) return;
      Thread.Sleep(20);
    }
    throw new Exception("Fixture descendants still active in cleanup job");
  }
  public void Dispose() { CloseHandle(job); }
}
'@
  function Add-PinnedChild($Item, $Pinned, $Job) {
    $child = $null
    try {
      try { $child = [Diagnostics.Process]::GetProcessById($Item.ProcessId); [void]$child.Handle }
      catch [ArgumentException] { return }
      if ($child.HasExited) { return }
      if (-not $Item.CreationDate -or [Math]::Abs(($child.StartTime.ToUniversalTime() - $Item.CreationDate.ToUniversalTime()).Ticks) -gt 10) {
        throw 'Fixture process creation identity mismatch.'
      }
      $Job.Add($child.Handle)
      $Pinned[[int]$Item.ProcessId] = $child
      $child = $null
    } finally { if ($child) { $child.Dispose() } }
  }
  for ($round = 0; $round -lt 5; $round++) {
    $remainingProcesses = @()
    $pinned = @{}; $job = [ReleaseCleanupJob]::new()
    try {
      foreach ($item in @(Get-CimInstance Win32_Process -Filter $filter -ErrorAction Stop)) {
        if ($item.ProcessId -ne $PID -and $item.CommandLine -match $pattern) { Add-PinnedChild $item $pinned $job }
      }
      # Existing descendants need explicit enrollment; later children inherit the
      # job. Read only pid/parent/start for unrelated host processes.
      $changed = $true
      while ($changed) {
        $changed = $false
        foreach ($item in @(Get-CimInstance -Query 'SELECT ProcessId,ParentProcessId,CreationDate FROM Win32_Process' -ErrorAction Stop)) {
          if ($item.ProcessId -eq $PID -or $pinned.ContainsKey([int]$item.ProcessId) -or -not $pinned.ContainsKey([int]$item.ParentProcessId)) { continue }
          if ($item.CreationDate.ToUniversalTime() -lt $pinned[[int]$item.ParentProcessId].StartTime.ToUniversalTime().AddTicks(-10)) { continue }
          Add-PinnedChild $item $pinned $job
          if ($pinned.ContainsKey([int]$item.ProcessId)) { $changed = $true }
        }
      }
      if ($round -lt 4) { $job.Stop() }
      foreach ($child in $pinned.Values) {
        if (-not $child.WaitForExit(5000)) { $remainingProcesses += $child.Id }
      }
    } finally {
      $job.Dispose()
      foreach ($child in $pinned.Values) { $child.Dispose() }
    }
    if (-not $remainingProcesses.Count) { break }
  }
} catch { $failures += $_.Exception.Message; throw }
finally {
  @{ remainingTasks=@($remainingTasks); remainingProcesses=@($remainingProcesses); errors=@($failures) } |
    ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $Output -Encoding UTF8
}
if ($remainingTasks.Count -or $remainingProcesses.Count) { throw 'Fixture cleanup incomplete.' }

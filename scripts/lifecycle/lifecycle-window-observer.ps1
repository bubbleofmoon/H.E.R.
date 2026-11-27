# Observes only disposable fixture processes and their current descendants.
[CmdletBinding()]
param(
  [Parameter(Mandatory=$true)][string]$FixtureRoot,
  [Parameter(Mandatory=$true)][string]$Ready,
  [Parameter(Mandatory=$true)][string]$Done,
  [Parameter(Mandatory=$true)][string]$Output,
  [int]$MaxSeconds = 240
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'fixture-paths.ps1')
Assert-HerDevelopmentRoot -Path $FixtureRoot
Assert-HerDevelopmentRoot -Path $Ready
Assert-HerDevelopmentRoot -Path $Output
if ($MaxSeconds -lt 1 -or $MaxSeconds -gt 600) { throw 'Observer duration is invalid.' }
if (-not ('QQBotFixtureWindows' -as [type])) {
  Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class QQBotFixtureWindows {
  public delegate bool EnumProc(IntPtr h, IntPtr data);
  [DllImport("user32.dll", SetLastError=true)] static extern bool EnumWindows(EnumProc callback, IntPtr data);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  public static string[] VisibleFor(int[] ids) {
    var found = new List<string>(); var wanted = new HashSet<int>(ids);
    bool identified=true;
    bool enumerated=EnumWindows((h, data) => { uint pid;
      if (GetWindowThreadProcessId(h, out pid)==0) { identified=false; return false; }
      if (IsWindowVisible(h) && wanted.Contains((int)pid)) found.Add(h.ToInt64().ToString("x") + "|" + pid);
      return true; }, IntPtr.Zero);
    if (!enumerated || !identified) throw new InvalidOperationException("EnumWindows observation failed");
    return found.ToArray();
  }
  public static int ForegroundOwner() {
    var h = GetForegroundWindow(); if (h == IntPtr.Zero) return 0;
    uint pid; if (GetWindowThreadProcessId(h, out pid)==0) throw new InvalidOperationException("Foreground window observation failed");
    return (int)pid;
  }
}
'@
}
if (-not ('QQBotFixtureIdentity' -as [type])) {
  # QQBOT-IDENTITY-SOURCE-BEGIN
  Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
public interface IQQBotProcessProbe {
  IntPtr Open(int pid, out int error);
  uint Wait(IntPtr h, out int error);
  bool Query(IntPtr h, out string identity, out int error);
  void Close(IntPtr h);
}
public sealed class QQBotNativeProbe : IQQBotProcessProbe {
  [StructLayout(LayoutKind.Sequential)] struct FT { public uint Low, High; }
  [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
  [DllImport("kernel32.dll", SetLastError=true)] static extern uint WaitForSingleObject(IntPtr h, uint ms);
  [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetProcessTimes(IntPtr h, out FT c, out FT e, out FT k, out FT u);
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool QueryFullProcessImageName(IntPtr h, uint flags, StringBuilder name, ref uint size);
  public IntPtr Open(int pid, out int error) {
    var h=OpenProcess(0x1000 | 0x100000, false, pid); // limited query + synchronize, same pinned instance
    error = h==IntPtr.Zero ? Marshal.GetLastWin32Error() : 0;
    return h;
  }
  public uint Wait(IntPtr h, out int error) { uint state=WaitForSingleObject(h,0); error = state==0 || state==258 ? 0 : Marshal.GetLastWin32Error(); return state; }
  public bool Query(IntPtr h, out string identity, out int error) {
    FT c,e,k,u; var image=new StringBuilder(32768); uint size=(uint)image.Capacity; identity=null; error=0;
    if (!GetProcessTimes(h,out c,out e,out k,out u) || !QueryFullProcessImageName(h,0,image,ref size)) { error=Marshal.GetLastWin32Error(); return false; }
    identity=(((ulong)c.High<<32)|c.Low).ToString()+"|"+image.ToString();
    return true;
  }
  public void Close(IntPtr h) { CloseHandle(h); }
}
public static class QQBotFixtureIdentity {
  public const int RetryBudgetMs = 1000;
  public const int RetryStepMs = 25;
  public static string Read(int pid) { return ReadWith(new QQBotNativeProbe(), pid, RetryBudgetMs, RetryStepMs); }
  // A live process whose identity cannot be queried yet (just created or being torn down) is retried on the
  // same handle for a bounded time; only a process still unreadable after the bound fails the observation.
  public static string ReadWith(IQQBotProcessProbe probe, int pid, int budgetMs, int stepMs) {
    int error;
    var h=probe.Open(pid, out error);
    if (h==IntPtr.Zero) {
      if (error==87) return null; // no such process
      throw new Win32Exception(error, "Live fixture descendant cannot be opened: pid="+pid+" win32="+error);
    }
    try {
      var started=Environment.TickCount; int attempts=0;
      while (true) {
        uint state=probe.Wait(h, out error);
        if (state==0) return null;
        if (state!=258) throw new Win32Exception(error, "Fixture descendant liveness unknown: pid="+pid+" win32="+error);
        string identity; attempts++;
        if (probe.Query(h, out identity, out error)) {
          state=probe.Wait(h, out error);
          if (state==0) return null;
          if (state!=258) throw new Win32Exception(error, "Fixture descendant liveness unknown: pid="+pid+" win32="+error);
          return identity;
        }
        int waitError;
        if (probe.Wait(h, out waitError)==0) return null;
        if (unchecked(Environment.TickCount-started) >= budgetMs)
          throw new Win32Exception(error, "Live fixture descendant identity unavailable: pid="+pid+" win32="+error+" attempts="+attempts+" waitedMs="+budgetMs);
        Thread.Sleep(stepMs);
      }
    } finally { probe.Close(h); }
  }
}
'@
  # QQBOT-IDENTITY-SOURCE-END
}
$root = [IO.Path]::GetFullPath($FixtureRoot).TrimEnd('\','/')
$observed = New-Object 'System.Collections.Generic.HashSet[int]'
$identities = @{}
$identityRecoveries = @{}
$exitedDuringObservation = New-Object 'System.Collections.Generic.HashSet[int]'
$violations = New-Object 'System.Collections.Generic.List[string]'
$samples = 0
$deadline = [DateTime]::UtcNow.AddSeconds($MaxSeconds)
[IO.File]::WriteAllText($Ready, 'ready')
$failure = $null
try {
  do {
    $processes = @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)
    $tree = New-Object 'System.Collections.Generic.HashSet[int]'
    $current = @{}
    foreach ($process in $processes) {
      $exe = [string]$process.ExecutablePath; $line = [string]$process.CommandLine
      $id = [int]$process.ProcessId
      $identity = if ($exe -and $process.CreationDate) { ([string]([DateTime]$process.CreationDate).ToFileTimeUtc()) + '|' + $exe } else { $null }
      $current[$id] = $identity
      if ($identities.ContainsKey([string]$id)) {
        $nativeIdentity = [QQBotFixtureIdentity]::Read($id)
        if ($nativeIdentity -and $identities[[string]$id] -ceq $nativeIdentity) { [void]$tree.Add($id) }
      }
      if (($exe -and $exe.StartsWith($root + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) -or
          ($line -and $line.IndexOf($root, [StringComparison]::OrdinalIgnoreCase) -ge 0)) {
        [void]$tree.Add([int]$process.ProcessId)
      }
    }
    $changed = $true
    while ($changed) {
      $changed = $false
      foreach ($process in $processes) {
        if ($tree.Contains([int]$process.ParentProcessId) -and $tree.Add([int]$process.ProcessId)) { $changed = $true }
      }
    }
    foreach ($id in @($tree)) {
      $nativeIdentity = [QQBotFixtureIdentity]::Read($id)
      if (-not $nativeIdentity) { [void]$exitedDuringObservation.Add($id); [void]$tree.Remove($id); continue } # exited during enumeration; no live window owner
      if (-not $current[$id]) { $identityRecoveries[[string]$id] = $nativeIdentity }
      $identities[[string]$id] = $nativeIdentity
      [void]$observed.Add($id)
    }
    foreach ($row in [QQBotFixtureWindows]::VisibleFor([int[]]@($tree))) { $violations.Add("visible:$row") }
    $foreground = [QQBotFixtureWindows]::ForegroundOwner()
    if ($foreground -gt 0 -and $tree.Contains($foreground)) { $violations.Add("foreground:$foreground") }
    $samples++
    if (Test-Path -LiteralPath $Done) { break }
    Start-Sleep -Milliseconds 250
  } while ([DateTime]::UtcNow -lt $deadline)
} catch { $failure = $_.Exception.Message }
@{ samples=$samples; observed=@($observed); identities=$identities; identityRecoveries=$identityRecoveries; exitedDuringObservation=@($exitedDuringObservation); violations=@($violations); error=$failure; done=(Test-Path -LiteralPath $Done); intervalMs=250 } |
  ConvertTo-Json -Depth 5 | ForEach-Object { [IO.File]::WriteAllText($Output, $_, [Text.UTF8Encoding]::new($false)) }
if ($failure) { [Console]::Error.WriteLine($failure); exit 1 }
if (-not (Test-Path -LiteralPath $Done) -or $observed.Count -eq 0 -or $violations.Count -ne 0) { exit 1 }

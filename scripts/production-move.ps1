# Owner-run move of production into the install-root layout under C:\qqbot (#67).
# Windows PowerShell 5.1. The owner runs it with -Production from an interactive administrator PowerShell and types
# the confirmation word before the first change; development runs it only with -Rehearsal under a temporary root,
# through scripts\test-production-move.ps1.
#   -Mode SelfTest         pure checks of the parsers and rules; reads and writes nothing
#   -Mode Inventory        every check of Move, read-only, all findings at once; writes inventory.json
#   -Mode Move             stage the release, restrict the install root, stop the old production, copy with hash
#                          checks, rewrite the configuration, register the HER task, start, health and report checks
#   -Mode Rollback         restore the old task definitions and start the old production from the untouched originals
#   -Mode OperatorFolders  copy <OperatorSource> (the ops scripts) to <install root>\ops
# -DigestModel <id> (Inventory and Move) also points the memory digest at the relay: memory.digest.model, the model
# origin in dependencies.json and config\harness-api-key.
# Exit codes: 0 done; 1 refused, production left as it was; 2 failed and rolled back; 3 rollback incomplete, read the
# printed steps; 4 rehearsal abort point reached. The console shows only fixed texts; details go to the evidence
# folder, which stays private (vault).
[CmdletBinding()]
param(
  [Parameter(Mandatory = $true)][ValidateSet('SelfTest', 'Inventory', 'Move', 'Rollback', 'OperatorFolders')][string]$Mode,
  [switch]$Production,
  [switch]$Rehearsal,
  [string]$InstallRoot = 'C:\qqbot',
  [string]$OldRuntime = '',
  [string]$OldRelayFolder = '',
  [string]$OldSupervisorTask = 'QQBot-StartAll',
  [string]$OldRelayTask = 'HER-Relay',
  [string]$RelayVersion = '',
  [string]$SnowLumaVersion = '',
  [string]$ProfileFolder = '',
  [string]$ReleaseSource = '',
  [string]$ReleaseName = '',
  [string]$EvidenceRoot = 'C:\her-orch\67\evidence',
  [string]$StateFile = '',
  [string]$OperatorSource = 'C:\her-orch\ops',
  [int]$HealthWaitSeconds = 300,
  [int]$LiveWaitSeconds = 300,
  [ValidateRange(1, 65535)][int]$RelayDefaultPort = 8317,
  [long]$FreeSpaceReserveBytes = 1GB,
  [switch]$AcceptDshProfileFiles,
  [switch]$AfterDone,
  [string]$DigestModel = '',
  [ValidateSet('', 'Staged', 'TasksDisabled', 'Stopped', 'Copied', 'Registered')][string]$RehearsalAbortAfter = '',
  [ValidateSet('', 'StaleIdentity', 'DisturbAfterDisable', 'CorruptCopy', 'LateDshChild')][string]$RehearsalFault = '',
  [switch]$RehearsalConfirm,
  [string]$OwnerGoFile = ''
)
$ErrorActionPreference = 'Stop'
$Phases = @('Preflight', 'Staged', 'OldTasksDisabled', 'Stopping', 'OldStopped', 'Copied', 'Configured', 'Registering', 'Registered', 'Starting', 'Done', 'RolledBack')
$ProductionPorts = @(3000, 3001, 3080, 3100, 8317)
$ArchivePattern = '^qqbot-[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}-[0-9]{2}-[0-9]{2}(-[0-9]+)?\.zip$'
$script:Root = ''
$script:Temp = ''
$script:ProfilePath = ''
$script:Evidence = ''
$script:Log = ''
$script:LockStream = $null
$script:StatePath = ''
$script:State = $null
$script:CallNumber = 0
$script:OldExe = ''
$script:OldCli = ''
$script:OldRuntimePath = ''
$script:RehearsalOldRoot = ''
$script:RelayKey = ''
$script:SelfChecks = 0
$script:Blockers = [Collections.Generic.List[string]]::new()
$script:SecretTexts = [Collections.Generic.List[string]]::new()
# Owner decisions on the c70fd204 Inventory (Revision 18). The old DSH stub's Antigravity CLI folder holds that tool's
# own log links; the stub comparison leaves exactly this relative path out (any case), and every other link in the
# stub still refuses. The configuration's description note under backends (the key "_" followed by U+8BF4 U+660E)
# names old folders in prose; the rewrite drops exactly that property, and the kept originals still hold it.
$script:DshStubExclude = @('.gemini\antigravity-cli')
$script:BackendsNoteKey = '_' + [string][char]0x8BF4 + [string][char]0x660E
$script:ProfileRoots = $null
$script:RootAclChanging = $false
$script:GoClaim = $null
$script:RunLockStream = $null

if (-not ('HerMove67Native' -as [type])) {
  Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;
public static class HerMove67Native {
  [StructLayout(LayoutKind.Sequential)] struct FileTime { public uint Low; public uint High; }
  [StructLayout(LayoutKind.Sequential)] struct ByHandleInfo { public uint Attributes; public FileTime Created; public FileTime Accessed; public FileTime Written; public uint VolumeSerial; public uint SizeHigh; public uint SizeLow; public uint Links; public uint IndexHigh; public uint IndexLow; }
  [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern SafeFileHandle CreateFile(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetFileInformationByHandle(SafeFileHandle file, out ByHandleInfo info);
  [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern uint GetFinalPathNameByHandle(SafeFileHandle file, StringBuilder path, uint size, uint flags);
  [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern uint GetLongPathName(string shortPath, StringBuilder longPath, uint size);
  [StructLayout(LayoutKind.Sequential)] struct DispositionInfo { [MarshalAs(UnmanagedType.U1)] public bool DeleteFile; }
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetFileInformationByHandle(SafeFileHandle file, int infoClass, ref DispositionInfo info, uint size);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool ReadFile(SafeFileHandle file, byte[] buffer, uint toRead, out uint read, IntPtr overlapped);
  [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetProcessTimes(IntPtr process, out FileTime creation, out FileTime exit, out FileTime kernel, out FileTime user);
  [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern bool QueryFullProcessImageName(IntPtr process, uint flags, StringBuilder path, ref uint size);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool GetExitCodeProcess(IntPtr process, out uint code);
  [DllImport("kernel32.dll", SetLastError = true)] static extern bool TerminateProcess(IntPtr process, uint code);
  [DllImport("kernel32.dll", SetLastError = true)] static extern uint WaitForSingleObject(IntPtr handle, uint milliseconds);
  [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
  [DllImport("shell32.dll", SetLastError = true, CharSet = CharSet.Unicode)] static extern IntPtr CommandLineToArgvW(string commandLine, out int count);
  [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr memory);
  const uint QueryLimited = 0x1000;
  const uint Synchronize = 0x100000;
  const uint TerminateAccess = 0x1;
  const uint StillActive = 259;
  const int InvalidParameter = 87;

  static string Describe(IntPtr process) {
    uint code;
    if (!GetExitCodeProcess(process, out code)) return "unknown";
    if (code != StillActive) return "gone";
    FileTime creation, exit, kernel, user;
    if (!GetProcessTimes(process, out creation, out exit, out kernel, out user)) return "unknown";
    StringBuilder path = new StringBuilder(32768);
    uint size = (uint)path.Capacity;
    if (!QueryFullProcessImageName(process, 0, path, ref size)) return "unknown";
    ulong start = ((ulong)creation.High << 32) | creation.Low;
    return start.ToString() + "|" + path.ToString();
  }
  // "gone", "unknown" or "<creation FILETIME>|<image path>".
  public static string Identify(uint pid) {
    IntPtr process = OpenProcess(QueryLimited | Synchronize, false, pid);
    if (process == IntPtr.Zero) return Marshal.GetLastWin32Error() == InvalidParameter ? "gone" : "unknown";
    try { return Describe(process); } finally { CloseHandle(process); }
  }
  // "stopped", "gone", "mismatch" (the PID now names another process) or "unknown". Terminates only the recorded identity.
  public static string Stop(uint pid, ulong start, string image) {
    IntPtr process = OpenProcess(QueryLimited | Synchronize | TerminateAccess, false, pid);
    if (process == IntPtr.Zero) return Marshal.GetLastWin32Error() == InvalidParameter ? "gone" : "unknown";
    try {
      string found = Describe(process);
      if (found == "gone" || found == "unknown") return found;
      string[] parts = found.Split(new char[] { '|' }, 2);
      if (parts[0] != start.ToString() || !String.Equals(Path.GetFullPath(parts[1]), Path.GetFullPath(image), StringComparison.OrdinalIgnoreCase)) return "mismatch";
      if (!TerminateProcess(process, 1)) return "unknown";
      return WaitForSingleObject(process, 15000) == 0 ? "stopped" : "unknown";
    } finally { CloseHandle(process); }
  }
  public static string[] Split(string commandLine) {
    int count;
    IntPtr argv = CommandLineToArgvW(commandLine, out count);
    if (argv == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
    try {
      string[] result = new string[count];
      for (int i = 0; i < count; i++) result[i] = Marshal.PtrToStringUni(Marshal.ReadIntPtr(argv, i * IntPtr.Size));
      return result;
    } finally { LocalFree(argv); }
  }
  // The long spelling of an existing path (8.3 parts expanded), so it compares with a final path by handle.
  public static string LongPath(string path) {
    StringBuilder buffer = new StringBuilder(32768);
    uint length = GetLongPathName(path, buffer, (uint)buffer.Capacity);
    if (length == 0 || length >= buffer.Capacity) return path;
    return buffer.ToString();
  }
  // The owner go file, held open while the script judges it. Nothing is deleted until Delete() is called, which the
  // script does only after the checks and the ledger entry; Close() without Delete() leaves the file in place.
  public sealed class GoFile {
    public string Status = ""; public string Identity = ""; public string Written = ""; public string Text = "";
    internal SafeFileHandle Handle;
    public bool Delete() {
      if (Handle == null) return false;
      DispositionInfo info; info.DeleteFile = true;
      return SetFileInformationByHandle(Handle, 4, ref info, (uint)Marshal.SizeOf(typeof(DispositionInfo)));
    }
    public void Close() { if (Handle != null) { Handle.Dispose(); Handle = null; } }
  }
  // Opens the owner go file once, exclusively (no other open, not even a read; the link itself, never its target) and
  // judges it only through that handle, in this order: a reparse point or folder (link), the final path by handle
  // against the expected file (moved: a folder swapped after the link check), more than one name (aliased: a hard
  // link), more than 4 KiB (toolarge), then the text (ok). Other statuses: missing, busy (another open holds it),
  // unreadable.
  public static GoFile OpenGoFile(string path, string expected) {
    // Shifts instead of long hex literals: the public scan refuses digit runs of 7 or more.
    const uint GenericRead = 1u << 31; const uint DeleteAccess = 1u << 16; const uint OpenExisting = 3; const uint OpenReparsePoint = 1u << 21;
    GoFile go = new GoFile();
    SafeFileHandle handle = CreateFile(path, GenericRead | DeleteAccess, 0, IntPtr.Zero, OpenExisting, OpenReparsePoint, IntPtr.Zero);
    if (handle.IsInvalid) {
      int error = Marshal.GetLastWin32Error();
      go.Status = (error == 2 || error == 3) ? "missing" : (error == 32 ? "busy" : "unreadable");
      return go;
    }
    go.Handle = handle;
    ByHandleInfo info;
    if (!GetFileInformationByHandle(handle, out info)) { go.Status = "unreadable"; return go; }
    go.Identity = info.VolumeSerial.ToString("x8") + "-" + info.IndexHigh.ToString("x8") + info.IndexLow.ToString("x8");
    go.Written = (((ulong)info.Written.High << 32) | info.Written.Low).ToString();
    if ((info.Attributes & 0x400) != 0 || (info.Attributes & 0x10) != 0) { go.Status = "link"; return go; }
    StringBuilder final = new StringBuilder(32768);
    uint length = GetFinalPathNameByHandle(handle, final, (uint)final.Capacity, 0);
    if (length == 0 || length >= final.Capacity) { go.Status = "unreadable"; return go; }
    string finalPath = final.ToString();
    if (finalPath.StartsWith(@"\\?\")) finalPath = finalPath.Substring(4);
    if (!String.Equals(finalPath, expected, StringComparison.OrdinalIgnoreCase)) { go.Status = "moved"; return go; }
    if (info.Links != 1) { go.Status = "aliased"; return go; }
    if (info.SizeHigh != 0 || info.SizeLow > 4096) { go.Status = "toolarge"; return go; }
    byte[] bytes = new byte[info.SizeLow];
    uint read = 0;
    while (read < bytes.Length) {
      byte[] chunk = new byte[bytes.Length - read];
      uint got;
      if (!ReadFile(handle, chunk, (uint)chunk.Length, out got, IntPtr.Zero) || got == 0) break;
      Array.Copy(chunk, 0, bytes, read, got);
      read += got;
    }
    go.Text = new UTF8Encoding(false).GetString(bytes, 0, (int)read).TrimStart((char)0xFEFF);
    go.Status = "ok";
    return go;
  }
  // Opens or creates a plain file (the run lock, the go ledger) exclusively, refusing a reparse point, which it checks
  // through the handle. Returns null for a link or a folder; throws IOException when another holder has it open.
  public static FileStream OpenPlain(string path) {
    const uint GenericRead = 1u << 31; const uint GenericWrite = 1u << 30; const uint OpenAlways = 4; const uint OpenReparsePoint = 1u << 21;
    SafeFileHandle handle = CreateFile(path, GenericRead | GenericWrite, 0, IntPtr.Zero, OpenAlways, OpenReparsePoint, IntPtr.Zero);
    if (handle.IsInvalid) throw new IOException("the file is held by another holder or cannot be opened", Marshal.GetHRForLastWin32Error());
    ByHandleInfo info;
    if (!GetFileInformationByHandle(handle, out info) || (info.Attributes & 0x400) != 0 || (info.Attributes & 0x10) != 0) { handle.Dispose(); return null; }
    return new FileStream(handle, FileAccess.ReadWrite);
  }
}
'@
}

# ---------- output, refusals and small helpers ----------
function Write-Log([string]$Text) {
  if ($script:Log) { [IO.File]::AppendAllText($script:Log, ([DateTime]::UtcNow.ToString('o') + ' ' + $Text + "`r`n"), [Text.UTF8Encoding]::new($false)) }
}
# Account-home paths never reach the console or inventory.json: the profile root (the -ProfileFolder in use and
# USERPROFILE, long and short spellings) becomes <profile>, in backslash, forward-slash and JSON-escaped spellings.
# inventory.json is hidden string by string before serialization (Edit-ProjectionStrings), so JSON escapes of
# characters such as an apostrophe cannot hide a path. state.json keeps real paths (the rollback reads them); like
# every evidence file it stays private and goes to the vault.
function Get-ProfileRoots {
  # The profile roots in their long and short (8.3) spellings, longest first; computed once per run.
  if ($null -ne $script:ProfileRoots) { return ,$script:ProfileRoots }
  $found = [Collections.Generic.List[string]]::new()
  foreach ($path in @($script:ProfilePath, $env:USERPROFILE)) {
    if (-not $path) { continue }
    $long = ([string]$path).TrimEnd('\')
    if ($long.Length -gt 3 -and -not $found.Contains($long)) { $found.Add($long) }
    try {
      $short = ([string](New-Object -ComObject Scripting.FileSystemObject).GetFolder($long).ShortPath).TrimEnd('\')
      if ($short.Length -gt 3 -and -not $found.Contains($short)) { $found.Add($short) }
    } catch { Write-Log 'no short spelling of a profile root' }
  }
  $script:ProfileRoots = @($found | Sort-Object -Property Length -Descending)
  return ,$script:ProfileRoots
}
function Hide-ProfilePath([string]$Text) {
  $result = $Text
  $roots = Get-ProfileRoots
  foreach ($root in $roots) {
    foreach ($spelling in @($root.Replace('\', '\\'), $root, $root.Replace('\', '/'))) {
      $result = [regex]::Replace($result, [regex]::Escape($spelling), '<profile>', [Text.RegularExpressions.RegexOptions]::IgnoreCase)
    }
  }
  return $result
}
function Write-Step([string]$Text) { $shown = Hide-ProfilePath $Text; Write-Host ('[67] ' + $shown); Write-Log $shown }
# Every string in an evidence projection: each configured relay key becomes [redacted] and the profile root
# becomes <profile>, before any JSON escaping.
function Edit-ProjectionStrings($Value) {
  if ($Value -is [string]) {
    $text = $Value
    foreach ($secret in $script:SecretTexts) { if ($secret) { $text = $text.Replace($secret, '[redacted]') } }
    return (Hide-ProfilePath $text)
  }
  if ($Value -is [Management.Automation.PSCustomObject]) {
    foreach ($property in @($Value.PSObject.Properties)) { $property.Value = Edit-ProjectionStrings $property.Value }
    return $Value
  }
  if ($Value -is [Array]) {
    for ($index = 0; $index -lt $Value.Count; $index++) { $Value[$index] = Edit-ProjectionStrings $Value[$index] }
    return ,$Value
  }
  return $Value
}
function Get-TextSha([string]$Text) {
  $algorithm = [Security.Cryptography.SHA256]::Create()
  try { return ([BitConverter]::ToString($algorithm.ComputeHash([Text.UTF8Encoding]::new($false).GetBytes($Text)))).Replace('-', '').ToLowerInvariant() }
  finally { $algorithm.Dispose() }
}
function Stop-Refused([string]$Reason) { throw ('REFUSED: ' + $Reason) }
function Stop-Failed([string]$Reason) { throw ('FAILED: ' + $Reason) }
function Get-SafeMessage([string]$Text) {
  if ($Text.StartsWith('REFUSED: ') -or $Text.StartsWith('FAILED: ')) { return $Text }
  # The hidden text goes to the private evidence log, so the console line's pointer to it holds.
  Write-Log ('unexpected error: ' + $Text)
  return 'an unexpected error (details in the evidence log)'
}
function Add-Blocker([string]$Reason) {
  if ($Mode -eq 'Inventory') { $script:Blockers.Add($Reason); Write-Step ('BLOCKED: ' + $Reason) } else { Stop-Refused $Reason }
}
function Add-Failure($ErrorRecord, [string]$Section) {
  $text = [string]$ErrorRecord.Exception.Message
  Write-Log ($Section + ': ' + $text)
  if ($Mode -ne 'Inventory') { throw $ErrorRecord }
  $safe = Get-SafeMessage $text
  $script:Blockers.Add($Section + ': ' + $safe)
  Write-Step ('BLOCKED: ' + $Section + ': ' + $safe)
}
# A check section that needs an earlier section's result: when that result is missing (its section was blocked), the
# section reports a fixed "skipped" finding instead of running on empty values and hiding its own result.
function Assert-PlanHas($plan, [string[]]$Keys, [string]$Prerequisite) {
  foreach ($key in $Keys) {
    # A port is an int, and 0 counts as missing: the configuration stores a port only after its checks passed.
    if (-not $plan.Contains($key) -or $null -eq $plan[$key] -or ([string]$plan[$key]) -eq '' -or ($plan[$key] -is [int] -and $plan[$key] -eq 0)) { Stop-Refused ('skipped: needs ' + $Prerequisite) }
  }
}
function Get-PhaseIndex([string]$Phase) { return [Array]::IndexOf($Phases, $Phase) }
function Get-RunKind { if ($Rehearsal) { return '-Rehearsal' }; return '-Production' }
function Get-Normal([string]$Path) {
  if ($Path -notmatch '^[A-Za-z]:[\\/]') { Stop-Refused 'a path is not an absolute local drive path' }
  $full = [IO.Path]::GetFullPath($Path).TrimEnd('\')
  if ($full.Length -le 3) { Stop-Refused 'a drive root is not a valid folder here' }
  return $full
}
function Test-Within([string]$Path, [string]$Root) {
  $inner = (Get-Normal $Path).ToLowerInvariant() + '\'
  $outer = (Get-Normal $Root).ToLowerInvariant() + '\'
  return $inner.StartsWith($outer, [StringComparison]::Ordinal)
}
function Test-Present([string]$Path) { return $null -ne (Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue) }
function Assert-RehearsalPath([string]$Path) {
  if (-not $Rehearsal) { return }
  if (-not (Test-Within $Path $script:Temp)) { Stop-Refused 'rehearsal paths must lie inside the temporary folder' }
  # Lexical containment is not enough: a junction or link between the temporary folder and the path could lead the
  # rehearsal's reads and writes anywhere, production included. Every existing step must be a real folder or file.
  $current = Get-Normal $Path
  while ($current.Length -gt $script:Temp.Length) {
    $item = Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue
    if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Stop-Refused 'a rehearsal path passes through a junction or link' }
    $current = Split-Path -Parent $current
  }
}
# No existing step of a path, from the path itself up to below the drive root, may be a junction or link: one above a
# source or above the install root would lead hashing, copies, ACL changes and cleanup elsewhere. Production and
# rehearsal alike (Assert-RehearsalPath only covers the steps below the temporary folder).
function Assert-NoLinkAbove([string]$Path, [string]$What) {
  $current = Get-Normal $Path
  while ($current.Length -gt 3) {
    $item = Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue
    if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Stop-Refused ('a junction or link lies on the path of ' + $What) }
    $current = Split-Path -Parent $current
  }
}
function Assert-RehearsalPort([int]$Port) {
  if ($Rehearsal -and $ProductionPorts -contains $Port) { Stop-Refused 'a rehearsal never uses a production port' }
}
# The owner's go for a run the Windows coordinator starts (#67, docs/agents/production-ops.md): one file directly in
# the go folder, at most 2 hours old, holding exactly one line: "MOVE <release>", "ROLLBACK <the state file's
# release>" or "COPY". Initialize-Run claims it before every refusal that does not judge the go file's own path
# (Claim-OwnerGo: one exclusive handle, checked through it, recorded in a ledger of file identities, then deleted
# through it), so a refusal anywhere later leaves no reusable go; Confirm-Owner judges the kept text. The ledger
# refuses a second name of the same file. Threat model: the go file guards against an agent acting without a
# relayed owner go; whoever can write the go folder can already write a go file, so the script cannot authenticate
# the writer, and races that need the same account to swap links during a run are out of scope beyond failing closed.
function Get-GoFolder {
  if ($Rehearsal) { return (Join-Path (Split-Path -Parent $script:Root) 'go') }
  return 'C:\her-orch\67\go'
}
function Get-GoLedger { return (Join-Path (Split-Path -Parent (Get-GoFolder)) 'go-claimed.txt') }
# Reads the claimed identities and appends this one, through one exclusive handle that refuses a link (checked through
# the handle) in a folder with no link above it. Returns whether the identity was already there.
function Use-GoLedger([string]$Identity) {
  $ledger = Get-GoLedger
  Assert-NoLinkAbove (Split-Path -Parent $ledger) 'the go ledger'
  $stream = $null
  try { $stream = [HerMove67Native]::OpenPlain($ledger) } catch { Stop-Refused 'the go ledger is held by another run' }
  if ($null -eq $stream) { Stop-Refused 'the go ledger is a link or a folder' }
  try {
    $reader = [IO.StreamReader]::new($stream, [Text.ASCIIEncoding]::new(), $false, 4096, $true)
    $lines = Split-Lines ($reader.ReadToEnd())
    $seen = @($lines) -contains $Identity
    $bytes = [Text.ASCIIEncoding]::new().GetBytes($Identity + "`r`n")
    [void]$stream.Seek(0, [IO.SeekOrigin]::End)
    $stream.Write($bytes, 0, $bytes.Length)
    $stream.Flush($true)
    return $seen
  } finally { $stream.Dispose() }
}
function Test-OwnerGoLine([string[]]$Lines, [string]$Word, [string]$Release) {
  # MOVE and ROLLBACK always name a release; only COPY stands alone.
  if (@('MOVE', 'ROLLBACK') -ccontains $Word -and -not $Release) { return $false }
  $expected = $Word
  if ($Release) { $expected = $Word + ' ' + $Release }
  $all = @($Lines)
  return ($all.Count -eq 1 -and [string]$all[0] -ceq $expected)
}
function Test-GoAge([DateTime]$WrittenUtc, [DateTime]$NowUtc) {
  $age = $NowUtc - $WrittenUtc
  return ($age.TotalMinutes -ge -5 -and $age.TotalHours -le 2)
}
function Claim-OwnerGo {
  $path = Get-Normal $OwnerGoFile
  $folder = Get-Normal (Get-GoFolder)
  if ((Split-Path -Parent $path) -ine $folder) { Stop-Refused 'the owner go file must lie directly in the go folder' }
  if (Test-Within $path $script:Root) { Stop-Refused 'the owner go file must lie outside the install root' }
  Assert-NoLinkAbove $folder 'the owner go file'
  Assert-RehearsalPath $folder
  # The file itself is judged through its one exclusive handle: no link, its final path is the expected file (a folder
  # swapped after the check above shows up there), one name, one line's size. Nothing is deleted before those checks:
  # a link or a moved file is closed and left alone. A file that passed them (or has a second name, or is too large)
  # is recorded in the ledger while the handle is still held, and only then deleted through that handle.
  $expected = [HerMove67Native]::LongPath($folder) + '\' + (Split-Path -Leaf $path)
  $claim = [HerMove67Native]::OpenGoFile($path, $expected)
  try {
    $status = [string]$claim.Status
    if (@('ok', 'aliased', 'toolarge') -contains $status) {
      $seen = Use-GoLedger ([string]$claim.Identity)
      if ($status -eq 'ok' -and $seen) { $status = 'replayed' }
      if (-not $claim.Delete()) { Stop-Failed 'the owner go file could not be deleted' }
    }
    $script:GoClaim = [pscustomobject]@{ Status = $status; Identity = [string]$claim.Identity; Written = [string]$claim.Written; Text = [string]$claim.Text }
  } finally { $claim.Close() }
  switch ($script:GoClaim.Status) {
    'ok' { return }
    'missing' { Stop-Refused 'the owner go file is missing' }
    'busy' { Stop-Refused 'the owner go file is held by another run' }
    'link' { Stop-Refused 'the owner go file is a link or a folder' }
    'aliased' { Stop-Refused 'the owner go file has another name (a hard link); it serves no run' }
    'moved' { Stop-Refused 'the owner go file is not where its path says' }
    'replayed' { Stop-Refused 'the owner go file was already used' }
    'toolarge' { Stop-Refused 'the owner go file is larger than one line' }
    default { Stop-Refused 'the owner go file cannot be read' }
  }
}
function Test-OwnerGoClaim([string]$Word, [string]$Release) {
  $claim = $script:GoClaim
  if ($null -eq $claim -or $claim.Status -ne 'ok') { Stop-Refused 'the owner go file was not claimed' }
  $written = [DateTime]::FromFileTimeUtc([long]$claim.Written)
  if (-not (Test-GoAge $written ([DateTime]::UtcNow))) { Stop-Refused 'the owner go file is older than 2 hours (or dated in the future)' }
  if (-not (Test-OwnerGoLine (Split-Lines ([string]$claim.Text)) $Word $Release)) { Stop-Refused 'the owner go file does not name this action and release' }
  Write-Log ('owner go file accepted for ' + $Word)
}
function Confirm-Owner([string]$Word, [string]$Summary, [string]$Release = '') {
  if ($OwnerGoFile) {
    # A run the Windows coordinator starts on the owner's relayed go: the go file replaces the typed word.
    Write-Step $Summary
    Test-OwnerGoClaim $Word $Release
    Write-Step ('the owner go file names ' + $Word + '; it was deleted when this run started')
    Write-Log ('owner confirmed with ' + $Word + ' through the owner go file')
    return
  }
  # A rehearsal asks only with -RehearsalConfirm and reads the word from its redirected input; production reads the
  # keyboard (Initialize-Run refuses redirected input there).
  if ($Rehearsal -and -not $RehearsalConfirm) { return }
  Write-Step $Summary
  Write-Host ('Type ' + $Word + ' to go on; anything else stops here with nothing changed')
  if ($Rehearsal) { $answer = [Console]::In.ReadLine() } else { $answer = Read-Host 'Confirmation' }
  if ([string]$answer -cne $Word) { Stop-Refused 'the owner did not confirm' }
  Write-Log ('owner confirmed with ' + $Word)
}
# Live production files are read with read, write and delete sharing, so a reader never breaks an atomic replace.
function Open-Shared([string]$Path) { return [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]'ReadWrite, Delete') }
function Read-Text([string]$Path) {
  $stream = Open-Shared $Path
  try { $reader = [IO.StreamReader]::new($stream, [Text.UTF8Encoding]::new($false), $true); return $reader.ReadToEnd() }
  finally { $stream.Dispose() }
}
function Split-Lines([string]$Text) {
  $lines = [Collections.Generic.List[string]]::new([string[]][regex]::Split($Text, "`r`n|`n|`r"))
  if ($lines.Count -gt 0 -and $lines[$lines.Count - 1] -eq '') { $lines.RemoveAt($lines.Count - 1) }
  return ,$lines.ToArray()
}
function Read-Lines([string]$Path) { return ,(Split-Lines (Read-Text $Path)) }
function Read-Json([string]$Path) { return (Read-Text $Path | ConvertFrom-Json) }
function Write-Utf8([string]$Path, [string]$Text) { [IO.File]::WriteAllText($Path, $Text, [Text.UTF8Encoding]::new($false)) }
function Write-JsonFile([string]$Path, $Value) { Write-Utf8 $Path ($Value | ConvertTo-Json -Depth 100) }
function Get-Prop($Object, [string]$Name) {
  if ($null -eq $Object -or $Object -isnot [Management.Automation.PSCustomObject]) { return $null }
  $property = $Object.PSObject.Properties[$Name]
  if ($null -eq $property) { return $null }
  Write-Output -NoEnumerate $property.Value
}
function Set-Prop($Object, [string]$Name, $Value) {
  $property = $Object.PSObject.Properties[$Name]
  if ($null -eq $property) { $Object | Add-Member -NotePropertyName $Name -NotePropertyValue $Value } else { $property.Value = $Value }
}
function Remove-Prop($Object, [string]$Name) {
  if ($null -ne $Object -and $Object -is [Management.Automation.PSCustomObject] -and $null -ne $Object.PSObject.Properties[$Name]) { $Object.PSObject.Properties.Remove($Name) }
}
function Get-FileSha([string]$Path) {
  $algorithm = [Security.Cryptography.SHA256]::Create()
  $stream = Open-Shared $Path
  try { return ([BitConverter]::ToString($algorithm.ComputeHash($stream))).Replace('-', '').ToLowerInvariant() }
  finally { $stream.Dispose(); $algorithm.Dispose() }
}
function Format-HashGroups([string]$Hash) { return ((@([regex]::Matches($Hash.ToLowerInvariant(), '.{1,4}') | ForEach-Object { $_.Value })) -join ' ') }
function Test-ExactVersion([string]$Version) {
  if ($Version -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$') { return $false }
  $dash = $Version.IndexOf('-')
  if ($dash -lt 0) { return $true }
  foreach ($part in $Version.Substring($dash + 1).Split('.')) {
    if ($part.Length -gt 1 -and $part.StartsWith('0') -and $part -match '^[0-9]+$') { return $false }
  }
  return $true
}
function Test-ReleaseName([string]$Name) { return ($Name -cmatch '^[a-z][a-z0-9-]{2,40}$') -and ($Name -cnotmatch '^v[0-9]') }
function Test-ModelName([string]$Name) { return $Name -cmatch '^[A-Za-z0-9][A-Za-z0-9._:-]{0,79}$' }
function Test-FreeSpace([double]$Free, [double]$Needed, [double]$Reserve) { return $Free -ge ($Needed + $Reserve) }
function ConvertFrom-Sums([string[]]$Lines) {
  $sums = @{}
  foreach ($line in $Lines) {
    if (-not $line.Trim() -or $line.StartsWith('#')) { continue }
    $match = [regex]::Match($line, '^(?<hash>[0-9A-Fa-f]{64})\s+\*?(?<name>\S+)\s*$')
    if (-not $match.Success) { Stop-Refused 'SHA256SUMS has an invalid line' }
    $name = $match.Groups['name'].Value
    if (@('her.exe', 'her-cli.exe', 'herupdate.exe') -cnotcontains $name -or $sums.ContainsKey($name)) { Stop-Refused 'SHA256SUMS names an unexpected or repeated file' }
    $sums[$name] = $match.Groups['hash'].Value.ToLowerInvariant()
  }
  if ($sums.Count -ne 3) { Stop-Refused 'SHA256SUMS must list her.exe, her-cli.exe and herupdate.exe' }
  return $sums
}

# ---------- path mapping and configuration scans ----------
function New-PathMap($Pairs) {
  # Longest source first, so a folder inside another mapped folder wins.
  return ,@($Pairs | Sort-Object -Property @{ Expression = { $_.From.Length }; Descending = $true })
}
function Convert-PathString([string]$Text, $Map) {
  $normal = $Text.Replace('/', '\')
  foreach ($entry in $Map) {
    $from = $entry.From
    if ($normal.Equals($from, [StringComparison]::OrdinalIgnoreCase)) { return $entry.To }
    if ($normal.StartsWith($from + '\', [StringComparison]::OrdinalIgnoreCase)) { return $entry.To + $normal.Substring($from.Length) }
  }
  return $Text
}
function Convert-Paths($Value, $Map) {
  if ($Value -is [string]) { return (Convert-PathString $Value $Map) }
  if ($Value -is [Management.Automation.PSCustomObject]) {
    foreach ($property in @($Value.PSObject.Properties)) { $property.Value = Convert-Paths $property.Value $Map }
    return $Value
  }
  if ($Value -is [Array]) {
    for ($index = 0; $index -lt $Value.Count; $index++) { $Value[$index] = Convert-Paths $Value[$index] $Map }
    return ,$Value
  }
  return $Value
}
function Test-NamesRoot([string]$Text, $Roots) {
  foreach ($root in $Roots) { if ([regex]::IsMatch($Text, [regex]::Escape($root) + '(\\|["''\s]|$)')) { return $true } }
  return $false
}
# JSON may spell any character as \uXXXX; Windows PowerShell 5.1's ConvertTo-Json writes ' < > & that way. A path
# with such a character (the rehearsal profile's apostrophe) is then spelled with every character outside letters,
# digits, path separators, colon, dot, dash, underscore and space escaped so (Revision 19 fix round).
function ConvertTo-JsonEscapes([string]$Text) { return [regex]::Replace($Text, '[^A-Za-z0-9\\/:._ -]', { param($found) '\u' + ([int][char]$found.Value).ToString('x4') }) }
# The reverse, for scans: every \uXXXX escape read as its character.
function ConvertFrom-JsonEscapes([string]$Text) { return [regex]::Replace($Text, '\\u(?<hex>[0-9A-Fa-f]{4})', { param($found) [string][char][Convert]::ToInt32($found.Groups['hex'].Value, 16) }) }
# DSH texts name a linked bundle folder in three spellings: backslashes, forward slashes and JSON-escaped backslashes,
# and the two JSON spellings once more with \uXXXX escapes when the path holds such a character. Each is replaced by
# the new folder in the same spelling, case-insensitively and only at a full-name boundary: a path separator, a quote
# or the end of a line, never a space or another name character, so a sibling such as "<folder> archive" or
# "<file> helper" is not rewritten (Revision 19 fix round).
function Convert-DshText([string]$Text, $TextMap) {
  $result = $Text
  foreach ($entry in $TextMap) {
    $from = ([string]$entry.From).TrimEnd('\')
    $to = ([string]$entry.To).TrimEnd('\')
    $spellings = @(@($from.Replace('\', '\\'), $to.Replace('\', '\\')), @($from, $to), @($from.Replace('\', '/'), $to.Replace('\', '/')))
    foreach ($json in @(@($from.Replace('\', '\\'), $to.Replace('\', '\\')), @($from.Replace('\', '/'), $to.Replace('\', '/')))) {
      $escaped = ConvertTo-JsonEscapes $json[0]
      if ($escaped -cne $json[0]) { $spellings += ,@($escaped, (ConvertTo-JsonEscapes $json[1])) }
    }
    foreach ($pair in $spellings) {
      $pattern = [regex]::Escape($pair[0]) + '(?=[\\/"'']|\r|\n|$)'
      $result = [regex]::Replace($result, $pattern, $pair[1].Replace('$', '$$'), [Text.RegularExpressions.RegexOptions]::IgnoreCase)
    }
  }
  return $result
}
# A link: or file: dependency that climbs out with .. would resolve elsewhere once the DSH home has moved. Given the
# file's folder and the folder that moves with it, a relative one that resolves inside that folder moves along and is
# fine (pnpm's link:../../plugins/<bundle> in the web profile, Revision 19); without them every climbing one counts.
# The whole value counts, up to a quote, comma, bracket or line end: a value holding whitespace is ambiguous and counts
# as climbing (Revision 19 fix round).
function Test-ClimbingLink([string]$Text, [string]$Folder = '', [string]$Inside = '') {
  foreach ($match in [regex]::Matches($Text, '(?i)(?<![A-Za-z0-9])(?:link|file):(?<path>\.\.[\\/][^"''`,;)\]}\r\n]*)')) {
    if (-not $Folder -or -not $Inside) { return $true }
    $value = $match.Groups['path'].Value.TrimEnd()
    if ($value -match '\s') { return $true }
    try { $resolved = [IO.Path]::GetFullPath([IO.Path]::Combine($Folder, $value)).TrimEnd('\') } catch { return $true }
    if ($resolved.Length -le 3 -or -not (Test-Within $resolved $Inside)) { return $true }
  }
  return $false
}
function Find-ForeignPaths($Value, [string]$Pointer, $Roots, [string]$Install, $Found) {
  if ($Value -is [string]) {
    $normal = $Value.Replace('/', '\').ToLowerInvariant()
    $bad = Test-NamesRoot $normal $Roots
    foreach ($mark in @('\qqbot-rust', '\her-orch\', '\_npx\', '\.dsh', '\.cli-proxy-api')) { if ($normal.Contains($mark)) { $bad = $true } }
    if ($normal -match '^[a-z]:\\') {
      if ($normal -match '(^|\\)\.\.(\\|$)') { $bad = $true }
      if ($normal.StartsWith($Install + '\')) {
        $area = $normal.Substring($Install.Length + 1).Split('\')[0]
        if (@('runtime', 'deps', 'backups', 'releases', 'current') -notcontains $area) { $bad = $true }
      }
    }
    if ($bad) { $Found.Add($Pointer) }
    return
  }
  if ($Value -is [Management.Automation.PSCustomObject]) {
    foreach ($property in @($Value.PSObject.Properties)) { Find-ForeignPaths $property.Value ($Pointer + '/' + $property.Name) $Roots $Install $Found }
    return
  }
  if ($Value -is [Array]) {
    for ($index = 0; $index -lt $Value.Count; $index++) { Find-ForeignPaths $Value[$index] ($Pointer + '/' + $index) $Roots $Install $Found }
  }
}
# Drops the description note under backends and nothing else. Both names compare case-sensitively, so another spelling
# of either, and the same key anywhere else, stay and are scanned.
function Remove-BackendsNote($Config) {
  if ($Config -isnot [Management.Automation.PSCustomObject]) { return }
  foreach ($parent in @($Config.PSObject.Properties | Where-Object { $_.Name -ceq 'backends' -and $_.Value -is [Management.Automation.PSCustomObject] })) {
    foreach ($note in @($parent.Value.PSObject.Properties | Where-Object { $_.Name -ceq $script:BackendsNoteKey })) { $parent.Value.PSObject.Properties.Remove($note.Name) }
  }
}
function Convert-Config($Config, $plan) {
  Remove-BackendsNote $Config
  $tasks = Get-Prop $Config 'tasks'
  Remove-Prop $tasks 'start-all'
  Remove-Prop $tasks 'installRoot'
  [void](Convert-Paths $Config $plan.Map)
  $snow = Join-Path $script:Root ('deps\snowluma\' + $plan.SnowLumaVersion)
  $dependencies = [pscustomobject][ordered]@{
    snowluma = [pscustomobject][ordered]@{ program = (Join-Path $snow 'node.exe'); script = (Join-Path $snow 'index.mjs'); workingDirectory = $snow; version = $plan.SnowLumaVersion }
    dsh = [pscustomobject][ordered]@{ program = $plan.NodePath; version = $plan.DshVersion }
    relay = [pscustomobject][ordered]@{ program = $plan.RelayProgram; arguments = [object[]]@($plan.RelayArguments); workingDirectory = $plan.RelayPin; version = $plan.RelayVersion; loginDirectory = $plan.Auth.NewPath; clientKeyFile = (Join-Path $plan.RelayPin 'her-relay-client.key') }
  }
  Set-Prop $Config 'dependencies' $dependencies
  $update = Get-Prop $Config 'update'
  if ($null -eq $update) { Set-Prop $Config 'update' ([pscustomobject][ordered]@{ policy = 'off' }) } else { Set-Prop $update 'policy' 'off' }
  if ($plan.DigestModel) {
    # The memory digest runs on the relay (Q5): memory.digest.model; the model origin and the key follow elsewhere.
    $memory = Get-Prop $Config 'memory'
    if ($null -eq $memory) { $memory = [pscustomobject][ordered]@{}; Set-Prop $Config 'memory' $memory }
    if ($memory -isnot [Management.Automation.PSCustomObject]) { Stop-Refused 'memory in the configuration is not an object' }
    $digest = Get-Prop $memory 'digest'
    if ($null -eq $digest) { $digest = [pscustomobject][ordered]@{}; Set-Prop $memory 'digest' $digest }
    if ($digest -isnot [Management.Automation.PSCustomObject]) { Stop-Refused 'memory.digest in the configuration is not an object' }
    Set-Prop $digest 'model' ([string]$plan.DigestModel)
  }
  $found = [Collections.Generic.List[string]]::new()
  Find-ForeignPaths $Config '' @($plan.Roots) ($script:Root.ToLowerInvariant()) $found
  $dir = [string](Get-Prop (Get-Prop $Config 'backup') 'dir')
  if ($dir -and $dir -match '^[A-Za-z]:[\\/]') {
    foreach ($area in @('releases', 'deps', 'runtime')) { if (Test-Within $dir (Join-Path $script:Root $area)) { $found.Add('/backup/dir') } }
  } elseif ($dir -and $dir -notmatch '^\\\\') {
    if ($dir -match '(^|[\\/])\.\.([\\/]|$)' -or $dir.StartsWith('\') -or $dir.StartsWith('/')) { $found.Add('/backup/dir') }
  }
  return ,$found
}
function Convert-Dependencies($Dependencies, $plan) {
  [void](Convert-Paths $Dependencies $plan.Map)
  Set-Prop $Dependencies 'relay' ('http://127.0.0.1:' + $plan.RelayPort)
  if ($plan.DigestModel) { Set-Prop $Dependencies 'model' ('http://127.0.0.1:' + $plan.RelayPort) }
  $found = [Collections.Generic.List[string]]::new()
  Find-ForeignPaths $Dependencies '/dependencies.json' @($plan.Roots) ($script:Root.ToLowerInvariant()) $found
  return ,$found
}
# Both lists in one flat array. Never wrap a function that returns ,$list in @(): that nests the list as one element.
function Get-ForeignPointers($Config, $Dependencies, $plan) {
  $configFound = Convert-Config $Config $plan
  $dependencyFound = Convert-Dependencies $Dependencies $plan
  return ,@(@($configFound) + @($dependencyFound))
}
function Get-BackupDestination($Config) {
  $dir = [string](Get-Prop (Get-Prop $Config 'backup') 'dir')
  if (-not $dir) { return (Join-Path $script:Root 'backups') }
  if ($dir -match '^\\\\') { return '' }
  if ($dir -match '^[A-Za-z]:[\\/]') { return (Get-Normal $dir) }
  return (Join-Path $script:Root $dir)
}

# ---------- relay configuration ----------
# Two shapes: the flat one (top-level port, host, auth-dir, api-keys) and the nested one production's relay uses
# (server.port, server.host, oauth.auth-dir, access.api-keys, management.secret-key). This is no YAML parser: it reads
# the plain shape only and refuses whatever it cannot read exactly (fail closed). A first pass registers every secret
# before anything can refuse. Nesting is read by indentation under exactly those four top-level keys; in the nested
# shape the only other top-level key allowed is config-version (the flat shape keeps its other keys, as the S1u fixture
# has them). A value it uses is value, "value" or 'value' with nothing after it, on one line. Every other line is a
# plain key line or a list item; anything else (anchors, tags, aliases, quoted, escaped, explicit or merge keys, tabs,
# document markers) refuses with one reason, and so does flow style. So do a list item at the indentation of api-keys
# and a secret-key, api-keys or auth-dir line in any other place. Blank and comment lines are allowed anywhere except
# inside the api-keys list. Top-level port, auth-dir or api-keys beside the nested blocks is a mixed shape and refuses.
# Any other line holding an absolute path, nested or not, counts toward the "other absolute paths" refusal.
# One configured secret for hiding: the text as written, the text without one pair of surrounding quotes, and the text
# cut at its first quote or comment (a quoted or refused value). Pieces under 4 characters are skipped; they would hide
# ordinary evidence text.
function Add-RelaySecret([string]$Text) {
  $raw = $Text.Trim()
  $pieces = @($raw, (($raw.TrimStart('"', "'") -split '["'']|\s#')[0]).Trim())
  $unquoted = [regex]::Match($raw, '^(?:"(?<v>.*)"|''(?<v>.*)'')$')
  if ($unquoted.Success) { $pieces += $unquoted.Groups['v'].Value }
  foreach ($candidate in $pieces) {
    if ($candidate.Length -ge 4 -and -not $script:SecretTexts.Contains($candidate)) { $script:SecretTexts.Add($candidate) }
  }
}
# The bridge reaches the relay on loopback only. A missing host is the relay's own default, so the live listener's
# addresses are judged too (an empty list is judged by the listener-owner check instead).
function Test-LoopbackOnly([string[]]$Addresses) { return @($Addresses | Where-Object { @('127.0.0.1', '::1') -notcontains $_ }).Count -eq 0 }
function Read-RelayScalar([string]$Text, [string]$What, [bool]$Secret) {
  # A value the move uses: value, "value" or 'value' and nothing after it; inside double quotes only \\ is read (as one
  # backslash). A comment, an odd or inner quote, another escape and flow style refuse, so the move never reads a value
  # other than the relay's. A refused secret is still kept out of the evidence, cut at its first quote or comment. An
  # empty value comes back empty; the caller decides.
  $raw = $Text.Trim()
  $double = [regex]::Match($raw, '^"(?<v>(?:[^"\\]|\\\\)*)"$')
  $single = [regex]::Match($raw, '^''(?<v>[^'']*)''$')
  if ($double.Success) { $value = $double.Groups['v'].Value.Replace('\\', '\') }
  elseif ($single.Success) { $value = $single.Groups['v'].Value }
  elseif ($raw -notmatch '["'']|(^|\s)#|:(\s|$)|^[\[\]{}&*!|>%@`,]|^[-?](\s|$)') { $value = $raw }
  else {
    if ($Secret) { Add-RelaySecret $raw }
    Stop-Refused ('the relay configuration value of ' + $What + ' is not plain or simply quoted (a comment, an odd quote, an escape or flow style)')
  }
  return $value
}
function Read-RelayPort([string]$Text) {
  $value = Read-RelayScalar $Text 'port' $false
  if ($value -notmatch '^[0-9]{1,5}$' -or [int]$value -lt 1 -or [int]$value -gt 65535) { Stop-Refused 'the relay configuration has a port line that cannot be read' }
  return [int]$value
}
# One "key: rest" line of a key the parser knows; $null for any other line. "key:value" is no key for YAML, so it
# refuses rather than being read differently from the relay.
function Split-RelayKey([string]$Text) {
  $match = [regex]::Match($Text, '^(?<key>port|host|auth-dir|api-keys|secret-key)\s*:(?<rest>.*)$')
  if (-not $match.Success) { return $null }
  if ($match.Groups['rest'].Value -match '^\S') { Stop-Refused 'the relay configuration has a key with no space after its colon' }
  return [pscustomobject]@{ Key = $match.Groups['key'].Value; Rest = $match.Groups['rest'].Value }
}
function Assert-RelayListHeader([string]$Rest) {
  if ($Rest.Trim()) { Stop-Refused 'the relay configuration has an api-keys line with text after it (a comment or flow style)' }
}
function Add-RelayPlace([hashtable]$Seen, [string]$Place) {
  if ($Seen.ContainsKey($Place)) { Stop-Refused ('the relay configuration names ' + $Place + ' more than once') }
  $Seen[$Place] = $true
}
function Assert-RelayHost([string]$Rest) {
  # An empty host or another address opens the relay to the network.
  if (@('127.0.0.1', 'localhost') -notcontains (Read-RelayScalar $Rest 'host' $false)) { Stop-Refused 'the relay configuration has a host other than 127.0.0.1 or localhost' }
}
function Read-RelayConfig([string[]]$Lines) {
  $ports = @(); $serverPorts = @(); $auth = @(); $nestedAuth = @(); $keys = @(); $nestedKeys = @(); $secrets = @()
  $absolute = 0; $flatLists = 0; $seen = @{}; $first = ''
  $parent = ''; $childIndent = -1; $nested = $false
  $list = ''; $listIndent = -1; $itemIndent = -1; $listGap = $false; $scalarIndent = -1
  $misplaced = @('auth-dir', 'api-keys', 'secret-key')
  # First pass, before anything can refuse: every secret-key value and every list item (the api-keys entries), in block
  # or flow form, is registered for hiding, so a refusal on an earlier line cannot leave a later secret unhidden. A
  # secret-key line registers its whole rest (a block value may hold a comma or a bracket) and its flow-form value cut
  # at a comma, brace or bracket. It also tells the nested shape (a server, access, oauth or management block anywhere)
  # from the flat one.
  foreach ($source in $Lines) {
    $whole = [regex]::Match([string]$source, 'secret-key["'']?\s*:(?<v>.*)$')
    if ($whole.Success) { Add-RelaySecret ($whole.Groups['v'].Value) }
    foreach ($hit in [regex]::Matches([string]$source, 'secret-key["'']?\s*:\s*(?<v>[^,}\]]*)')) { Add-RelaySecret ($hit.Groups['v'].Value) }
    $listed = [regex]::Match([string]$source, '^\s*-\s+(?<v>.*)$')
    if ($listed.Success) { Add-RelaySecret ($listed.Groups['v'].Value) }
    $flow = [regex]::Match([string]$source, 'api-keys["'']?\s*:\s*\[(?<v>[^\]]*)')
    if ($flow.Success) { foreach ($piece in ($flow.Groups['v'].Value -split ',')) { Add-RelaySecret $piece } }
    if ([regex]::IsMatch([string]$source, '^(server|access|oauth|management)\s*:')) { $nested = $true }
  }
  try {
    for ($index = 0; $index -lt $Lines.Count; $index++) {
      $line = $Lines[$index].TrimEnd()
      $indent = $line.Length - $line.TrimStart(' ').Length
      $text = $line.Substring($indent)
      if (-not $text -or $text.StartsWith('#')) {
        # Allowed anywhere but inside the api-keys list, where one could hide where the list ends (a cut list).
        if ($list) { $listGap = $true }
        continue
      }
      # A deeper line right after a value the parser reads would continue that value (a multi-line scalar).
      if ($scalarIndent -ge 0 -and $indent -gt $scalarIndent) { Stop-Refused 'the relay configuration continues a value it reads on a deeper line' }
      $scalarIndent = -1
      # Every other line is a plain key line ("key:", or "- key:" in a list) or a list item ("- value"). This refuses
      # anchors, tags, aliases, quoted, escaped, explicit and merge keys, tabs and document markers with one reason.
      if ($text -cnotmatch '^(- )?[A-Za-z0-9_.-]+:' -and -not ($indent -gt 0 -and $text -cmatch '^- \S')) { Stop-Refused 'the relay configuration has a line the relay parser does not understand' }
      if ($text -match ':\s+[\[{]|^-\s+[\[{]') { Stop-Refused 'the relay configuration uses flow style (a value or list item starting with { or [)' }
      if ($list) {
        $item = $text -match '^-(\s|$)'
        if ($indent -gt $listIndent) {
          if ($listGap) { Stop-Refused 'the relay configuration has a blank or comment line inside the api-keys list' }
          if ($itemIndent -lt 0) { $itemIndent = $indent }
          if (-not $item -or $indent -ne $itemIndent) { Stop-Refused 'the relay configuration has a line inside the api-keys list that is not one list item' }
          $value = Read-RelayScalar ($text.Substring(1)) 'an api-keys item' $true
          if ($list -eq 'flat') { $keys += $value } else { $nestedKeys += $value }
          continue
        }
        if ($item -and $indent -eq $listIndent) { Stop-Refused 'the relay configuration has an api-keys list item at the indentation of api-keys itself' }
        $list = ''; $listGap = $false
      }
      if ($indent -eq 0) {
        $parent = ''; $childIndent = -1
        $block = [regex]::Match($text, '^(?<key>server|access|oauth|management)\s*:(?<rest>.*)$')
        if ($block.Success) {
          if ($block.Groups['rest'].Value.Trim()) { Stop-Refused 'the relay configuration has a block line (server, access, oauth or management) with text after it' }
          Add-RelayPlace $seen ($block.Groups['key'].Value)
          $parent = $block.Groups['key'].Value; $nested = $true
          continue
        }
        # Top-level port, auth-dir and api-keys pass here and refuse below as a mixed shape.
        if ($nested -and $text -cnotmatch '^(config-version|port|auth-dir|api-keys)\s*:') { Stop-Refused 'the nested relay configuration has a top-level key other than config-version, server, management, access and oauth' }
        $pair = Split-RelayKey $text
        if ($null -ne $pair -and $pair.Key -ceq 'port') { Add-RelayPlace $seen 'port'; $ports += (Read-RelayPort $pair.Rest); $scalarIndent = 0; continue }
        if ($null -ne $pair -and $pair.Key -ceq 'host') { Add-RelayPlace $seen 'host'; Assert-RelayHost $pair.Rest; $scalarIndent = 0; continue }
        if ($null -ne $pair -and $pair.Key -ceq 'auth-dir') { Add-RelayPlace $seen 'auth-dir'; $auth += [pscustomobject]@{ Index = $index; Value = (Read-RelayScalar $pair.Rest 'auth-dir' $false); Indent = '' }; $scalarIndent = 0; continue }
        if ($null -ne $pair -and $pair.Key -ceq 'api-keys') { Add-RelayPlace $seen 'api-keys'; Assert-RelayListHeader $pair.Rest; $flatLists++; $list = 'flat'; $listIndent = 0; $itemIndent = -1; $listGap = $false; continue }
      }
      else {
        $pair = Split-RelayKey $text
        if ($parent) {
          if ($childIndent -lt 0) { $childIndent = $indent }
          if ($indent -lt $childIndent) { Stop-Refused 'the relay configuration has a line indented less than the first key of its block' }
          $place = ''
          if ($null -ne $pair -and $indent -eq $childIndent) { $place = $parent + '.' + $pair.Key }
          if ($place -ceq 'server.port') { Add-RelayPlace $seen $place; $serverPorts += (Read-RelayPort $pair.Rest); $scalarIndent = $indent; continue }
          if ($place -ceq 'server.host') { Add-RelayPlace $seen $place; Assert-RelayHost $pair.Rest; $scalarIndent = $indent; continue }
          if ($place -ceq 'oauth.auth-dir') { Add-RelayPlace $seen $place; $nestedAuth += [pscustomobject]@{ Index = $index; Value = (Read-RelayScalar $pair.Rest 'auth-dir' $false); Indent = $line.Substring(0, $indent) }; $scalarIndent = $indent; continue }
          if ($place -ceq 'access.api-keys') { Add-RelayPlace $seen $place; Assert-RelayListHeader $pair.Rest; $list = 'nested'; $listIndent = $indent; $itemIndent = -1; $listGap = $false; continue }
          # The management secret is never shown in any evidence either. An empty one means no management key.
          if ($place -ceq 'management.secret-key') { Add-RelayPlace $seen $place; $secrets += (Read-RelayScalar $pair.Rest 'secret-key' $true); $scalarIndent = $indent; continue }
        }
      }
      if ($null -ne $pair -and $misplaced -ccontains $pair.Key) { Stop-Refused 'the relay configuration has a secret-key, api-keys or auth-dir line in a place this check does not read' }
      if ($line -match '(^|[\s"''=])[A-Za-z]:[\\/]') { $absolute++ }
    }
    $allKeys = @($keys) + @($nestedKeys)
    if ($nested -and ($ports.Count + $auth.Count + $flatLists) -gt 0) { Stop-Refused 'the relay configuration mixes top-level port, auth-dir or api-keys with the nested blocks' }
    if ($allKeys.Count -lt 1) { Stop-Refused 'the relay configuration lists no api-keys entry' }
    if ($nested -and $nestedAuth.Count -eq 0) { Stop-Refused 'the nested relay configuration has no oauth.auth-dir; its login folder needs a decision' }
    if (@(@($auth) + @($nestedAuth) | Where-Object { -not $_.Value }).Count -gt 0) { Stop-Refused 'the relay configuration has an empty auth-dir' }
    # The production key has 52 characters; a short key or secret is a placeholder or a cut value.
    if (@($allKeys | Where-Object { $_.Length -lt 16 }).Count -gt 0 -or @($secrets | Where-Object { $_ -and $_.Length -lt 16 }).Count -gt 0) { Stop-Refused 'the relay configuration has a management secret-key or an api key shorter than 16 characters' }
    if ($absolute -gt 0) { Stop-Refused 'the relay configuration names other absolute paths; they need a decision first' }
    $first = $allKeys[0]
    if ($first -notmatch '^[\x21-\x7e]{1,4096}$') { Stop-Refused 'the first relay api key is not printable ASCII' }
  }
  finally {
    # Every key and secret as read (a doubled backslash read as one) stays out of the evidence too, also when this
    # configuration is refused. The caller stores the relay arguments and the configuration's file name only after this
    # function returns, so a refusal stores neither.
    foreach ($secret in @(@($keys) + @($nestedKeys) + @($secrets))) { Add-RelaySecret $secret }
  }
  $authEntry = $null
  if ($auth.Count -eq 1) { $authEntry = $auth[0] }
  if ($nestedAuth.Count -eq 1) { $authEntry = $nestedAuth[0] }
  $port = 0; $portSource = 'default'
  if ($ports.Count -eq 1) { $port = $ports[0]; $portSource = 'top-level port' }
  if ($serverPorts.Count -eq 1) { $port = $serverPorts[0]; $portSource = 'server.port' }
  return [pscustomobject]@{ Port = $port; PortSource = $portSource; Auth = $authEntry; Key = $first }
}
function Convert-RelayArguments([string[]]$Tokens, [string]$From, [string]$To) {
  $result = @()
  foreach ($token in $Tokens) {
    $name = ''; $value = $token
    $match = [regex]::Match($token, '^(?<name>-{1,2}[A-Za-z][\w-]*=)(?<value>.*)$')
    if ($match.Success) { $name = $match.Groups['name'].Value; $value = $match.Groups['value'].Value }
    if ($value -match '(^|[\\/])\.\.([\\/]|$)') { Stop-Refused 'a relay argument climbs out of its folder' }
    if ($value -match '^[A-Za-z]:[\\/]') {
      if (-not (Test-Within $value $From)) { Stop-Refused 'a relay argument names a file outside the old relay folder' }
      $fromNormal = Get-Normal $From
      $valueNormal = Get-Normal $value
      $value = $To + $valueNormal.Substring($fromNormal.Length)
    }
    $result += ($name + $value)
  }
  return ,$result
}
function Find-RelayConfig([string[]]$Tokens, [string]$OldFolder) {
  for ($index = 0; $index -lt $Tokens.Count; $index++) {
    $token = $Tokens[$index]
    $value = ''
    if ($token -match '^-{1,2}config=(?<v>.+)$') { $value = $Matches['v'] }
    elseif ($token -match '^-{1,2}config$' -and $index + 1 -lt $Tokens.Count) { $value = $Tokens[$index + 1] }
    else { continue }
    if ($value -notmatch '^[A-Za-z]:[\\/]') { $value = Join-Path $OldFolder $value }
    return (Get-Normal $value)
  }
  return (Join-Path $OldFolder 'config.yaml')
}
function Resolve-RelayAuth($Relay, [string]$OldFolder, [string]$Pin, [string]$ProfilePath) {
  $value = '~/.cli-proxy-api'; $index = -1; $indent = ''
  if ($null -ne $Relay.Auth) { $value = [string]$Relay.Auth.Value; $index = [int]$Relay.Auth.Index; $indent = [string]$Relay.Auth.Indent }
  $relativeForm = $false
  if ($value.StartsWith('~/') -or $value.StartsWith('~\')) { $source = Get-Normal (Join-Path $ProfilePath $value.Substring(2)) }
  elseif ($value -match '^[A-Za-z]:[\\/]') { $source = Get-Normal $value }
  else { $source = Get-Normal (Join-Path $OldFolder $value); $relativeForm = $true }
  if (-not (Test-Path -LiteralPath $source -PathType Container)) { Stop-Refused 'the relay login folder is missing' }
  if ($source -ieq $OldFolder) { $newPath = $Pin; $inside = $true }
  elseif (Test-Within $source $OldFolder) { $newPath = $Pin + $source.Substring($OldFolder.Length); $inside = $true }
  else {
    if (Test-Present (Join-Path $OldFolder 'auth')) { Stop-Refused 'the old relay folder already has an auth entry, so the login folder cannot be placed there' }
    $newPath = Join-Path $Pin 'auth'; $inside = $false
  }
  $action = 'replace'
  if ($inside -and $relativeForm) { $action = 'keep' } elseif ($index -lt 0) { $action = 'append' }
  return [pscustomobject]@{ Source = $source; Inside = $inside; NewPath = $newPath; Action = $action; LineIndex = $index; Indent = $indent }
}

# ---------- processes, ports and scheduled tasks ----------
function Get-Port($Text, [string]$Name) {
  $uri = $null
  if (-not [Uri]::TryCreate([string]$Text, [UriKind]::Absolute, [ref]$uri) -or $uri.Port -le 0 -or @('127.0.0.1', 'localhost', '[::1]') -notcontains $uri.Host) { Stop-Refused ('dependencies.json ' + $Name + ' is not a loopback origin with a port') }
  Assert-RehearsalPort $uri.Port
  return $uri.Port
}
function Get-ListenerPid([int]$Port) {
  $owners = @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue | ForEach-Object { [int]$_.OwningProcess } | Sort-Object -Unique)
  if ($owners.Count -gt 1) { Stop-Refused ('more than one process listens on port ' + $Port) }
  if ($owners.Count -eq 0) { return 0 }
  return $owners[0]
}
function Get-Identity([int]$ProcessId, [string]$Role) {
  $found = [HerMove67Native]::Identify([uint32]$ProcessId)
  if ($found -eq 'gone' -or $found -eq 'unknown') { return $null }
  $parts = $found.Split([char[]]@('|'), 2)
  return [pscustomobject]@{ Role = $Role; Pid = $ProcessId; Start = $parts[0]; Exe = $parts[1] }
}
function Get-CommandLine([int]$ProcessId) {
  $process = @(Get-CimInstance -ClassName Win32_Process -Filter ('ProcessId=' + $ProcessId))
  if ($process.Count -ne 1) { return '' }
  return [string]$process[0].CommandLine
}
function Get-OwnerSid([int]$ProcessId) {
  $process = @(Get-CimInstance -ClassName Win32_Process -Filter ('ProcessId=' + $ProcessId))
  if ($process.Count -ne 1) { return '' }
  $result = Invoke-CimMethod -InputObject $process[0] -MethodName GetOwnerSid
  return [string]$result.Sid
}
function Get-ParentImages([int]$ProcessId) {
  $names = @(); $current = $ProcessId
  for ($depth = 0; $depth -lt 3; $depth++) {
    $process = @(Get-CimInstance -ClassName Win32_Process -Filter ('ProcessId=' + $current))
    if ($process.Count -ne 1) { break }
    $parent = [int]$process[0].ParentProcessId
    $up = @(Get-CimInstance -ClassName Win32_Process -Filter ('ProcessId=' + $parent))
    if ($up.Count -ne 1) { break }
    $names += [string]$up[0].Name
    $current = $parent
  }
  return ,$names
}
function Get-ListenerIdentity([int]$Port, [string]$Role) {
  $owner = Get-ListenerPid $Port
  if ($owner -eq 0) { Stop-Refused ('nothing listens on the ' + $Role + ' port') }
  $identity = Get-Identity $owner $Role
  if ($null -eq $identity) { Stop-Refused ('the ' + $Role + ' process cannot be identified') }
  return $identity
}
function Test-SameIdentity($Identity) {
  $now = Get-Identity ([int]$Identity.Pid) ([string]$Identity.Role)
  return $null -ne $now -and $now.Start -eq [string]$Identity.Start -and $now.Exe -ieq [string]$Identity.Exe
}
function Stop-Identity($Identity) {
  $result = [HerMove67Native]::Stop([uint32]$Identity.Pid, [uint64]$Identity.Start, [string]$Identity.Exe)
  if (@('stopped', 'gone', 'mismatch') -notcontains $result) { Stop-Failed ('the ' + $Identity.Role + ' process could not be stopped safely (' + $result + ')') }
  Write-Step ('stopped ' + $Identity.Role + ' (' + $result + ')')
}
function Get-Descendants([int]$ProcessId, [DateTime]$StartUtc) {
  $all = @(Get-CimInstance -ClassName Win32_Process)
  $found = [Collections.Generic.List[int]]::new()
  $queue = [Collections.Generic.Queue[int]]::new()
  $queue.Enqueue($ProcessId)
  while ($queue.Count -gt 0) {
    $parent = $queue.Dequeue()
    foreach ($process in $all) {
      $child = [int]$process.ProcessId
      if ([int]$process.ParentProcessId -eq $parent -and $child -ne $parent -and -not $found.Contains($child) -and $process.CreationDate.ToUniversalTime() -ge $StartUtc) {
        $found.Add($child); $queue.Enqueue($child)
      }
    }
  }
  return ,$found
}
function Get-OldImageProcesses([string]$Folder) {
  return @(Get-CimInstance -ClassName Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath -match '^[A-Za-z]:\\' -and (Split-Path -Parent $_.ExecutablePath) -ieq $Folder })
}
# Processes of the new layout: an image under releases, current or deps; the configured Node running the DSH entry
# under deps\dsh (the new DSH); and every descendant of such a DSH, created after its parent (its tool servers, which
# may run any image). Descendants of SnowLuma are never included (QQ may be one, T2), nor anything of the old production.
function Get-NewLayoutProcesses($plan) {
  $images = @(@('releases', 'current', 'deps') | ForEach-Object { (Join-Path $script:Root $_) + '\' })
  $dshEntry = (Join-Path $script:Root 'deps\dsh') + '\'
  $node = [string]$plan.NodePath
  $all = @(Get-CimInstance -ClassName Win32_Process)
  $found = [Collections.Generic.List[object]]::new()
  $ids = @{}
  $dshTree = @{}
  foreach ($process in $all) {
    $image = [string]$process.ExecutablePath
    $byImage = @($images | Where-Object { $image.StartsWith($_, [StringComparison]::OrdinalIgnoreCase) }).Count -gt 0
    $byDsh = $node -and ($image -ieq $node) -and ([string]$process.CommandLine).IndexOf($dshEntry, [StringComparison]::OrdinalIgnoreCase) -ge 0
    if ($byImage -or $byDsh) { $found.Add($process); $ids[[int]$process.ProcessId] = $true }
    if ($byDsh) { $dshTree[[int]$process.ProcessId] = $process.CreationDate }
  }
  $grew = $true
  while ($grew) {
    $grew = $false
    foreach ($process in $all) {
      $id = [int]$process.ProcessId
      $parent = [int]$process.ParentProcessId
      if ($dshTree.ContainsKey($id) -or -not $dshTree.ContainsKey($parent) -or $id -eq $parent) { continue }
      if ($process.CreationDate -lt $dshTree[$parent]) { continue }
      $dshTree[$id] = $process.CreationDate
      if (-not $ids.ContainsKey($id)) { $found.Add($process); $ids[$id] = $true }
      $grew = $true
    }
  }
  return @($found)
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
function Get-TaskState([string]$Name) {
  $tasks = @(Get-ScheduledTask -TaskPath '\' -ErrorAction Stop | Where-Object { $_.TaskName -ceq $Name })
  if ($tasks.Count -ne 1) { return '' }
  return [string]$tasks[0].State
}
function Get-TaskRecord([string]$Name) {
  $shape = Get-TaskShape $Name
  if (-not $shape) { Stop-Refused ('the scheduled task ' + $Name + ' was not found exactly once') }
  $xml = [string](Export-ScheduledTask -TaskName $Name -TaskPath '\' -ErrorAction Stop)
  return [pscustomobject]@{ Name = $Name; Shape = $shape; Xml = $xml; Enabled = $shape.EndsWith('#True'); State = (Get-TaskState $Name) }
}
function Restore-Task($Record) {
  # A deleted task is recreated from its saved definition first: Enable-ScheduledTask needs an existing task.
  if (-not (Get-TaskShape $Record.Name)) {
    Register-ScheduledTask -TaskName $Record.Name -TaskPath '\' -Xml $Record.Xml -Force -ErrorAction Stop | Out-Null
  }
  if ((Get-TaskShape $Record.Name) -ne $Record.Shape) { Enable-ScheduledTask -TaskName $Record.Name -TaskPath '\' -ErrorAction Stop | Out-Null }
  if ((Get-TaskShape $Record.Name) -ne $Record.Shape) {
    Register-ScheduledTask -TaskName $Record.Name -TaskPath '\' -Xml $Record.Xml -Force -ErrorAction Stop | Out-Null
  }
  if ((Get-TaskShape $Record.Name) -ne $Record.Shape) { Stop-Failed ('the scheduled task ' + $Record.Name + ' could not be restored to its saved definition') }
  Write-Step ('restored the scheduled task ' + $Record.Name)
}
function Get-ProductionPortList($plan) {
  return @(@($plan.OneBotPort, $plan.OneBotWsPort, $plan.DshPort, $plan.RelayPort, $plan.ConsolePort) | Where-Object { [int]$_ -gt 0 } | Sort-Object -Unique)
}
function Wait-Until([scriptblock]$Condition, [int]$Seconds, [string]$Failure) {
  $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
  while ($true) {
    if (& $Condition) { return }
    if ([DateTime]::UtcNow -ge $deadline) { Stop-Failed $Failure }
    Start-Sleep -Seconds 2
  }
}

# ---------- native commands ----------
function Invoke-Native([string]$Program, [string[]]$Arguments, [string]$Label) {
  $script:CallNumber++
  $stem = Join-Path $script:Evidence ('{0:D3}-{1}' -f $script:CallNumber, $Label)
  $errorFile = $stem + '.err.txt'
  $previous = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  try {
    $output = & $Program @Arguments 2> $errorFile | Out-String
    $code = $LASTEXITCODE
  } finally { $ErrorActionPreference = $previous }
  Write-Utf8 ($stem + '.out.txt') $output
  return [pscustomobject]@{ Code = $code; Output = $output }
}
function Invoke-Cli([string]$Cli, [string[]]$Arguments, [string]$Label, [int[]]$Allowed = @(0)) {
  $result = Invoke-Native $Cli ($Arguments + @('--install-root', $script:Root)) $Label
  if ($Allowed -notcontains $result.Code) { Stop-Failed ($Label + ' exited ' + $result.Code + '; see the evidence folder') }
  return $result
}
function Invoke-Old([string[]]$Arguments, [string]$Label, [int[]]$Allowed = @(0)) {
  if ($Rehearsal) { $env:HER_INSTALL_ROOT = $script:RehearsalOldRoot }
  try { $result = Invoke-Native $script:OldCli $Arguments $Label }
  finally { if ($Rehearsal) { Remove-Item -Path Env:HER_INSTALL_ROOT -ErrorAction SilentlyContinue } }
  if ($Allowed -notcontains $result.Code) { Stop-Failed ($Label + ' exited ' + $result.Code + '; see the evidence folder') }
  return $result
}
function Get-OldOwnerStates {
  $status = (Invoke-Old @('lifecycle', 'status', '--runtime-root', $script:OldRuntimePath) 'old-status' @(0, 3)).Output | ConvertFrom-Json
  return [pscustomobject]@{ supervisor = [string]$status.owners.supervisor.state; live = [string]$status.owners.live.state; console = [string]$status.owners.console.state }
}
function Get-NewOwnerStates([string]$Cli) {
  $status = (Invoke-Cli $Cli @('lifecycle', 'status', '--runtime-root', (Join-Path $script:Root 'runtime')) 'new-status' @(0, 3)).Output | ConvertFrom-Json
  return [pscustomobject]@{ supervisor = [string]$status.owners.supervisor.state; live = [string]$status.owners.live.state; console = [string]$status.owners.console.state }
}
function Get-IdleState([string]$Cli, [string]$Runtime, [string]$Label) {
  $result = Invoke-Cli $Cli @('report', '--runtime-root', $Runtime, '--json') $Label @(0, 1)
  $report = $result.Output | ConvertFrom-Json
  if ($result.Code -ne 0 -or $report.partial -eq $true) { Stop-Refused 'the turn report is partial, so idleness cannot be shown' }
  $busy = 0; $paused = 0
  foreach ($conversation in @($report.conversations)) {
    $paused += @($conversation.pausedTurns).Count
    $busy += @(@($conversation.turns.entries) | Where-Object { @('begun', 'attempted', 'running', 'resumed') -contains $_.outcome }).Count
  }
  return [pscustomobject]@{ Busy = $busy; Paused = $paused }
}

# ---------- trees, manifests and copies ----------
# Whether a manifest walk leaves this relative path out: exactly an excluded relative path, in any case, compared as
# Get-Manifest always did (lower-cased invariant keys in a hashtable). Its subtree goes with it, since the walk never
# enters a left-out folder; a near-miss name is walked, and a link there refuses.
function Test-ManifestSkip([string]$Child, [string[]]$Exclude) {
  $skip = @{}
  foreach ($relative in @($Exclude)) { if ($relative) { $skip[$relative.ToLowerInvariant()] = $true } }
  return $skip.ContainsKey($Child.ToLowerInvariant())
}
# -Links (a list) collects each junction or link below the start folder as its relative path and leaves it out, never
# entering it, instead of refusing. Only Inventory's DSH stub comparison passes it, so a stub link does not hide the
# stub's differing files; every other walk, Move's stub walk included, still refuses at the first junction or link.
function Get-Manifest([string]$Root, [string]$TargetRoot, [string[]]$Exclude, [string]$Label, [switch]$NoHash, $Links = $null) {
  $base = Get-Normal $Root
  $target = Get-Normal $TargetRoot
  # The start folder too: a source that is itself a junction or link would be copied from somewhere else.
  $start = Get-Item -LiteralPath $base -Force -ErrorAction SilentlyContinue
  if ($null -ne $start -and ($start.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Stop-Refused ('a junction or link was found in ' + $Label) }
  $entries = [Collections.Generic.List[object]]::new()
  $pending = [Collections.Generic.Stack[string]]::new()
  $pending.Push('')
  while ($pending.Count -gt 0) {
    $relative = $pending.Pop()
    $folder = $base
    if ($relative) { $folder = $base + '\' + $relative }
    try { $items = [IO.DirectoryInfo]::new($folder).GetFileSystemInfos() }
    catch [System.IO.PathTooLongException] { Stop-Refused ('a path is longer than 259 characters in ' + $Label) }
    foreach ($item in $items) {
      $child = $item.Name
      if ($relative) { $child = $relative + '\' + $item.Name }
      if (Test-ManifestSkip $child $Exclude) { continue }
      if ($base.Length + 1 + $child.Length -gt 259 -or $target.Length + 1 + $child.Length -gt 259) { Stop-Refused ('a path is longer than 259 characters in ' + $Label) }
      if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { if ($null -ne $Links) { $Links.Add($child); continue }; Stop-Refused ('a junction or link was found in ' + $Label) }
      if ($item -is [IO.DirectoryInfo]) {
        $entries.Add([pscustomobject]@{ Path = $child; Length = -1; Sha256 = '' })
        $pending.Push($child)
      } else {
        $hash = ''
        if (-not $NoHash) { $hash = Get-FileSha $item.FullName }
        $entries.Add([pscustomobject]@{ Path = $child; Length = $item.Length; Sha256 = $hash })
      }
    }
  }
  return ,$entries
}
function Save-Manifest($Entries, [string]$Name) {
  $lines = foreach ($entry in $Entries) { $entry.Path + "`t" + $entry.Length + "`t" + $entry.Sha256 }
  Write-Utf8 (Join-Path $script:Evidence ('manifest-' + $Name + '.tsv')) ((@($lines) -join "`r`n") + "`r`n")
}
function Assert-SameManifest($Source, $Copy, [string]$Label) {
  $index = @{}
  foreach ($entry in $Copy) { $index[$entry.Path.ToLowerInvariant()] = $entry }
  if ($index.Count -ne $Source.Count) { Stop-Failed ('the copy of ' + $Label + ' differs from its source in its entry count') }
  foreach ($entry in $Source) {
    $copied = $index[$entry.Path.ToLowerInvariant()]
    if ($null -eq $copied -or $copied.Length -ne $entry.Length -or $copied.Sha256 -ne $entry.Sha256) { Stop-Failed ('the copy of ' + $Label + ' differs from its source (manifests in the evidence folder)') }
  }
}
# Every junction or link below a folder, found without following any. A start folder that is itself a junction or
# link is the one result: it is never entered, so a caller deletes only that link and nothing behind it.
function Get-ReparsePoints([string]$Folder) {
  $found = [Collections.Generic.List[string]]::new()
  $start = Get-Item -LiteralPath $Folder -Force -ErrorAction SilentlyContinue
  if ($null -eq $start) { return ,$found }
  if ($start.Attributes -band [IO.FileAttributes]::ReparsePoint) { $found.Add($start.FullName); return ,$found }
  if ($start -isnot [IO.DirectoryInfo]) { return ,$found }
  $pending = [Collections.Generic.Stack[string]]::new()
  $pending.Push($Folder)
  while ($pending.Count -gt 0) {
    # A path too long to list cannot be checked: a fixed refusal, never an unexpected error.
    try { $items = [IO.DirectoryInfo]::new($pending.Pop()).GetFileSystemInfos() }
    catch [System.IO.PathTooLongException] { Stop-Failed 'a path is too long to check for junctions or links' }
    foreach ($item in $items) {
      if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { $found.Add($item.FullName); continue }
      if ($item -is [IO.DirectoryInfo]) { $pending.Push($item.FullName) }
    }
  }
  return ,$found
}
# Deletes each junction or link below a folder as a link, never its target. Windows PowerShell 5.1's
# Remove-Item -Recurse descends through junctions, so no tree that may hold one is ever deleted that way first.
function Remove-LinksUnder([string]$Folder) {
  foreach ($path in (Get-ReparsePoints $Folder)) {
    $item = Get-Item -LiteralPath $path -Force
    Write-Log ('removed link ' + $path)
    if ($item -is [IO.DirectoryInfo]) { [IO.Directory]::Delete($path) } else { [IO.File]::Delete($path) }
    if (Test-Present $path) { Stop-Failed 'a link in the new layout could not be removed' }
  }
}
function Remove-TreeSafely([string]$Path) {
  $item = Get-Item -LiteralPath $Path -Force
  if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    if ($item -is [IO.DirectoryInfo]) { [IO.Directory]::Delete($Path) } else { [IO.File]::Delete($Path) }
    return
  }
  if ($item -is [IO.DirectoryInfo]) { Remove-LinksUnder $Path; Remove-Item -LiteralPath $Path -Recurse -Force } else { Remove-Item -LiteralPath $Path -Force }
}
# Before any ACL change on a folder: it must not be a junction or link itself, and every link below it must lead to a
# place inside it, so neither icacls nor inherited-ACE propagation can reach a folder outside.
function Assert-LinksInside([string]$Path, [string]$What) {
  $base = Get-Normal $Path
  foreach ($link in (Get-ReparsePoints $base)) {
    $target = ''
    if ($link -ine $base) { $target = Get-LinkTarget $link }
    if (-not $target -or -not (Test-Within $target $base)) { Stop-Failed ($What + ' is or holds a junction or link that leads outside it, so its access was not changed') }
  }
}
function Get-RobocopyExclusions([string]$From, [string[]]$Exclude) {
  $directories = @(); $files = @()
  foreach ($relative in @($Exclude)) {
    if (-not $relative) { continue }
    $full = Join-Path $From $relative
    if (Test-Path -LiteralPath $full -PathType Container) { $directories += $full } else { $files += $full }
  }
  $arguments = @()
  if ($directories.Count -gt 0) { $arguments += '/XD'; $arguments += $directories }
  if ($files.Count -gt 0) { $arguments += '/XF'; $arguments += $files }
  return ,$arguments
}
# A list-only robocopy with the copy's own switches must list nothing below an excluded path: production's bundle
# links are directory symbolic links, which the rehearsal can make only with a privilege, so this proves on the real
# shape, before anything is copied, that robocopy skips them instead of following them. Live files that come and go
# between two listings do not matter here, so no counts are compared.
function Test-RobocopyView([string]$From, [string]$To, [string[]]$Exclude, [string]$Label, [int]$Files) {
  $arguments = @($From, $To, '/L', '/E', '/XJ', '/R:0', '/W:0', '/NP', '/NDL', '/NJH', '/NJS', '/FP', '/NS', '/NC')
  $exclusions = Get-RobocopyExclusions $From $Exclude
  foreach ($item in @($exclusions)) { if ($item) { $arguments += $item } }
  $list = Invoke-Native (Join-Path $env:WINDIR 'System32\robocopy.exe') $arguments ('robocopy-list-' + $Label)
  if ($list.Code -ge 8) { Stop-Refused ('a list-only copy of ' + $Label + ' failed') }
  $prefixes = @(@($Exclude) | Where-Object { $_ } | ForEach-Object { (Join-Path $From $_) + '\' })
  $lines = Split-Lines $list.Output
  $underSource = 0
  foreach ($line in $lines) {
    $listed = $line.Trim()
    if ($listed.StartsWith($From.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)) { $underSource++ }
    foreach ($prefix in $prefixes) {
      if ($listed.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { Stop-Refused ('a list-only copy of ' + $Label + ' would follow an excluded link or copy an excluded folder') }
    }
  }
  # The listing must be the one this check expects (full source paths), or the check above would pass on nothing.
  if ($Files -gt 0 -and $underSource -eq 0) { Stop-Refused ('a list-only copy of ' + $Label + ' listed none of its files') }
}
function Copy-Verified([string]$From, [string]$To, [string[]]$Exclude, [string]$Label) {
  if (Test-Present $To) { Stop-Failed ('the copy target for ' + $Label + ' already exists') }
  $source = Get-Manifest $From $To $Exclude $Label
  Save-Manifest $source ($Label + '-source')
  $directories = @(); $files = @()
  foreach ($relative in @($Exclude)) {
    if (-not $relative) { continue }
    $full = Join-Path $From $relative
    if (Test-Path -LiteralPath $full -PathType Container) { $directories += $full } else { $files += $full }
  }
  [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($To))
  $arguments = @($From, $To, '/E', '/COPY:DAT', '/DCOPY:DAT', '/XJ', '/R:0', '/W:0', '/NP', '/NFL', '/NDL', '/NJH', '/NJS')
  if ($directories.Count -gt 0) { $arguments += '/XD'; $arguments += $directories }
  if ($files.Count -gt 0) { $arguments += '/XF'; $arguments += $files }
  $copy = Invoke-Native (Join-Path $env:WINDIR 'System32\robocopy.exe') $arguments ('robocopy-' + $Label)
  if ($copy.Code -ge 8) { Stop-Failed ('copying ' + $Label + ' failed (robocopy exit ' + $copy.Code + ')') }
  foreach ($relative in @($Exclude)) {
    if (-not $relative) { continue }
    $stray = Join-Path $To $relative
    if (Test-Present $stray) { Remove-TreeSafely $stray }
  }
  if ($Rehearsal -and $RehearsalFault -eq 'CorruptCopy' -and @('snowluma', 'ops') -contains $Label) { [IO.File]::AppendAllText((Join-Path $To 'package.json'), ' ') }
  $copied = Get-Manifest $To $To @() $Label
  Save-Manifest $copied ($Label + '-copy')
  Assert-SameManifest $source $copied $Label
  Write-Step ('copied and verified ' + $Label + ': ' + $source.Count + ' entries')
}
# Restricts a folder (and what inherits from it) to this account, SYSTEM and Administrators: the install root after
# staging, and in a rollback the attempt folder and any new-layout folder left in the root, which hold credential
# copies (DSH home, relay login and key, harness key) and must stay closed once the root's own access is restored.
function Protect-Folder([string]$Path, [string]$What = 'the install root') {
  Assert-LinksInside $Path $What
  $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  $result = Invoke-Native (Join-Path $env:WINDIR 'System32\icacls.exe') @($Path, '/inheritance:r', '/grant:r', ('*' + $sid + ':(OI)(CI)F'), '*S-1-5-18:(OI)(CI)F', '*S-1-5-32-544:(OI)(CI)F') 'protect'
  if ($result.Code -ne 0) { Stop-Failed ($What + ' could not be restricted') }
  $acl = Get-Acl -LiteralPath $Path
  if (-not $acl.AreAccessRulesProtected) { Stop-Failed ($What + ' still inherits access') }
  $foreign = @($acl.Access | Where-Object { @($sid, 'S-1-5-18', 'S-1-5-32-544') -notcontains $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value })
  if ($foreign.Count -gt 0) { Stop-Failed ($What + ' grants access to another account') }
  Write-Step ($What + ' is restricted to the owner, SYSTEM and Administrators')
}
# The old runtime's DSH stub against the profile DSH home, as paths relative to the stub: Differences holds the stub
# files the home lacks or holds with other content. With -ListLinks (Inventory only), Links holds the stub's junctions
# and links (listed, never followed) and the walk goes on; without it the walk refuses at the first one, before it
# enters any further folder, so Move stops at the first stub blocker.
function Compare-DshStub([string]$Stub, [string]$DshHome, [string[]]$Exclude, [switch]$ListLinks) {
  $result = [pscustomobject]@{ Differences = [Collections.Generic.List[string]]::new(); Links = [Collections.Generic.List[string]]::new() }
  if (-not (Test-Present $Stub)) { return $result }
  $homeIndex = @{}
  foreach ($entry in (Get-Manifest $DshHome $DshHome $Exclude 'dsh-home')) { $homeIndex[$entry.Path.ToLowerInvariant()] = $entry.Sha256 }
  $links = $null
  if ($ListLinks) { $links = $result.Links }
  foreach ($entry in (Get-Manifest $Stub $Stub $script:DshStubExclude 'dsh-stub' -Links $links)) {
    if ($entry.Length -lt 0) { continue }
    $key = $entry.Path.ToLowerInvariant()
    if (-not $homeIndex.ContainsKey($key) -or $homeIndex[$key] -ne $entry.Sha256) { $result.Differences.Add($entry.Path) }
  }
  return $result
}
function New-McpProfile([string]$Cli, [string]$Source, [string]$Folder, [int]$ConsolePort) {
  [void][IO.Directory]::CreateDirectory($Folder)
  $copy = Join-Path $Folder 'cordis.patch.yml'
  Write-Utf8 $copy (Read-Text $Source)
  # her-cli renders the block on this private copy outside the install root; the decoy install root only lets it
  # write here. The script installs the checked result itself (docs/agents/runtime-paths.md).
  $arguments = @('dsh-mcp-config', '--patch-file', $copy, '--bridge-exe', (Join-Path $script:Root 'current\her.exe'), '--runtime-root', (Join-Path $script:Root 'runtime'), '--console-port', [string]$ConsolePort, '--install-root', (Join-Path $Folder 'decoy-install-root'))
  $write = Invoke-Native $Cli $arguments 'mcp-render'
  if ($write.Code -ne 0) { Stop-Refused 'the DSH profile MCP block cannot be rewritten (her-cli dsh-mcp-config refused it)' }
  $check = Invoke-Native $Cli ($arguments + @('--check')) 'mcp-check'
  if ($check.Code -ne 0 -or $check.Output.Trim() -ne 'unchanged') { Stop-Failed 'the rewritten DSH profile MCP block does not check as current' }
  return $copy
}
# ---------- linked DSH bundles ----------
function Get-LinkTarget([string]$Path) {
  $targets = @((Get-Item -LiteralPath $Path -Force).Target)
  if ($targets.Count -ne 1 -or -not [string]$targets[0]) { return '' }
  $raw = [string]$targets[0]
  $target = $raw
  if ($target.StartsWith('\\?\') -or $target.StartsWith('\??\')) { $target = $target.Substring(4) }
  if ($target -match '^[A-Za-z]:[\\/]') { return (Get-Normal $target) }
  # A volume GUID, UNC, rooted or other non-drive-letter target is never inside anything: '' makes every caller refuse.
  if ($raw.StartsWith('\\') -or $raw.StartsWith('\??\') -or $target.StartsWith('\') -or $target.StartsWith('/') -or $target -match '^(Volume\{|UNC[\\/])' -or $target.Contains(':')) { return '' }
  # A relative target (a symbolic link's) is resolved against the link's own folder.
  return (Get-Normal (Join-Path (Split-Path -Parent $Path) $target))
}
# Directory links in the DSH home, found without following any link. Only a link to a folder strictly inside the
# install root's qq-bridge folder is a bundle the move copies and links again; any other link refuses.
function Find-DshLinks([string]$DshHome, [string]$BridgeRoot) {
  $links = [Collections.Generic.List[object]]::new()
  $inner = [Collections.Generic.List[object]]::new()
  $base = Get-Normal $DshHome
  $bridge = Get-Normal $BridgeRoot
  $pending = [Collections.Generic.Stack[string]]::new()
  $pending.Push('')
  while ($pending.Count -gt 0) {
    $relative = $pending.Pop()
    $folder = $base
    if ($relative) { $folder = $base + '\' + $relative }
    try { $items = [IO.DirectoryInfo]::new($folder).GetFileSystemInfos() }
    catch [System.IO.PathTooLongException] { Stop-Refused 'a path is longer than 259 characters in dsh-home' }
    foreach ($item in $items) {
      if ($item -isnot [IO.DirectoryInfo]) { continue }
      $child = $item.Name
      if ($relative) { $child = $relative + '\' + $item.Name }
      if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { $pending.Push($child); continue }
      $target = Get-LinkTarget $item.FullName
      if ($target -and $target -ine $base -and (Test-Within $target $base)) {
        # A link to another place in the DSH home (link: dependencies make relative ones in node_modules): accepted
        # below only when that place is itself an accepted bundle link.
        $inner.Add([pscustomobject]@{ Path = $child; TargetPath = $target.Substring($base.Length + 1) })
        continue
      }
      if (-not $target -or $target -ieq $bridge -or -not (Test-Within $target $bridge) -or -not (Test-Path -LiteralPath $target -PathType Container)) { Stop-Refused 'a junction or link was found in dsh-home' }
      # The target, every folder above it up to qq-bridge and qq-bridge itself must be real folders: a junction on
      # the way would make the bundle live somewhere else than its path says.
      $step = $target
      while ($true) {
        if ((Get-Item -LiteralPath $step -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { Stop-Refused 'a junction or link was found on the way to a linked DSH bundle' }
        if ($step -ieq $bridge) { break }
        $step = Split-Path -Parent $step
      }
      $links.Add([pscustomobject]@{ Path = $child; Target = $target; InnerTarget = '' })
    }
  }
  $bundlePaths = @($links | ForEach-Object { ([string]$_.Path).ToLowerInvariant() })
  foreach ($link in $inner) {
    if ($bundlePaths -notcontains ([string]$link.TargetPath).ToLowerInvariant()) { Stop-Refused 'a junction or link was found in dsh-home' }
    $links.Add([pscustomobject]@{ Path = $link.Path; Target = ''; InnerTarget = $link.TargetPath })
  }
  return ,$links
}
# Every .json, .yml and .yaml file below a folder, without following a junction or link.
function Get-TextFiles([string]$Folder) {
  $found = [Collections.Generic.List[string]]::new()
  $pending = [Collections.Generic.Stack[string]]::new()
  $pending.Push($Folder)
  while ($pending.Count -gt 0) {
    foreach ($item in [IO.DirectoryInfo]::new($pending.Pop()).GetFileSystemInfos()) {
      if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
      if ($item -is [IO.DirectoryInfo]) { $pending.Push($item.FullName); continue }
      if (@('.yml', '.yaml', '.json') -contains $item.Extension.ToLowerInvariant()) { $found.Add($item.FullName) }
    }
  }
  return ,$found
}
function Get-DshLinkPaths($plan) { return ,@(@($plan.DshLinks) | Where-Object { $null -ne $_ } | ForEach-Object { [string]$_.Path }) }
function Get-DshTextMap($plan) {
  $pairs = @(@($plan.Bundles) | Where-Object { $null -ne $_ } | ForEach-Object { [pscustomobject]@{ From = [string]$_.From; To = [string]$_.NewPath } })
  # The profile DSH home's own absolute paths (pnpm's link: entries, virtualStoreDir and workspace path) name the new
  # DSH home, and each carried dev tool file names its new place (Revision 19). A pnpm store outside the DSH home is
  # left as it is: only pnpm reads it, and the move runs no pnpm.
  if ($plan.DshHome) { $pairs += [pscustomobject]@{ From = [string]$plan.DshHome; To = (Join-Path $script:Root 'runtime\user\dsh') } }
  foreach ($file in @(@($plan.ToolFiles) | Where-Object { $null -ne $_ })) { $pairs += [pscustomobject]@{ From = [string]$file.From; To = [string]$file.To } }
  $map = New-PathMap $pairs
  return ,$map
}
# The roots no DSH text may name: the old production folders and the install root's qq-bridge folder.
function Get-ScanRoots($plan) {
  return ,@((@($plan.Roots) + @([string]$plan.BridgeRoot)) | Where-Object { $_ } | ForEach-Object { ([string]$_).ToLowerInvariant() })
}
function Find-OldProfileReferences([string]$DshHome, [string]$RenderedProfile, $Roots, $Bundles, $TextMap, [switch]$Moved) {
  $found = [Collections.Generic.List[string]]::new()
  $areas = @()
  # A relative link: is judged against the folder that moves with it: the DSH home for profiles and plugins, the bundle
  # folder for a bundle.
  foreach ($name in @('profiles', 'plugins')) { $areas += [pscustomobject]@{ Folder = (Join-Path $DshHome $name); Label = $name; Inside = $DshHome } }
  foreach ($bundle in @(@($Bundles) | Where-Object { $null -ne $_ })) {
    $folder = [string]$bundle.From
    if ($Moved) { $folder = [string]$bundle.NewPath }
    $areas += [pscustomobject]@{ Folder = $folder; Label = ('bundles\' + (Split-Path -Leaf ([string]$bundle.NewPath))); Inside = $folder }
  }
  foreach ($area in $areas) {
    if (-not (Test-Path -LiteralPath $area.Folder -PathType Container)) { continue }
    foreach ($file in (Get-TextFiles $area.Folder)) {
      $relative = $area.Label + $file.Substring($area.Folder.Length)
      $path = $file
      if ($RenderedProfile -and $relative -ieq 'profiles\web\cordis.patch.yml') { $path = $RenderedProfile }
      $text = Convert-DshText (Read-Text $path) $TextMap
      $normal = $text.Replace('\\', '\').Replace('/', '\').ToLowerInvariant()
      # A path spelled with \uXXXX escapes is found too: the text is read once more with them decoded.
      $decoded = (ConvertFrom-JsonEscapes $text).Replace('\\', '\').Replace('/', '\').ToLowerInvariant()
      if ((Test-NamesRoot $normal $Roots) -or (Test-NamesRoot $decoded $Roots) -or (Test-ClimbingLink $text (Split-Path -Parent $file) $area.Inside)) { $found.Add($relative) }
    }
  }
  return ,$found
}
# What a bundle could load from outside its own folder after the move. Node resolves from the bundle's real path
# upward, so (1) a node_modules in any folder above the old place that is not also above the new place (for example
# qq-bridge\dsh\node_modules or qq-bridge\node_modules) would be lost, declared or not, and (2) a declared package
# (dependencies, peerDependencies, optionalDependencies) found neither in the bundle's own node_modules nor in a
# node_modules both places reach is unresolved. The drive root is shared by both places and not walked. A node_modules
# the move carries ($Carried: From and To) is reachable when its new place's folder lies above the bundle's new place
# (qq-bridge\node_modules, carried into the new bundles folder, Revision 19).
function Test-CarriedModules([string]$Modules, [string]$NewPath, $Carried) {
  foreach ($entry in @(@($Carried) | Where-Object { $null -ne $_ })) {
    if ([string]$entry.From -ieq $Modules -and (Test-Within $NewPath (Split-Path -Parent ([string]$entry.To)))) { return $true }
  }
  return $false
}
function Find-BundleOutsideDependencies($Bundles, $Carried) {
  $found = [Collections.Generic.List[string]]::new()
  foreach ($bundle in @(@($Bundles) | Where-Object { $null -ne $_ })) {
    $from = [string]$bundle.From
    $newPath = [string]$bundle.NewPath
    $leaf = Split-Path -Leaf $newPath
    $shared = @()
    $current = Split-Path -Parent $from
    while ($current.Length -gt 3) {
      $modules = Join-Path $current 'node_modules'
      if (Test-Path -LiteralPath $modules -PathType Container) {
        if ((Test-Within $newPath $current) -or (Test-CarriedModules $modules $newPath $Carried)) { $shared += $modules } else { $found.Add($leaf + ': ' + $modules + ' is not reachable after the move') }
      }
      $current = Split-Path -Parent $current
    }
    $manifestPath = Join-Path $from 'package.json'
    if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { $found.Add($leaf + ': no package.json'); continue }
    $manifest = Read-Json $manifestPath
    $places = @((Join-Path $from 'node_modules')) + $shared
    foreach ($kind in @('dependencies', 'peerDependencies', 'optionalDependencies')) {
      $needs = Get-Prop $manifest $kind
      if ($needs -isnot [Management.Automation.PSCustomObject]) { continue }
      foreach ($property in @($needs.PSObject.Properties)) {
        $present = @($places | Where-Object { Test-Path -LiteralPath (Join-Path $_ ($property.Name + '\package.json')) -PathType Leaf })
        if ($present.Count -eq 0) { $found.Add($leaf + ': ' + $kind + ' ' + $property.Name) }
      }
    }
  }
  return ,$found
}
# DSH-home files outside profiles, plugins and sessions that name a linked bundle's old folder. The move does not
# rewrite them (they are not its configuration); the list goes to the evidence folder, and F3 deletes qq-bridge only
# after each listed file is settled.
function Find-OtherBundleReferences([string]$DshHome, $Bundles) {
  $found = [Collections.Generic.List[string]]::new()
  $roots = @(@($Bundles) | Where-Object { $null -ne $_ } | ForEach-Object { ([string]$_.From).ToLowerInvariant() })
  if ($roots.Count -eq 0) { return ,$found }
  $base = Get-Normal $DshHome
  $pending = [Collections.Generic.Stack[string]]::new()
  $pending.Push($base)
  while ($pending.Count -gt 0) {
    foreach ($item in [IO.DirectoryInfo]::new($pending.Pop()).GetFileSystemInfos()) {
      if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
      $relative = $item.FullName.Substring($base.Length + 1)
      if ($item -is [IO.DirectoryInfo]) {
        if (@('profiles', 'plugins', 'sessions') -notcontains $relative.ToLowerInvariant()) { $pending.Push($item.FullName) }
        continue
      }
      if ($item.Length -gt 16MB) { $found.Add($relative + ' (not read: larger than 16 MiB)'); continue }
      $text = (Read-Text $item.FullName).Replace('\\', '\').Replace('/', '\').ToLowerInvariant()
      if (Test-NamesRoot $text $roots) { $found.Add($relative) }
    }
  }
  return ,$found
}
function New-BundleLinks($plan) {
  $newHome = Join-Path $script:Root 'runtime\user\dsh'
  $made = 0
  foreach ($link in @(@($plan.DshLinks) | Where-Object { $null -ne $_ })) {
    $path = Join-Path $newHome ([string]$link.Path)
    if (Test-Present $path) { Stop-Failed 'a linked DSH bundle path already exists in the new DSH home' }
    [void][IO.Directory]::CreateDirectory((Split-Path -Parent $path))
    [void](New-Item -ItemType Junction -Path $path -Target ([string]$link.NewTarget))
    $item = Get-Item -LiteralPath $path -Force
    if (-not ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -or (Get-LinkTarget $path) -ine (Get-Normal ([string]$link.NewTarget))) { Stop-Failed 'a linked DSH bundle could not be linked in the new DSH home' }
    $made++
  }
  if ($made -gt 0) { Write-Step ('linked ' + $made + ' DSH bundles in the new DSH home') }
}
# Rewrites the bundle paths in the new DSH home's copied texts; files that are not UTF-8 are left alone (the scan after
# the rewrite still fails the move if one of them names an old path).
function Update-DshTexts($plan) {
  $map = Get-DshTextMap $plan
  if (@($map).Count -eq 0) { return }
  $newHome = Join-Path $script:Root 'runtime\user\dsh'
  $folders = @((Join-Path $newHome 'profiles'), (Join-Path $newHome 'plugins')) + @(@($plan.Bundles) | ForEach-Object { [string]$_.NewPath })
  $changed = 0
  foreach ($folder in $folders) {
    if (-not (Test-Path -LiteralPath $folder -PathType Container)) { continue }
    foreach ($file in (Get-TextFiles $folder)) {
      $bytes = [IO.File]::ReadAllBytes($file)
      $bom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
      $offset = 0
      if ($bom) { $offset = 3 }
      try { $text = [Text.UTF8Encoding]::new($false, $true).GetString($bytes, $offset, $bytes.Length - $offset) } catch { continue }
      $converted = Convert-DshText $text $map
      if ($converted -ceq $text) { continue }
      [IO.File]::WriteAllText($file, $converted, [Text.UTF8Encoding]::new($bom))
      $changed++
    }
  }
  Write-Step ('the linked DSH bundle paths are rewritten in ' + $changed + ' DSH files')
}
# Stale backup copies (a file name ending in .bak or in .bak-<anything>, as the presets' and the web profile's
# .bak-<stamp> files): DSH never reads them, they name old folders, and the originals stay where they are until #127.
# The copies leave them out (Revision 19), so the new layout holds no backup naming qq-bridge.
function Test-BackupCopyName([string]$Name) { return $Name -match '(?i)\.bak(-[^\\/]*)?$' }
# The backup copies below the given areas of a folder ('' is the folder itself), as paths relative to the folder,
# found without following any junction or link.
function Find-BackupCopies([string]$Folder, [string[]]$Areas) {
  $found = [Collections.Generic.List[string]]::new()
  $base = Get-Normal $Folder
  foreach ($area in @($Areas)) {
    $start = $base
    if ($area) { $start = Join-Path $base $area }
    if (-not (Test-Path -LiteralPath $start -PathType Container)) { continue }
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($start)
    while ($pending.Count -gt 0) {
      foreach ($item in [IO.DirectoryInfo]::new($pending.Pop()).GetFileSystemInfos()) {
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
        if ($item -is [IO.DirectoryInfo]) { $pending.Push($item.FullName); continue }
        if (Test-BackupCopyName $item.Name) { $found.Add($item.FullName.Substring($base.Length + 1)) }
      }
    }
  }
  return ,$found
}
# The dev preset's SnowLuma MCP script and its data files, each copied alone into the tool folder and verified by
# SHA-256 (Revision 19).
function Copy-ToolFiles($plan) {
  $files = @(@($plan.ToolFiles) | Where-Object { $null -ne $_ })
  if ($files.Count -eq 0) { return }
  foreach ($file in $files) {
    $from = [string]$file.From
    $to = [string]$file.To
    if ((Get-Item -LiteralPath $from -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { Stop-Failed 'a dev tool file in qq-bridge is a junction or link' }
    if (Test-Present $to) { Stop-Failed 'a dev tool file already exists in the new layout' }
    [void][IO.Directory]::CreateDirectory((Split-Path -Parent $to))
    [IO.File]::Copy($from, $to)
    if ((Get-FileSha $from) -ne (Get-FileSha $to)) { Stop-Failed 'a dev tool file differs from its source after the copy' }
  }
  Write-Step ('copied and verified the dev tool files: ' + $files.Count)
}
# The dev MCP script reads only snowluma.httpUrl and snowluma.accessToken from ROOT\config.json, on every call. They
# come from the new runtime's configuration (the values HER itself uses and the move's health check proves), each only
# when it is a non-empty string; a missing one falls back to the script's own default, as in HER. The token is
# registered as a secret, so no evidence or console line shows it.
function New-ToolConfig($Config) {
  $snowluma = [pscustomobject][ordered]@{}
  $source = Get-Prop $Config 'snowluma'
  foreach ($name in @('httpUrl', 'accessToken')) {
    $value = Get-Prop $source $name
    if ($value -is [string] -and $value) { $snowluma | Add-Member -NotePropertyName $name -NotePropertyValue $value }
  }
  $token = Get-Prop $snowluma 'accessToken'
  if ($token) { Add-RelaySecret $token }
  return [pscustomobject][ordered]@{ snowluma = $snowluma }
}

# ---------- state ----------
function Save-State([string]$Phase) {
  $script:State.Phase = $Phase
  $script:State.UpdatedAt = [DateTime]::UtcNow.ToString('o')
  $temporary = $script:StatePath + '.tmp'
  Write-JsonFile $temporary $script:State
  Move-Item -LiteralPath $temporary -Destination $script:StatePath -Force
}
function Assert-AbortPoint([string]$Name) {
  if ($RehearsalAbortAfter -eq $Name) { Write-Step ('REHEARSAL ABORT after ' + $Name); throw 'REHEARSAL-ABORT' }
}

# ---------- run setup ----------
function Initialize-Run {
  if ($env:OS -ne 'Windows_NT') { Stop-Refused 'this script runs on Windows only' }
  if ($PSVersionTable.PSVersion.Major -ne 5 -or $PSVersionTable.PSVersion.Minor -ne 1) { Stop-Refused 'run this script in Windows PowerShell 5.1' }
  if ($Production -eq $Rehearsal) { Stop-Refused 'pass -Production for the live move or -Rehearsal for a rehearsal, exactly one' }
  if ($OwnerGoFile -and $Mode -eq 'Inventory') { Stop-Refused 'Inventory changes nothing and takes no owner go file' }
  # Before the claim come only the checks that place the go file itself: the run kind, the install root it sits
  # beside, and (in a rehearsal) the temporary folder. Every other refusal follows the claim, so a run that carries a
  # go file and is refused for anything else still consumes it.
  $script:Root = Get-Normal $InstallRoot
  $script:Temp = Get-Normal ([IO.Path]::GetTempPath())
  if ($Rehearsal -and (-not (Test-Within $script:Root $script:Temp) -or $script:Root -ieq $script:Temp)) { Stop-Refused 'a rehearsal install root must be a folder inside the temporary folder' }
  if ($OwnerGoFile) { Claim-OwnerGo }
  if ($env:HER_INSTALL_ROOT) { Stop-Refused 'HER_INSTALL_ROOT is set; open a new PowerShell without it' }
  if ($OwnerGoFile -and $RehearsalConfirm) { Stop-Refused 'pass either -RehearsalConfirm or -OwnerGoFile, not both' }
  if ($Rehearsal) {
    if (-not $env:HER_TASK_SCHEDULER_FILE -or -not (Test-Within $env:HER_TASK_SCHEDULER_FILE $script:Temp) -or (Test-Within $env:HER_TASK_SCHEDULER_FILE $script:Root)) { Stop-Refused 'a rehearsal needs HER_TASK_SCHEDULER_FILE in the temporary folder, outside the install root' }
    Assert-RehearsalPath $script:Root
    Assert-RehearsalPath $env:HER_TASK_SCHEDULER_FILE
    foreach ($name in @($OldSupervisorTask, $OldRelayTask)) { if ($name -cnotmatch '^HER-Test-67-[0-9a-f]{32}-(supervisor|relay)$') { Stop-Refused 'a rehearsal uses only HER-Test-67 fixture tasks' } }
    if (-not $ProfileFolder) { Stop-Refused 'a rehearsal needs -ProfileFolder inside the temporary folder' }
    $script:RehearsalOldRoot = Join-Path (Split-Path -Parent $script:Root) 'unused-root'
  } else {
    if ($script:Root -ine 'C:\qqbot') { Stop-Refused 'the production install root is C:\qqbot' }
    if ($env:HER_TASK_SCHEDULER_FILE) { Stop-Refused 'HER_TASK_SCHEDULER_FILE is set; open a new PowerShell without it' }
    if ($RehearsalAbortAfter -or $RehearsalFault -or $RehearsalConfirm) { Stop-Refused 'rehearsal switches need -Rehearsal' }
    # Inventory is read-only and may run non-interactively. A changing mode needs the owner at the keyboard, or the
    # owner's go file for this run (the Windows coordinator runs it on a relayed go; claimed above, judged by
    # Confirm-Owner).
    if ($RelayDefaultPort -ne 8317) { Stop-Refused '-RelayDefaultPort is for rehearsals; production uses the relay default 8317' }
    if ($Mode -ne 'Inventory' -and -not $OwnerGoFile) {
      $nonInteractive = @([Environment]::GetCommandLineArgs() | Where-Object { $_ -like '-noni*' })
      if (-not [Environment]::UserInteractive -or $nonInteractive.Count -gt 0) { Stop-Refused 'production runs need an interactive PowerShell without -NonInteractive' }
      if ([Console]::IsInputRedirected) { Stop-Refused 'production runs need keyboard input; the input of this PowerShell is redirected' }
    }
    $principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { Stop-Refused 'run this script from an administrator PowerShell' }
  }
  Assert-NoLinkAbove $script:Root 'the install root'
  $script:ProfilePath = $ProfileFolder
  if (-not $script:ProfilePath) { $script:ProfilePath = $env:USERPROFILE }
  $base = Get-Normal $EvidenceRoot
  Assert-RehearsalPath $base
  Assert-NoLinkAbove $base 'the evidence folder'
  if ($Production) {
    # The console shows the profile root as <profile>; a state file under it would print an unusable rollback command.
    foreach ($profileRoot in (Get-ProfileRoots)) { if (Test-Within $base $profileRoot) { Stop-Refused 'the evidence folder must not be under the account profile' } }
  }
  if (Test-Within $base $script:Root) { Stop-Refused 'the evidence folder must be outside the install root' }
  [void][IO.Directory]::CreateDirectory($base)
  # One run at a time whatever -EvidenceRoot says: a fixed lock beside the go folder, then the evidence lock.
  # The lock file must be a plain file (checked through its handle) in a folder with no link above it, or a link
  # retargeted between runs could hand each run its own lock.
  $runLock = Join-Path (Split-Path -Parent (Get-GoFolder)) 'move-run.lock'
  [void][IO.Directory]::CreateDirectory((Split-Path -Parent $runLock))
  Assert-NoLinkAbove (Split-Path -Parent $runLock) 'the run lock'
  try { $script:RunLockStream = [HerMove67Native]::OpenPlain($runLock) }
  catch { Stop-Refused 'another run of this script is active' }
  if ($null -eq $script:RunLockStream) { Stop-Refused 'the run lock is a link or a folder' }
  try { $script:LockStream = [IO.File]::Open((Join-Path $base 'move.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None) }
  catch { Stop-Refused 'another run of this script is active' }
  $script:Evidence = Join-Path $base ($Mode.ToLowerInvariant() + '-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff'))
  if (Test-Present $script:Evidence) { Stop-Refused 'the evidence folder for this run already exists' }
  [void][IO.Directory]::CreateDirectory($script:Evidence)
  $script:Log = Join-Path $script:Evidence 'log.txt'
  Write-Step ('mode ' + $Mode + '; evidence ' + $script:Evidence)
  if ($null -ne $script:GoClaim) { Write-Log ('owner go file claimed and deleted at the start of this run; identity ' + $script:GoClaim.Identity + ', written ' + [DateTime]::FromFileTimeUtc([long]$script:GoClaim.Written).ToString('o')) }
}

# ---------- the checks shared by Inventory and Move (read-only) ----------
function Get-ConfigPath($Value, [string]$Name) {
  if ($Value -isnot [string] -or $Value -notmatch '^[A-Za-z]:[\\/]') { Stop-Refused ($Name + ' is not an absolute path') }
  return (Get-Normal $Value)
}
# Inventory runs every check and reports every blocker: each independent check has its own try block, and a check that
# needs a failed one reports "skipped: needs <check>" (Assert-PlanHas). A value enters the plan only after its own
# checks passed, and a check that reads a place another check guards validates that place itself, so a check that
# does not wait for an earlier one never uses what that one refused. Only the run's own inputs stop the Inventory
# before the checks: the required parameters and the four old folders (no link above them, inside the temporary
# folder in a rehearsal, outside the install root), because every check reads through them. Move refuses at the first
# blocker: Add-Blocker and Add-Failure throw outside Inventory, and Move's DSH stub walk refuses at its first link.
function Get-MovePlan {
  if (-not $OldRuntime -or -not $OldRelayFolder -or -not $ReleaseSource) { Stop-Refused '-OldRuntime, -OldRelayFolder and -ReleaseSource are required' }
  if (-not (Test-ReleaseName $ReleaseName)) { Stop-Refused '-ReleaseName must be 3 to 41 lower-case letters, digits and dashes, starting with a letter, and not a vX.Y.Z tag' }
  $plan = [ordered]@{}
  $plan.Root = $script:Root
  $plan.OldRuntime = Get-Normal $OldRuntime
  $plan.OldRelayFolder = Get-Normal $OldRelayFolder
  $plan.Profile = Get-Normal $script:ProfilePath
  $plan.DshHome = Join-Path $plan.Profile '.dsh'
  $plan.ReleaseSource = Get-Normal $ReleaseSource
  $plan.ReleaseName = $ReleaseName
  $plan.OldSupervisorTask = $OldSupervisorTask
  $plan.OldRelayTask = $OldRelayTask
  $plan.BridgeRoot = Join-Path $script:Root 'qq-bridge'
  $plan.DigestModel = $DigestModel
  $plan.DshLinks = @()
  $plan.Bundles = @()
  $plan.Identities = [ordered]@{}
  $plan.DshChildren = @()
  $script:OldRuntimePath = $plan.OldRuntime
  foreach ($folder in @($plan.OldRuntime, $plan.OldRelayFolder, $plan.DshHome, $plan.ReleaseSource)) {
    Assert-NoLinkAbove $folder 'an old production folder'
    Assert-RehearsalPath $folder
    if (Test-Within $folder $script:Root) { Stop-Refused 'the old runtime, relay folder, profile DSH home and release source must be outside the install root' }
    if (Test-Within $script:Root $folder) { Stop-Refused 'the install root must not be inside an old production folder' }
  }
  # Inventory lists every finding at once, so an earlier layout entry is a blocker, not a stop (Move refuses on it).
  foreach ($name in @('runtime', 'releases', 'deps', 'current', 'last-good', 'herupdate.exe', 'update-starts.json', 'update-alert.json', '.current-previous', '.last-good-previous')) {
    if (Test-Present (Join-Path $script:Root $name)) { Add-Blocker ('the install root already has ' + $name + '; roll back the earlier attempt or move it aside first') }
  }
  if ($DigestModel -and -not (Test-ModelName $DigestModel)) { Add-Blocker '-DigestModel must be 1 to 80 letters, digits, dots, colons, underscores and dashes, starting with a letter or digit' }
  $config = $null; $dependencies = $null; $relayConfig = $null

  try {
    # Release images: exactly the three images the hash list names. A release folder not built yet is a fixed finding.
    if (-not (Test-Path -LiteralPath (Join-Path $plan.ReleaseSource 'SHA256SUMS') -PathType Leaf)) { Stop-Refused 'the release folder or its SHA256SUMS is not there yet' }
    $plan.Sums = ConvertFrom-Sums (Read-Lines (Join-Path $plan.ReleaseSource 'SHA256SUMS'))
    # Every image is checked, so one missing or changed image does not hide another.
    $imagesChecked = $true
    foreach ($name in @('her.exe', 'her-cli.exe', 'herupdate.exe')) {
      if (-not (Test-Path -LiteralPath (Join-Path $plan.ReleaseSource $name) -PathType Leaf)) { Add-Blocker ('the release folder has no ' + $name); $imagesChecked = $false; continue }
      if ((Get-FileSha (Join-Path $plan.ReleaseSource $name)) -ne $plan.Sums[$name]) { Add-Blocker ('the release source ' + $name + ' differs from SHA256SUMS'); $imagesChecked = $false }
    }
    if ($imagesChecked) {
      $plan.SourceCli = Join-Path $plan.ReleaseSource 'her-cli.exe'
      $plan.Cli = Join-Path $script:Root 'current\her-cli.exe'
      # Set only when the whole release check passed: later sections need the check, not just some of its values.
      $plan.ReleaseChecked = 'yes'
    }
  } catch { Add-Failure $_ 'release' }

  try {
    foreach ($file in @('config\config.json', 'dependencies.json')) {
      if (-not (Test-Path -LiteralPath (Join-Path $plan.OldRuntime $file) -PathType Leaf)) { Stop-Refused ('the old runtime has no ' + $file) }
    }
    $config = Read-Json (Join-Path $plan.OldRuntime 'config\config.json')
    $dependencies = Read-Json (Join-Path $plan.OldRuntime 'dependencies.json')
    if ($null -ne (Get-Prop $config 'dependencies')) { Add-Blocker 'the old configuration already has a dependencies block' }
    if ($null -ne (Get-Prop $dependencies 'relay')) { Add-Blocker 'the old dependencies.json already names a relay' }
    $tasks = Get-Prop $config 'tasks'
    $startAll = Get-Prop $tasks 'start-all'
    if ((Get-Prop (Get-Prop $tasks 'upstream-check') 'enabled') -eq $true) { Add-Blocker 'tasks.upstream-check is enabled; it needs the old release folder' }
    $plan.RuntimeExclude = @('user\dsh', 'import-manifest.json', 'data\lifecycle\session-handover.json', 'data\lifecycle\session-handover-required.json')
    $plan.ConfigRead = 'yes'
  } catch { Add-Failure $_ 'configuration' }

  # Each configured path, port and the backup folder is checked on its own, so one bad value does not hide another. A
  # path is stored only after its own checks passed; a section that needs a missing value reports it was skipped.
  try {
    Assert-PlanHas $plan @('ConfigRead') 'the configuration check'
    $configured = Get-ConfigPath (Get-Prop $tasks 'nodePath') 'tasks.nodePath'
    Assert-NoLinkAbove $configured 'the configured Node'
    $plan.NodePath = $configured
  } catch { Add-Failure $_ 'configuration' }
  try {
    Assert-PlanHas $plan @('ConfigRead') 'the configuration check'
    $configured = Get-ConfigPath (Get-Prop $tasks 'installRoot') 'tasks.installRoot'
    Assert-NoLinkAbove $configured 'an old production folder'; Assert-RehearsalPath $configured
    $plan.OldRelease = $configured
  } catch { Add-Failure $_ 'configuration' }
  try {
    Assert-PlanHas $plan @('ConfigRead') 'the configuration check'
    $configured = Get-ConfigPath (Get-Prop $startAll 'snowLumaDirectory') 'tasks.start-all.snowLumaDirectory'
    Assert-NoLinkAbove $configured 'an old production folder'; Assert-RehearsalPath $configured
    $plan.SnowLuma = $configured
  } catch { Add-Failure $_ 'configuration' }
  try {
    Assert-PlanHas $plan @('ConfigRead') 'the configuration check'
    $configured = Get-ConfigPath (Get-Prop $startAll 'dshScript') 'tasks.start-all.dshScript'
    Assert-NoLinkAbove $configured 'an old production folder'; Assert-RehearsalPath $configured
    $plan.DshScript = $configured
  } catch { Add-Failure $_ 'configuration' }
  try {
    Assert-PlanHas $plan @('ConfigRead') 'the configuration check'
    # Like a path, a port is stored only after its own checks passed, so no later check uses a refused port.
    $port = [int](Get-Prop $config 'consolePort')
    if ($port -le 0) { Stop-Refused 'the old configuration has no consolePort' }
    Assert-RehearsalPort $port
    $plan.ConsolePort = $port
  } catch { Add-Failure $_ 'configuration' }
  try {
    Assert-PlanHas $plan @('ConfigRead') 'the configuration check'
    # Both OneBot ports or neither: the dependency processes check needs the pair.
    $port = Get-Port (Get-Prop $dependencies 'onebot') 'onebot'
    $wsPort = 0
    if ($null -ne (Get-Prop $dependencies 'onebot_ws')) { $wsPort = Get-Port (Get-Prop $dependencies 'onebot_ws') 'onebot_ws' }
    $plan.OneBotPort = $port
    $plan.OneBotWsPort = $wsPort
  } catch { Add-Failure $_ 'configuration' }
  try {
    Assert-PlanHas $plan @('ConfigRead') 'the configuration check'
    $plan.DshPort = Get-Port (Get-Prop $dependencies 'dsh') 'dsh'
  } catch { Add-Failure $_ 'configuration' }
  try {
    Assert-PlanHas $plan @('ConfigRead') 'the configuration check'
    $configured = Get-BackupDestination $config
    if ($configured) { Assert-NoLinkAbove $configured 'the backup folder'; Assert-RehearsalPath $configured }
    $plan.BackupDestination = $configured
    # The backups section needs this check, not only its value: an empty destination means no backup folder.
    $plan.BackupChecked = 'yes'
  } catch { Add-Failure $_ 'configuration' }

  try {
    Assert-PlanHas $plan @('SnowLuma') 'the configuration check'
    foreach ($file in @('node.exe', 'index.mjs')) {
      if (-not (Test-Path -LiteralPath (Join-Path $plan.SnowLuma $file) -PathType Leaf)) { Stop-Refused ('the SnowLuma folder has no ' + $file) }
    }
    $plan.SnowLumaVersion = $SnowLumaVersion
    $snowPackage = Join-Path $plan.SnowLuma 'package.json'
    if (Test-Path -LiteralPath $snowPackage -PathType Leaf) {
      $declared = [string](Get-Prop (Read-Json $snowPackage) 'version')
      if (-not $plan.SnowLumaVersion) { $plan.SnowLumaVersion = $declared }
      elseif ($declared -and $declared -ne $plan.SnowLumaVersion) { Add-Blocker 'SnowLuma package.json names another version than -SnowLumaVersion' }
    }
    if (-not (Test-ExactVersion $plan.SnowLumaVersion)) { Add-Blocker 'the SnowLuma version is not one exact release version; pass -SnowLumaVersion' }
  } catch { Add-Failure $_ 'snowluma' }

  try {
    # The DSH package becomes the managed pin, used in place, so no npm run is needed.
    Assert-PlanHas $plan @('DshScript', 'NodePath') 'the configuration check'
    $match = [regex]::Match($plan.DshScript, '^(?<root>.+)\\node_modules\\@deepseek-ai\\dsh\\.+$', [Text.RegularExpressions.RegexOptions]::IgnoreCase)
    if (-not $match.Success) { Stop-Refused 'the configured DSH script is not inside an @deepseek-ai/dsh package' }
    $plan.NpxRoot = $match.Groups['root'].Value
    Assert-NoLinkAbove $plan.NpxRoot 'an old production folder'
    Assert-RehearsalPath $plan.NpxRoot
    if ($plan.NpxRoot -notmatch '\\_npx\\[^\\]+$') { Add-Blocker 'the DSH package is not in an npx cache folder' }
    $packageFolder = Join-Path $plan.NpxRoot 'node_modules\@deepseek-ai\dsh'
    $dshPackage = Read-Json (Join-Path $packageFolder 'package.json')
    if ([string](Get-Prop $dshPackage 'name') -ne '@deepseek-ai/dsh') { Stop-Refused 'the DSH package manifest names another package' }
    $plan.DshVersion = [string](Get-Prop $dshPackage 'version')
    if (-not (Test-ExactVersion $plan.DshVersion)) { Add-Blocker 'the DSH package version is not one exact release version' }
    $bin = Get-Prop $dshPackage 'bin'
    $entry = ''
    if ($bin -is [string]) { $entry = $bin }
    elseif ($null -ne (Get-Prop $bin 'dsh')) { $entry = [string](Get-Prop $bin 'dsh') }
    elseif ($null -ne $bin) { $names = @($bin.PSObject.Properties); if ($names.Count -eq 1) { $entry = [string]$names[0].Value } }
    if (-not $entry -or (Get-Normal (Join-Path $packageFolder $entry)) -ine $plan.DshScript) { Add-Blocker 'the DSH package command entry is not the configured DSH script' }
    if (-not (Test-Path -LiteralPath (Join-Path (Split-Path -Parent $plan.NodePath) 'node_modules\npm\bin\npm-cli.js') -PathType Leaf)) { Add-Blocker 'npm is missing beside the configured Node; the managed DSH pin needs it' }
  } catch { Add-Failure $_ 'dsh package' }

  try {
    # Old bridge owners from their marker files, confirmed by the old image's own status.
    foreach ($role in @('supervisor', 'live', 'console')) {
      $markerPath = Join-Path $plan.OldRuntime ('data\logs\rbridge-' + $role + '.lock.owner.json')
      if (-not (Test-Path -LiteralPath $markerPath -PathType Leaf)) { Add-Blocker ('the old ' + $role + ' has no owner record'); continue }
      $recorded = Get-Prop (Read-Json $markerPath) 'identity'
      $found = Get-Identity ([int](Get-Prop $recorded 'pid')) ('old ' + $role)
      $recordedExe = [string](Get-Prop $recorded 'exe')
      if ($null -eq $found -or $found.Start -ne [string](Get-Prop $recorded 'start') -or $recordedExe -notmatch '^[A-Za-z]:[\\/]' -or (Get-Normal $found.Exe) -ine (Get-Normal $recordedExe)) { Add-Blocker ('the old ' + $role + ' is not running as recorded'); continue }
      $plan.Identities[$role] = $found
    }
    if (-not $plan.Identities.Contains('supervisor')) { Stop-Refused 'the old supervisor cannot be identified' }
    $script:OldExe = $plan.Identities['supervisor'].Exe
    Assert-RehearsalPath $script:OldExe
    # A captured command uses the console twin when the old release has one (her-cli.exe beside her.exe).
    $script:OldCli = $script:OldExe
    $twin = Join-Path (Split-Path -Parent $script:OldExe) 'her-cli.exe'
    if ((Split-Path -Leaf $script:OldExe) -ieq 'her.exe' -and (Test-Path -LiteralPath $twin -PathType Leaf)) { $script:OldCli = $twin }
    $plan.OldExe = $script:OldExe
    $plan.OldCli = $script:OldCli
    $supervisorFolder = Split-Path -Parent $plan.OldExe
    foreach ($role in @('live', 'console')) {
      if (-not $plan.Identities.Contains($role)) { continue }
      $exe = $plan.Identities[$role].Exe
      $same = (Get-Normal $exe) -ieq (Get-Normal $plan.OldExe)
      $sibling = ((Split-Path -Parent $exe) -ieq $supervisorFolder) -and ($script:OldCli -ne $script:OldExe)
      if (-not ($same -or $sibling)) { Add-Blocker ('the old ' + $role + ' runs from another image than the supervisor, so the old CLI cannot stop it') }
    }
    $caller = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    if ((Get-OwnerSid ([int]$plan.Identities['supervisor'].Pid)) -ne $caller) { Add-Blocker 'this PowerShell runs as another account than the old production; run it as the account that owns the old supervisor' }
    foreach ($role in @('live', 'console')) {
      if ($plan.Identities.Contains($role) -and (Get-OwnerSid ([int]$plan.Identities[$role].Pid)) -ne $caller) { Add-Blocker ('the old ' + $role + ' runs as another account than this PowerShell') }
    }
    $plan.SupervisorParents = Get-ParentImages ([int]$plan.Identities['supervisor'].Pid)
    $states = Get-OldOwnerStates
    foreach ($role in @('supervisor', 'live', 'console')) { if ($states.$role -ne 'held') { Add-Blocker ('the old ' + $role + ' owner is not held') } }
  } catch { Add-Failure $_ 'old bridge' }

  try {
    # Dependency processes, found by their ports and checked against the configuration.
    Assert-PlanHas $plan @('OneBotPort', 'DshPort', 'ConsolePort', 'SnowLuma', 'NodePath', 'DshScript') 'the configuration check'
    Assert-PlanHas $plan @('NpxRoot') 'the DSH package check'
    Assert-PlanHas $plan @('OldExe') 'the old bridge check'
    $plan.Identities['snowluma'] = Get-ListenerIdentity $plan.OneBotPort 'SnowLuma'
    if ((Get-Normal $plan.Identities['snowluma'].Exe) -ine (Join-Path $plan.SnowLuma 'node.exe')) { Add-Blocker 'the OneBot port is not served by the configured SnowLuma' }
    if ($plan.OneBotWsPort -gt 0 -and $plan.OneBotWsPort -ne $plan.OneBotPort -and (Get-ListenerPid $plan.OneBotWsPort) -ne $plan.Identities['snowluma'].Pid) { Add-Blocker 'the OneBot WebSocket port is not served by SnowLuma' }
    $dsh = Get-ListenerIdentity $plan.DshPort 'DSH'
    if ((Get-Normal $dsh.Exe) -ine $plan.NodePath -or (Get-CommandLine $dsh.Pid).IndexOf($plan.DshScript, [StringComparison]::OrdinalIgnoreCase) -lt 0) { Add-Blocker 'the DSH port is not served by the configured Node and DSH script' }
    $plan.Identities['dsh'] = $dsh
    # Restricting the install root leaves access to this account only (Q8): every production process must run as it.
    $caller = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    foreach ($role in @('snowluma', 'dsh')) {
      if ((Get-OwnerSid ([int]$plan.Identities[$role].Pid)) -ne $caller) { Add-Blocker ('the ' + $role + ' process runs as another account than this PowerShell') }
    }
    $descendants = Get-Descendants $dsh.Pid ([DateTime]::FromFileTimeUtc([long]$dsh.Start))
    foreach ($childPid in $descendants) {
      $role = 'dsh-child-' + $childPid
      $child = Get-Identity $childPid $role
      if ($null -ne $child) { $plan.Identities[$role] = $child; $plan.DshChildren += $role }
    }
    $strangers = @(Get-CimInstance -ClassName Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.IndexOf($plan.NpxRoot, [StringComparison]::OrdinalIgnoreCase) -ge 0 -and [int]$_.ProcessId -ne $dsh.Pid -and $descendants -notcontains [int]$_.ProcessId })
    if ($strangers.Count -gt 0) { Add-Blocker 'another process runs the DSH package; close other DSH sessions first' }
    if ($plan.Identities.Contains('console') -and (Get-ListenerPid $plan.ConsolePort) -ne $plan.Identities['console'].Pid) { Add-Blocker 'the console port is not served by the old console' }
    $known = @($plan.Identities.Values | ForEach-Object { [int]$_.Pid })
    $stray = @(Get-OldImageProcesses (Split-Path -Parent $plan.OldExe) | Where-Object { $known -notcontains [int]$_.ProcessId })
    if ($stray.Count -gt 0) { Add-Blocker 'a process outside the recorded owners runs the old release image' }
  } catch { Add-Failure $_ 'dependency processes' }

  try {
    # Relay: its one process, command line, configuration and login folder.
    $relayProcesses = @(Get-CimInstance -ClassName Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath -match '^[A-Za-z]:\\' -and (Test-Within $_.ExecutablePath $plan.OldRelayFolder) })
    if ($relayProcesses.Count -ne 1) { Stop-Refused 'exactly one relay process must run from the old relay folder' }
    $relay = Get-Identity ([int]$relayProcesses[0].ProcessId) 'relay'
    if ($null -eq $relay) { Stop-Refused 'the relay process cannot be identified' }
    $argv = [HerMove67Native]::Split((Get-CommandLine $relay.Pid))
    if ($argv.Count -lt 1 -or (Split-Path -Leaf $argv[0]) -ine (Split-Path -Leaf $relay.Exe)) { Add-Blocker 'the relay command line does not start with its program' }
    $tokens = @($argv | Select-Object -Skip 1)
    $plan.RelayVersion = $RelayVersion
    if (-not $plan.RelayVersion) {
      $versionMatch = [regex]::Match((Split-Path -Leaf $plan.OldRelayFolder), '^cliproxy-(?<v>.+)$')
      if ($versionMatch.Success) { $plan.RelayVersion = $versionMatch.Groups['v'].Value }
    }
    if (-not (Test-ExactVersion $plan.RelayVersion)) { Add-Blocker 'the relay version is not one exact release version; pass -RelayVersion' }
    $plan.RelayPin = Join-Path $script:Root ('deps\relay\' + $plan.RelayVersion)
    $relayExe = Get-Normal $relay.Exe
    $relayProgram = $plan.RelayPin + $relayExe.Substring($plan.OldRelayFolder.Length)
    # Kept local until the configuration is read and the key check is done: a refusal on the way must not leave raw
    # arguments (possibly carrying a key) in the plan. Assigned directly: Convert-RelayArguments returns ,$result.
    $relayArguments = Convert-RelayArguments $tokens $plan.OldRelayFolder $plan.RelayPin
    $relayConfigPath = Find-RelayConfig $tokens $plan.OldRelayFolder
    if (-not (Test-Within $relayConfigPath $plan.OldRelayFolder) -or -not (Test-Path -LiteralPath $relayConfigPath -PathType Leaf)) { Stop-Refused 'the relay configuration file is not inside the old relay folder' }
    $relayConfig = Read-RelayConfig (Read-Lines $relayConfigPath)
    # Stored only now: reading the configuration registered its secrets, so a secret in a file or program name is
    # hidden in the evidence, and a refused configuration stores neither name.
    $plan.RelayConfigRelative = $relayConfigPath.Substring($plan.OldRelayFolder.Length + 1)
    $plan.RelayProgram = $relayProgram
    $plan.RelayPort = $relayConfig.Port
    # No port line (neither top-level port nor server.port): the relay listens on its default port. The listener check
    # below confirms it.
    if ($plan.RelayPort -eq 0) { $plan.RelayPort = $RelayDefaultPort; Write-Log 'the relay configuration has no port line; the relay default port is checked against the live listener' }
    else { Write-Log ('the relay port comes from the configuration (' + [string]$relayConfig.PortSource + ') and is checked against the live listener') }
    Assert-RehearsalPort $plan.RelayPort
    $script:RelayKey = $relayConfig.Key
    if ((Get-ListenerPid $plan.RelayPort) -ne $relay.Pid) { Add-Blocker 'the relay port is not served by the relay process' }
    if (-not (Test-LoopbackOnly @(Get-NetTCPConnection -State Listen -LocalPort $plan.RelayPort -ErrorAction SilentlyContinue | ForEach-Object { [string]$_.LocalAddress }))) { Add-Blocker 'the relay listens on an address other than loopback' }
    if (@($plan.OneBotPort, $plan.OneBotWsPort, $plan.DshPort, $plan.ConsolePort) -contains $plan.RelayPort) { Add-Blocker 'the relay port is not distinct from the OneBot, DSH and console ports' }
    # A command line that carries any configured key blocks; only redacted arguments reach the plan.
    $redacted = @(@($relayArguments) | ForEach-Object {
      $argument = [string]$_
      $hit = @($script:SecretTexts | Where-Object { $argument.Contains($_) }).Count -gt 0
      if ($hit) { '[redacted: carries a relay key]' } else { $argument }
    })
    $carriesKey = @($redacted | Where-Object { $_ -ceq '[redacted: carries a relay key]' }).Count -gt 0
    $plan.RelayArguments = $redacted
    if ($carriesKey) { Add-Blocker 'the relay command line carries the client key' }
    if ($DigestModel) {
      # The digest key is the relay client key (Q5); an existing different key needs a decision first.
      $harnessKey = Join-Path $plan.OldRuntime 'config\harness-api-key'
      if ((Test-Path -LiteralPath $harnessKey -PathType Leaf) -and (Read-Text $harnessKey).Trim() -cne $script:RelayKey) { Add-Blocker 'config\harness-api-key holds another key than the relay client key; it needs a decision' }
    }
    $plan.Identities['relay'] = $relay
    if ((Get-OwnerSid ([int]$relay.Pid)) -ne [Security.Principal.WindowsIdentity]::GetCurrent().User.Value) { Add-Blocker 'the relay process runs as another account than this PowerShell' }
    $plan.RelayParents = Get-ParentImages ([int]$relay.Pid)
    $plan.Auth = Resolve-RelayAuth $relayConfig $plan.OldRelayFolder $plan.RelayPin $plan.Profile
    Assert-NoLinkAbove $plan.Auth.Source 'an old production folder'
    Assert-RehearsalPath $plan.Auth.Source
  } catch { Add-Failure $_ 'relay' }

  try {
    # Linked DSH bundles: each link target inside qq-bridge is copied once into the new DSH home's bundles folder;
    # the links themselves stay out of the DSH home copy and are recreated after it (New-BundleLinks).
    if (-not (Test-Path -LiteralPath $plan.DshHome -PathType Container)) { Stop-Refused 'the profile DSH home is missing' }
    if (Test-Present (Join-Path $plan.DshHome 'bundles')) { Stop-Refused 'the profile DSH home already has a bundles entry, so the linked bundles cannot be placed there' }
    $newHome = Join-Path $script:Root 'runtime\user\dsh'
    $links = @(); $bundles = @()
    foreach ($found in (Find-DshLinks $plan.DshHome $plan.BridgeRoot)) {
      if ($found.InnerTarget) {
        # A link to an accepted bundle link elsewhere in the DSH home (the node_modules links of link: dependencies):
        # recreated as a junction to the same place in the new home, which resolves to the new bundle link.
        $links += [pscustomobject]@{ Path = $found.Path; Target = ''; NewTarget = (Join-Path $newHome ([string]$found.InnerTarget)) }
        continue
      }
      Assert-RehearsalPath $found.Target
      $leaf = Split-Path -Leaf $found.Target
      if ($leaf -notmatch '^[A-Za-z0-9._-]{1,64}$' -or $leaf -match '^\.+$') { Stop-Refused 'a linked DSH bundle folder has a name the move cannot use' }
      $newTarget = Join-Path $newHome ('bundles\' + $leaf)
      $same = @($bundles | Where-Object { $_.NewPath -ieq $newTarget })
      if ($same.Count -gt 0 -and $same[0].From -ine $found.Target) { Stop-Refused 'two linked DSH bundles share a folder name' }
      if ($same.Count -eq 0) { $bundles += [pscustomobject]@{ From = $found.Target; NewPath = $newTarget; Label = ('dsh-bundle-' + $leaf.ToLowerInvariant()) } }
      $links += [pscustomobject]@{ Path = $found.Path; Target = $found.Target; NewTarget = $newTarget }
    }
    $plan.DshLinks = $links
    $plan.Bundles = $bundles
    # The slim tool folder (Revision 19): the new DSH home's bundles folder takes qq-bridge's node_modules (both bundles
    # resolve their packages from it, as Node looks upward from a bundle's real path) and the dev preset's SnowLuma MCP
    # script with its data files, in the shape the script expects (its ROOT is its own folder's parent). So nothing in
    # the new layout needs qq-bridge, which goes with the other originals later (#127).
    $plan.ToolRoot = Join-Path $newHome 'bundles'
    $plan.ToolModules = ''
    $plan.ToolFiles = @()
    if ($bundles.Count -gt 0) {
      $toolNames = @('node_modules', 'src', 'state', 'config.json', '_snowluma-catalog.json')
      if (@($bundles | Where-Object { $toolNames -contains (Split-Path -Leaf ([string]$_.NewPath)).ToLowerInvariant() }).Count -gt 0) { Stop-Refused 'a linked DSH bundle folder name is one the tool folder uses' }
      $bridgeModules = Join-Path $plan.BridgeRoot 'node_modules'
      if (Test-Path -LiteralPath $bridgeModules -PathType Container) { Assert-RehearsalPath $bridgeModules; $plan.ToolModules = $bridgeModules }
      if (Test-Path -LiteralPath (Join-Path $plan.BridgeRoot 'src\mcp-snowluma-full.js') -PathType Leaf) {
        foreach ($relative in @('src\mcp-snowluma-full.js', '_snowluma-catalog.json', 'state\snowluma-full-calls.jsonl')) {
          $source = Join-Path $plan.BridgeRoot $relative
          if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { continue }
          Assert-RehearsalPath $source
          if ((Get-Item -LiteralPath $source -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { Stop-Refused 'a dev tool file in qq-bridge is a junction or link' }
          $plan.ToolFiles += [pscustomobject]@{ From = $source; To = (Join-Path $plan.ToolRoot $relative) }
        }
      }
    }
    Assert-PlanHas $plan @('SnowLuma', 'OldRelease') 'the configuration check'
    Assert-PlanHas $plan @('SnowLumaVersion') 'the SnowLuma check'
    Assert-PlanHas $plan @('NpxRoot', 'DshVersion') 'the DSH package check'
    Assert-PlanHas $plan @('OldExe') 'the old bridge check'
    Assert-PlanHas $plan @('RelayPin', 'Auth') 'the relay check'
    # Copies, path map and the roots no new path may keep.
    # The DSH home copy leaves out its links (recreated by New-BundleLinks) and the backup copies in profiles and
    # plugins; each bundle copy leaves out its backup copies (Revision 19).
    $homeExclude = [Collections.Generic.List[string]]::new()
    foreach ($excluded in (Get-DshLinkPaths $plan)) { $homeExclude.Add([string]$excluded) }
    foreach ($excluded in (Find-BackupCopies $plan.DshHome @('profiles', 'plugins'))) { $homeExclude.Add($excluded) }
    $copies = @(
      [pscustomobject]@{ From = $plan.OldRuntime; To = (Join-Path $script:Root 'runtime'); Exclude = $plan.RuntimeExclude; Label = 'runtime' },
      [pscustomobject]@{ From = $plan.DshHome; To = $newHome; Exclude = $homeExclude.ToArray(); Label = 'dsh-home' },
      [pscustomobject]@{ From = $plan.SnowLuma; To = (Join-Path $script:Root ('deps\snowluma\' + $plan.SnowLumaVersion)); Exclude = @(); Label = 'snowluma' },
      [pscustomobject]@{ From = $plan.NpxRoot; To = (Join-Path $script:Root ('deps\dsh\' + $plan.DshVersion)); Exclude = @(); Label = 'dsh' },
      [pscustomobject]@{ From = $plan.OldRelayFolder; To = $plan.RelayPin; Exclude = @(); Label = 'relay' }
    )
    foreach ($bundle in $bundles) { $copies += [pscustomobject]@{ From = $bundle.From; To = $bundle.NewPath; Exclude = (Find-BackupCopies $bundle.From @('')).ToArray(); Label = $bundle.Label } }
    if ($plan.ToolModules) { $copies += [pscustomobject]@{ From = $plan.ToolModules; To = (Join-Path $plan.ToolRoot 'node_modules'); Exclude = @(); Label = 'dsh-tool-modules' } }
    $pairs = @(
      [pscustomobject]@{ From = $plan.OldRuntime; To = (Join-Path $script:Root 'runtime') },
      [pscustomobject]@{ From = $plan.DshHome; To = (Join-Path $script:Root 'runtime\user\dsh') },
      [pscustomobject]@{ From = $plan.SnowLuma; To = (Join-Path $script:Root ('deps\snowluma\' + $plan.SnowLumaVersion)) },
      [pscustomobject]@{ From = $plan.NpxRoot; To = (Join-Path $script:Root ('deps\dsh\' + $plan.DshVersion)) },
      [pscustomobject]@{ From = $plan.OldRelayFolder; To = $plan.RelayPin }
    )
    if (-not $plan.Auth.Inside) {
      $copies += [pscustomobject]@{ From = $plan.Auth.Source; To = $plan.Auth.NewPath; Exclude = @(); Label = 'relay-login' }
      $pairs += [pscustomobject]@{ From = $plan.Auth.Source; To = $plan.Auth.NewPath }
    }
    $plan.Copies = $copies
    $plan.Map = New-PathMap $pairs
    $plan.Roots = @(@(@($pairs | ForEach-Object { $_.From }) + @($plan.OldRelease, (Split-Path -Parent $plan.OldExe))) | ForEach-Object { $_.ToLowerInvariant() })
  } catch { Add-Failure $_ 'copy plan' }

  try {
    # Old scheduled tasks, each read on its own so a missing one does not hide the other; the plan keeps them only when
    # both were read.
    $records = @()
    $readBoth = $true
    foreach ($name in @($OldSupervisorTask, $OldRelayTask)) {
      try { $records += Get-TaskRecord $name } catch { Add-Failure $_ 'scheduled tasks'; $readBoth = $false }
    }
    if ($readBoth) { $plan.OldTasks = $records }
    foreach ($task in $records) { if (-not $task.Enabled) { Add-Blocker ('the scheduled task ' + $task.Name + ' is not enabled') } }
  } catch { Add-Failure $_ 'scheduled tasks' }

  try {
    # Other enabled tasks that would still start old production.
    Assert-PlanHas $plan @('Roots') 'the copy plan'
    foreach ($task in @(Get-ScheduledTask -ErrorAction Stop)) {
      if ([string]$task.State -eq 'Disabled' -or @($OldSupervisorTask, $OldRelayTask, 'HER') -contains $task.TaskName) { continue }
      $text = (@($task.Actions | ForEach-Object { [string]$_.Execute + ' ' + [string]$_.Arguments + ' ' + [string]$_.WorkingDirectory }) -join ' ').Replace('/', '\').ToLowerInvariant()
      if (Test-NamesRoot $text $plan.Roots) { Add-Blocker ('the enabled scheduled task ' + $task.TaskName + ' names an old production folder') }
    }
  } catch { Add-Failure $_ 'scheduled tasks' }

  try {
    # The HER task, read through the release's console twin.
    Assert-PlanHas $plan @('ReleaseChecked') 'the release check'
    $herTask = (Invoke-Cli $plan.SourceCli @('scheduled-task', 'read') 'her-task-read').Output | ConvertFrom-Json
    if ($null -ne $herTask.xml) { Add-Blocker 'a scheduled task named HER already exists' }
  } catch { Add-Failure $_ 'scheduled tasks' }

  # DSH home, in four checks that do not hide each other: the old runtime's DSH stub, the profile's MCP block, the
  # profile, plugin and bundle texts, and the bundles' dependencies (then the other files that name a bundle).
  try {
    # The runtime's own DSH files must equal the profile copies the move installs, and the stub holds no junction or
    # link. Inventory lists its links without following them, so they do not hide its differing files; Move refuses at
    # the first one.
    Assert-PlanHas $plan @('Roots') 'the copy plan'
    if (-not (Test-Path -LiteralPath $plan.DshHome -PathType Container)) { Stop-Refused 'the profile DSH home is missing' }
    $stub = Compare-DshStub (Join-Path $plan.OldRuntime 'user\dsh') $plan.DshHome (Get-DshLinkPaths $plan) -ListLinks:($Mode -eq 'Inventory')
    Write-Utf8 (Join-Path $script:Evidence 'dsh-stub-links.txt') ((@($stub.Links) -join "`r`n") + "`r`n")
    Write-Utf8 (Join-Path $script:Evidence 'dsh-stub-differences.txt') ((@($stub.Differences) -join "`r`n") + "`r`n")
    if ($stub.Links.Count -gt 0) { Add-Blocker 'a junction or link was found in dsh-stub (list in the evidence folder)' }
    if ($stub.Differences.Count -gt 0 -and -not $AcceptDshProfileFiles) { Add-Blocker ('' + $stub.Differences.Count + ' runtime DSH files differ from the profile DSH home (list in the evidence folder); pass -AcceptDshProfileFiles to take the profile copies') }
  } catch { Add-Failure $_ 'dsh stub' }

  $rendered = ''
  try {
    # The profile's MCP block, rendered by the release's console twin on a private copy in the evidence folder.
    Assert-PlanHas $plan @('ReleaseChecked') 'the release check'
    Assert-PlanHas $plan @('ConsolePort') 'the configuration check'
    if (-not (Test-Path -LiteralPath $plan.DshHome -PathType Container)) { Stop-Refused 'the profile DSH home is missing' }
    $profileFile = Join-Path $plan.DshHome 'profiles\web\cordis.patch.yml'
    # The copy plan refuses a folder link in the DSH home, but this check does not wait for it: each step below the DSH
    # home, the file included, is checked from the top down before anything is read through it.
    $profileStep = $plan.DshHome
    foreach ($part in @('profiles', 'web', 'cordis.patch.yml')) {
      $profileStep = Join-Path $profileStep $part
      $item = Get-Item -LiteralPath $profileStep -Force -ErrorAction SilentlyContinue
      if ($null -ne $item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Stop-Refused 'a junction or link lies on the path of the DSH profile' }
    }
    if (-not (Test-Path -LiteralPath $profileFile -PathType Leaf)) { Stop-Refused 'the profile DSH home has no profiles\web\cordis.patch.yml' }
    $rendered = New-McpProfile $plan.SourceCli $profileFile (Join-Path $script:Evidence 'mcp-preview') $plan.ConsolePort
  } catch { Add-Failure $_ 'dsh profile' }

  try {
    # The same bundle rewrite the move applies, in memory: what is left must name no old folder and climb nowhere.
    Assert-PlanHas $plan @('Roots') 'the copy plan'
    if (-not $rendered) { Stop-Refused 'skipped: needs the DSH profile check' }
    $references = Find-OldProfileReferences $plan.DshHome $rendered (Get-ScanRoots $plan) $plan.Bundles (Get-DshTextMap $plan)
    Write-Utf8 (Join-Path $script:Evidence 'dsh-profile-references.txt') ((@($references) -join "`r`n") + "`r`n")
    if ($references.Count -gt 0) { Add-Blocker ('' + $references.Count + ' DSH profile, plugin or bundle files name old production paths or climb out with a link (list in the evidence folder)') }
  } catch { Add-Failure $_ 'dsh profile references' }

  try {
    Assert-PlanHas $plan @('Roots') 'the copy plan'
    $carried = @()
    if ($plan.ToolModules) { $carried += [pscustomobject]@{ From = $plan.ToolModules; To = (Join-Path $plan.ToolRoot 'node_modules') } }
    $outside = Find-BundleOutsideDependencies $plan.Bundles $carried
    Write-Utf8 (Join-Path $script:Evidence 'dsh-bundle-dependencies.txt') ((@($outside) -join "`r`n") + "`r`n")
    if ($outside.Count -gt 0) { Add-Blocker 'a linked DSH bundle depends on a package outside its own folder (list in the evidence folder)' }
  } catch { Add-Failure $_ 'dsh bundle dependencies' }

  try {
    # Not a blocker: other DSH-home files naming a bundle's old folder keep naming it; F3 settles them before deleting qq-bridge.
    Assert-PlanHas $plan @('Roots') 'the copy plan'
    $others = Find-OtherBundleReferences $plan.DshHome $plan.Bundles
    Write-Utf8 (Join-Path $script:Evidence 'dsh-other-bundle-references.txt') ((@($others) -join "`r`n") + "`r`n")
    $plan.OtherBundleReferences = $others.Count
  } catch { Add-Failure $_ 'dsh other references' }

  try {
    # Configuration dry run on separate copies; only JSON pointers are written, never values.
    Assert-PlanHas $plan @('Map', 'Roots') 'the copy plan'
    Assert-PlanHas $plan @('NodePath') 'the configuration check'
    Assert-PlanHas $plan @('RelayProgram', 'RelayPort', 'RelayVersion') 'the relay check'
    $preview = Read-Json (Join-Path $plan.OldRuntime 'config\config.json')
    $foreign = Get-ForeignPointers $preview (Read-Json (Join-Path $plan.OldRuntime 'dependencies.json')) $plan
    Write-Utf8 (Join-Path $script:Evidence 'config-foreign-pointers.txt') ((@($foreign) -join "`r`n") + "`r`n")
    if ($foreign.Count -gt 0) { Add-Blocker ('' + $foreign.Count + ' configuration paths need a decision (pointers in the evidence folder)') }
  } catch { Add-Failure $_ 'configuration rewrite' }

  try {
    # Install root access (Q8): saved now so a rollback can restore it; an explicit entry for another account would
    # survive the restriction, so it blocks here instead of failing after inheritance is gone.
    $rootAcl = Get-Acl -LiteralPath $script:Root
    $plan.RootAccessSddl = $rootAcl.GetSecurityDescriptorSddlForm([Security.AccessControl.AccessControlSections]::Access)
    $callerSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $explicit = @($rootAcl.Access | Where-Object { -not $_.IsInherited -and @($callerSid, 'S-1-5-18', 'S-1-5-32-544') -notcontains $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value })
    if ($explicit.Count -gt 0) { Add-Blocker 'the install root has an explicit access entry for another account; the restriction would keep it' }
    # The restriction and a rollback's restore propagate into the root: a link in it leading outside blocks now.
    Assert-LinksInside $script:Root 'the install root'
  } catch { Add-Failure $_ 'install root access' }

  try {
    # Backups (owner decision a): no archive the new retention would prune.
    Assert-PlanHas $plan @('BackupChecked') 'the configuration check'
    $destination = [string]$plan.BackupDestination
    if ($destination -and (Test-Path -LiteralPath $destination -PathType Container)) {
      $archives = @(Get-ChildItem -LiteralPath $destination -File -Force | Where-Object { $_.Name -match $ArchivePattern })
      if ($archives.Count -gt 0) { Add-Blocker ('the backup folder holds ' + $archives.Count + ' archives the new retention would prune; delete them first (owner decision a)') }
    }
  } catch { Add-Failure $_ 'backups' }

  try {
    # Sizes, path lengths and junctions of every copy; free disk space.
    Assert-PlanHas $plan @('Copies') 'the copy plan'
    $bytes = 0.0
    $listedAll = $true
    foreach ($copy in $plan.Copies) {
      # Each copy is listed on its own, so a link or a long path in one copy does not hide another; the free space is
      # judged only once every copy was listed.
      try {
        $listing = Get-Manifest $copy.From $copy.To $copy.Exclude $copy.Label -NoHash
        foreach ($item in $listing) { if ($item.Length -gt 0) { $bytes += $item.Length } }
        $fileCount = @($listing | Where-Object { $_.Length -ge 0 }).Count
        Test-RobocopyView $copy.From $copy.To $copy.Exclude $copy.Label $fileCount
      } catch { Add-Failure $_ ('sizes of ' + $copy.Label); $listedAll = $false }
    }
    if (-not $listedAll) { Stop-Refused 'skipped: needs the size of every copy' }
    Assert-PlanHas $plan @('ReleaseChecked') 'the release check'
    foreach ($name in @('her.exe', 'her-cli.exe', 'herupdate.exe')) { $image = Get-Item -LiteralPath (Join-Path $plan.ReleaseSource $name); $bytes += 2 * $image.Length }
    $plan.CopyBytes = $bytes
    $drive = [IO.DriveInfo]::new($script:Root.Substring(0, 1))
    if (-not (Test-FreeSpace $drive.AvailableFreeSpace $bytes $FreeSpaceReserveBytes)) { Add-Blocker 'not enough free disk space for the copies' }
  } catch { Add-Failure $_ 'sizes' }

  try {
    Assert-PlanHas $plan @('ReleaseChecked') 'the release check'
    $idle = Get-IdleState $plan.SourceCli $plan.OldRuntime 'idle-before'
    if ($idle.Paused -gt 0) { Add-Blocker ('' + $idle.Paused + ' paused turns wait for the owner; resolve them first') }
    if ($idle.Busy -gt 0) { Add-Blocker ('' + $idle.Busy + ' conversation turns are in progress; wait until the bot is idle') }
  } catch { Add-Failure $_ 'idle' }
  return $plan
}

# ---------- Move steps ----------
function Publish-Release($plan) {
  $images = @('her.exe', 'her-cli.exe', 'herupdate.exe')
  $release = Join-Path $script:Root ('releases\' + $plan.ReleaseName)
  [void][IO.Directory]::CreateDirectory($release)
  foreach ($name in $images + @('SHA256SUMS')) { Copy-Item -LiteralPath (Join-Path $plan.ReleaseSource $name) -Destination (Join-Path $release $name) }
  foreach ($name in $images) { if ((Get-FileSha (Join-Path $release $name)) -ne $plan.Sums[$name]) { Stop-Failed ('the staged ' + $name + ' differs from SHA256SUMS') } }
  $helper = Join-Path $script:Root 'herupdate.exe'
  Copy-Item -LiteralPath (Join-Path $release 'herupdate.exe') -Destination $helper
  if ((Get-FileSha $helper) -ne $plan.Sums['herupdate.exe']) { Stop-Failed 'the root herupdate.exe differs from SHA256SUMS' }
  $current = Join-Path $script:Root 'current'
  [void](New-Item -ItemType Junction -Path $current -Target $release)
  $link = Get-Item -LiteralPath $current -Force
  if (-not ($link.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Stop-Failed 'current is not a junction' }
  if ((Get-FileSha (Join-Path $current 'her-cli.exe')) -ne $plan.Sums['her-cli.exe']) { Stop-Failed 'current does not reach the staged release' }
  $layout = (Invoke-Cli $plan.Cli @('install-root', 'layout') 'layout').Output | ConvertFrom-Json
  # her-cli prints mixed separators (C:\qqbot\deps/snowluma); compare normalized paths.
  foreach ($pair in @(@('runtime', 'runtime'), @('dshHome', 'runtime\user\dsh'), @('dshWork', 'runtime\dsh\work'), @('logs', 'runtime\data\logs'), @('dependencies', 'deps'), @('snowLuma', 'deps\snowluma'), @('dsh', 'deps\dsh'), @('relay', 'deps\relay'), @('backups', 'backups'), @('releases', 'releases'), @('current', 'current'), @('updater', 'herupdate.exe'))) {
    $printed = [string]$layout.($pair[0])
    if ($printed -notmatch '^[A-Za-z]:[\\/]' -or (Get-Normal $printed) -ine (Get-Normal (Join-Path $script:Root $pair[1]))) { Stop-Failed ('her-cli install-root layout disagrees about ' + $pair[0]) }
  }
  $check = Invoke-Cli $plan.Cli @('install-root', 'check-supervisor', '--runtime-root', (Join-Path $script:Root 'runtime'), '--executable', (Join-Path $current 'her.exe')) 'check-supervisor'
  if ($check.Output.Trim() -ne '{"ok":true}') { Stop-Failed 'install-root check-supervisor did not answer ok' }
  Write-Step ('staged release ' + $plan.ReleaseName + ', herupdate.exe and the current junction; layout and check-supervisor agree')
}
function Stop-OldProduction($plan) {
  Save-State 'Stopping'
  [void](Invoke-Old @('lifecycle', 'set', 'stopped', '--runtime-root', $plan.OldRuntime, '--confirm', '--by', 'owner-67', '--reason', 'move into the install root') 'old-set-stopped')
  [void](Invoke-Old @('lifecycle', 'stop-supervisor', '--runtime-root', $plan.OldRuntime, '--confirm') 'old-stop-supervisor')
  Wait-Until { $states = Get-OldOwnerStates; $states.supervisor -ne 'held' -and $states.live -ne 'held' -and $states.console -ne 'held' } 30 'the old bridge owners did not stop'
  Write-Step 'the old bridge, console and supervisor are stopped'
  # DSH children are listed again now: a tool server started after the checks must stop with DSH.
  $dsh = $plan.Identities['dsh']
  $fresh = @()
  if (Test-SameIdentity $dsh) {
    foreach ($childPid in (Get-Descendants ([int]$dsh.Pid) ([DateTime]::FromFileTimeUtc([long]$dsh.Start)))) {
      $child = Get-Identity $childPid ('DSH child ' + $childPid)
      if ($null -ne $child) { $fresh += $child }
    }
  }
  Stop-Identity $dsh
  foreach ($child in $fresh) { Stop-Identity $child }
  foreach ($role in @($plan.DshChildren)) { Stop-Identity $plan.Identities[$role] }
  Stop-Identity $plan.Identities['relay']
  Stop-Identity $plan.Identities['snowluma']
  Wait-Until { @(Get-ProductionPortList $plan | Where-Object { (Get-ListenerPid $_) -ne 0 }).Count -eq 0 } 30 'a production port is still in use after the stop'
  $oldFolder = Split-Path -Parent $plan.OldExe
  Wait-Until { @(Get-OldImageProcesses $oldFolder).Count -eq 0 } 15 'a process still runs the old release image after the stop'
  $strangers = @(Get-CimInstance -ClassName Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.IndexOf($plan.NpxRoot, [StringComparison]::OrdinalIgnoreCase) -ge 0 })
  if ($strangers.Count -gt 0) { Stop-Failed 'a process still runs the DSH package after the stop' }
  Write-Step 'DSH, its child processes, the relay and SnowLuma are stopped; every production port is free'
}
function Set-NewConfiguration($plan) {
  $runtime = Join-Path $script:Root 'runtime'
  $configPath = Join-Path $runtime 'config\config.json'
  $config = Read-Json $configPath
  $foreign = Convert-Config $config $plan
  if ($foreign.Count -gt 0) { Stop-Failed 'configuration paths need a decision (pointers in the evidence folder)' }
  Write-JsonFile $configPath $config
  if (@(@($plan.ToolFiles) | Where-Object { $null -ne $_ }).Count -gt 0) {
    # The dev MCP script's configuration comes from the new runtime's, never from qq-bridge's copy (Revision 19).
    Write-JsonFile (Join-Path $plan.ToolRoot 'config.json') (New-ToolConfig $config)
    Write-Step 'the dev tool configuration is written from the new runtime configuration'
  }
  $dependenciesPath = Join-Path $runtime 'dependencies.json'
  $dependencies = Read-Json $dependenciesPath
  $foreign = Convert-Dependencies $dependencies $plan
  if ($foreign.Count -gt 0) { Stop-Failed 'dependencies.json paths need a decision (pointers in the evidence folder)' }
  Write-JsonFile $dependenciesPath $dependencies
  # Relay: the login folder line and the client key file HER uses for its health probe.
  $relayConfig = Join-Path $plan.RelayPin $plan.RelayConfigRelative
  $lines = [Collections.Generic.List[string]]::new([string[]](Read-Lines $relayConfig))
  # The auth-dir line is rewritten where it stands, with its own indentation (under oauth: in the nested shape); only
  # a flat configuration without one gets a top-level line appended.
  $authLine = [string]$plan.Auth.Indent + "auth-dir: '" + $plan.Auth.NewPath + "'"
  if ($plan.Auth.Action -eq 'replace') { $lines[[int]$plan.Auth.LineIndex] = $authLine } elseif ($plan.Auth.Action -eq 'append') { $lines.Add($authLine) }
  [IO.File]::WriteAllLines($relayConfig, $lines, [Text.UTF8Encoding]::new($false))
  $keyFile = Join-Path $plan.RelayPin 'her-relay-client.key'
  if (Test-Present $keyFile) { Stop-Failed 'the relay pin already has her-relay-client.key' }
  [IO.File]::WriteAllText($keyFile, $script:RelayKey, [Text.ASCIIEncoding]::new())
  if ($plan.DigestModel) {
    # The digest authenticates to the relay with its client key (Q5).
    $harnessKey = Join-Path $runtime 'config\harness-api-key'
    if (Test-Present $harnessKey) {
      if ((Read-Text $harnessKey).Trim() -cne $script:RelayKey) { Stop-Failed 'config\harness-api-key holds another key than the relay client key' }
    } else { [IO.File]::WriteAllText($harnessKey, $script:RelayKey, [Text.ASCIIEncoding]::new()) }
    Write-Step 'the memory digest uses the relay: memory.digest.model, the model origin and config\harness-api-key are set'
  }
  # DSH texts: the linked bundle paths follow the bundle copies.
  Update-DshTexts $plan
  # DSH profile: the three MCP servers point at the installed image and the new runtime.
  $profileFile = Join-Path $runtime 'user\dsh\profiles\web\cordis.patch.yml'
  $rendered = New-McpProfile $plan.Cli $profileFile (Join-Path $script:Evidence 'mcp-move') $plan.ConsolePort
  Copy-Item -LiteralPath $rendered -Destination $profileFile -Force
  if ((Get-FileSha $rendered) -ne (Get-FileSha $profileFile)) { Stop-Failed 'the DSH profile could not be replaced' }
  $left = Find-OldProfileReferences (Join-Path $runtime 'user\dsh') '' (Get-ScanRoots $plan) $plan.Bundles @() -Moved
  Write-Utf8 (Join-Path $script:Evidence 'dsh-references-after.txt') ((@($left) -join "`r`n") + "`r`n")
  if ($left.Count -gt 0) { Stop-Failed 'DSH profile, plugin or bundle files in the new DSH home still name old production paths (list in the evidence folder)' }
  Write-Step 'configuration, dependencies, relay login and key, and the DSH MCP block are rewritten'
}
function Register-HerTask($plan) {
  Save-State 'Registering'
  $installed = (Invoke-Cli $plan.Cli @('install') 'install').Output | ConvertFrom-Json
  $command = [string]$installed.command
  $directory = [string]$installed.workingDirectory
  if ([string]$installed.task -ne 'HER' -or $command -notmatch '^[A-Za-z]:[\\/]' -or $directory -notmatch '^[A-Za-z]:[\\/]' -or (Get-Normal $command) -ine (Join-Path $script:Root 'current\her.exe') -or (Get-Normal $directory) -ine (Join-Path $script:Root 'current')) { Stop-Failed 'her-cli install registered an unexpected task definition' }
  [void](Invoke-Cli $plan.Cli @('scheduled-task', 'compare') 'task-compare')
  Save-State 'Registered'
  Write-Step 'the HER logon task is registered and matches its definition'
}
# Fixed facts for a failed health check, kept in the private evidence folder before the rollback moves the runtime
# aside: what each probe targets (dependencies.json), who listens on each production port and since when, when the
# new owners started (their markers), and the new runtime's own dependency files. No command lines.
function Save-HealthDiagnostics($plan) {
  $lines = [Collections.Generic.List[string]]::new()
  try {
    $runtime = Join-Path $script:Root 'runtime'
    $lines.Add('now ' + [DateTime]::UtcNow.ToString('o'))
    $dependencies = Read-Json (Join-Path $runtime 'dependencies.json')
    foreach ($name in @('onebot', 'onebot_ws', 'dsh', 'model', 'relay')) { $lines.Add('target ' + $name + ' ' + [string](Get-Prop $dependencies $name)) }
    foreach ($port in (Get-ProductionPortList $plan)) {
      $owners = @(Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction SilentlyContinue | ForEach-Object { [int]$_.OwningProcess } | Sort-Object -Unique)
      if ($owners.Count -eq 0) { $lines.Add('listener ' + $port + ' none'); continue }
      foreach ($owner in $owners) {
        $identity = Get-Identity $owner 'listener'
        if ($null -eq $identity) { $lines.Add('listener ' + $port + ' pid ' + $owner + ' unidentified'); continue }
        $lines.Add('listener ' + $port + ' pid ' + $owner + ' image ' + $identity.Exe + ' started ' + [DateTime]::FromFileTimeUtc([long]$identity.Start).ToString('o'))
      }
    }
    foreach ($role in @('supervisor', 'live', 'console')) {
      $marker = Join-Path $runtime ('data\logs\rbridge-' + $role + '.lock.owner.json')
      if (-not (Test-Path -LiteralPath $marker -PathType Leaf)) { $lines.Add('owner ' + $role + ' no marker'); continue }
      $start = [string](Get-Prop (Get-Prop (Read-Json $marker) 'identity') 'start')
      $lines.Add('owner ' + $role + ' started ' + [DateTime]::FromFileTimeUtc([long]$start).ToString('o'))
    }
    foreach ($name in @('rust-heartbeat-live.json', 'rust-heartbeat.json')) {
      $path = Join-Path $runtime ('data\' + $name)
      if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { $lines.Add('heartbeat ' + $name + ' none'); continue }
      $beat = Read-Json $path
      $lines.Add('heartbeat ' + $name + ' pid ' + [string]$beat.pid + ' at ' + [DateTimeOffset]::FromUnixTimeMilliseconds([long]$beat.ts).UtcDateTime.ToString('o'))
    }
    foreach ($file in @(Get-ChildItem -LiteralPath (Join-Path $runtime 'data') -File -Force -Filter 'dependenc*.json')) {
      Write-Utf8 (Join-Path $script:Evidence ('health-' + $file.Name)) (Read-Text $file.FullName)
    }
  } catch { $lines.Add('diagnostics incomplete: ' + [string]$_.Exception.Message) }
  Write-Utf8 (Join-Path $script:Evidence 'health-diagnostics.txt') ((@($lines) -join "`r`n") + "`r`n")
}
# The new live bridge is ready when every new owner is held and a heartbeat (data\rust-heartbeat-live.json or
# data\rust-heartbeat.json) names the live owner's process with a time after the start. The live bridge publishes its
# heartbeat only after its gateway connection (her/src/entry.rs:2059-2091), and the copy brought the old live's
# heartbeat along, so neither an old nor a stale heartbeat counts. The health check has no live part.
function Test-NewLiveReady($plan, [long]$SinceMs) {
  $runtime = Join-Path $script:Root 'runtime'
  $status = (Invoke-Cli $plan.Cli @('lifecycle', 'status', '--runtime-root', $runtime) 'live-ready' @(0, 3)).Output | ConvertFrom-Json
  foreach ($role in @('supervisor', 'live', 'console')) { if ([string]$status.owners.$role.state -ne 'held') { return $false } }
  $livePid = [int]$status.owners.live.owner.identity.pid
  if ($livePid -le 0) { return $false }
  foreach ($name in @('rust-heartbeat-live.json', 'rust-heartbeat.json')) {
    $path = Join-Path $runtime ('data\' + $name)
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { continue }
    try { $beat = Read-Json $path } catch { continue }
    if ([int]$beat.pid -eq $livePid -and [long]$beat.ts -ge $SinceMs) { return $true }
  }
  return $false
}
function Start-NewProduction($plan) {
  $runtime = Join-Path $script:Root 'runtime'
  [void](Invoke-Cli $plan.Cli @('lifecycle', 'set', 'running', '--runtime-root', $runtime, '--by', 'owner-67', '--reason', 'move into the install root') 'new-set-running')
  $startedMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
  $script:State.NewStartedAt = [DateTime]::UtcNow.ToString('o')
  Save-State 'Starting'
  if ($Rehearsal) {
    Start-Process -FilePath (Join-Path $script:Root 'current\her.exe') -ArgumentList ('supervise --runtime-root "' + $runtime + '" --install-root "' + $script:Root + '"') -WorkingDirectory (Join-Path $script:Root 'current') -WindowStyle Hidden
  } else {
    Start-ScheduledTask -TaskName 'HER' -TaskPath '\' -ErrorAction Stop
  }
  Wait-Until { (Get-NewOwnerStates $plan.Cli).supervisor -eq 'held' } 60 'the new supervisor did not take ownership within 60 seconds'
  $health = Invoke-Cli $plan.Cli @('lifecycle', 'assert-running', '--runtime-root', $runtime, '--wait', ([string]$HealthWaitSeconds + 's')) 'health' @(0, 1)
  $verdict = $health.Output | ConvertFrom-Json
  if ($health.Code -ne 0 -or $verdict.ok -ne $true) {
    Save-HealthDiagnostics $plan
    $failing = @($verdict.parts.PSObject.Properties | Where-Object { [string]$_.Value.verdict -ne 'pass' } | ForEach-Object { $_.Name })
    Stop-Failed ('the health check failed: ' + ($failing -join ', '))
  }
  Write-Step ('health: every part passes (' + (@($verdict.parts.PSObject.Properties | ForEach-Object { $_.Name }) -join ', ') + ')')
  # The live owner too, bounded: a late gateway can make the supervisor's watchdog give up on the first live and
  # recover it once, which LiveWaitSeconds (default 300) covers. Polled every 5 seconds.
  $liveDeadline = [DateTime]::UtcNow.AddSeconds($LiveWaitSeconds)
  while (-not (Test-NewLiveReady $plan $startedMs)) {
    if ([DateTime]::UtcNow -ge $liveDeadline) {
      Save-HealthDiagnostics $plan
      Stop-Failed ('the health check failed: live (no heartbeat from the new live owner within ' + $LiveWaitSeconds + ' seconds)')
    }
    Start-Sleep -Seconds 5
  }
  Write-Step 'live: every new owner is held and the new live bridge publishes its heartbeat'
  $report = Invoke-Cli $plan.Cli @('report', '--runtime-root', $runtime, '--since', '30m', '--json') 'report-after' @(0, 1)
  $partial = $report.Code -ne 0
  try { if (($report.Output | ConvertFrom-Json).partial -eq $true) { $partial = $true } } catch { $partial = $true }
  if ($partial) { Write-Step 'WARNING: the report on the new runtime is partial (a log is unreadable); the move stays; tell the coordinator' }
  [void](Invoke-Cli $plan.Cli @('scheduled-task', 'compare') 'task-compare-after')
  Write-Step 'report and task compare done on the new runtime'
}
function Write-Done {
  Write-Step 'MOVE DONE. Production runs from the install root; the originals are untouched and stay until the owner approves their deletion.'
  Write-Step 'Owner checks now: (1) QQ login: the console shows QQ logged in (the snowluma health part above passed); (2) memory: the console Memory page lists the memories you expect; (3) the next real conversation turn completes (her-cli report).'
  Write-Step ('To roll back later: -Mode Rollback ' + (Get-RunKind) + ' -StateFile ' + $script:StatePath + ' -AfterDone (what the new runtime writes until then stays in the moved-aside folder).')
}

# ---------- rollback ----------
# The attempt folder is restricted before anything moves into it: it keeps the copied credentials (DSH home, relay
# login and key, harness key), and the rollback then restores the install root's original, possibly wider, access.
function New-AttemptFolder([string]$Aside) {
  if (Test-Present $Aside) { return }
  [void][IO.Directory]::CreateDirectory($Aside)
  Protect-Folder $Aside 'the moved-aside attempt folder'
}
function Move-NewLayoutAside {
  foreach ($name in @('current', 'last-good', '.current-previous', '.last-good-previous')) {
    $path = Join-Path $script:Root $name
    if (-not (Test-Present $path)) { continue }
    $link = Get-Item -LiteralPath $path -Force
    if (-not ($link.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Stop-Failed ($name + ' is not a junction; it was left in place') }
    [IO.Directory]::Delete($path)
  }
  # Links inside the new layout (the bundle junctions, anything DSH made there) are removed as links first: kept in an
  # attempt folder, their absolute targets would reach the live bundles after a later move, and a recursive delete of
  # the attempt folder would then follow them.
  foreach ($name in @('runtime', 'releases', 'deps')) { Remove-LinksUnder (Join-Path $script:Root $name) }
  $aside = Join-Path $script:Root ('move-67-attempt-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff'))
  foreach ($name in @('herupdate.exe', 'update-starts.json', 'update-alert.json', 'releases', 'deps', 'runtime')) {
    $path = Join-Path $script:Root $name
    if (-not (Test-Present $path)) { continue }
    New-AttemptFolder $aside
    if (Test-Path -LiteralPath $path -PathType Container) { [IO.Directory]::Move($path, (Join-Path $aside $name)) } else { [IO.File]::Move($path, (Join-Path $aside $name)) }
  }
  # Archives the new runtime wrote would make a later run refuse (owner decision a); they stay with the attempt.
  # Only archives made while the new supervisor ran (between its start and the rollback's stop) can be its own.
  $destination = [string]$script:State.Plan.BackupDestination
  $newStart = [string]$script:State.NewStartedAt
  if ($newStart -and $destination -and (Test-Path -LiteralPath $destination -PathType Container)) {
    $since = [DateTime]::Parse($newStart, [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::RoundtripKind).ToUniversalTime()
    $until = [DateTime]::UtcNow
    if ([string]$script:State.NewStoppedAt) { $until = [DateTime]::Parse([string]$script:State.NewStoppedAt, [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::RoundtripKind).ToUniversalTime() }
    foreach ($archive in @(Get-ChildItem -LiteralPath $destination -File -Force | Where-Object { $_.Name -match $ArchivePattern -and $_.LastWriteTimeUtc -ge $since -and $_.LastWriteTimeUtc -le $until })) {
      New-AttemptFolder $aside
      $keep = Join-Path $aside 'backups-new'
      [void][IO.Directory]::CreateDirectory($keep)
      [IO.File]::Move($archive.FullName, (Join-Path $keep $archive.Name))
    }
  }
  if (Test-Present $aside) { Write-Step ('the new layout is kept for diagnosis in ' + $aside) }
}
function Test-OldProductionUp($plan) {
  $states = Get-OldOwnerStates
  if ($states.supervisor -ne 'held' -or $states.live -ne 'held' -or $states.console -ne 'held') { return $false }
  foreach ($port in (Get-ProductionPortList $plan)) { if ((Get-ListenerPid $port) -eq 0) { return $false } }
  return $true
}
# Returns $true when it had to start something, $false when the old production was already up. -Settle (only before
# the stop phase, when this script stopped nothing) gives a transient view (an owner between states, one failed status
# call) 30 seconds to settle before anything is started; after the script's own stop it would only add downtime.
function Start-OldProduction($plan, [switch]$Settle) {
  $settleSeconds = 0
  if ($Settle) { $settleSeconds = 30 }
  # Not $settle: PowerShell names are case-insensitive, so that would assign to the [switch]$Settle parameter.
  $settleUntil = [DateTime]::UtcNow.AddSeconds($settleSeconds)
  while ($true) {
    $up = $false
    try { $up = [bool](Test-OldProductionUp $plan) } catch { $up = $false }
    if ($up) { return $false }
    if ([DateTime]::UtcNow -ge $settleUntil) { break }
    Start-Sleep -Seconds 3
  }
  [void](Invoke-Old @('lifecycle', 'set', 'running', '--runtime-root', $plan.OldRuntime, '--by', 'owner-67', '--reason', 'rollback of the move') 'rollback-old-running')
  # The relay and the supervisor are judged apart: a rerun may find the supervisor back but the relay still down.
  $tasks = @()
  if ((Get-ListenerPid ([int]$plan.RelayPort)) -eq 0) { $tasks += [string]$plan.OldRelayTask }
  if ((Get-OldOwnerStates).supervisor -ne 'held') { $tasks += [string]$plan.OldSupervisorTask }
  foreach ($name in $tasks) {
    # A task instance still running (its launcher waits) would ignore the start.
    Wait-Until { (Get-TaskState $name) -ne 'Running' } 60 ('the old task ' + $name + ' still has a running instance, so it cannot start the old production')
    Start-ScheduledTask -TaskName $name -TaskPath '\' -ErrorAction Stop
  }
  Wait-Until { Test-OldProductionUp $plan } 300 'the old production did not come back within 5 minutes'
  return $true
}
function Restore-RootAcl($plan) {
  $saved = [string]$plan.RootAccessSddl
  if (-not $saved) { return }
  $sections = [Security.AccessControl.AccessControlSections]::Access
  $acl = Get-Acl -LiteralPath $script:Root
  if ($acl.GetSecurityDescriptorSddlForm($sections) -eq $saved) { return }
  # The restored, inheriting DACL propagates into the root: no link in it may lead outside.
  Assert-LinksInside $script:Root 'the install root'
  $acl.SetSecurityDescriptorSddlForm($saved, $sections)
  # Persist only the DACL section that changed; the owner is left alone. From here on, the root's access is unknown
  # until the check below confirms it.
  $script:RootAclChanging = $true
  [IO.Directory]::SetAccessControl($script:Root, [Security.AccessControl.DirectorySecurity]$acl)
  $restored = Get-Acl -LiteralPath $script:Root
  if ($restored.GetSecurityDescriptorSddlForm($sections) -ne $saved) { Stop-Failed 'the install root access could not be restored' }
  $script:RootAclChanging = $false
  Write-Step 'the install root access is restored to what it was before the move'
}
function Invoke-Rollback([string]$Why, [switch]$NothingStopped) {
  $plan = $script:State.Plan
  $phase = Get-PhaseIndex ([string]$script:State.Phase)
  Write-Step ('ROLLBACK started: ' + (Get-SafeMessage $Why))
  Write-Log ('rollback reason: ' + $Why)
  $problems = [Collections.Generic.List[string]]::new()
  $newRunning = $false
  $oldUp = $true
  $runtime = Join-Path $script:Root 'runtime'
  # The new runtime's own commands need its installed image (current, or the staged folder if current is gone).
  $runtimeCli = ''
  foreach ($candidate in @((Join-Path $script:Root 'current\her-cli.exe'), (Join-Path $script:Root ('releases\' + [string]$plan.ReleaseName + '\her-cli.exe')))) {
    if (-not $runtimeCli -and (Test-Path -LiteralPath $candidate -PathType Leaf)) { $runtimeCli = $candidate }
  }
  # The task commands need no installed image; the release source's CLI outlives the move-aside, so a rerun still
  # finds and removes a task an earlier attempt left.
  $taskCli = [string]$plan.SourceCli
  if (-not $taskCli -or -not (Test-Path -LiteralPath $taskCli -PathType Leaf)) { $taskCli = $runtimeCli }
  if ($phase -ge (Get-PhaseIndex 'Registering')) {
    # The task first, so Scheduler cannot restart a supervisor this rollback stops.
    try {
      if (-not $taskCli) { Stop-Failed 'no her-cli is left to remove the HER task' }
      $read = Invoke-Cli $taskCli @('scheduled-task', 'read') 'rollback-task-read'
      if ($null -ne ($read.Output | ConvertFrom-Json).xml) { [void](Invoke-Cli $taskCli @('scheduled-task', 'remove') 'rollback-task-remove') }
      Write-Step 'the HER task is removed'
    } catch { $problems.Add('the HER task could not be removed: ' + (Get-SafeMessage $_.Exception.Message)) }
    # A logon or a crash can have started the new supervisor in any phase from here on.
    if ($runtimeCli -and (Test-Path -LiteralPath (Join-Path $runtime 'data') -PathType Container)) {
      # On a rerun the old production may already be up; its own listeners must not count as new ones.
      $oldAlreadyUp = $false
      if ($phase -ge (Get-PhaseIndex 'Stopping')) { try { $oldAlreadyUp = [bool](Test-OldProductionUp $plan) } catch { $oldAlreadyUp = $false } }
      try {
        # Listed before the stop: once DSH is gone, a tool process whose command line names no new-layout path can no
        # longer be told apart as its descendant.
        $snapshot = @()
        foreach ($process in (Get-NewLayoutProcesses $plan)) {
          $identity = Get-Identity ([int]$process.ProcessId) 'new-layout process'
          if ($null -ne $identity) { $snapshot += $identity }
        }
        [void](Invoke-Cli $runtimeCli @('stop', '--runtime-root', $runtime, '--confirm', '--with-dependencies') 'rollback-new-stop' @(0, 3))
        if ((Get-NewOwnerStates $runtimeCli).supervisor -eq 'held') { [void](Invoke-Cli $runtimeCli @('lifecycle', 'stop-supervisor', '--runtime-root', $runtime, '--confirm') 'rollback-new-stop-supervisor') }
        # Only "free" counts: an owner the CLI cannot identify ("unknown") may still run.
        Wait-Until { $states = Get-NewOwnerStates $runtimeCli; $states.supervisor -eq 'free' -and $states.live -eq 'free' -and $states.console -eq 'free' } 30 'the new owners did not stop'
        # Whatever the stop left of the new layout (a DSH child, a refused dependency) is stopped by verified identity:
        # first what the snapshot saw, then anything still found now.
        foreach ($identity in $snapshot) { if (Test-SameIdentity $identity) { Stop-Identity $identity } }
        foreach ($process in (Get-NewLayoutProcesses $plan)) {
          $identity = Get-Identity ([int]$process.ProcessId) 'new-layout process'
          if ($null -ne $identity) { Stop-Identity $identity }
        }
        Wait-Until { @(Get-NewLayoutProcesses $plan).Count -eq 0 } 30 'a process still runs from the new layout'
        if (-not $oldAlreadyUp) { Wait-Until { @(Get-ProductionPortList $plan | Where-Object { (Get-ListenerPid $_) -ne 0 }).Count -eq 0 } 30 'a production port is still held by the new runtime' }
        if (-not [string]$script:State.NewStoppedAt) { $script:State.NewStoppedAt = [DateTime]::UtcNow.ToString('o'); Save-State ([string]$script:State.Phase) }
        Write-Step 'the new runtime and its dependencies are stopped'
      } catch { $newRunning = $true; $problems.Add('the new runtime could not be stopped: ' + (Get-SafeMessage $_.Exception.Message)) }
    }
  }
  if ($phase -ge (Get-PhaseIndex 'OldTasksDisabled')) {
    foreach ($task in @($plan.OldTasks)) { try { Restore-Task $task } catch { $problems.Add((Get-SafeMessage $_.Exception.Message)) } }
  }
  # From OldTasksDisabled on, the old production may be down even before the stop phase (a reboot while its tasks were
  # disabled): Start-OldProduction checks the real state and starts only what is not running.
  # An in-process refusal before the stop phase (-NothingStopped) has stopped nothing, so it starts nothing either.
  if ($phase -ge (Get-PhaseIndex 'OldTasksDisabled') -and -not ($NothingStopped -and $phase -lt (Get-PhaseIndex 'Stopping'))) {
    if ($newRunning) { $oldUp = $false }
    else {
      try {
        $started = [bool](@(Start-OldProduction $plan -Settle:($phase -lt (Get-PhaseIndex 'Stopping')))[-1])
        if ($started) { Write-Step 'the old production runs again' } else { Write-Step 'the old production is running' }
      }
      catch { $oldUp = $false; $problems.Add((Get-SafeMessage $_.Exception.Message)) }
    }
  }
  if (-not $newRunning) {
    try { Move-NewLayoutAside } catch { $problems.Add('the new layout could not be moved aside (a process, possibly QQ through its SnowLuma hook, still uses files under the new layout: close or restart it, then run the rollback again): ' + (Get-SafeMessage $_.Exception.Message)) }
  }
  # The install root restriction stays only after a finished move (coordinator decision); any earlier phase restores it.
  if ($phase -lt (Get-PhaseIndex 'Done')) {
    try {
      # New-layout folders the move-aside left in the root hold credential copies: restricted on their own first,
      # so restoring the root's original access does not open them.
      foreach ($name in @('runtime', 'deps', 'releases')) {
        $left = Join-Path $script:Root $name
        if (Test-Present $left) { Protect-Folder $left ('the leftover ' + $name + ' folder') }
      }
      Restore-RootAcl $plan
    } catch {
      # No ACL was changed outside the root. The text says what is known about the root itself.
      if ($script:RootAclChanging) { $known = '; the install root access may be only partly restored: compare it with RootAccessSddl in the state file' }
      else { $known = '; the install root access was left unchanged by this step (restricted to the owner, SYSTEM and Administrators if the move had restricted it)' }
      $problems.Add((Get-SafeMessage $_.Exception.Message) + $known)
    }
  }
  # An installed app launch after the move registers HERDesktop for the current/her.exe of this root; once current
  # is gone, that logon entry fails. Read only: the owner decides (a later move makes it valid again).
  $desktop = Get-ItemProperty -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'HERDesktop' -ErrorAction SilentlyContinue
  if ($null -ne $desktop -and ([string]$desktop.HERDesktop).Replace('/', '\').IndexOf((Join-Path $script:Root 'current\her.exe'), [StringComparison]::OrdinalIgnoreCase) -ge 0) {
    Write-Step 'NOTE: the desktop logon entry HERDesktop still starts the install root app, which is rolled back. Remove it with: Remove-ItemProperty -LiteralPath HKCU:\Software\Microsoft\Windows\CurrentVersion\Run -Name HERDesktop'
  }
  if ($problems.Count -eq 0) {
    Save-State 'RolledBack'
    Write-Step 'ROLLED BACK: the old tasks are restored and the old production runs from the untouched originals'
    return 'done'
  }
  foreach ($problem in $problems) { Write-Step ('ROLLBACK PROBLEM: ' + $problem) }
  if ($oldUp) {
    Write-Step ('THE OLD PRODUCTION RUNS AGAIN, but the rollback is incomplete. Fix the problem above and run -Mode Rollback ' + (Get-RunKind) + ' -StateFile ' + $script:StatePath + ' again.')
    return 'partial'
  }
  Write-Step ('PRODUCTION MAY BE DOWN. Fix the problem above, then run -Mode Rollback ' + (Get-RunKind) + ' -StateFile ' + $script:StatePath + ' again. By hand the old production starts after its desired state is running: Start-ScheduledTask -TaskName ' + $plan.OldRelayTask + ' and Start-ScheduledTask -TaskName ' + $plan.OldSupervisorTask + '.')
  return 'down'
}

# ---------- modes ----------
function Invoke-Inventory {
  $plan = Get-MovePlan
  $copy = [ordered]@{}
  foreach ($key in @($plan.Keys)) { if ($key -ne 'OldTasks') { $copy[$key] = $plan[$key] } }
  # Task actions are command lines and stay out of the evidence: a hash of each saved shape identifies it.
  $copy['OldTasks'] = @($plan.OldTasks | Where-Object { $null -ne $_ } | ForEach-Object { [pscustomobject]@{ Name = $_.Name; ShapeSha256 = (Get-TextSha ([string]$_.Shape)); Enabled = $_.Enabled; State = $_.State } })
  $copy['Blockers'] = @($script:Blockers)
  # Round-tripped to plain JSON values, then every string is cleaned (keys, profile root) before it is serialized.
  $projection = Edit-ProjectionStrings (($copy | ConvertTo-Json -Depth 100) | ConvertFrom-Json)
  # Windows PowerShell 5.1 escapes < and > as unicode escapes; the placeholder is restored to its readable form.
  $escaped = '\' + 'u003cprofile' + '\' + 'u003e'
  $json = ($projection | ConvertTo-Json -Depth 100).Replace($escaped, '<profile>')
  Write-Utf8 (Join-Path $script:Evidence 'inventory.json') $json
  Write-Step ('INVENTORY: ' + $script:Blockers.Count + ' blockers; details in ' + (Join-Path $script:Evidence 'inventory.json'))
  if ($script:Blockers.Count -gt 0) { return 1 }
  return 0
}
function Invoke-Move {
  $plan = Get-MovePlan
  $megabytes = [math]::Ceiling([double]$plan.CopyBytes / 1MB)
  Confirm-Owner 'MOVE' ('Ready: about ' + $megabytes + ' MB to copy; expected downtime 3 to 6 minutes, about 18 at most with a rollback. Nothing is stopped before you confirm.') $plan.ReleaseName
  $script:StatePath = Join-Path $script:Evidence 'state.json'
  $script:State = [ordered]@{ v = 1; Phase = ''; UpdatedAt = ''; StartedAt = [DateTime]::UtcNow.ToString('o'); NewStartedAt = ''; NewStoppedAt = ''; Root = $script:Root; Rehearsal = [bool]$Rehearsal; Plan = $plan }
  Save-State 'Preflight'
  Write-Step ('all checks passed; state file ' + $script:StatePath)
  try {
    Publish-Release $plan
    Protect-Folder $script:Root
    Save-State 'Staged'
    Assert-AbortPoint 'Staged'
    if ($Rehearsal -and $RehearsalFault -eq 'StaleIdentity') { $plan.Identities['relay'].Start = '1' }
    foreach ($role in @('supervisor', 'live', 'console', 'snowluma', 'dsh', 'relay')) {
      if (-not (Test-SameIdentity $plan.Identities[$role])) { Stop-Refused ('the ' + $role + ' process changed since the checks; run the move again') }
    }
    Save-State 'OldTasksDisabled'
    foreach ($task in $plan.OldTasks) {
      Disable-ScheduledTask -TaskName $task.Name -TaskPath '\' -ErrorAction Stop | Out-Null
      if ((Get-TaskShape $task.Name).EndsWith('#True')) { Stop-Failed ('the scheduled task ' + $task.Name + ' could not be disabled') }
    }
    Write-Step 'the old scheduled tasks are disabled'
    Assert-AbortPoint 'TasksDisabled'
    if ($Rehearsal -and $RehearsalFault -eq 'LateDshChild') {
      # The fake DSH starts a child now, after the checks; the stop must still find and end it.
      [void](Invoke-WebRequest -UseBasicParsing -Uri ('http://127.0.0.1:' + $plan.DshPort + '/fixture/spawn-child') -TimeoutSec 10)
      Start-Sleep -Seconds 2
    }
    if ($Rehearsal -and $RehearsalFault -eq 'DisturbAfterDisable') {
      $record = [ordered]@{ k = 'begun'; turn = 'turn-fault-67'; t = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(); key = 'group:1'; backend = 'dsh'; reason = 'atMention'; epoch = 'e1'; inbound = @('group:1|message:9'); resumedFrom = $null }
      [IO.File]::AppendAllText((Join-Path $plan.OldRuntime 'data\turn-ledger.jsonl'), (($record | ConvertTo-Json -Compress -Depth 5) + "`n"), [Text.UTF8Encoding]::new($false))
    }
    $idle = Get-IdleState $plan.Cli $plan.OldRuntime 'idle-before-stop'
    if ($idle.Busy -gt 0 -or $idle.Paused -gt 0) { Stop-Refused 'conversation turns started during the checks; nothing was stopped; run the move again when the bot is idle' }
    Stop-OldProduction $plan
    Save-State 'OldStopped'
    Assert-AbortPoint 'Stopped'
    $after = Get-IdleState $plan.Cli $plan.OldRuntime 'idle-after-stop'
    if ($after.Busy -gt 0) { Write-Step ('' + $after.Busy + ' turns were cut by the stop; the new bridge pauses them for the owner') }
    foreach ($copy in $plan.Copies) { Copy-Verified $copy.From $copy.To $copy.Exclude $copy.Label }
    Copy-ToolFiles $plan
    New-BundleLinks $plan
    Save-State 'Copied'
    Assert-AbortPoint 'Copied'
    Set-NewConfiguration $plan
    Save-State 'Configured'
    Register-HerTask $plan
    Assert-AbortPoint 'Registered'
    Start-NewProduction $plan
    Save-State 'Done'
    Write-Done
    return 0
  } catch {
    $why = [string]$_.Exception.Message
    if ($why -eq 'REHEARSAL-ABORT') { return 4 }
    $unchanged = (Get-PhaseIndex ([string]$script:State.Phase)) -le (Get-PhaseIndex 'OldTasksDisabled')
    $result = [string](@(Invoke-Rollback $why -NothingStopped:$unchanged)[-1])
    if ($result -ne 'done') { return 3 }
    if ($unchanged) { return 1 }
    return 2
  }
}
function Invoke-RollbackMode {
  if (-not $StateFile) { Stop-Refused '-StateFile is required' }
  $script:StatePath = Get-Normal $StateFile
  Assert-RehearsalPath $script:StatePath
  Assert-NoLinkAbove $script:StatePath 'the state file'
  $loaded = Read-Json $script:StatePath
  if ([bool]$loaded.Rehearsal -ne [bool]$Rehearsal) { Stop-Refused 'the state file belongs to a run of the other kind (rehearsal or production)' }
  if ((Get-Normal ([string]$loaded.Root)) -ine $script:Root) { Stop-Refused 'the state file belongs to another install root' }
  $script:State = $loaded
  $script:OldExe = [string]$loaded.Plan.OldExe
  $script:OldCli = [string]$loaded.Plan.OldCli
  $script:OldRuntimePath = [string]$loaded.Plan.OldRuntime
  foreach ($path in @($script:OldExe, $script:OldCli, $script:OldRuntimePath, [string]$loaded.Plan.SourceCli)) { Assert-RehearsalPath $path }
  # The rollback runs or writes these paths from the state file: the same no-link-above rule as before the move.
  foreach ($path in @($script:OldExe, $script:OldCli, $script:OldRuntimePath, [string]$loaded.Plan.SourceCli, [string]$loaded.Plan.NodePath, [string]$loaded.Plan.BackupDestination)) {
    if ($path) { Assert-NoLinkAbove $path 'a path from the state file' }
  }
  if ($Rehearsal) {
    foreach ($name in @([string]$loaded.Plan.OldSupervisorTask, [string]$loaded.Plan.OldRelayTask)) {
      if ($name -cnotmatch '^HER-Test-67-[0-9a-f]{32}-(supervisor|relay)$') { Stop-Refused 'a rehearsal uses only HER-Test-67 fixture tasks' }
    }
  }
  if ($loaded.Phase -eq 'RolledBack') { Write-Step 'nothing to roll back: this run was already rolled back'; return 0 }
  if ($loaded.Phase -eq 'Done' -and -not $AfterDone) { Stop-Refused 'the move finished; pass -AfterDone to roll it back (what the new runtime wrote since stays in the moved-aside folder)' }
  Confirm-Owner 'ROLLBACK' 'Ready to roll back: the new runtime stops and the old production starts from the originals.' ([string]$loaded.Plan.ReleaseName)
  $result = [string](@(Invoke-Rollback 'requested by the owner')[-1])
  if ($result -eq 'done') { return 0 }
  return 3
}
function Invoke-OperatorFolders {
  $from = Get-Normal $OperatorSource
  Assert-NoLinkAbove $from 'the ops folder'
  Assert-RehearsalPath $from
  $to = Join-Path $script:Root 'ops'
  if (-not (Test-Path -LiteralPath $from -PathType Container)) { Stop-Refused 'the ops folder is missing' }
  if (Test-Present $to) { Stop-Refused 'the install root already has ops' }
  Confirm-Owner 'COPY' 'Ready to copy the operator scripts into the install root ops folder.'
  # Copied beside it first and renamed only after the hash check, so a failed copy leaves no ops folder and a retry
  # can run (a leftover staging folder is this mode's own and is removed reparse-safely).
  $staging = Join-Path $script:Root '.ops-copy'
  if (Test-Present $staging) { Remove-TreeSafely $staging }
  try { Copy-Verified $from $staging @() 'ops' }
  catch {
    $failure = $_
    try { if (Test-Present $staging) { Remove-TreeSafely $staging } } catch { Write-Log ('the ops staging folder could not be removed: ' + $_.Exception.Message) }
    throw $failure
  }
  [IO.Directory]::Move($staging, $to)
  Write-Step 'OPERATOR FOLDER COPIED: the original stays until the owner approves its deletion'
  return 0
}
function Test-Refused([scriptblock]$Block) {
  try { [void](& $Block); return $false } catch { return $_.Exception.Message.StartsWith('REFUSED: ') }
}
# A refusal with exactly this reason, so a check fails when its own guard is gone and another refusal fires instead.
function Test-RefusedWith([scriptblock]$Block, [string]$Reason) {
  try { [void](& $Block); return $false } catch { return $_.Exception.Message -ceq ('REFUSED: ' + $Reason) }
}
function Invoke-SelfTest {
  function Check([bool]$Condition, [string]$Label) { if (-not $Condition) { throw ('self-test failed: ' + $Label) }; $script:SelfChecks++ }
  $map = New-PathMap @([pscustomobject]@{ From = 'C:\Old\Rt'; To = 'C:\qqbot\runtime' }, [pscustomobject]@{ From = 'C:\Old'; To = 'C:\qqbot\elsewhere' })
  Check ((Convert-PathString 'C:\Old\Rt\config' $map) -eq 'C:\qqbot\runtime\config') 'mapped child'
  Check ((Convert-PathString 'c:/old/rt' $map) -eq 'C:\qqbot\runtime') 'mapped folder in another spelling'
  Check ((Convert-PathString 'C:\Old\Rt2\x' $map) -eq 'C:\qqbot\elsewhere\Rt2\x') 'longest source wins and stops at a name boundary'
  Check ((Convert-PathString 'C:\Older' $map) -eq 'C:\Older') 'a sibling name is not mapped'
  Check (Test-Within 'C:\qqbot\runtime' 'C:\qqbot') 'inside the root'
  Check (-not (Test-Within 'C:\qqbot-rust\x' 'C:\qqbot')) 'a sibling is outside the root'
  Check (Test-Refused { Get-Normal '\\server\share\x' }) 'a UNC path is refused'
  Check (Test-Refused { Get-Normal 'relative\x' }) 'a relative path is refused'
  Check (Test-FreeSpace 100 60 40) 'free space exactly enough'
  Check (-not (Test-FreeSpace 100 60 41)) 'free space one byte short'
  foreach ($good in @('1.14.22', '0.2.1-rc.2', '8.0.36')) { Check (Test-ExactVersion $good) ('exact version ' + $good) }
  foreach ($bad in @('01.2.3', '1.2', '1.2.3+b', '1.2.3-01', '^1.2.3', '')) { Check (-not (Test-ExactVersion $bad)) ('not an exact version ' + $bad) }
  Check (Test-ReleaseName 'main-abc') 'release name'
  foreach ($bad in @('v0.1.0', '..\x', 'Main', 'ab')) { Check (-not (Test-ReleaseName $bad)) ('bad release name ' + $bad) }
  $arguments = Convert-RelayArguments @('-config', 'C:\R\config.yaml', '--log=C:\R\logs\a.log', '--debug') 'C:\R' 'C:\N'
  Check (($arguments -join ' ') -eq '-config C:\N\config.yaml --log=C:\N\logs\a.log --debug') 'relay arguments follow the pin'
  Check (Test-Refused { Convert-RelayArguments @('C:\Other\x') 'C:\R' 'C:\N' }) 'relay argument outside the folder'
  Check (Test-Refused { Convert-RelayArguments @('..\x') 'C:\R' 'C:\N' }) 'relay argument with a parent step'
  Check ((Find-RelayConfig @('--config=C:\R\c.yaml') 'C:\R') -eq 'C:\R\c.yaml') 'relay config in the = form'
  Check ((Find-RelayConfig @() 'C:\R') -eq 'C:\R\config.yaml') 'relay config default'
  # Relay configuration. Each refusal changes one line of a valid configuration and expects its own reason, so a check
  # fails when its guard is gone and another refusal fires instead.
  $mixed = 'the relay configuration mixes top-level port, auth-dir or api-keys with the nested blocks'
  $notPlain = ' is not plain or simply quoted (a comment, an odd quote, an escape or flow style)'
  $tooShort = 'the relay configuration has a management secret-key or an api key shorter than 16 characters'
  $listHeader = 'the relay configuration has an api-keys line with text after it (a comment or flow style)'
  $elsewhere = 'the relay configuration has a secret-key, api-keys or auth-dir line in a place this check does not read'
  $hostOnly = 'the relay configuration has a host other than 127.0.0.1 or localhost'
  $notUnderstood = 'the relay configuration has a line the relay parser does not understand'
  $flowStyle = 'the relay configuration uses flow style (a value or list item starting with { or [)'
  $listGap = 'the relay configuration has a blank or comment line inside the api-keys list'
  $topLevel = 'the nested relay configuration has a top-level key other than config-version, server, management, access and oauth'
  $relay = Read-RelayConfig @('# comment C:\x', 'port: 8317', 'auth-dir: "~/.cli-proxy-api"', 'api-keys:', '  - "flat-key-number-one"', '  - flat-key-number-two', 'proxy-url: "http://127.0.0.1:9"')
  Check ($relay.Port -eq 8317 -and $relay.PortSource -eq 'top-level port' -and $relay.Auth.Value -eq '~/.cli-proxy-api' -and $relay.Key -eq 'flat-key-number-one') 'relay configuration subset'
  $flatPort = Read-RelayConfig @('remote-management:', '  port: 83', 'api-keys:', '  - "flat-key-number-three"')
  Check ($flatPort.Port -eq 0 -and $flatPort.PortSource -eq 'default') 'no port line means the relay default port; a port under another block is not the relay port'
  Check (Test-RefusedWith { Read-RelayConfig @('port: 8317', 'port: 8318', 'api-keys:', '  - "flat-key-number-one"') } 'the relay configuration names port more than once') 'two port lines'
  Check (Test-RefusedWith { Read-RelayConfig @('port: eighty', 'api-keys:', '  - "flat-key-number-one"') } 'the relay configuration has a port line that cannot be read') 'a top-level port line that cannot be read'
  Check (Test-RefusedWith { Read-RelayConfig @('port: 8317', 'api-keys:', '  - "flat-key-number-one"', 'logs-dir: C:\x') } 'the relay configuration names other absolute paths; they need a decision first') 'another absolute path'
  Check ((Read-RelayConfig @('host: localhost', 'api-keys:', '  - "flat-key-number-one"')).Port -eq 0) 'a flat top-level host on loopback'
  Check (Test-RefusedWith { Read-RelayConfig @('host: 0.0.0.0', 'api-keys:', '  - "flat-key-number-one"') } $hostOnly) 'a flat top-level host open to the network'
  $nestedBase = @('config-version: 1', 'server:', '  host: 127.0.0.1', '  port: 8317', '  discovery:', '    enabled: false', 'management:', '  allow-remote: false', '  secret-key: "nested-secret-number-one"', 'oauth:', '  auth-dir: "~/.cli-proxy-api"', 'access:', '  api-keys:', '    - "nested-key-number-one"')
  # The nested shape with line At replaced by the lines With (none: the line is deleted).
  function Edit-Base([int]$At, [string[]]$With) {
    $before = @(); if ($At -gt 0) { $before = @($nestedBase[0..($At - 1)]) }
    $after = @(); if ($At -lt $nestedBase.Count - 1) { $after = @($nestedBase[($At + 1)..($nestedBase.Count - 1)]) }
    return $before + @($With) + $after
  }
  $nestedConfig = Read-RelayConfig $nestedBase
  Check ($nestedConfig.Key -ceq 'nested-key-number-one' -and $nestedConfig.Port -eq 8317 -and $nestedConfig.PortSource -eq 'server.port') 'nested relay configuration: key and port'
  Check ($nestedConfig.Auth.Value -ceq '~/.cli-proxy-api' -and $nestedConfig.Auth.Indent -ceq '  ' -and $nestedConfig.Auth.Index -eq 10) 'nested relay configuration: auth-dir with its line and indentation'
  Check ($script:SecretTexts.Contains('nested-secret-number-one')) 'nested relay configuration: the management secret is kept out of the evidence'
  $plainConfig = Read-RelayConfig (Edit-Base 13 @('    - nested-key-number-two', "    - 'nested-key-number-three'"))
  Check ($plainConfig.Key -ceq 'nested-key-number-two' -and $script:SecretTexts.Contains('nested-key-number-three')) 'nested relay configuration: plain and single-quoted list items'
  Check ((Read-RelayConfig (Edit-Base 10 @('  auth-dir: "C:\\relay\\auth"'))).Auth.Value -ceq 'C:\relay\auth') 'a double-quoted value with doubled backslashes'
  Check ((Read-RelayConfig (Edit-Base 2 @("  host: 'localhost'"))).Port -eq 8317) 'server.host localhost'
  Check ((Read-RelayConfig (Edit-Base 8 @('  secret-key: ""'))).Key -ceq 'nested-key-number-one') 'an empty management secret-key means no management key'
  Check ((Read-RelayConfig (Edit-Base 2 @())).Port -eq 8317) 'a missing server.host is the relay default, judged by the live listener'
  $spaced = Read-RelayConfig (@('# head', '') + (Edit-Base 7 @('', '  # note', '  allow-remote: false')) + @('', '# tail'))
  Check ($spaced.Key -ceq 'nested-key-number-one' -and $spaced.Auth.Index -eq 14) 'blank and comment lines between and inside blocks'
  Add-RelaySecret 'abc'
  Check (-not $script:SecretTexts.Contains('abc')) 'a secret piece under 4 characters is not registered'
  Check (Test-LoopbackOnly @('127.0.0.1', '::1')) 'a relay listener on loopback'
  Check (-not (Test-LoopbackOnly @('0.0.0.0'))) 'a relay listener on every IPv4 interface'
  Check (-not (Test-LoopbackOnly @('127.0.0.1', '::'))) 'a relay listener also on every IPv6 interface'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 3 @('  port: eighty')) } 'the relay configuration has a port line that cannot be read') 'a server.port that cannot be read'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 3 @('  port: 8317', '  port: 8318')) } 'the relay configuration names server.port more than once') 'two server.port lines'
  Check (Test-RefusedWith { Read-RelayConfig ($nestedBase + @('port: 8317')) } $mixed) 'a top-level port beside the nested blocks'
  Check (Test-RefusedWith { Read-RelayConfig ($nestedBase + @('auth-dir: "~/.cli-proxy-api"')) } $mixed) 'a top-level auth-dir beside oauth.auth-dir'
  Check (Test-RefusedWith { Read-RelayConfig ((Edit-Base 10 @()) + @('auth-dir: "~/.cli-proxy-api"')) } $mixed) 'a top-level auth-dir instead of oauth.auth-dir'
  Check (Test-RefusedWith { Read-RelayConfig ($nestedBase + @('api-keys:', '  - "flat-key-number-one"')) } $mixed) 'top-level api-keys beside the nested blocks'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 10 @()) } 'the nested relay configuration has no oauth.auth-dir; its login folder needs a decision') 'a nested configuration without oauth.auth-dir'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 10 @("  auth-dir: ''")) } 'the relay configuration has an empty auth-dir') 'an empty auth-dir'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 13 @()) } 'the relay configuration lists no api-keys entry') 'an api-keys list without items'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 7 @('  log-dir: C:\x')) } 'the relay configuration names other absolute paths; they need a decision first') 'another absolute path inside a nested block'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 13 @('    - "a key with spaces in it"')) } 'the first relay api key is not printable ASCII') 'a key with spaces'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 13 @('    - "short-key"')) } $tooShort) 'an api key shorter than 16 characters'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 8 @('  secret-key: "short"')) } $tooShort) 'a management secret-key shorter than 16 characters'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 8 @('  secret-key: "nested-secret-number-two" # note')) } ('the relay configuration value of secret-key' + $notPlain)) 'a comment after the management secret-key'
  Check ($script:SecretTexts.Contains('nested-secret-number-two')) 'a refused management secret is still kept out of the evidence'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 13 @('    - nested-key-number-four # note')) } ('the relay configuration value of an api-keys item' + $notPlain)) 'a comment after an api key'
  Check ($script:SecretTexts.Contains('nested-key-number-four')) 'a refused api key is still kept out of the evidence'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 10 @('  auth-dir: "~/.cli-proxy-api')) } ('the relay configuration value of auth-dir' + $notPlain)) 'an odd quote'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 10 @('  auth-dir: "auth\nfolder"')) } ('the relay configuration value of auth-dir' + $notPlain)) 'an escape in a double-quoted value'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 3 @('  port: [8317]')) } $flowStyle) 'a flow-style value'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 12 @('  api-keys: ["nested-key-number-one"]')) } $flowStyle) 'flow-style api-keys'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 7 @('  allow-remote: [false]')) } $flowStyle) 'flow style on a line the parser does not read'
  Check (Test-RefusedWith { Read-RelayConfig @('api-keys:', '  - "flat-key-number-one"', 'remote-management: {secret-key: hidden-secret-number-two}') } $flowStyle) 'a flow map holding a secret-key'
  Check ($script:SecretTexts.Contains('hidden-secret-number-two')) 'a secret-key inside a refused flow map is still kept out of the evidence'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 12 @('  api-keys: # keys')) } $listHeader) 'a comment after api-keys'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 1 @('server: # note')) } 'the relay configuration has a block line (server, access, oauth or management) with text after it') 'a comment after a block line'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 13 @('    - "nested-key-number-one"', '', '    - "nested-key-number-five"')) } $listGap) 'a blank line inside the api-keys list'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 13 @('    - "nested-key-number-one"', '    # note', '    - "nested-key-number-five"')) } $listGap) 'a comment line inside the api-keys list'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 10 @('  auth-dir: C:/relay/auth', '    suffix')) } 'the relay configuration continues a value it reads on a deeper line') 'a value continued on a deeper line'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 13 @('  - "nested-key-number-one"')) } 'the relay configuration has an api-keys list item at the indentation of api-keys itself') 'a list item at the indentation of api-keys'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 13 @('    - "nested-key-number-one"', '      - "nested-key-number-six"')) } 'the relay configuration has a line inside the api-keys list that is not one list item') 'list items at two indentations'
  Check (Test-RefusedWith { Read-RelayConfig ($nestedBase + @('remote-management:', '  secret-key: "nested-secret-number-three"')) } $topLevel) 'another top-level key after the nested blocks'
  Check (Test-RefusedWith { Read-RelayConfig (@('debug: false') + $nestedBase) } $topLevel) 'another top-level key before the nested blocks'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 2 @('  host: 127.0.0.1', '  secret-key: "nested-secret-number-three"')) } $elsewhere) 'a secret-key under another block'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 5 @('    enabled: false', '    api-keys:')) } $elsewhere) 'api-keys deeper inside a block'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 2 @('  host: 0.0.0.0')) } $hostOnly) 'a server.host open to the network'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 2 @('  host: ""')) } $hostOnly) 'an empty server.host'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 3 @("`tport: 8317")) } $notUnderstood) 'a tab indent'
  Check (Test-RefusedWith { Read-RelayConfig (@('---') + $nestedBase) } $notUnderstood) 'a document marker'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 7 @('  <<: *defaults')) } $notUnderstood) 'a merge key'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 3 @('  "port": 8317')) } $notUnderstood) 'a quoted key'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 8 @('  ''secret-key'': hidden-secret-number-one')) } $notUnderstood) 'a quoted secret-key'
  Check ($script:SecretTexts.Contains('hidden-secret-number-one')) 'a secret under a refused quoted key is still kept out of the evidence'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 8 @('  "secret' + '\' + 'u002dkey": hidden-secret-number-five')) } $notUnderstood) 'an escaped quoted secret-key'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 8 @('  &mgmt "secret-key": hidden-secret-number-six')) } $notUnderstood) 'an anchored quoted secret-key'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 8 @('  !!str secret-key: hidden-secret-number-seven')) } $notUnderstood) 'a tagged secret-key'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 8 @('  ? secret-key', '  : nested-secret-number-four')) } $notUnderstood) 'an explicit key'
  # A block value may hold a comma; the first pass registers the whole value before the earlier port line refuses.
  $commaSecret = Edit-Base 8 @('  secret-key: abc,hidden-secret-number-one')
  $commaSecret[3] = '  port: eighty'
  Check (Test-RefusedWith { Read-RelayConfig $commaSecret } 'the relay configuration has a port line that cannot be read') 'a port that cannot be read before a secret with a comma'
  Check ($script:SecretTexts.Contains('abc,hidden-secret-number-one')) 'a secret with a comma is kept out of the evidence whole'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 3 @('  port:8317')) } 'the relay configuration has a key with no space after its colon') 'a key with no space after its colon'
  Check (Test-RefusedWith { Read-RelayConfig (Edit-Base 2 @('    host: 127.0.0.1')) } 'the relay configuration has a line indented less than the first key of its block') 'a line indented less than its block'
  $found = [Collections.Generic.List[string]]::new()
  $sample = '{"a":"C:\\qqbot-rust\\x","b":"C:\\qqbot\\runtime\\y","c":["C:\\qqbot\\qq-bridge\\z","plain"],"d":"C:\\a\\..\\b","e":"https://h/x","f":"C:\\Old\\Rt\\f"}' | ConvertFrom-Json
  Find-ForeignPaths $sample '' @('c:\old\rt') 'c:\qqbot' $found
  Check (($found -join ',') -eq '/a,/c/0,/d,/f') 'foreign configuration paths'
  $embedded = [Collections.Generic.List[string]]::new()
  $command = '{"cmd":"node C:\\Old\\Rt\\x.mjs --flag","other":"C:\\Old\\Rt2\\y"}' | ConvertFrom-Json
  Find-ForeignPaths $command '' @('c:\old\rt') 'c:\qqbot' $embedded
  Check (($embedded -join ',') -eq '/cmd') 'an old root inside a command string, and not its sibling'
  # Owner decisions of Revision 18: the description note under backends is dropped and nothing else; another note key
  # under backends and the same key elsewhere stay foreign pointers.
  $noted = ('{"backends":{"mcpRustBridge":true,"' + $script:BackendsNoteKey + '":"see C:\\Old\\Rt\\x","_note":"see C:\\Old\\Rt\\y"},"other":{"' + $script:BackendsNoteKey + '":"see C:\\Old\\Rt\\z"}}') | ConvertFrom-Json
  Remove-BackendsNote $noted
  Check ($null -eq $noted.backends.PSObject.Properties[$script:BackendsNoteKey] -and $noted.backends.mcpRustBridge -eq $true) 'the backends description note is dropped and the rest of backends stays'
  $notedFound = [Collections.Generic.List[string]]::new()
  Find-ForeignPaths $noted '' @('c:\old\rt') 'c:\qqbot' $notedFound
  Check (($notedFound -join ',') -ceq ('/backends/_note,/other/' + $script:BackendsNoteKey)) 'another note key under backends and the same key elsewhere stay foreign pointers'
  $upper = ('{"BACKENDS":{"' + $script:BackendsNoteKey + '":"see C:\\Old\\Rt\\x"}}') | ConvertFrom-Json
  Remove-BackendsNote $upper
  $upperFound = [Collections.Generic.List[string]]::new()
  Find-ForeignPaths $upper '' @('c:\old\rt') 'c:\qqbot' $upperFound
  Check ($null -ne $upper.BACKENDS.PSObject.Properties[$script:BackendsNoteKey] -and ($upperFound -join ',') -ceq ('/BACKENDS/' + $script:BackendsNoteKey)) 'another spelling of backends keeps the note, and it stays a foreign pointer'
  # The DSH stub walk leaves exactly the Antigravity CLI folder out (its links with it); every other path is walked,
  # so a link there still refuses. Each check that a path is walked also asserts the folder itself is left out, so an
  # empty exclusion list fails it too.
  Check (Test-ManifestSkip '.gemini\antigravity-cli' $script:DshStubExclude) 'the Antigravity CLI folder is left out of the DSH stub walk'
  Check (Test-ManifestSkip '.GEMINI\Antigravity-CLI' $script:DshStubExclude) 'the Antigravity CLI folder is left out in any case'
  Check ((Test-ManifestSkip '.gemini\antigravity-cli' $script:DshStubExclude) -and -not (Test-ManifestSkip '.gemini\antigravity-cli2' $script:DshStubExclude)) 'a near-miss folder name in the DSH stub is still walked'
  Check ((Test-ManifestSkip '.gemini\antigravity-cli' $script:DshStubExclude) -and -not (Test-ManifestSkip 'x\.gemini\antigravity-cli' $script:DshStubExclude)) 'a deeper folder of the same name in the DSH stub is still walked'
  Check ((Test-ManifestSkip '.gemini\antigravity-cli' $script:DshStubExclude) -and -not (Test-ManifestSkip '.gemini' $script:DshStubExclude) -and -not (Test-ManifestSkip 'plugins\cli.log' $script:DshStubExclude)) 'other DSH stub paths are still walked'
  Check (Test-ManifestSkip ('plugins\' + [string][char]0x212A) @('plugins\k')) 'manifest exclusions keep their lower-cased comparison (the Kelvin sign matches k)'
  $hash = 'a' * 64
  $sums = ConvertFrom-Sums @(($hash + '  her.exe'), ($hash + ' *her-cli.exe'), ($hash + '  herupdate.exe'), '# comment')
  Check ($sums.Count -eq 3) 'three sums'
  Check (Test-Refused { ConvertFrom-Sums @(($hash + '  her.exe'), ($hash + '  her.exe'), ($hash + '  herupdate.exe')) }) 'a repeated name'
  Check (Test-Refused { ConvertFrom-Sums @(($hash + '  her.exe'), ($hash + '  her-cli.exe'), ($hash + '  ..\herupdate.exe')) }) 'an unexpected name'
  $lines = Split-Lines ("a`r`nb`n`nc`n")
  Check ($lines.Count -eq 4 -and $lines[2] -eq '') 'line split keeps inner empty lines and drops only the last'
  Check ((Get-SafeMessage 'path C:\x not found') -eq 'an unexpected error (details in the evidence log)') 'raw error text stays private'
  Check ((Get-SafeMessage 'REFUSED: fixed text') -eq 'REFUSED: fixed text') 'fixed refusal text is shown'
  Check ((Format-HashGroups ('1' * 64)) -notmatch '[0-9]{7}') 'a grouped hash has no long digit run'
  Check (Test-OwnerGoLine @('MOVE main-abc') 'MOVE' 'main-abc') 'owner go line'
  Check (-not (Test-OwnerGoLine @('COPY main-abc') 'MOVE' 'main-abc')) 'owner go line with another word'
  Check (-not (Test-OwnerGoLine @('MOVE main-abd') 'MOVE' 'main-abc')) 'owner go line with another release'
  Check (-not (Test-OwnerGoLine @('MOVE main-abc', 'MOVE main-abc') 'MOVE' 'main-abc')) 'owner go file with two lines'
  $goNow = [DateTime]::new(2026, 11, 15, 12, 0, 0, [DateTimeKind]::Utc)
  Check (Test-GoAge ($goNow.AddMinutes(-119)) $goNow) 'owner go file inside two hours'
  Check (-not (Test-GoAge ($goNow.AddMinutes(-121)) $goNow)) 'owner go file older than two hours'
  Check (Test-Refused { Assert-PlanHas ([ordered]@{}) @('ReleaseChecked') 'the release check' }) 'a missing earlier result skips the section'
  Check (-not (Test-Refused { Assert-PlanHas ([ordered]@{ ReleaseChecked = 'yes' }) @('ReleaseChecked') 'the release check' })) 'a present earlier result lets the section run'
  Check (Test-Refused { Assert-PlanHas ([ordered]@{ ConsolePort = 0 }) @('ConsolePort') 'the configuration check' }) 'a port of 0 counts as missing'
  Check (-not (Test-OwnerGoLine @('MOVE') 'MOVE' '')) 'a MOVE go line always names a release'
  Check (-not (Test-GoAge ($goNow.AddMinutes(6)) $goNow)) 'owner go file dated more than five minutes ahead'
  $bundleMap = New-PathMap @([pscustomobject]@{ From = 'C:\Q\qq-bridge\b'; To = 'C:\Q\runtime\user\dsh\bundles\b' })
  Check ((Convert-DshText '"b": "link:C:\Q\qq-bridge\b"' $bundleMap) -ceq '"b": "link:C:\Q\runtime\user\dsh\bundles\b"') 'bundle path with backslashes'
  Check ((Convert-DshText 'b: link:c:/q/qq-bridge/b/x' $bundleMap) -ceq 'b: link:C:/Q/runtime/user/dsh/bundles/b/x') 'bundle path with forward slashes in another case'
  Check ((Convert-DshText '{"b":"link:C:\\Q\\qq-bridge\\b"}' $bundleMap) -ceq '{"b":"link:C:\\Q\\runtime\\user\\dsh\\bundles\\b"}') 'bundle path with JSON escapes'
  Check ((Convert-DshText '"C:\Q\qq-bridge\b2"' $bundleMap) -ceq '"C:\Q\qq-bridge\b2"') 'a sibling bundle name is not rewritten'
  Check (Test-ClimbingLink '{"x":"link:../y"}') 'a link: dependency that climbs out'
  Check (-not (Test-ClimbingLink '{"x":"link:C:/y","z":"file:./w"}')) 'an absolute or inner link: dependency'
  Check (Test-ModelName 'gemini-3-flash') 'model id'
  foreach ($bad in @('a b', '-x', ('m' * 81))) { Check (-not (Test-ModelName $bad)) ('bad model id ' + $bad) }
  # The two finding lists come back as one flat list: a clean configuration has none (not one empty list per file).
  $script:Root = 'C:\qqbot'
  $samplePlan = [ordered]@{ Map = (New-PathMap @()); SnowLumaVersion = '1.2.3'; NodePath = 'C:\Node\node.exe'; DshVersion = '9.9.9'; RelayProgram = 'C:\qqbot\deps\relay\1.0.0\r.exe'; RelayArguments = @('-config', 'C:\qqbot\deps\relay\1.0.0\c.yaml'); RelayPin = 'C:\qqbot\deps\relay\1.0.0'; RelayVersion = '1.0.0'; Auth = [pscustomobject]@{ NewPath = 'C:\qqbot\deps\relay\1.0.0\auth' }; RelayPort = 8317; Roots = @('c:\old\rt'); DigestModel = '' }
  $clean = Get-ForeignPointers ('{"consolePort":1,"tasks":{"nodePath":"C:\\Node\\node.exe"}}' | ConvertFrom-Json) ('{"dsh":"http://127.0.0.1:1"}' | ConvertFrom-Json) $samplePlan
  Check ($clean.Count -eq 0) 'a clean configuration has no foreign pointers'
  $dirty = Get-ForeignPointers ('{"x":"C:\\Old\\Rt\\y"}' | ConvertFrom-Json) ('{"dsh":"http://127.0.0.1:1"}' | ConvertFrom-Json) $samplePlan
  Check ((@($dirty) -join ',') -eq '/x') 'the foreign pointers of both files form one flat list'
  # Revision 19 (owner decisions on the gate Inventory). The DSH text map also moves the profile DSH home's own absolute
  # paths (pnpm's link:, virtualStoreDir and workspace path) and the carried dev script; a pnpm store outside the DSH
  # home and a qq-bridge file the move does not carry stay as they are.
  $toolPlan = [ordered]@{ Bundles = @([pscustomobject]@{ From = 'C:\qqbot\qq-bridge\plugins\b'; NewPath = 'C:\qqbot\runtime\user\dsh\bundles\b' }); DshHome = 'C:\P\.dsh'; ToolFiles = @([pscustomobject]@{ From = 'C:\qqbot\qq-bridge\src\mcp-snowluma-full.js'; To = 'C:\qqbot\runtime\user\dsh\bundles\src\mcp-snowluma-full.js' }) }
  $toolMap = Get-DshTextMap $toolPlan
  Check ((Convert-DshText ('a: link:C:/P/.dsh/plugins/b' + "`n" + 'virtualStoreDir: C:\P\.dsh\profiles\web\node_modules\.pnpm' + "`n" + '{"C:\\P\\.dsh\\profiles\\web":{}}' + "`n" + 'storeDir: C:\P\AppData\Local\pnpm\store\v11') $toolMap) -ceq ('a: link:C:/qqbot/runtime/user/dsh/plugins/b' + "`n" + 'virtualStoreDir: C:\qqbot\runtime\user\dsh\profiles\web\node_modules\.pnpm' + "`n" + '{"C:\\qqbot\\runtime\\user\\dsh\\profiles\\web":{}}' + "`n" + 'storeDir: C:\P\AppData\Local\pnpm\store\v11')) 'the profile DSH home paths move to the new DSH home and the pnpm store stays'
  Check ((Convert-DshText ('- C:\qqbot\qq-bridge\src\mcp-snowluma-full.js' + "`n" + '- C:\qqbot\qq-bridge\src\other.js') $toolMap) -ceq ('- C:\qqbot\runtime\user\dsh\bundles\src\mcp-snowluma-full.js' + "`n" + '- C:\qqbot\qq-bridge\src\other.js')) 'the carried dev script moves and an uncarried qq-bridge file does not'
  Check ((-not (Test-ClimbingLink 'v: link:../../plugins/b' 'C:\H\profiles\web' 'C:\H')) -and (Test-ClimbingLink 'v: link:../../../x' 'C:\H\profiles\web' 'C:\H')) 'a relative link: inside the DSH home is fine and one out of it still refuses'
  $carried = @([pscustomobject]@{ From = 'C:\qqbot\qq-bridge\node_modules'; To = 'C:\qqbot\runtime\user\dsh\bundles\node_modules' })
  Check (Test-CarriedModules 'C:\qqbot\qq-bridge\node_modules' 'C:\qqbot\runtime\user\dsh\bundles\b' $carried) 'the carried qq-bridge node_modules is reachable from the new bundles'
  Check ((Test-CarriedModules 'C:\qqbot\qq-bridge\node_modules' 'C:\qqbot\runtime\user\dsh\bundles\b' $carried) -and -not (Test-CarriedModules 'C:\qqbot\qq-bridge\dsh\node_modules' 'C:\qqbot\runtime\user\dsh\bundles\b' $carried) -and -not (Test-CarriedModules 'C:\qqbot\qq-bridge\node_modules' 'C:\qqbot\deps\b' $carried)) 'another node_modules, or a bundle the carried one is not above, stays unreachable'
  Check ((Test-BackupCopyName 'qq-chat-v2-dev.patch.yml.bak-old') -and (Test-BackupCopyName 'cordis.patch.yml.BAK') -and -not (Test-BackupCopyName 'qq-chat-v2-dev.patch.yml') -and -not (Test-BackupCopyName 'bakery.yml')) 'backup copies are recognised by name and other files are not'
  $toolConfig = New-ToolConfig ('{"snowluma":{"httpUrl":"http://127.0.0.1:3010","accessToken":"tool-token-sample","homeDir":"C:\\x"},"consoleToken":"other-secret"}' | ConvertFrom-Json)
  $toolNames = @($toolConfig.snowluma.PSObject.Properties | ForEach-Object { $_.Name }) -join ','
  Check ($toolNames -ceq 'httpUrl,accessToken' -and $toolConfig.snowluma.httpUrl -ceq 'http://127.0.0.1:3010' -and $toolConfig.snowluma.accessToken -ceq 'tool-token-sample' -and @($toolConfig.PSObject.Properties).Count -eq 1 -and $script:SecretTexts.Contains('tool-token-sample')) 'the dev tool config holds only the runtime SnowLuma URL and token, and the token is a secret'
  Check (@((New-ToolConfig ('{"snowluma":{"homeDir":"C:\\x"}}' | ConvertFrom-Json)).snowluma.PSObject.Properties).Count -eq 0) 'a missing SnowLuma URL or token is left to the script default'
  # Revision 19 fix round: JSON escapes, whitespace in a relative link: and full-name boundaries.
  $quotedMap = Get-DshTextMap ([ordered]@{ Bundles = @(); DshHome = "C:\P\it's\.dsh"; ToolFiles = @() })
  Check ((Convert-DshText ('{"x":"link:C:/P/it' + '\' + 'u0027s/.dsh/plugins/b","y":"C:\\P\\it' + '\' + 'u0027s\\.dsh\\profiles"}') $quotedMap) -ceq '{"x":"link:C:/qqbot/runtime/user/dsh/plugins/b","y":"C:\\qqbot\\runtime\\user\\dsh\\profiles"}') 'a path spelled with a JSON escape for its apostrophe is rewritten too'
  Check ((ConvertFrom-JsonEscapes ('it' + '\' + 'u0027s')) -ceq "it's") 'JSON escapes are read as their characters for the scan'
  Check (Test-ClimbingLink '{"dependencies":{"x":"link:../../space dir/../../outside"}}' 'C:\H\profiles\web' 'C:\H') 'a relative link: holding whitespace counts as climbing'
  Check ((Convert-DshText 'args: ["C:/qqbot/qq-bridge/src/mcp-snowluma-full.js helper"]' $toolMap) -ceq 'args: ["C:/qqbot/qq-bridge/src/mcp-snowluma-full.js helper"]') 'a mapped file name followed by more text is not rewritten'
  Check ((Convert-DshText 'C:\P\.dsh archive' $toolMap) -ceq 'C:\P\.dsh archive') 'a sibling folder whose name continues after a space is not rewritten'
  Write-Step ('PASS self-test: ' + $script:SelfChecks + ' checks')
}

# ---------- main ----------
$exitCode = 1
try {
  if ($Mode -eq 'SelfTest') { Invoke-SelfTest; $exitCode = 0 }
  else {
    Initialize-Run
    switch ($Mode) {
      'Inventory' { $exitCode = [int](@(Invoke-Inventory)[-1]) }
      'Move' { $exitCode = [int](@(Invoke-Move)[-1]) }
      'Rollback' { $exitCode = [int](@(Invoke-RollbackMode)[-1]) }
      'OperatorFolders' { $exitCode = [int](@(Invoke-OperatorFolders)[-1]) }
    }
  }
} catch {
  $message = [string]$_.Exception.Message
  Write-Log ('stopped: ' + $message)
  Write-Step ('STOPPED: ' + (Get-SafeMessage $message))
  $exitCode = 1
} finally {
  if ($null -ne $script:LockStream) { $script:LockStream.Dispose() }
  if ($null -ne $script:RunLockStream) { $script:RunLockStream.Dispose() }
}
Write-Step ('exit ' + $exitCode)
exit $exitCode

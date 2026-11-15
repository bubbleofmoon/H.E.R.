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
  [long]$FreeSpaceReserveBytes = 1GB,
  [switch]$AcceptDshProfileFiles,
  [switch]$AfterDone,
  [ValidateSet('', 'Staged', 'Stopped', 'Copied', 'Registered')][string]$RehearsalAbortAfter = '',
  [ValidateSet('', 'StaleIdentity', 'DisturbAfterDisable', 'CorruptCopy', 'LateDshChild')][string]$RehearsalFault = '',
  [switch]$RehearsalConfirm
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

if (-not ('HerMove67Native' -as [type])) {
  Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
public static class HerMove67Native {
  [StructLayout(LayoutKind.Sequential)] struct FileTime { public uint Low; public uint High; }
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
}
'@
}

# ---------- output, refusals and small helpers ----------
function Write-Log([string]$Text) {
  if ($script:Log) { [IO.File]::AppendAllText($script:Log, ([DateTime]::UtcNow.ToString('o') + ' ' + $Text + "`r`n"), [Text.UTF8Encoding]::new($false)) }
}
function Write-Step([string]$Text) { Write-Host ('[67] ' + $Text); Write-Log $Text }
function Stop-Refused([string]$Reason) { throw ('REFUSED: ' + $Reason) }
function Stop-Failed([string]$Reason) { throw ('FAILED: ' + $Reason) }
function Get-SafeMessage([string]$Text) {
  if ($Text.StartsWith('REFUSED: ') -or $Text.StartsWith('FAILED: ')) { return $Text }
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
function Get-PhaseIndex([string]$Phase) { return [Array]::IndexOf($Phases, $Phase) }
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
  if ($Rehearsal -and -not (Test-Within $Path $script:Temp)) { Stop-Refused 'rehearsal paths must lie inside the temporary folder' }
}
function Assert-RehearsalPort([int]$Port) {
  if ($Rehearsal -and $ProductionPorts -contains $Port) { Stop-Refused 'a rehearsal never uses a production port' }
}
function Confirm-Owner([string]$Word, [string]$Summary) {
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
function Convert-Config($Config, $plan) {
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
  $found = [Collections.Generic.List[string]]::new()
  Find-ForeignPaths $Dependencies '/dependencies.json' @($plan.Roots) ($script:Root.ToLowerInvariant()) $found
  return ,$found
}
function Get-BackupDestination($Config) {
  $dir = [string](Get-Prop (Get-Prop $Config 'backup') 'dir')
  if (-not $dir) { return (Join-Path $script:Root 'backups') }
  if ($dir -match '^\\\\') { return '' }
  if ($dir -match '^[A-Za-z]:[\\/]') { return (Get-Normal $dir) }
  return (Join-Path $script:Root $dir)
}

# ---------- relay configuration ----------
function Read-RelayConfig([string[]]$Lines) {
  $ports = @(); $auth = @(); $keys = @(); $absolute = 0
  for ($index = 0; $index -lt $Lines.Count; $index++) {
    $line = $Lines[$index]
    if ($line.Trim().StartsWith('#')) { continue }
    if ($line -match '^port:\s*["'']?(?<port>[0-9]{1,5})["'']?\s*(#.*)?$') { $ports += [int]$Matches['port']; continue }
    if ($line -match '^auth-dir:\s*(?<value>.*?)\s*$') { $auth += [pscustomobject]@{ Index = $index; Value = $Matches['value'].Trim('"', "'") }; continue }
    if ($line -match '^api-keys:\s*$') {
      while ($index + 1 -lt $Lines.Count -and $Lines[$index + 1] -match '^\s+-\s*(?<key>.+?)\s*$') { $keys += $Matches['key'].Trim('"', "'"); $index++ }
      continue
    }
    if ($line -match '(^|[\s"''=])[A-Za-z]:[\\/]') { $absolute++ }
  }
  if ($ports.Count -ne 1) { Stop-Refused 'the relay configuration needs exactly one top-level port line' }
  if ($auth.Count -gt 1) { Stop-Refused 'the relay configuration has more than one auth-dir line' }
  if ($keys.Count -lt 1) { Stop-Refused 'the relay configuration lists no api-keys entry' }
  if ($absolute -gt 0) { Stop-Refused 'the relay configuration names other absolute paths; they need a decision first' }
  $first = $keys[0]
  if ($first -notmatch '^[\x21-\x7e]{1,4096}$') { Stop-Refused 'the first relay api key is not printable ASCII' }
  $authEntry = $null
  if ($auth.Count -eq 1) { $authEntry = $auth[0] }
  return [pscustomobject]@{ Port = $ports[0]; Auth = $authEntry; Key = $first }
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
  $value = '~/.cli-proxy-api'; $index = -1
  if ($null -ne $Relay.Auth) { $value = [string]$Relay.Auth.Value; $index = [int]$Relay.Auth.Index }
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
  return [pscustomobject]@{ Source = $source; Inside = $inside; NewPath = $newPath; Action = $action; LineIndex = $index }
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
  Enable-ScheduledTask -TaskName $Record.Name -TaskPath '\' -ErrorAction Stop | Out-Null
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
function Get-Manifest([string]$Root, [string]$TargetRoot, [string[]]$Exclude, [string]$Label, [switch]$NoHash) {
  $base = Get-Normal $Root
  $target = Get-Normal $TargetRoot
  $skip = @{}
  foreach ($relative in @($Exclude)) { if ($relative) { $skip[$relative.ToLowerInvariant()] = $true } }
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
      if ($skip.ContainsKey($child.ToLowerInvariant())) { continue }
      if ($base.Length + 1 + $child.Length -gt 259 -or $target.Length + 1 + $child.Length -gt 259) { Stop-Refused ('a path is longer than 259 characters in ' + $Label) }
      if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { Stop-Refused ('a junction or link was found in ' + $Label) }
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
    if (Test-Present $stray) { Remove-Item -LiteralPath $stray -Recurse -Force }
  }
  if ($Rehearsal -and $RehearsalFault -eq 'CorruptCopy' -and $Label -eq 'snowluma') { [IO.File]::AppendAllText((Join-Path $To 'package.json'), ' ') }
  $copied = Get-Manifest $To $To @() $Label
  Save-Manifest $copied ($Label + '-copy')
  Assert-SameManifest $source $copied $Label
  Write-Step ('copied and verified ' + $Label + ': ' + $source.Count + ' entries')
}
function Protect-Folder([string]$Path) {
  $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
  $result = Invoke-Native (Join-Path $env:WINDIR 'System32\icacls.exe') @($Path, '/inheritance:r', '/grant:r', ('*' + $sid + ':(OI)(CI)F'), '*S-1-5-18:(OI)(CI)F', '*S-1-5-32-544:(OI)(CI)F') 'protect'
  if ($result.Code -ne 0) { Stop-Failed 'the install root could not be restricted' }
  $acl = Get-Acl -LiteralPath $Path
  if (-not $acl.AreAccessRulesProtected) { Stop-Failed 'the install root still inherits access' }
  $foreign = @($acl.Access | Where-Object { @($sid, 'S-1-5-18', 'S-1-5-32-544') -notcontains $_.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value })
  if ($foreign.Count -gt 0) { Stop-Failed 'the install root grants access to another account' }
  Write-Step 'the install root is restricted to the owner, SYSTEM and Administrators'
}
function Compare-DshStub([string]$Stub, [string]$DshHome) {
  $differences = [Collections.Generic.List[string]]::new()
  if (-not (Test-Present $Stub)) { return ,$differences }
  $homeIndex = @{}
  foreach ($entry in (Get-Manifest $DshHome $DshHome @() 'dsh-home')) { $homeIndex[$entry.Path.ToLowerInvariant()] = $entry.Sha256 }
  foreach ($entry in (Get-Manifest $Stub $Stub @() 'dsh-stub')) {
    if ($entry.Length -lt 0) { continue }
    $key = $entry.Path.ToLowerInvariant()
    if (-not $homeIndex.ContainsKey($key) -or $homeIndex[$key] -ne $entry.Sha256) { $differences.Add($entry.Path) }
  }
  return ,$differences
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
function Find-OldProfileReferences([string]$DshHome, [string]$RenderedProfile, $Roots) {
  $found = [Collections.Generic.List[string]]::new()
  foreach ($area in @('profiles', 'plugins')) {
    $folder = Join-Path $DshHome $area
    if (-not (Test-Present $folder)) { continue }
    foreach ($file in @(Get-ChildItem -LiteralPath $folder -Recurse -File -Force | Where-Object { @('.yml', '.yaml', '.json') -contains $_.Extension.ToLowerInvariant() })) {
      $relative = $area + $file.FullName.Substring($folder.Length)
      $path = $file.FullName
      if ($relative -ieq 'profiles\web\cordis.patch.yml') { $path = $RenderedProfile }
      $text = (Read-Text $path).Replace('\\', '\').Replace('/', '\').ToLowerInvariant()
      if (Test-NamesRoot $text $Roots) { $found.Add($relative) }
    }
  }
  return ,$found
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
  if ($env:HER_INSTALL_ROOT) { Stop-Refused 'HER_INSTALL_ROOT is set; open a new PowerShell without it' }
  $script:Root = Get-Normal $InstallRoot
  $script:Temp = Get-Normal ([IO.Path]::GetTempPath())
  if ($Rehearsal) {
    if (-not (Test-Within $script:Root $script:Temp) -or $script:Root -ieq $script:Temp) { Stop-Refused 'a rehearsal install root must be a folder inside the temporary folder' }
    if (-not $env:HER_TASK_SCHEDULER_FILE -or -not (Test-Within $env:HER_TASK_SCHEDULER_FILE $script:Temp) -or (Test-Within $env:HER_TASK_SCHEDULER_FILE $script:Root)) { Stop-Refused 'a rehearsal needs HER_TASK_SCHEDULER_FILE in the temporary folder, outside the install root' }
    foreach ($name in @($OldSupervisorTask, $OldRelayTask)) { if ($name -cnotmatch '^HER-Test-67-[0-9a-f]{32}-(supervisor|relay)$') { Stop-Refused 'a rehearsal uses only HER-Test-67 fixture tasks' } }
    if (-not $ProfileFolder) { Stop-Refused 'a rehearsal needs -ProfileFolder inside the temporary folder' }
    $script:RehearsalOldRoot = Join-Path (Split-Path -Parent $script:Root) 'unused-root'
  } else {
    if ($script:Root -ine 'C:\qqbot') { Stop-Refused 'the production install root is C:\qqbot' }
    if ($env:HER_TASK_SCHEDULER_FILE) { Stop-Refused 'HER_TASK_SCHEDULER_FILE is set; open a new PowerShell without it' }
    if ($RehearsalAbortAfter -or $RehearsalFault -or $RehearsalConfirm) { Stop-Refused 'rehearsal switches need -Rehearsal' }
    $nonInteractive = @([Environment]::GetCommandLineArgs() | Where-Object { $_ -like '-noni*' })
    if (-not [Environment]::UserInteractive -or $nonInteractive.Count -gt 0) { Stop-Refused 'production runs need an interactive PowerShell without -NonInteractive' }
    if ([Console]::IsInputRedirected) { Stop-Refused 'production runs need keyboard input; the input of this PowerShell is redirected' }
    $principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { Stop-Refused 'run this script from an administrator PowerShell' }
  }
  $script:ProfilePath = $ProfileFolder
  if (-not $script:ProfilePath) { $script:ProfilePath = $env:USERPROFILE }
  $base = Get-Normal $EvidenceRoot
  Assert-RehearsalPath $base
  if (Test-Within $base $script:Root) { Stop-Refused 'the evidence folder must be outside the install root' }
  [void][IO.Directory]::CreateDirectory($base)
  try { $script:LockStream = [IO.File]::Open((Join-Path $base 'move.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None) }
  catch { Stop-Refused 'another run of this script is active' }
  $script:Evidence = Join-Path $base ($Mode.ToLowerInvariant() + '-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff'))
  if (Test-Present $script:Evidence) { Stop-Refused 'the evidence folder for this run already exists' }
  [void][IO.Directory]::CreateDirectory($script:Evidence)
  $script:Log = Join-Path $script:Evidence 'log.txt'
  Write-Step ('mode ' + $Mode + '; evidence ' + $script:Evidence)
}

# ---------- the checks shared by Inventory and Move (read-only) ----------
function Get-ConfigPath($Value, [string]$Name) {
  if ($Value -isnot [string] -or $Value -notmatch '^[A-Za-z]:[\\/]') { Stop-Refused ($Name + ' is not an absolute path') }
  return (Get-Normal $Value)
}
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
  $plan.Identities = [ordered]@{}
  $plan.DshChildren = @()
  $script:OldRuntimePath = $plan.OldRuntime
  foreach ($folder in @($plan.OldRuntime, $plan.OldRelayFolder, $plan.DshHome, $plan.ReleaseSource)) {
    Assert-RehearsalPath $folder
    if (Test-Within $folder $script:Root) { Stop-Refused 'the old runtime, relay folder, profile DSH home and release source must be outside the install root' }
    if (Test-Within $script:Root $folder) { Stop-Refused 'the install root must not be inside an old production folder' }
  }
  foreach ($name in @('runtime', 'releases', 'deps', 'current', 'last-good', 'herupdate.exe', 'update-starts.json', 'update-alert.json', '.current-previous', '.last-good-previous')) {
    if (Test-Present (Join-Path $script:Root $name)) { Stop-Refused ('the install root already has ' + $name + '; roll back the earlier attempt or move it aside first') }
  }
  $config = $null; $dependencies = $null; $relayConfig = $null

  try {
    # Release images: exactly the three images the hash list names.
    $plan.Sums = ConvertFrom-Sums (Read-Lines (Join-Path $plan.ReleaseSource 'SHA256SUMS'))
    foreach ($name in @('her.exe', 'her-cli.exe', 'herupdate.exe')) {
      if ((Get-FileSha (Join-Path $plan.ReleaseSource $name)) -ne $plan.Sums[$name]) { Stop-Refused ('the release source ' + $name + ' differs from SHA256SUMS') }
    }
    $plan.SourceCli = Join-Path $plan.ReleaseSource 'her-cli.exe'
    $plan.Cli = Join-Path $script:Root 'current\her-cli.exe'
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
    $plan.NodePath = Get-ConfigPath (Get-Prop $tasks 'nodePath') 'tasks.nodePath'
    $plan.OldRelease = Get-ConfigPath (Get-Prop $tasks 'installRoot') 'tasks.installRoot'
    $plan.SnowLuma = Get-ConfigPath (Get-Prop $startAll 'snowLumaDirectory') 'tasks.start-all.snowLumaDirectory'
    $plan.DshScript = Get-ConfigPath (Get-Prop $startAll 'dshScript') 'tasks.start-all.dshScript'
    foreach ($folder in @($plan.OldRelease, $plan.SnowLuma, $plan.DshScript)) { Assert-RehearsalPath $folder }
    $plan.ConsolePort = [int](Get-Prop $config 'consolePort')
    if ($plan.ConsolePort -le 0) { Stop-Refused 'the old configuration has no consolePort' }
    Assert-RehearsalPort $plan.ConsolePort
    $plan.OneBotPort = Get-Port (Get-Prop $dependencies 'onebot') 'onebot'
    $plan.OneBotWsPort = 0
    if ($null -ne (Get-Prop $dependencies 'onebot_ws')) { $plan.OneBotWsPort = Get-Port (Get-Prop $dependencies 'onebot_ws') 'onebot_ws' }
    $plan.DshPort = Get-Port (Get-Prop $dependencies 'dsh') 'dsh'
    $plan.RuntimeExclude = @('user\dsh', 'import-manifest.json', 'data\lifecycle\session-handover.json', 'data\lifecycle\session-handover-required.json')
    $plan.BackupDestination = Get-BackupDestination $config
    if ($plan.BackupDestination) { Assert-RehearsalPath $plan.BackupDestination }
  } catch { Add-Failure $_ 'configuration' }

  try {
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
    $match = [regex]::Match($plan.DshScript, '^(?<root>.+)\\node_modules\\@deepseek-ai\\dsh\\.+$', [Text.RegularExpressions.RegexOptions]::IgnoreCase)
    if (-not $match.Success) { Stop-Refused 'the configured DSH script is not inside an @deepseek-ai/dsh package' }
    $plan.NpxRoot = $match.Groups['root'].Value
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
    $plan.RelayProgram = $plan.RelayPin + $relayExe.Substring($plan.OldRelayFolder.Length)
    $plan.RelayArguments = @(Convert-RelayArguments $tokens $plan.OldRelayFolder $plan.RelayPin)
    $relayConfigPath = Find-RelayConfig $tokens $plan.OldRelayFolder
    if (-not (Test-Within $relayConfigPath $plan.OldRelayFolder) -or -not (Test-Path -LiteralPath $relayConfigPath -PathType Leaf)) { Stop-Refused 'the relay configuration file is not inside the old relay folder' }
    $plan.RelayConfigRelative = $relayConfigPath.Substring($plan.OldRelayFolder.Length + 1)
    $relayConfig = Read-RelayConfig (Read-Lines $relayConfigPath)
    $plan.RelayPort = $relayConfig.Port
    Assert-RehearsalPort $plan.RelayPort
    $script:RelayKey = $relayConfig.Key
    if ((Get-ListenerPid $plan.RelayPort) -ne $relay.Pid) { Add-Blocker 'the relay port is not served by the relay process' }
    if (@($plan.OneBotPort, $plan.OneBotWsPort, $plan.DshPort, $plan.ConsolePort) -contains $plan.RelayPort) { Add-Blocker 'the relay port is not distinct from the OneBot, DSH and console ports' }
    foreach ($argument in $plan.RelayArguments) { if ($argument.Contains($script:RelayKey)) { Add-Blocker 'the relay command line carries the client key' } }
    $plan.Identities['relay'] = $relay
    if ((Get-OwnerSid ([int]$relay.Pid)) -ne [Security.Principal.WindowsIdentity]::GetCurrent().User.Value) { Add-Blocker 'the relay process runs as another account than this PowerShell' }
    $plan.RelayParents = Get-ParentImages ([int]$relay.Pid)
    $plan.Auth = Resolve-RelayAuth $relayConfig $plan.OldRelayFolder $plan.RelayPin $plan.Profile
    Assert-RehearsalPath $plan.Auth.Source
  } catch { Add-Failure $_ 'relay' }

  try {
    # Copies, path map and the roots no new path may keep.
    $copies = @(
      [pscustomobject]@{ From = $plan.OldRuntime; To = (Join-Path $script:Root 'runtime'); Exclude = $plan.RuntimeExclude; Label = 'runtime' },
      [pscustomobject]@{ From = $plan.DshHome; To = (Join-Path $script:Root 'runtime\user\dsh'); Exclude = @(); Label = 'dsh-home' },
      [pscustomobject]@{ From = $plan.SnowLuma; To = (Join-Path $script:Root ('deps\snowluma\' + $plan.SnowLumaVersion)); Exclude = @(); Label = 'snowluma' },
      [pscustomobject]@{ From = $plan.NpxRoot; To = (Join-Path $script:Root ('deps\dsh\' + $plan.DshVersion)); Exclude = @(); Label = 'dsh' },
      [pscustomobject]@{ From = $plan.OldRelayFolder; To = $plan.RelayPin; Exclude = @(); Label = 'relay' }
    )
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
    # Old scheduled tasks, other enabled tasks that would still start old production, and the HER task.
    $plan.OldTasks = @((Get-TaskRecord $OldSupervisorTask), (Get-TaskRecord $OldRelayTask))
    foreach ($task in $plan.OldTasks) { if (-not $task.Enabled) { Add-Blocker ('the scheduled task ' + $task.Name + ' is not enabled') } }
    foreach ($task in @(Get-ScheduledTask -ErrorAction Stop)) {
      if ([string]$task.State -eq 'Disabled' -or @($OldSupervisorTask, $OldRelayTask, 'HER') -contains $task.TaskName) { continue }
      $text = (@($task.Actions | ForEach-Object { [string]$_.Execute + ' ' + [string]$_.Arguments + ' ' + [string]$_.WorkingDirectory }) -join ' ').Replace('/', '\').ToLowerInvariant()
      if (Test-NamesRoot $text $plan.Roots) { Add-Blocker ('the enabled scheduled task ' + $task.TaskName + ' names an old production folder') }
    }
    $herTask = (Invoke-Cli $plan.SourceCli @('scheduled-task', 'read') 'her-task-read').Output | ConvertFrom-Json
    if ($null -ne $herTask.xml) { Add-Blocker 'a scheduled task named HER already exists' }
  } catch { Add-Failure $_ 'scheduled tasks' }

  try {
    # DSH home: the runtime's own DSH files must equal the profile copies the move installs.
    if (-not (Test-Path -LiteralPath $plan.DshHome -PathType Container)) { Stop-Refused 'the profile DSH home is missing' }
    $profileFile = Join-Path $plan.DshHome 'profiles\web\cordis.patch.yml'
    if (-not (Test-Path -LiteralPath $profileFile -PathType Leaf)) { Stop-Refused 'the profile DSH home has no profiles\web\cordis.patch.yml' }
    $differences = Compare-DshStub (Join-Path $plan.OldRuntime 'user\dsh') $plan.DshHome
    Write-Utf8 (Join-Path $script:Evidence 'dsh-stub-differences.txt') ((@($differences) -join "`r`n") + "`r`n")
    if ($differences.Count -gt 0 -and -not $AcceptDshProfileFiles) { Add-Blocker ('' + $differences.Count + ' runtime DSH files differ from the profile DSH home (list in the evidence folder); pass -AcceptDshProfileFiles to take the profile copies') }
    $rendered = New-McpProfile $plan.SourceCli $profileFile (Join-Path $script:Evidence 'mcp-preview') $plan.ConsolePort
    $references = Find-OldProfileReferences $plan.DshHome $rendered $plan.Roots
    Write-Utf8 (Join-Path $script:Evidence 'dsh-profile-references.txt') ((@($references) -join "`r`n") + "`r`n")
    if ($references.Count -gt 0) { Add-Blocker ('' + $references.Count + ' DSH profile or plugin files name old production paths (list in the evidence folder)') }
  } catch { Add-Failure $_ 'dsh home' }

  try {
    # Configuration dry run on separate copies; only JSON pointers are written, never values.
    $preview = Read-Json (Join-Path $plan.OldRuntime 'config\config.json')
    $foreign = @(Convert-Config $preview $plan)
    $foreign += @(Convert-Dependencies (Read-Json (Join-Path $plan.OldRuntime 'dependencies.json')) $plan)
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
  } catch { Add-Failure $_ 'install root access' }

  try {
    # Backups (owner decision a): no archive the new retention would prune.
    $destination = [string]$plan.BackupDestination
    if ($destination -and (Test-Path -LiteralPath $destination -PathType Container)) {
      $archives = @(Get-ChildItem -LiteralPath $destination -File -Force | Where-Object { $_.Name -match $ArchivePattern })
      if ($archives.Count -gt 0) { Add-Blocker ('the backup folder holds ' + $archives.Count + ' archives the new retention would prune; delete them first (owner decision a)') }
    }
  } catch { Add-Failure $_ 'backups' }

  try {
    # Sizes, path lengths and junctions of every copy; free disk space.
    $bytes = 0.0
    foreach ($copy in $plan.Copies) {
      foreach ($item in (Get-Manifest $copy.From $copy.To $copy.Exclude $copy.Label -NoHash)) { if ($item.Length -gt 0) { $bytes += $item.Length } }
    }
    foreach ($name in @('her.exe', 'her-cli.exe', 'herupdate.exe')) { $image = Get-Item -LiteralPath (Join-Path $plan.ReleaseSource $name); $bytes += 2 * $image.Length }
    $plan.CopyBytes = $bytes
    $drive = [IO.DriveInfo]::new($script:Root.Substring(0, 1))
    if (-not (Test-FreeSpace $drive.AvailableFreeSpace $bytes $FreeSpaceReserveBytes)) { Add-Blocker 'not enough free disk space for the copies' }
  } catch { Add-Failure $_ 'sizes' }

  try {
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
  $dependenciesPath = Join-Path $runtime 'dependencies.json'
  $dependencies = Read-Json $dependenciesPath
  $foreign = Convert-Dependencies $dependencies $plan
  if ($foreign.Count -gt 0) { Stop-Failed 'dependencies.json paths need a decision (pointers in the evidence folder)' }
  Write-JsonFile $dependenciesPath $dependencies
  # Relay: the login folder line and the client key file HER uses for its health probe.
  $relayConfig = Join-Path $plan.RelayPin $plan.RelayConfigRelative
  $lines = [Collections.Generic.List[string]]::new([string[]](Read-Lines $relayConfig))
  $authLine = "auth-dir: '" + $plan.Auth.NewPath + "'"
  if ($plan.Auth.Action -eq 'replace') { $lines[[int]$plan.Auth.LineIndex] = $authLine } elseif ($plan.Auth.Action -eq 'append') { $lines.Add($authLine) }
  [IO.File]::WriteAllLines($relayConfig, $lines, [Text.UTF8Encoding]::new($false))
  $keyFile = Join-Path $plan.RelayPin 'her-relay-client.key'
  if (Test-Present $keyFile) { Stop-Failed 'the relay pin already has her-relay-client.key' }
  [IO.File]::WriteAllText($keyFile, $script:RelayKey, [Text.ASCIIEncoding]::new())
  # DSH profile: the three MCP servers point at the installed image and the new runtime.
  $profileFile = Join-Path $runtime 'user\dsh\profiles\web\cordis.patch.yml'
  $rendered = New-McpProfile $plan.Cli $profileFile (Join-Path $script:Evidence 'mcp-move') $plan.ConsolePort
  Copy-Item -LiteralPath $rendered -Destination $profileFile -Force
  if ((Get-FileSha $rendered) -ne (Get-FileSha $profileFile)) { Stop-Failed 'the DSH profile could not be replaced' }
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
function Start-NewProduction($plan) {
  $runtime = Join-Path $script:Root 'runtime'
  [void](Invoke-Cli $plan.Cli @('lifecycle', 'set', 'running', '--runtime-root', $runtime, '--by', 'owner-67', '--reason', 'move into the install root') 'new-set-running')
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
    $failing = @($verdict.parts.PSObject.Properties | Where-Object { [string]$_.Value.verdict -ne 'pass' } | ForEach-Object { $_.Name })
    Stop-Failed ('the health check failed: ' + ($failing -join ', '))
  }
  Write-Step ('health: every part passes (' + (@($verdict.parts.PSObject.Properties | ForEach-Object { $_.Name }) -join ', ') + ')')
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
  Write-Step ('To roll back later: -Mode Rollback -Production -StateFile ' + $script:StatePath + ' -AfterDone (what the new runtime writes until then stays in the moved-aside folder).')
}

# ---------- rollback ----------
function Move-NewLayoutAside {
  foreach ($name in @('current', 'last-good', '.current-previous', '.last-good-previous')) {
    $path = Join-Path $script:Root $name
    if (-not (Test-Present $path)) { continue }
    $link = Get-Item -LiteralPath $path -Force
    if (-not ($link.Attributes -band [IO.FileAttributes]::ReparsePoint)) { Stop-Failed ($name + ' is not a junction; it was left in place') }
    [IO.Directory]::Delete($path)
  }
  $aside = Join-Path $script:Root ('move-67-attempt-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss-fff'))
  foreach ($name in @('herupdate.exe', 'update-starts.json', 'update-alert.json', 'releases', 'deps', 'runtime')) {
    $path = Join-Path $script:Root $name
    if (-not (Test-Present $path)) { continue }
    [void][IO.Directory]::CreateDirectory($aside)
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
function Start-OldProduction($plan) {
  if (Test-OldProductionUp $plan) { return }
  [void](Invoke-Old @('lifecycle', 'set', 'running', '--runtime-root', $plan.OldRuntime, '--by', 'owner-67', '--reason', 'rollback of the move') 'rollback-old-running')
  if ((Get-OldOwnerStates).supervisor -ne 'held') {
    $tasks = @()
    if ((Get-ListenerPid ([int]$plan.RelayPort)) -eq 0) { $tasks += [string]$plan.OldRelayTask }
    $tasks += [string]$plan.OldSupervisorTask
    foreach ($name in $tasks) {
      # A task instance still running (its launcher waits) would ignore the start.
      Wait-Until { (Get-TaskState $name) -ne 'Running' } 60 ('the old task ' + $name + ' still has a running instance, so it cannot start the old production')
      Start-ScheduledTask -TaskName $name -TaskPath '\' -ErrorAction Stop
    }
  }
  Wait-Until { Test-OldProductionUp $plan } 300 'the old production did not come back within 5 minutes'
}
function Restore-RootAcl($plan) {
  $saved = [string]$plan.RootAccessSddl
  if (-not $saved) { return }
  $sections = [Security.AccessControl.AccessControlSections]::Access
  $acl = Get-Acl -LiteralPath $script:Root
  if ($acl.GetSecurityDescriptorSddlForm($sections) -eq $saved) { return }
  $acl.SetSecurityDescriptorSddlForm($saved, $sections)
  # Persist only the DACL section that changed; the owner is left alone.
  [IO.Directory]::SetAccessControl($script:Root, [Security.AccessControl.DirectorySecurity]$acl)
  $restored = Get-Acl -LiteralPath $script:Root
  if ($restored.GetSecurityDescriptorSddlForm($sections) -ne $saved) { Stop-Failed 'the install root access could not be restored' }
  Write-Step 'the install root access is restored to what it was before the move'
}
function Invoke-Rollback([string]$Why) {
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
        [void](Invoke-Cli $runtimeCli @('stop', '--runtime-root', $runtime, '--confirm', '--with-dependencies') 'rollback-new-stop' @(0, 3))
        if ((Get-NewOwnerStates $runtimeCli).supervisor -eq 'held') { [void](Invoke-Cli $runtimeCli @('lifecycle', 'stop-supervisor', '--runtime-root', $runtime, '--confirm') 'rollback-new-stop-supervisor') }
        Wait-Until { $states = Get-NewOwnerStates $runtimeCli; $states.supervisor -ne 'held' -and $states.live -ne 'held' -and $states.console -ne 'held' } 30 'the new owners did not stop'
        if (-not $oldAlreadyUp) { Wait-Until { @(Get-ProductionPortList $plan | Where-Object { (Get-ListenerPid $_) -ne 0 }).Count -eq 0 } 30 'a production port is still held by the new runtime' }
        if (-not [string]$script:State.NewStoppedAt) { $script:State.NewStoppedAt = [DateTime]::UtcNow.ToString('o'); Save-State ([string]$script:State.Phase) }
        Write-Step 'the new runtime and its dependencies are stopped'
      } catch { $newRunning = $true; $problems.Add('the new runtime could not be stopped: ' + (Get-SafeMessage $_.Exception.Message)) }
    }
  }
  if ($phase -ge (Get-PhaseIndex 'OldTasksDisabled')) {
    foreach ($task in @($plan.OldTasks)) { try { Restore-Task $task } catch { $problems.Add((Get-SafeMessage $_.Exception.Message)) } }
  }
  if ($phase -ge (Get-PhaseIndex 'Stopping')) {
    if ($newRunning) { $oldUp = $false }
    else {
      try { Start-OldProduction $plan; Write-Step 'the old production runs again' }
      catch { $oldUp = $false; $problems.Add((Get-SafeMessage $_.Exception.Message)) }
    }
  }
  if (-not $newRunning) {
    try { Move-NewLayoutAside } catch { $problems.Add('the new layout could not be moved aside: ' + (Get-SafeMessage $_.Exception.Message)) }
  }
  # The install root restriction stays only after a finished move (coordinator decision); any earlier phase restores it.
  if ($phase -lt (Get-PhaseIndex 'Done')) {
    try { Restore-RootAcl $plan } catch { $problems.Add((Get-SafeMessage $_.Exception.Message)) }
  }
  if ($problems.Count -eq 0) {
    Save-State 'RolledBack'
    Write-Step 'ROLLED BACK: the old tasks are restored and the old production runs from the untouched originals'
    return 'done'
  }
  foreach ($problem in $problems) { Write-Step ('ROLLBACK PROBLEM: ' + $problem) }
  if ($oldUp) {
    Write-Step ('THE OLD PRODUCTION RUNS AGAIN, but the rollback is incomplete. Fix the problem above and run -Mode Rollback -StateFile ' + $script:StatePath + ' again.')
    return 'partial'
  }
  Write-Step ('PRODUCTION MAY BE DOWN. Fix the problem above, then run -Mode Rollback -StateFile ' + $script:StatePath + ' again. By hand the old production starts after its desired state is running: Start-ScheduledTask -TaskName ' + $plan.OldRelayTask + ' and Start-ScheduledTask -TaskName ' + $plan.OldSupervisorTask + '.')
  return 'down'
}

# ---------- modes ----------
function Invoke-Inventory {
  $plan = Get-MovePlan
  $copy = [ordered]@{}
  foreach ($key in @($plan.Keys)) { if ($key -ne 'OldTasks') { $copy[$key] = $plan[$key] } }
  $copy['OldTasks'] = @($plan.OldTasks | ForEach-Object { [pscustomobject]@{ Name = $_.Name; Shape = $_.Shape; Enabled = $_.Enabled; State = $_.State } })
  $copy['Blockers'] = @($script:Blockers)
  Write-JsonFile (Join-Path $script:Evidence 'inventory.json') $copy
  Write-Step ('INVENTORY: ' + $script:Blockers.Count + ' blockers; details in ' + (Join-Path $script:Evidence 'inventory.json'))
  if ($script:Blockers.Count -gt 0) { return 1 }
  return 0
}
function Invoke-Move {
  $plan = Get-MovePlan
  $megabytes = [math]::Ceiling([double]$plan.CopyBytes / 1MB)
  Confirm-Owner 'MOVE' ('Ready: about ' + $megabytes + ' MB to copy; expected downtime 3 to 6 minutes, about 13 at most with a rollback. Nothing is stopped before you confirm.')
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
    $result = [string](@(Invoke-Rollback $why)[-1])
    if ($result -ne 'done') { return 3 }
    if ($unchanged) { return 1 }
    return 2
  }
}
function Invoke-RollbackMode {
  if (-not $StateFile) { Stop-Refused '-StateFile is required' }
  $script:StatePath = Get-Normal $StateFile
  Assert-RehearsalPath $script:StatePath
  $loaded = Read-Json $script:StatePath
  if ([bool]$loaded.Rehearsal -ne [bool]$Rehearsal) { Stop-Refused 'the state file belongs to a run of the other kind (rehearsal or production)' }
  if ((Get-Normal ([string]$loaded.Root)) -ine $script:Root) { Stop-Refused 'the state file belongs to another install root' }
  $script:State = $loaded
  $script:OldExe = [string]$loaded.Plan.OldExe
  $script:OldCli = [string]$loaded.Plan.OldCli
  $script:OldRuntimePath = [string]$loaded.Plan.OldRuntime
  foreach ($path in @($script:OldExe, $script:OldCli, $script:OldRuntimePath, [string]$loaded.Plan.SourceCli)) { Assert-RehearsalPath $path }
  if ($Rehearsal) {
    foreach ($name in @([string]$loaded.Plan.OldSupervisorTask, [string]$loaded.Plan.OldRelayTask)) {
      if ($name -cnotmatch '^HER-Test-67-[0-9a-f]{32}-(supervisor|relay)$') { Stop-Refused 'a rehearsal uses only HER-Test-67 fixture tasks' }
    }
  }
  if ($loaded.Phase -eq 'RolledBack') { Write-Step 'nothing to roll back: this run was already rolled back'; return 0 }
  if ($loaded.Phase -eq 'Done' -and -not $AfterDone) { Stop-Refused 'the move finished; pass -AfterDone to roll it back (what the new runtime wrote since stays in the moved-aside folder)' }
  Confirm-Owner 'ROLLBACK' 'Ready to roll back: the new runtime stops and the old production starts from the originals.'
  $result = [string](@(Invoke-Rollback 'requested by the owner')[-1])
  if ($result -eq 'done') { return 0 }
  return 3
}
function Invoke-OperatorFolders {
  $from = Get-Normal $OperatorSource
  Assert-RehearsalPath $from
  $to = Join-Path $script:Root 'ops'
  if (-not (Test-Path -LiteralPath $from -PathType Container)) { Stop-Refused 'the ops folder is missing' }
  if (Test-Present $to) { Stop-Refused 'the install root already has ops' }
  Confirm-Owner 'COPY' 'Ready to copy the operator scripts into the install root ops folder.'
  Copy-Verified $from $to @() 'ops'
  Write-Step 'OPERATOR FOLDER COPIED: the original stays until the owner approves its deletion'
  return 0
}
function Test-Refused([scriptblock]$Block) {
  try { [void](& $Block); return $false } catch { return $_.Exception.Message.StartsWith('REFUSED: ') }
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
  $relay = Read-RelayConfig @('# comment C:\x', 'port: 8317', 'auth-dir: "~/.cli-proxy-api"', 'api-keys:', '  - "key-one"', '  - key-two', 'proxy-url: "http://127.0.0.1:9"')
  Check ($relay.Port -eq 8317 -and $relay.Auth.Value -eq '~/.cli-proxy-api' -and $relay.Key -eq 'key-one') 'relay configuration subset'
  Check (Test-Refused { Read-RelayConfig @('port: 1', 'port: 2', 'api-keys:', '  - k') }) 'two port lines'
  Check (Test-Refused { Read-RelayConfig @('port: 1', 'api-keys:', '  - k', 'logs-dir: C:\x') }) 'another absolute path'
  Check (Test-Refused { Read-RelayConfig @('port: 1', 'api-keys:', '  - "a b"') }) 'a key with a space'
  Check (Test-Refused { Read-RelayConfig @('port: 1') }) 'no key'
  $found = [Collections.Generic.List[string]]::new()
  $sample = '{"a":"C:\\qqbot-rust\\x","b":"C:\\qqbot\\runtime\\y","c":["C:\\qqbot\\qq-bridge\\z","plain"],"d":"C:\\a\\..\\b","e":"https://h/x","f":"C:\\Old\\Rt\\f"}' | ConvertFrom-Json
  Find-ForeignPaths $sample '' @('c:\old\rt') 'c:\qqbot' $found
  Check (($found -join ',') -eq '/a,/c/0,/d,/f') 'foreign configuration paths'
  $embedded = [Collections.Generic.List[string]]::new()
  $command = '{"cmd":"node C:\\Old\\Rt\\x.mjs --flag","other":"C:\\Old\\Rt2\\y"}' | ConvertFrom-Json
  Find-ForeignPaths $command '' @('c:\old\rt') 'c:\qqbot' $embedded
  Check (($embedded -join ',') -eq '/cmd') 'an old root inside a command string, and not its sibling'
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
}
Write-Step ('exit ' + $exitCode)
exit $exitCode

# Build the Rust bridge.
# Usage: powershell -NoProfile -File her\build.ps1 -Release [-CandidateDir <absolute path>]
# A build produces a candidate only. Deployment is a separate operation.
#
# IMPORTANT - why this script is careful with resources:
#   This box is a 2-vCPU VM whose commit charge sits around 90% (19.9/22.2 GB).
#   rustc + the MSVC linker can briefly ask for 1-2 GB; when that happens the whole
#   machine pages and every other process stalls - including the live QQ bridge
#   serving real users (observed 2026-10-21: a group message waited ~3 minutes
#   while a release build was linking). So builds run:
#     * at BelowNormal priority,
#     * with -j 1 (one rustc at a time) to keep the memory peak flat.
#   Slower, but it does not freeze the bot.
param(
    [switch]$Release,
    [string]$CandidateDir = ''
)

$ErrorActionPreference = 'Stop'
$env:CARGO_HOME = Join-Path $env:USERPROFILE '.cargo'
$env:CARGO_BUILD_JOBS = '1'

if (-not $CandidateDir) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss-fff'
    $CandidateDir = Join-Path (Join-Path $PSScriptRoot '..\backups\her-candidates') ("$stamp-$PID")
}
if (-not [IO.Path]::IsPathRooted($CandidateDir)) {
    throw 'CandidateDir must be an absolute path.'
}
$candidatePath = [IO.Path]::GetFullPath($CandidateDir).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
$liveTarget = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'target')).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
function Get-CandidateHardLinkTargets([string]$Path) {
    if (-not ('QQBotCandidateLinks' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
public static class QQBotCandidateLinks {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, ExactSpelling=true, SetLastError=true)]
    static extern IntPtr FindFirstFileNameW(string path, uint flags, ref uint length, StringBuilder name);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, ExactSpelling=true, SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    static extern bool FindNextFileNameW(IntPtr handle, ref uint length, StringBuilder name);
    [DllImport("kernel32.dll", ExactSpelling=true, SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    static extern bool FindClose(IntPtr handle);
    public static string[] Targets(string path) {
        var name = new StringBuilder(32768); uint length=(uint)name.Capacity;
        IntPtr handle=FindFirstFileNameW(path, 0, ref length, name);
        if (handle==new IntPtr(-1) || handle==IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        var targets=new List<string>();
        try {
            while (true) {
                targets.Add(Path.GetFullPath(Path.Combine(Path.GetPathRoot(path), name.ToString().TrimStart('\\'))));
                name.Clear(); length=(uint)name.Capacity;
                if (!FindNextFileNameW(handle, ref length, name)) {
                    int error=Marshal.GetLastWin32Error();
                    if (error==38) break; // ERROR_HANDLE_EOF
                    throw new Win32Exception(error);
                }
            }
        } finally {
            if (!FindClose(handle)) throw new Win32Exception(Marshal.GetLastWin32Error());
        }
        return targets.ToArray();
    }
}
'@
    }
    [QQBotCandidateLinks]::Targets($Path)
}
# Lexical comparisons alone miss a junction or symlink in any existing ancestor.
# Refuse such paths before the toolchain is launched, including when the final
# candidate directory has not been created yet.
$ancestor = $candidatePath
while ($ancestor) {
    if (Test-Path -LiteralPath $ancestor) {
        $entry = Get-Item -LiteralPath $ancestor -Force -ErrorAction Stop
        if (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw 'CandidateDir must not contain a redirected path component.'
        }
    }
    $parent = [IO.Directory]::GetParent($ancestor)
    if ($null -eq $parent) { break }
    $ancestor = $parent.FullName
}
if ($candidatePath.Equals($liveTarget, [StringComparison]::OrdinalIgnoreCase) -or
    $candidatePath.StartsWith($liveTarget + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
    $candidatePath.StartsWith($liveTarget + [IO.Path]::AltDirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'CandidateDir must not be the live build directory or a directory inside it.'
}
# Cargo writes beneath the candidate as well as at its root. Refuse existing
# redirected directories and outside artifact aliases before giving it that tree.
if (Test-Path -LiteralPath $candidatePath) {
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($candidatePath)
    while ($pending.Count -gt 0) {
        foreach ($entry in Get-ChildItem -LiteralPath $pending.Pop() -Force -ErrorAction Stop) {
            if (($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw 'Candidate output must not contain a redirected path component.'
            }
            if ($entry.LinkType -eq 'HardLink') {
                # Cargo links its own artifact names within the target tree.
                # Require all names enumerated by the Windows filesystem API
                # to stay there, with the live build tree always excluded.
                $targets = @(Get-CandidateHardLinkTargets $entry.FullName)
                if ($targets.Count -eq 0) { throw 'Candidate artifact aliases cannot be identified.' }
                foreach ($target in $targets) {
                    if (-not $target -or -not [IO.Path]::IsPathRooted([string]$target)) { throw 'Candidate artifact aliases cannot be identified.' }
                    $targetPath = [IO.Path]::GetFullPath([string]$target)
                    $inside = $targetPath.StartsWith($candidatePath + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)
                    $live = $targetPath.Equals($liveTarget, [StringComparison]::OrdinalIgnoreCase) -or
                        $targetPath.StartsWith($liveTarget + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)
                    if (-not $inside -or $live) { throw 'Candidate artifact has an alias outside the candidate or in the live build tree.' }
                }
            }
            if ($entry.PSIsContainer) { $pending.Push($entry.FullName) }
        }
    }
}
$env:CARGO_TARGET_DIR = $candidatePath
Write-Host ("candidate directory: " + $candidatePath)

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vcvars = $null
if (Test-Path $vswhere) {
    $vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
    if ($vsPath) {
        $candidate = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
        if (Test-Path $candidate) { $vcvars = $candidate }
    }
}
if (-not $vcvars) {
    Write-Host 'vcvars64.bat not found: MSVC C++ toolchain is missing.'
    Write-Host 'Install: winget install --id Microsoft.VisualStudio.2022.BuildTools --override "--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"'
    exit 1
}

Write-Host ("toolchain: " + $vcvars)
$buildArgs = if ($Release) { 'build --release -j 1' } else { 'build -j 1' }
$cmd = '"' + $vcvars + '" >nul && cargo +stable-x86_64-pc-windows-msvc ' + $buildArgs

# Run at BelowNormal so the live bridge keeps its CPU when it needs it.
$p = Start-Process -FilePath 'cmd.exe' -ArgumentList '/c', $cmd -WorkingDirectory $PSScriptRoot -WindowStyle Hidden -PassThru -Wait
exit $p.ExitCode

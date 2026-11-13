# Build/run the Rust memory probe with the MSVC toolchain.
# Usage: powershell -NoProfile -File C:\qqbot\her-memory\run.ps1
$ErrorActionPreference = 'Stop'
$env:CARGO_HOME = Join-Path $env:USERPROFILE '.cargo'

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
if (-not (Test-Path $vcvars)) { Write-Host 'vcvars64.bat not found'; exit 1 }

$cmd = '"' + $vcvars + '" >nul && cd /d C:\qqbot\her-memory && cargo +stable-x86_64-pc-windows-msvc run --release'
cmd /c $cmd
exit $LASTEXITCODE

# Run clippy (warnings as errors) with the MSVC environment
$ErrorActionPreference = 'Stop'
$env:CARGO_HOME = Join-Path $env:USERPROFILE '.cargo'
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$vcvars = Join-Path $vsPath 'VC\Auxiliary\Build\vcvars64.bat'
$cmd = '"' + $vcvars + '" >nul && cargo +stable-x86_64-pc-windows-msvc clippy --release --message-format=short -- -D warnings'
cmd /c $cmd
exit $LASTEXITCODE

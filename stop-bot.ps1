# Request a persistent stop through the selected runtime's lifecycle owner.
# All parameters are explicit so an unattended invocation cannot guess a production path.
param(
  [string]$RuntimeRoot = '',
  [string]$Executable = '',
  [switch]$Confirm
)

$ErrorActionPreference = 'Stop'
. ([IO.Path]::Combine($PSScriptRoot, 'external-path.ps1'))
if (-not $Confirm -or -not $RuntimeRoot -or -not $Executable -or
    -not [IO.Path]::IsPathRooted($RuntimeRoot) -or -not [IO.Path]::IsPathRooted($Executable)) {
  Write-Error 'Usage: stop-bot.ps1 -RuntimeRoot <absolute runtime> -Executable <absolute her> -Confirm'
  exit 2
}
$CliExecutable = Get-HerExecutable $Executable
& $CliExecutable lifecycle set stopped --runtime-root $RuntimeRoot --by stop-bot.ps1 --reason 'operator stop' --confirm | Out-String
exit $LASTEXITCODE

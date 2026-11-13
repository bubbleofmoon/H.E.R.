# Shared fixture boundary: no path comparisons are reimplemented in PowerShell.
function Get-HerFixtureCli {
  param([string]$Cli = $env:HER_TEST_CLI)
  $candidate = Get-Variable -Name Candidate -ValueOnly -ErrorAction SilentlyContinue
  if (-not $Cli -and $candidate) { $Cli = Join-Path (Split-Path -Parent $candidate) 'her-cli.exe' }
  if (-not $Cli -and $env:CARGO_TARGET_DIR) {
    $Cli = Join-Path $env:CARGO_TARGET_DIR 'debug\her-cli.exe'
  }
  if (-not $Cli) { $Cli = Join-Path $PSScriptRoot '..\her\target\debug\her-cli.exe' }
  if (-not (Test-Path -LiteralPath $Cli -PathType Leaf)) {
    throw 'Set HER_TEST_CLI to the built her-cli before allocating fixture roots.'
  }
  return [IO.Path]::GetFullPath($Cli)
}
function Assert-HerDevelopmentRoot {
  param([Parameter(Mandatory)][string]$Path, [string]$Cli = $env:HER_TEST_CLI)
  $Cli = Get-HerFixtureCli -Cli $Cli
  $selected = if ([IO.Path]::IsPathRooted($Path)) { $Path } else { [IO.Path]::Combine($PWD.ProviderPath, $Path) }
  $runtimeSelection = $env:HER_RUNTIME_ROOT
  try {
    $env:HER_RUNTIME_ROOT = $null
    $result = & $Cli install-root check-development --runtime-root $selected 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "Fixture root refused: $result" }
  } finally { $env:HER_RUNTIME_ROOT = $runtimeSelection }
}

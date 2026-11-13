# Native PS 5.1 regression for the shared helper retained by dependency/maintenance scripts.
$ErrorActionPreference = 'Stop'
$root = Join-Path ([IO.Path]::GetTempPath()) ('path-entry-' + [guid]::NewGuid().ToString('N'))
. (Join-Path $PSScriptRoot 'fixture-paths.ps1')
Assert-HerDevelopmentRoot -Path $root
try {
  [IO.Directory]::CreateDirectory((Join-Path $root 'runtime')) | Out-Null
  $shared = Join-Path $PSScriptRoot '..\external-path.ps1'
  if (Test-Path -LiteralPath $shared) { Copy-Item -LiteralPath $shared -Destination $root }
  . $shared
  if ((ConvertTo-ExternalPath ('\\?\' + (Join-Path $root 'runtime'))) -cne (Join-Path $root 'runtime')) { throw 'Canonical runtime conversion failed' }
  if ((ConvertTo-ExternalPath '\\?\UNC\server\share\dir') -cne '\\server\share\dir') { throw 'UNC conversion failed' }
  foreach ($invalid in @('\\?\GLOBALROOT\Device\volume','\\?\C:\trailing.\file','\\?\C:\NUL.txt','\\?\C:\file:stream','\\?\UNC\server')) {
    $refused=$false
    try { $null=ConvertTo-ExternalPath $invalid } catch { $refused=$true }
    if (-not $refused) { throw "Unsafe conversion accepted: $invalid" }
  }
  'PASS external helper accepts canonical runtime; UNC converted; unrepresentable paths refused'
} finally {
  if ($root -and [IO.Path]::GetFullPath($root).StartsWith([IO.Path]::GetTempPath()) -and (Split-Path -Leaf $root) -like 'path-entry-*') {
    Remove-Item -LiteralPath $root -Recurse -Force
  }
}

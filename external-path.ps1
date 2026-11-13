# Filesystem spelling and paired HER image selection at PowerShell process boundaries.
function ConvertTo-ExternalPath([string]$Path) {
  if (-not $Path.StartsWith('\\?\', [StringComparison]::Ordinal)) { return $Path }
  $rest = $Path.Substring(4)
  if ($rest.StartsWith('UNC\', [StringComparison]::OrdinalIgnoreCase)) {
    $tail = $rest.Substring(4)
    if ($tail -notmatch '^[^\\]+\\[^\\]+(?:\\|$)') { throw 'Verbatim UNC path has no server/share.' }
    $ordinary = '\\' + $tail
    $components = $tail.Split('\')
  } elseif ($rest -cmatch '^[A-Za-z]:\\') {
    $ordinary = $rest
    $components = $rest.Substring(3).Split('\')
  } else { throw 'Verbatim device path has no ordinary filesystem spelling.' }
  if ($ordinary.Length -ge 260) { throw 'Verbatim path is too long for external tools.' }
  foreach ($part in $components) {
    if ($part -match '[ .]$|[/:"<>|?*\x00-\x1f]' -or $part -match '^(CON|PRN|AUX|NUL|CONIN\$|CONOUT\$|COM[1-9\u00b9\u00b2\u00b3]|LPT[1-9\u00b9\u00b2\u00b3])(?:\.|$)') {
      throw 'Verbatim path cannot be represented safely for external tools.'
    }
  }
  return $ordinary
}

# CLI capture and resident startup use different PE subsystems from the same directory.
# Other executable names remain supported for isolated launcher fixtures.
function Get-HerExecutable([string]$Path, [switch]$Resident) {
  $Path = ConvertTo-ExternalPath $Path
  $name = [IO.Path]::GetFileName($Path)
  if ($name -ine 'her.exe' -and $name -ine 'her-cli.exe') { return $Path }
  $target = if ($Resident) { 'her.exe' } else { 'her-cli.exe' }
  $selected = Join-Path ([IO.Path]::GetDirectoryName($Path)) $target
  if (-not (Test-Path -LiteralPath $selected -PathType Leaf)) { throw "Required sibling executable is missing: $selected" }
  return $selected
}

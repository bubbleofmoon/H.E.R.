# Windows PowerShell 5.1 collection regression; parses scripts without executing them.
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -ne 5 -or $PSVersionTable.PSVersion.Minor -ne 1) {
  throw 'Run this regression in Windows PowerShell 5.1.'
}

function Find-UnsafeCollectionAccess([System.Management.Automation.Language.Ast]$Ast) {
  foreach ($access in $Ast.FindAll({
    param($node)
    ($node -is [System.Management.Automation.Language.MemberExpressionAst] -and
      $node.Member.Value -in @('Count','Length')) -or
      $node -is [System.Management.Automation.Language.IndexExpressionAst]
  }, $true)) {
    $expression = if ($access -is [System.Management.Automation.Language.IndexExpressionAst]) { $access.Target } else { $access.Expression }
    # @(...) is already a collection. Unwrap ordinary nested parentheses only.
    while ($expression -is [System.Management.Automation.Language.ParenExpressionAst]) {
      $pipeline = $expression.Pipeline
      $commands = @($pipeline.PipelineElements | Where-Object { $_ -is [System.Management.Automation.Language.CommandAst] })
      if ($commands.Count -gt 0) {
        # -Raw deliberately measures characters, not the number of file objects.
        $rawStringLength = $false
        if ($access -is [System.Management.Automation.Language.MemberExpressionAst] -and $access.Member.Value -eq 'Length' -and
            $pipeline.PipelineElements.Count -eq 1 -and $commands[0].GetCommandName() -eq 'Get-Content') {
          $raw = @($commands[0].CommandElements | Where-Object {
            $_ -is [System.Management.Automation.Language.CommandParameterAst] -and $_.ParameterName -eq 'Raw' -and $null -eq $_.Argument
          })
          $rawStringLength = $raw.Count -eq 1
        }
        if (-not $rawStringLength) { $access }
        break
      }
      $elements = @($pipeline.PipelineElements)
      if ($elements.Count -ne 1 -or $elements[0] -isnot [System.Management.Automation.Language.CommandExpressionAst]) { break }
      $expression = $elements[0].Expression
    }
  }
}

function Find-UncachedProcessExit([System.Management.Automation.Language.Ast]$Ast) {
  foreach ($assignment in $Ast.FindAll({ param($node)
    $node -is [System.Management.Automation.Language.AssignmentStatementAst]
  }, $true)) {
    if ($assignment.Left -isnot [System.Management.Automation.Language.VariableExpressionAst]) { continue }
    $commands = @($assignment.Right.FindAll({ param($node)
      $node -is [System.Management.Automation.Language.CommandAst] -and $node.GetCommandName() -eq 'Start-Process'
    }, $true))
    if ($commands.Count -ne 1) { continue }
    $parameters = @($commands[0].CommandElements | Where-Object { $_ -is [System.Management.Automation.Language.CommandParameterAst] })
    if (@($parameters | Where-Object ParameterName -eq 'PassThru').Count -eq 0) { continue }
    # Start-Process -Wait acquires/retains the native handle inside the cmdlet.
    if (@($parameters | Where-Object { $_.ParameterName -eq 'Wait' -and ($null -eq $_.Argument -or $_.Argument.Extent.Text -eq '$true') }).Count -gt 0) { continue }
    $name = $assignment.Left.VariablePath.UserPath
    $scope = $assignment.Parent
    $consumers = @($scope.FindAll({ param($node)
      ($node -is [System.Management.Automation.Language.MemberExpressionAst] -and
        $node.Expression -is [System.Management.Automation.Language.VariableExpressionAst] -and
        $node.Expression.VariablePath.UserPath -eq $name -and $node.Member.Value -eq 'ExitCode') -or
      ($node -is [System.Management.Automation.Language.ReturnStatementAst] -and
        @($node.FindAll({ param($value)
          $value -is [System.Management.Automation.Language.VariableExpressionAst] -and $value.VariablePath.UserPath -eq $name
        }, $true)).Count -gt 0)
    }, $true))
    if ($consumers.Count -eq 0) { continue }
    # A later/conditional read can race process exit. Require the next statement
    # to read Handle directly, also covering Process objects returned to callers.
    $following = @($scope.Statements | Where-Object { $_.Extent.StartOffset -ge $assignment.Extent.EndOffset } | Select-Object -First 1)
    $cached = $false
    if ($following.Count -eq 1) {
      foreach ($member in $following[0].FindAll({ param($node)
        $node -is [System.Management.Automation.Language.MemberExpressionAst] -and
        $node.Expression -is [System.Management.Automation.Language.VariableExpressionAst] -and
        $node.Expression.VariablePath.UserPath -eq $name -and $node.Member.Value -eq 'Handle'
      }, $true)) {
        $parent = $member.Parent
        while ($parent -and $parent -ne $following[0] -and
            $parent -isnot [System.Management.Automation.Language.IfStatementAst] -and
            $parent -isnot [System.Management.Automation.Language.ScriptBlockAst]) { $parent = $parent.Parent }
        $writeOnly = $following[0] -is [System.Management.Automation.Language.AssignmentStatementAst] -and
          $following[0].Left.Extent.StartOffset -le $member.Extent.StartOffset -and
          $following[0].Left.Extent.EndOffset -ge $member.Extent.EndOffset
        if ($parent -eq $following[0] -and $following[0] -isnot [System.Management.Automation.Language.IfStatementAst] -and -not $writeOnly) { $cached = $true }
      }
    }
    if (-not $cached) { $assignment }
  }
}

# Parser examples are data: never invoke host process/task/network commands.
$cases = @(
  @{Text='(Get-CimInstance Win32_Process | Where-Object Name -eq node.exe).Count'; Expected=1},
  @{Text='(Get-NetTCPConnection).Length'; Expected=1},
  @{Text='((Get-ScheduledTask)).Count'; Expected=1},
  @{Text='(Get-Process)[0]'; Expected=1},
  @{Text='($json | ConvertFrom-Json).Count'; Expected=1},
  @{Text='($items | Select-Object -First 1).Length'; Expected=1},
  @{Text='@($items | Where-Object enabled).Count'; Expected=0},
  @{Text='@(Get-Process)[0]'; Expected=0},
  @{Text='(Get-Content -LiteralPath $trace -Raw).Length'; Expected=0},
  @{Text='(Get-Content -LiteralPath $trace).Length'; Expected=1},
  @{Text='(Get-Content -Raw:$false).Length'; Expected=1},
  @{Text='(Get-Content -Raw | Where-Object { $_ }).Length'; Expected=1},
  @{Text='([IO.File]::ReadAllText($trace)).Length'; Expected=0},
  @{Text='# (Get-Process).Count'; Expected=0},
  @{Text='"(Get-Process).Count"'; Expected=0}
)
foreach ($case in $cases) {
  $tokens = $null; $errors = $null
  $ast = [System.Management.Automation.Language.Parser]::ParseInput($case.Text, [ref]$tokens, [ref]$errors)
  if (@($errors).Count -ne 0 -or @(Find-UnsafeCollectionAccess $ast).Count -ne $case.Expected) { throw "Parser regression: $($case.Text)" }
}

$exitCases = @(
  @{Text='$p = Start-Process fake -PassThru; $p.WaitForExit(); exit $p.ExitCode'; Expected=1},
  @{Text='$p = Start-Process fake -PassThru; $null = $p.Handle; $p.WaitForExit(); exit $p.ExitCode'; Expected=0},
  @{Text='$p = Start-Process fake -PassThru; [void]$p.Handle; exit $p.ExitCode'; Expected=0},
  @{Text='$p = Start-Process fake -PassThru -Wait; exit $p.ExitCode'; Expected=0},
  @{Text='$p = Start-Process fake -PassThru -Wait:$false; exit $p.ExitCode'; Expected=1},
  @{Text='$p = Start-Process fake -PassThru; $p.WaitForExit(); $null = $p.Handle; exit $p.ExitCode'; Expected=1},
  @{Text='$p = Start-Process fake -PassThru; if ($ready) { $null = $p.Handle }; exit $p.ExitCode'; Expected=1},
  @{Text='$p = Start-Process fake -PassThru; $null = $other.Handle; exit $p.ExitCode'; Expected=1},
  @{Text='$p = Start-Process fake -PassThru; $p.Handle = 0; exit $p.ExitCode'; Expected=1},
  @{Text='function Launch { $p = Start-Process fake -PassThru; return $p }; $child = Launch; exit $child.ExitCode'; Expected=1},
  @{Text='function Launch { $p = Start-Process fake -PassThru; $null = $p.Handle; return $p }; $child = Launch; exit $child.ExitCode'; Expected=0},
  @{Text='Start-Process fake -PassThru | Out-Null'; Expected=0},
  @{Text='# $p = Start-Process fake -PassThru; exit $p.ExitCode'; Expected=0}
)
foreach ($case in $exitCases) {
  $tokens = $null; $errors = $null
  $ast = [System.Management.Automation.Language.Parser]::ParseInput($case.Text, [ref]$tokens, [ref]$errors)
  if (@($errors).Count -ne 0 -or @(Find-UncachedProcessExit $ast).Count -ne $case.Expected) { throw "Process parser regression: $($case.Text)" }
}

# Exact fixture predicate: zero, one and several matching JSON run records.
foreach ($count in @(0,1,2)) {
  $rows = @([pscustomobject]@{source='scheduled';state='succeeded'})
  for ($i = 0; $i -lt $count; $i++) { $rows += [pscustomobject]@{source='manual';state='succeeded'} }
  $runs = (@{runs=$rows} | ConvertTo-Json -Depth 3 | ConvertFrom-Json).runs
  $matched = @($runs | Where-Object { $_.source -eq 'manual' -and $_.state -eq 'succeeded' })
  if ($matched.Count -ne $count -or ($matched.Count -gt 0) -ne ($count -gt 0)) { throw "Run predicate failed with $count matches." }
}

$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
# Keep discovery inside the worktree; do not follow links or enumerate installed/baseline trees.
$pending = [Collections.Generic.Stack[string]]::new()
$pending.Push($repo)
$scanned = 0; $violations = @()
while ($pending.Count -gt 0) {
  foreach ($item in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { continue }
    if ($item.PSIsContainer) {
      if ($item.Name -notin @('.git','.scratch','backups','qq-bridge','graphify-out','node_modules','target','.venv')) { $pending.Push($item.FullName) }
    } elseif ($item.Extension -ieq '.ps1') {
      $tokens = $null; $errors = $null
      # Read repository UTF-8 explicitly: PS 5.1 ParseFile otherwise assumes ANSI for BOM-less sources.
      $ast = [System.Management.Automation.Language.Parser]::ParseInput([IO.File]::ReadAllText($item.FullName), [ref]$tokens, [ref]$errors)
      if (@($errors).Count -ne 0) { throw "PowerShell parse error in $($item.FullName): $errors" }
      foreach ($access in Find-UnsafeCollectionAccess $ast) {
        $violations += ('{0}:{1}: wrap collection command/pipeline in @(...): {2}' -f $item.FullName.Substring($repo.Length + 1), $access.Extent.StartLineNumber, $access.Extent.Text)
      }
      foreach ($spawn in Find-UncachedProcessExit $ast) {
        $violations += ('{0}:{1}: cache .Handle immediately after -PassThru before reading/returning process exit status: {2}' -f $item.FullName.Substring($repo.Length + 1), $spawn.Extent.StartLineNumber, $spawn.Left.Extent.Text)
      }
      $scanned++
    }
  }
}
if ($violations.Count -gt 0) { $violations | Write-Output; throw "Unsafe PowerShell accesses: $($violations.Count)" }
Write-Host "PASS PowerShell 5.1: $scanned scripts parsed; collection/Process exit scans; parser cases; 0/1/2 manual-success matches."

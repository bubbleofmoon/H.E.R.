# Hands an issue to the next side in one step: posts the handoff comment through
# scripts/gh-public.ps1, then moves the baton label (docs/agents/baton.md "Handing over").
#
#   powershell -NoProfile -File scripts/handoff.ps1 <issue> <linux|windows|owner> <body-file>
#
# Mirrors scripts/handoff.sh: the same checks, refusal kinds and exit codes (2 refused and
# nothing changed; gh-public's 1, 2 or 3 when the body is refused and nothing changed; 4 when gh
# fails while posting the comment (it may or may not be on the issue) or while moving the label
# (the comment is posted). gh-public.ps1 runs as a child of this same
# PowerShell, exactly as it runs when called by hand.
$ErrorActionPreference = 'Continue'
$repo = 'bubbleofmoon/H.E.R.'
$root = $PSScriptRoot
if (-not $root) { $root = Split-Path -Parent $MyInvocation.MyCommand.Path }
$ghPublic = Join-Path $root 'gh-public.ps1'
$arrow = [string][char]0x2192             # built here so this file stays ASCII

function Refuse([string]$kind) {
    [Console]::Error.WriteLine("handoff: refusal=$kind")
    exit 2
}

$argv_ = @($args)
if ($argv_.Count -ne 3) { Refuse 'usage' }
$issue = [string]$argv_[0]
$side = [string]$argv_[1]
$body = [string]$argv_[2]
if ($issue -notmatch '^[1-9][0-9]*$') { Refuse 'issue-number' }
if (@('linux', 'windows', 'owner') -cnotcontains $side) { Refuse 'side' }
if (-not (Test-Path -LiteralPath $body -PathType Leaf)) { Refuse 'missing-file' }
$bodyPath = (Resolve-Path -LiteralPath $body).Path
$lines = [IO.File]::ReadAllLines($bodyPath, (New-Object Text.UTF8Encoding $false))
$first = ''
if ($lines.Count -gt 0) { $first = $lines[0].TrimStart([char]0xFEFF).TrimEnd("`r") }
if ($first -cne "**Handoff $arrow $side**") { Refuse 'heading' }
$label = "baton:$side"

# Reads only: nothing is written unless the issue is open and the label exists.
$view = @(& gh issue view $issue --repo $repo --json 'state,labels' --jq '.state, .labels[].name')
if ($LASTEXITCODE -ne 0 -or $view.Count -lt 1) { Refuse 'issue-read' }
if ([string]$view[0] -cne 'OPEN') { Refuse 'issue-not-open' }
$known = @(& gh label list --repo $repo --search 'baton:' --limit 100 --json name --jq '.[].name')
if ($LASTEXITCODE -ne 0) { Refuse 'label-read' }
if ($known -cnotcontains $label) { Refuse 'unknown-label' }
$remove = @($view | Select-Object -Skip 1 | Where-Object { $_ -clike 'baton:*' -and $_ -cne $label })

$shell = (Get-Process -Id $PID).Path
$edit = @('issue', 'edit', $issue, '--repo', $repo, '--add-label', $label)
if ($remove.Count -gt 0) { $edit += @('--remove-label', ($remove -join ',')) }

& $shell -NoProfile -ExecutionPolicy Bypass -File $ghPublic --body-file $bodyPath issue comment $issue --repo $repo
$code = $LASTEXITCODE
if ($code -eq 4) {
    [Console]::Error.WriteLine("handoff: FAILED: gh failed while posting the comment; it may or may not be on the issue, and the baton did not move. Check the issue, then post again or run: gh $($edit -join ' ')")
    exit 4
}
if ($code -ne 0) {
    [Console]::Error.WriteLine("handoff: the comment was not posted (gh-public exit $code); labels unchanged")
    exit $code
}

& gh @edit
if ($LASTEXITCODE -ne 0) {
    [Console]::Error.WriteLine("handoff: FAILED: gh failed while moving the label; the comment is posted but the baton did not move; run: gh $($edit -join ' ')")
    exit 4
}
Write-Output "handoff: #$issue -> $label"

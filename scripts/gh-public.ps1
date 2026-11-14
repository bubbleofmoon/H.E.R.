# Posts a GitHub comment, issue or pull-request body through gh, but only after the public
# scan passes over every piece of text that would be published.
#
#   powershell -NoProfile -File scripts/gh-public.ps1 --body-file note.md `
#       issue comment 166 --repo bubbleofmoon/H.E.R.
#   powershell -NoProfile -File scripts/gh-public.ps1 --body-file note.md `
#       issue create --repo bubbleofmoon/H.E.R. --title "Ticket 166" --label windows
#
# Supported: `gh issue create|comment` and `gh pr create|comment`, one body file given with
# --body-file (every --body-file in the call must name that same existing regular file), a
# repository selector (--repo, --repo=, -R, -R=, -R<attached>) that must name the public
# repository, a --title (required for create) and any number of --label values. Everything else is
# refused before anything runs: a body typed on the command line, a body on stdin, a second
# body file, -F/-f fields, attachments, --web, --editor, --fill, any other command and any
# other repository. The text of a title or a label is scanned like the body, because it is
# published too, and it is scanned in full: every line of a multiline value.
#
# The arguments are parsed once, and an option consumes its own value, so a value that looks
# like a flag (`--title --body-file=body.md`) is text and never an option: it cannot stand in
# for the required body source. gh then receives arguments rebuilt from what was parsed, so
# gh publishes exactly the text that was scanned and is never left prompting for a body.
#
# A title or label value that spans lines is refused here: Windows PowerShell 5.1 cannot
# carry a newline inside one native argument, so this wrapper would forward less than it
# scanned. Write a multiline title as an issue body file instead, which is forwarded as a file
# handle and keeps every byte.
#
# A refusal prints a fixed kind and the argument position, never a supplied value, and exits
# non-zero without calling gh: 2 for a refused invocation, 1 for public scan findings, 3 when gitleaks cannot be found (refusal=scanner-missing, which says nothing about the text). Install
# gitleaks first (scripts/ci/install-gitleaks.py or set HER_GITLEAKS); a scan that cannot run
# the secret check refuses too.
$ErrorActionPreference = 'Continue'
$publicRepo = 'bubbleofmoon/H.E.R.'
$root = $PSScriptRoot
if (-not $root) { $root = Split-Path -Parent $MyInvocation.MyCommand.Path }
$scan = Join-Path $root 'scan-public.py'
$py = 'python'
if (-not (Get-Command $py -ErrorAction SilentlyContinue)) { $py = 'python3' }

function Refuse([string]$kind, [int]$position) {
    [Console]::Error.WriteLine("gh-public: refusal=$kind argument-position=$position")
    exit 2
}

function Resolve-BodyPath([string]$path) {
    # A comparable full path for an existing regular file, or an empty string for anything else.
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { return '' }
    return (Resolve-Path -LiteralPath $path).Path
}

function Invoke-ScanFile([string]$path) {
    # Runs the guard over one file and shows only its metadata report.
    $out = [IO.Path]::GetTempFileName()
    $err = [IO.Path]::GetTempFileName()
    try {
        # Hidden, no console; the handle is cached before waiting so the exit code survives.
        $scanProcess = Start-Process -FilePath $py -ArgumentList @('-I', ('"' + $scan + '"'), '--stdin') `
            -RedirectStandardInput $path -RedirectStandardOutput $out -RedirectStandardError $err `
            -PassThru -NoNewWindow
        $null = $scanProcess.Handle
        $scanProcess.WaitForExit()
        $report = [IO.File]::ReadAllText($out, [Text.Encoding]::UTF8).Trim() + "`n" +
                  [IO.File]::ReadAllText($err, [Text.Encoding]::UTF8).Trim()
        # Write-Host, not Write-Output: the metadata report is for the human, and keeping it
        # out of the pipeline keeps the function's return value a single exit code.
        Write-Host $report
        return $scanProcess.ExitCode
    } finally {
        Remove-Item -LiteralPath $out, $err -Force -ErrorAction SilentlyContinue
    }
}

function Refuse-Scan([int]$code, [string]$field, [int]$position) {
    if ($code -eq 3) {
        Write-Host "gh-public: refusal=scanner-missing field=$field argument-position=$position (set HER_GITLEAKS to the pinned gitleaks: docs/agents/rust-validation.md)"
        exit 3
    }
    Write-Host "gh-public: refusal=scan-findings field=$field argument-position=$position"
    Write-Host 'gh-public: refused (public scan findings above; matched text is never shown)'
    exit 1
}

$args_ = @($args)
if ($args_.Count -lt 1) { Refuse 'usage' 0 }

# --- one parse over the arguments -------------------------------------------------
# A flag consumes its own value, so the value of --title is never read as an option.
$body = ''              # the one body source, resolved so spellings compare
$bodyPosition = 0       # where the caller wrote the body value being checked
$bodyOptions = 0        # how many real --body-file options were consumed
$commandWord = ''
$subcommand = ''
$number = ''
$repoSeen = 0
$titleText = ''
$fieldNames = @()       # parallel arrays: field name, argument position, full text
$fieldPositions = @()
$fieldTexts = @()

$i = 0
while ($i -lt $args_.Count) {
    $position = $i + 1                                        # position of this token
    $token = [string]$args_[$i]
    $hasNext = ($i + 1) -lt $args_.Count
    $next = if ($hasNext) { [string]$args_[$i + 1] } else { '' }
    $value = ''
    $valuePosition = $position                                # where this option's value sits
    $consumed = $false
    $kind = ''
    if ($token -eq '--body-file') {
        if (-not $hasNext) { Refuse 'missing-body-file' 0 }
        $kind = 'body'; $value = $next; $consumed = $true; $valuePosition = $i + 2; $bodyPosition = $i + 2
    }
    elseif ($token.StartsWith('--body-file=')) {
        $kind = 'body'; $value = $token.Substring('--body-file='.Length); $bodyPosition = $position
    }
    elseif ($token -eq '--repo' -or $token -eq '-R') {
        if (-not $hasNext) { Refuse 'repository-selector' 0 }
        $kind = 'repo'; $value = $next; $consumed = $true; $valuePosition = $i + 2
    }
    elseif ($token.StartsWith('--repo=')) { $kind = 'repo'; $value = $token.Substring('--repo='.Length) }
    elseif ($token.StartsWith('-R=')) { $kind = 'repo'; $value = $token.Substring('-R='.Length) }
    elseif ($token.StartsWith('-R') -and $token.Length -gt 2) { $kind = 'repo'; $value = $token.Substring(2) }
    elseif ($token -eq '--title') {
        if (-not $hasNext) { Refuse 'unsupported-argument' ($i + 2) }
        $kind = 'title'; $value = $next; $consumed = $true; $valuePosition = $i + 2
    }
    elseif ($token.StartsWith('--title=')) { $kind = 'title'; $value = $token.Substring('--title='.Length) }
    elseif ($token -eq '--label') {
        if (-not $hasNext) { Refuse 'unsupported-argument' ($i + 2) }
        $kind = 'label'; $value = $next; $consumed = $true; $valuePosition = $i + 2
    }
    elseif ($token.StartsWith('--label=')) { $kind = 'label'; $value = $token.Substring('--label='.Length) }
    elseif ($token -match '^(?:https?://)' -or $token.Contains('github.com/')) {
        if ($token.Contains("github.com/$publicRepo/")) { Refuse 'unsupported-argument' $position }
        Refuse 'repository-url' $position                       # a URL is not an issue number
    }
    elseif ($token.StartsWith('-')) { Refuse 'unsupported-argument' $position }
    else {
        $kind = 'positional'
        if (-not $commandWord) {
            if ($token -in 'issue', 'pr') { $commandWord = $token } else { Refuse 'unsupported-command' $position }
        }
        elseif (-not $subcommand) {
            if ($token -in 'create', 'comment') { $subcommand = $token } else { Refuse 'unsupported-command' $position }
        }
        elseif ($token -notmatch '^[0-9]+$' -or $number) { Refuse 'unsupported-argument' $position }
        else { $number = $token }
    }

    switch ($kind) {
        'body' {
            if ($value -eq '-') { Refuse body-source $bodyPosition }
            $resolved = Resolve-BodyPath $value
            if (-not $resolved) { Refuse 'missing-file' $bodyPosition }
            elseif (-not $body) { $body = $resolved }
            elseif ($resolved -ne $body) { Refuse 'body-file-mismatch' $bodyPosition }
            $bodyOptions++
        }
        'repo' {
            if ($value -ne $publicRepo) { Refuse repository-selector $valuePosition }
            $repoSeen++
        }
        'title' {
            if (-not $value) { Refuse unsupported-argument $valuePosition }
            if ($titleText) { Refuse unsupported-argument $position }        # one title only
            if ($value -match '[\r\n]') { Refuse unsupported-argument $valuePosition }
            $titleText = $value
            $fieldNames += 'title'; $fieldPositions += $valuePosition; $fieldTexts += $value
        }
        'label' {
            if (-not $value) { Refuse unsupported-argument $valuePosition }
            if ($value -match '[\r\n]') { Refuse unsupported-argument $valuePosition }
            $fieldNames += 'label'; $fieldPositions += $valuePosition; $fieldTexts += $value
        }
    }
    $i = if ($consumed) { $i + 2 } else { $i + 1 }
}

# A body source has to be a real --body-file option: a value that only looks like one is
# title text, and a call with no body option would leave gh prompting for a body by hand.
if ($bodyOptions -lt 1 -or -not $body) { Refuse missing-body-file 0 }
if (-not $commandWord -or -not $subcommand) { Refuse unsupported-command 0 }
if ($repoSeen -lt 1) { Refuse repository-selector 0 }
if ($subcommand -eq 'comment' -and -not $number) { Refuse unsupported-argument 0 }
if ($subcommand -eq 'create' -and $number) { Refuse unsupported-argument 0 }
if ($subcommand -eq 'create' -and -not ($fieldNames -contains 'title')) { Refuse missing-title 0 }  # gh would prompt for an unscanned title

$scanCode = Invoke-ScanFile $body
if ($scanCode -ne 0) { Refuse-Scan $scanCode body-file $bodyPosition }
for ($index = 0; $index -lt $fieldNames.Count; $index++) {
    $temp = [IO.Path]::GetTempFileName()
    try {
        # The whole value, every line of it, goes to the scan in one file.
        [IO.File]::WriteAllText($temp, [string]$fieldTexts[$index] + "`n", (New-Object Text.UTF8Encoding $false))
        $scanCode = Invoke-ScanFile $temp
        if ($scanCode -ne 0) { Refuse-Scan $scanCode $fieldNames[$index] $fieldPositions[$index] }
    }
    finally {
        Remove-Item -LiteralPath $temp -Force -ErrorAction SilentlyContinue
    }
}

# gh receives arguments rebuilt from the parsed call: exactly the text that was scanned.
$argv = @($commandWord, $subcommand)
if ($number) { $argv += $number }
$argv += @('--repo', $publicRepo, '--body-file', $body)
for ($index = 0; $index -lt $fieldNames.Count; $index++) {
    if ($fieldNames[$index] -eq 'title') { $argv += @('--title', [string]$fieldTexts[$index]) }
}
for ($index = 0; $index -lt $fieldNames.Count; $index++) {
    if ($fieldNames[$index] -eq 'label') { $argv += @('--label', [string]$fieldTexts[$index]) }
}
& gh @argv
if ($null -eq $LASTEXITCODE) { exit 1 }
exit $LASTEXITCODE

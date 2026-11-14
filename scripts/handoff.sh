#!/usr/bin/env bash
# Hands an issue to the next side in one step: posts the handoff comment through
# scripts/gh-public.sh, then moves the baton label (docs/agents/baton.md "Handing over").
#
#   scripts/handoff.sh <issue> <linux|windows|owner> <body-file>
#
# The body file's first line must be the handoff heading for that side ("**Handoff", the
# arrow, the side, "**"). Before anything is written it checks the arguments and reads the
# issue and the repository's baton labels: a refusal exits 2 and changes nothing. The comment
# goes first, so the receiver never wakes to a stale comment; a body gh-public refuses exits
# with gh-public's code (1 findings, 2 refused invocation, 3 no secret scanner) and changes
# nothing. Then one `gh issue edit` adds baton:<side> and removes every other baton:* label.
# If that edit fails, the comment is already posted: the script exits 4, says so and prints
# the label command to run by hand. A refusal prints a fixed kind, never a supplied value.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo='bubbleofmoon/H.E.R.'
arrow=$(printf '\342\206\222')          # U+2192, built here so this file stays ASCII

refuse() {
  echo "handoff: refusal=$1" >&2
  exit 2
}

[ $# -eq 3 ] || refuse usage
issue=$1
side=$2
body=$3
case "$issue" in '' | *[!0-9]* | 0*) refuse issue-number ;; esac
case "$side" in linux | windows | owner) ;; *) refuse side ;; esac
[ -f "$body" ] || refuse missing-file
first=$(head -n 1 -- "$body" | tr -d '\r')
first=${first#$'\357\273\277'}            # a UTF-8 byte order mark is not part of the heading
[ "$first" = "**Handoff $arrow $side**" ] || refuse heading
label="baton:$side"

# Reads only: nothing is written unless the issue is open and the label exists.
view=$(gh issue view "$issue" --repo "$repo" --json state,labels --jq '.state, .labels[].name') || refuse issue-read
[ "$(printf '%s\n' "$view" | head -n 1)" = OPEN ] || refuse issue-not-open
known=$(gh label list --repo "$repo" --search 'baton:' --limit 100 --json name --jq '.[].name') || refuse label-read
printf '%s\n' "$known" | grep -Fxq -- "$label" || refuse unknown-label
remove=$(printf '%s\n' "$view" | tail -n +2 | grep '^baton:' | grep -Fxv -- "$label" | paste -sd, -)

bash "$here/gh-public.sh" --body-file "$body" issue comment "$issue" --repo "$repo"
code=$?
if [ $code -ne 0 ]; then
  echo "handoff: the comment was not posted (gh-public exit $code); labels unchanged" >&2
  exit $code
fi

edit=(issue edit "$issue" --repo "$repo" --add-label "$label")
[ -z "$remove" ] || edit+=(--remove-label "$remove")
if ! gh "${edit[@]}"; then
  echo "handoff: FAILED: the comment is posted but the baton did not move; run: gh ${edit[*]}" >&2
  exit 4
fi
echo "handoff: #$issue -> $label"

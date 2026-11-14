#!/usr/bin/env bash
# Posts a GitHub comment, issue or pull-request body through gh, but only after the public
# scan passes over every piece of text that would be published.
#
#   scripts/gh-public.sh --body-file note.md issue comment 166 --repo bubbleofmoon/H.E.R.
#   scripts/gh-public.sh --body-file note.md issue create --repo bubbleofmoon/H.E.R. \
#       --title "Ticket 166" --label windows
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
# A refusal prints a fixed kind and the argument position, never a supplied value, and exits
# non-zero without calling gh: 2 for a refused invocation, 1 for public scan findings, 3 when gitleaks cannot be found (refusal=scanner-missing, which says nothing about the text). Install
# gitleaks first (scripts/ci/install-gitleaks.py or set HER_GITLEAKS); a scan that cannot run
# the secret check refuses too.
set -u
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
scan=$here/scan-public.py
public_repo='bubbleofmoon/H.E.R.'
py=python
command -v python >/dev/null 2>&1 || py=python3

refuse() {
  echo "gh-public: refusal=$1 argument-position=${2:-0}" >&2
  exit 2
}
resolve() { # prints a comparable path for an existing file, or nothing
  local file=$1
  [ -f "$file" ] || return 0
  (cd "$(dirname "$file")" && printf '%s/%s\n' "$(pwd)" "$(basename "$file")")
}
scan_refusal() { # <scan exit code> <field> <argument position>
  if [ "$1" -eq 3 ]; then
    echo "gh-public: refusal=scanner-missing field=$2 argument-position=$3 (set HER_GITLEAKS to the pinned gitleaks: docs/agents/rust-validation.md)" >&2
    exit 3
  fi
  echo "gh-public: refusal=scan-findings field=$2 argument-position=$3" >&2
  echo 'gh-public: refused (public scan findings above; matched text is never shown)' >&2
  exit 1
}

[ $# -ge 1 ] || refuse usage 0

# --- one parse over the arguments -------------------------------------------------
body=''                  # the one body source, resolved so spellings compare
body_position=0          # where the caller wrote the body value being checked
body_options=0           # how many real --body-file options were consumed
command_word=''
subcommand=''
number=''
repo_seen=0
title_text=''
field_names=()           # parallel arrays: field name, argument position, full text
field_positions=()
field_texts=()
index=0
while [ $index -lt $# ]; do
  index=$((index + 1))
  token=${@:$index:1}
  next=${@:$((index + 1)):1}
  value=''
  consumed=0             # 1 when the value is the token after this one
  case "$token" in
    --body-file) [ $index -lt $# ] || refuse missing-body-file 0
                 value=$next; consumed=1; body_position=$((index + 1)) ;;
    --body-file=*) value=${token#--body-file=}; body_position=$index ;;
    --body-file*) refuse unsupported-argument "$index" ;;
    --repo | -R) [ $index -lt $# ] || refuse repository-selector 0
                 value=$next; consumed=1 ;;
    --repo=*) value=${token#--repo=} ;;
    -R=*) value=${token#-R=} ;;
    -R?*) value=${token#-R} ;;
    --title) [ $index -lt $# ] || refuse unsupported-argument $((index + 1))
             value=$next; consumed=1 ;;
    --title=*) value=${token#--title=} ;;
    --label) [ $index -lt $# ] || refuse unsupported-argument $((index + 1))
             value=$next; consumed=1 ;;
    --label=*) value=${token#--label=} ;;
    http://* | https://* | *github.com/*)
      case "$token" in
        *"github.com/$public_repo/"*) refuse unsupported-argument "$index" ;;  # a URL is not an issue number
        *) refuse repository-url "$index" ;;
      esac ;;
    -*) refuse unsupported-argument "$index" ;;
    *)
      if [ -z "$command_word" ]; then
        case "$token" in issue | pr) command_word=$token ;; *) refuse unsupported-command "$index" ;; esac
      elif [ -z "$subcommand" ]; then
        case "$token" in create | comment) subcommand=$token ;; *) refuse unsupported-command "$index" ;; esac
      else
        case "$token" in '' | *[!0-9]*) refuse unsupported-argument "$index" ;; esac
        [ -z "$number" ] || refuse unsupported-argument "$index"
        number=$token
      fi ;;
  esac
  [ $consumed -eq 0 ] || index=$((index + 1))     # the value is text, never parsed as an option

  case "$token" in
    --body-file | --body-file=*)
      [ "$value" != '-' ] || refuse body-source "$body_position"
      resolved=$(resolve "$value")
      if [ -z "$resolved" ]; then
        refuse missing-file "$body_position"
      elif [ -z "$body" ]; then
        body=$resolved
      elif [ "$resolved" != "$body" ]; then
        refuse body-file-mismatch "$body_position"
      fi
      body_options=$((body_options + 1)) ;;
    --repo | --repo=* | -R | -R=* | -R?*)
      [ "$value" = "$public_repo" ] || refuse repository-selector "$index"
      repo_seen=$((repo_seen + 1)) ;;
    --title | --title=*)
      [ -n "$value" ] || refuse unsupported-argument "$index"
      [ -z "$title_text" ] || refuse unsupported-argument "$index"      # one title only
      title_text=$value
      field_names+=('title'); field_positions+=("$index"); field_texts+=("$value") ;;
    --label | --label=*)
      [ -n "$value" ] || refuse unsupported-argument "$index"
      field_names+=('label'); field_positions+=("$index"); field_texts+=("$value") ;;
  esac
done

# A body source has to be a real --body-file option: a value that only looks like one is
# title text, and a call with no body option would leave gh prompting for a body by hand.
[ $body_options -ge 1 ] && [ -n "$body" ] || refuse missing-body-file 0
[ -n "$command_word" ] && [ -n "$subcommand" ] || refuse unsupported-command 0
[ "$repo_seen" -ge 1 ] || refuse repository-selector 0
case "$subcommand" in
  comment) [ -n "$number" ] || refuse unsupported-argument 0 ;;
  create) [ -z "$number" ] || refuse unsupported-argument 0
    [ -n "$title_text" ] || refuse missing-title 0 ;;      # gh would prompt for an unscanned title
esac

"$py" -I "$scan" --stdin <"$body"
code=$?
[ $code -eq 0 ] || scan_refusal "$code" body-file "$body_position"
count=${#field_names[@]}
i=0
while [ $i -lt $count ]; do
  # The whole value goes to the scan, every line of it: no line-oriented trimming.
  printf '%s\n' "${field_texts[$i]}" | "$py" -I "$scan" --stdin
  code=$?
  [ $code -eq 0 ] || scan_refusal "$code" "${field_names[$i]}" "${field_positions[$i]}"
  i=$((i + 1))
done

# gh receives arguments rebuilt from the parsed call: exactly the text that was scanned.
argv=("$command_word" "$subcommand")
[ -z "$number" ] || argv+=("$number")
argv+=(--repo "$public_repo" --body-file "$body")
i=0
while [ $i -lt $count ]; do
  case "${field_names[$i]}" in
    title) argv+=(--title "${field_texts[$i]}") ;;
    label) argv+=(--label "${field_texts[$i]}") ;;
  esac
  i=$((i + 1))
done
exec gh "${argv[@]}"

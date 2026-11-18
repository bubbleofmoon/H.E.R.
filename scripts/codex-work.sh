#!/usr/bin/env bash
# Runs a Codex plan or implement task for one ticket through scripts/codex-task.sh and verifies what it left.
# usage: scripts/codex-work.sh implement <name> <worktree> <prompt-file> [model] [effort] [minutes]
#        scripts/codex-work.sh plan <ticket> <name> <prompt-file> [model] [effort] [minutes]
# Defaults: gpt-6.2-sol; effort medium for implement, high for plan; 360 minutes. Receipts sit next to the prompt:
# codex-task's <name>.report.md and <name>.exit.txt, plus <name>.push.txt and <name>.staged.txt (implement) or
# <name>.plan.txt (plan: `installed` or `no-plan`).
# implement: HER_GITLEAKS must name the gitleaks binary (Codex's own push runs the pre-push hook with it). Before the run
#   the worktree must be clean, its HEAD on her/<n>-<slug> (never main), and <orch>/<n> and its tmp must not be links.
#   Codex (workspace-write, network) writes the worktree, <orch>/<n>/tmp and the repository's common git dir, and
#   commits and pushes its own ticket branch. After it exits 0 with a report this script checks, in order, and writes
#   the first line of <name>.push.txt: `branch-changed <branch>`, `dirty <paths>` (the worktree is not clean),
#   `no-changes` (no new commit), `unpushed` (the remote head is not the local HEAD) or `pushed <sha>`. The first,
#   second and fourth exit 8; the others exit 0. A second line `protected <paths>` names the commits' paths under
#   .githooks/ or in the scan scripts, scan config and scan exceptions, which the gate must review.
#   <name>.staged.txt lists the commits since the pre-run HEAD (`commit <sha> <subject>`) and the name-status of
#   every path they touched (`<status><TAB><path>`).
# plan: Codex (workspace-write) writes only <orch>/<n>/scratch and <orch>/<n>/plan-out; the plan it leaves there as a
#   regular, single-link plan.md is copied to <orch>/<n>/plan.md, and plan.sha256 holds its sha256sum line.
# <orch> is $HER_ORCH, default /root/orch/her.
set -uo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
orch=${HER_ORCH:-/root/orch/her}
die() { echo "codex-work: $*" >&2; exit 64; }
word='^[A-Za-z0-9][A-Za-z0-9_-]*$'
mode=${1:-}

case "$mode" in
implement)
  [ $# -ge 4 ] && [ $# -le 7 ] || die 'usage: implement <name> <worktree> <prompt-file> [model] [effort] [minutes]'
  name=$2 wt=$3 prompt=$4 model=${5:-gpt-6.2-sol} effort=${6:-medium} minutes=${7:-360}
  [[ $name =~ $word ]] || die "bad name: $name"
  [[ $minutes =~ ^[1-9][0-9]{0,2}$ ]] || die "bad minutes: $minutes"
  [ -f "$prompt" ] || die "no prompt file: $prompt"
  [ -n "${HER_GITLEAKS:-}" ] && [ -f "$HER_GITLEAKS" ] && [ -x "$HER_GITLEAKS" ] || die 'HER_GITLEAKS must name the gitleaks binary'
  export HER_GITLEAKS
  branch_of() { git -C "$wt" symbolic-ref --short -q HEAD 2> /dev/null; }
  [ -d "$wt" ] && [ "$(git -C "$wt" rev-parse --show-toplevel 2> /dev/null)" = "$(realpath -- "$wt")" ] || die "not a worktree root: $wt"
  branch=$(branch_of)
  [[ $branch =~ ^her/[0-9]+-[A-Za-z0-9._-]+$ ]] || die "HEAD is not a ticket branch (her/<n>-<slug>): ${branch:-detached}"
  [ -z "$(git -C "$wt" status --porcelain --untracked-files=all)" ] || die "the worktree is not clean: $wt"
  ticket=${branch#her/}; ticket=${ticket%%-*}
  [ ! -L "$orch/$ticket" ] && [ ! -L "$orch/$ticket/tmp" ] || die "a link in the ticket folder: $orch/$ticket"
  common=$(git -C "$wt" rev-parse --path-format=absolute --git-common-dir 2> /dev/null) && [ -d "$common" ] || die "no git common dir for $wt"
  pre=$(git -C "$wt" rev-parse HEAD)
  out=$(dirname -- "$(realpath -- "$prompt")")
  mkdir -p -- "$orch/$ticket/tmp"
  [ ! -L "$orch/$ticket/tmp" ] || die "a link in the ticket folder: $orch/$ticket/tmp"
  rm -f -- "$out/$name.push.txt" "$out/$name.staged.txt"
  TMPDIR="$orch/$ticket/tmp" "$here/codex-task.sh" "$name" "$wt" "$prompt" "$model" "$effort" workspace-write "$minutes" \
    "$orch/$ticket/tmp" "$common"
  code=$?
  [ "$code" -eq 0 ] || exit "$code"
  receipt() { printf '%s\n' "$@" > "$out/$name.push.txt"; }
  now=$(branch_of)
  if [ "$now" != "$branch" ]; then receipt "branch-changed ${now:-detached}"; exit 8; fi
  range="$pre..HEAD"
  {
    git -C "$wt" log --format='commit %H %s' "$range"
    git -C "$wt" -c core.quotepath=off log -m --no-renames --name-status --format= "$range" | sed '/^$/d' | sort -u
  } > "$out/$name.staged.txt" || exit 8
  stray=$(git -C "$wt" -c core.quotepath=off status --porcelain --untracked-files=all --no-renames | cut -c4- | paste -sd' ' -)
  if [ -n "$stray" ]; then receipt "dirty $stray"; exit 8; fi
  sha=$(git -C "$wt" rev-parse HEAD)
  if [ "$sha" = "$pre" ]; then receipt no-changes; echo "no-changes: the run left no commit" >&2; exit 0; fi
  remote=$(git -C "$wt" ls-remote origin "refs/heads/$branch" 2> /dev/null | cut -f1)
  if [ "$remote" != "$sha" ]; then receipt unpushed; echo "unpushed: the remote head is not $sha" >&2; exit 8; fi
  held=
  while IFS= read -r p; do
    case "${p,,}" in
      .githooks/*|scripts/check-repo.py|scripts/scan-public.py|scripts/tests/test_scan_public.py|.gitleaks.toml|.public-scan.json) held="$held${held:+ }$p" ;;
    esac
  done < <(git -C "$wt" -c core.quotepath=off log -m --no-renames --name-only --format= "$range" | sort -u)
  if [ -n "$held" ]; then receipt "pushed $sha" "protected $held"; echo "protected $held" >&2; else receipt "pushed $sha"; fi
  echo "pushed $sha"
  ;;
plan)
  [ $# -ge 4 ] && [ $# -le 7 ] || die 'usage: plan <ticket> <name> <prompt-file> [model] [effort] [minutes]'
  ticket=$2 name=$3 prompt=$4 model=${5:-gpt-6.2-sol} effort=${6:-high} minutes=${7:-360}
  [[ $ticket =~ ^[1-9][0-9]*$ ]] || die "bad ticket: $ticket"
  [[ $name =~ $word ]] || die "bad name: $name"
  [[ $minutes =~ ^[1-9][0-9]{0,2}$ ]] || die "bad minutes: $minutes"
  [ -f "$prompt" ] || die "no prompt file: $prompt"
  dir=$orch/$ticket scratch=$orch/$ticket/scratch stage=$orch/$ticket/plan-out
  [ ! -L "$dir" ] && [ -d "$scratch" ] && [ ! -L "$scratch" ] || die "no scratch export (scripts/plan-export.sh $ticket --replace): $scratch"
  [ ! -L "$stage" ] || die "plan-out is a link: $stage"
  out=$(dirname -- "$(realpath -- "$prompt")")
  rm -f -- "$out/$name.plan.txt"
  rm -rf -- "$stage"
  mkdir -- "$stage"
  TMPDIR="$stage" "$here/codex-task.sh" "$name" "$scratch" "$prompt" "$model" "$effort" workspace-write "$minutes" "$stage"
  code=$?
  [ "$code" -eq 0 ] || { rm -rf -- "$stage"; exit "$code"; }
  # The plan must be a regular file with one link, copied into a fresh file: never moved, so no link or open handle
  # of the sandboxed run reaches plan.md. Every step is checked; `installed` follows the last one.
  src=$stage/plan.md tmp=
  installed=0
  if [ -f "$src" ] && [ ! -L "$src" ] && [ -s "$src" ] && [ "$(stat -c %h -- "$src")" = 1 ] && [ ! -d "$dir/plan.md" ]; then
    if tmp=$(mktemp -- "$dir/.plan.md.XXXXXX") && cat -- "$src" > "$tmp" && mv -fT -- "$tmp" "$dir/plan.md" \
       && (cd -- "$dir" && sha256sum plan.md > .plan.sha256.tmp && mv -fT -- .plan.sha256.tmp plan.sha256); then
      installed=1
    fi
  fi
  [ -n "$tmp" ] && rm -f -- "$tmp" "$dir/.plan.sha256.tmp"
  rm -rf -- "$stage"
  if [ "$installed" -ne 1 ]; then
    echo no-plan > "$out/$name.plan.txt"
    echo "codex-work: the run left no regular single-link plan-out/plan.md that could be installed" >&2
    exit 1
  fi
  echo installed > "$out/$name.plan.txt"
  cat -- "$dir/plan.sha256"
  ;;
*) die 'usage: codex-work.sh implement|plan ...' ;;
esac

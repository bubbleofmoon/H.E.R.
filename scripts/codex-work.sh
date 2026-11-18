#!/usr/bin/env bash
# Runs a Codex plan or implement task for one ticket through scripts/codex-task.sh; the wrapper, never Codex,
# touches git and installs the result.
# usage: scripts/codex-work.sh implement <name> <worktree> <prompt-file> <message-file> [model] [effort] [minutes]
#        scripts/codex-work.sh plan <ticket> <name> <prompt-file> [model] [effort] [minutes]
# Defaults: gpt-6.2-sol; effort medium for implement, high for plan; 360 minutes. Receipts sit next to the prompt:
# codex-task's <name>.report.md and <name>.exit.txt, plus <name>.push.txt (implement: `pushed <sha>`, `no-changes`,
# `commit-failed` or `push-failed`) or <name>.plan.txt (plan: `installed` or `no-plan`).
# implement: HER_GITLEAKS must name the gitleaks binary; the worktree's HEAD must be her/<n>-<slug>, never main.
#   Codex (workspace-write) writes only the worktree and <orch>/<n>/tmp. After it exits 0 with a report, this script
#   stages the worktree, commits with the message file and pushes the branch; it exits 7 when that fails.
# plan: Codex (workspace-write) writes only <orch>/<n>/scratch and <orch>/<n>/plan-out; the plan it leaves in
#   plan-out/plan.md becomes <orch>/<n>/plan.md, and plan.sha256 holds its sha256sum line.
# <orch> is $HER_ORCH, default /root/orch/her.
set -uo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
orch=${HER_ORCH:-/root/orch/her}
die() { echo "codex-work: $*" >&2; exit 64; }
word='^[A-Za-z0-9][A-Za-z0-9_-]*$'
mode=${1:-}

case "$mode" in
implement)
  [ $# -ge 5 ] && [ $# -le 8 ] || die 'usage: implement <name> <worktree> <prompt-file> <message-file> [model] [effort] [minutes]'
  name=$2 wt=$3 prompt=$4 message=$5 model=${6:-gpt-6.2-sol} effort=${7:-medium} minutes=${8:-360}
  [[ $name =~ $word ]] || die "bad name: $name"
  [[ $minutes =~ ^[1-9][0-9]{0,2}$ ]] || die "bad minutes: $minutes"
  [ -f "$prompt" ] || die "no prompt file: $prompt"
  [ -s "$message" ] || die "no commit message: $message"
  [ -n "${HER_GITLEAKS:-}" ] && [ -f "$HER_GITLEAKS" ] && [ -x "$HER_GITLEAKS" ] || die 'HER_GITLEAKS must name the gitleaks binary'
  branch_of() { git -C "$wt" symbolic-ref --short -q HEAD 2> /dev/null; }
  [ -d "$wt" ] && [ "$(git -C "$wt" rev-parse --show-toplevel 2> /dev/null)" = "$(realpath -- "$wt")" ] || die "not a worktree root: $wt"
  branch=$(branch_of)
  [[ $branch =~ ^her/[0-9]+-[A-Za-z0-9._-]+$ ]] || die "HEAD is not a ticket branch (her/<n>-<slug>): ${branch:-detached}"
  ticket=${branch#her/}; ticket=${ticket%%-*}
  out=$(dirname -- "$(realpath -- "$prompt")")
  mkdir -p -- "$orch/$ticket/tmp"
  rm -f -- "$out/$name.push.txt"
  "$here/codex-task.sh" "$name" "$wt" "$prompt" "$model" "$effort" workspace-write "$minutes" "$orch/$ticket/tmp"
  code=$?
  [ "$code" -eq 0 ] || exit "$code"
  [ "$(branch_of)" = "$branch" ] || die "HEAD moved during the run: $(branch_of)"
  git -C "$wt" add -A || exit 7
  if git -C "$wt" diff --cached --quiet; then
    echo no-changes > "$out/$name.push.txt"
    echo "no-changes: the run left nothing to commit" >&2
    exit 0
  fi
  if ! git -C "$wt" commit -q -F "$message"; then echo commit-failed > "$out/$name.push.txt"; exit 7; fi
  sha=$(git -C "$wt" rev-parse HEAD)
  git -C "$wt" push -q -u origin "$branch"
  pushed=$?
  remote=$(git -C "$wt" ls-remote origin "refs/heads/$branch" 2> /dev/null | cut -f1)
  if [ "$pushed" -ne 0 ] || [ "$remote" != "$sha" ]; then echo push-failed > "$out/$name.push.txt"; exit 7; fi
  echo "pushed $sha" | tee "$out/$name.push.txt"
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
  "$here/codex-task.sh" "$name" "$scratch" "$prompt" "$model" "$effort" workspace-write "$minutes" "$stage"
  code=$?
  if [ "$code" -eq 0 ] && [ -s "$stage/plan.md" ] && [ ! -L "$stage/plan.md" ]; then
    mv -f -- "$stage/plan.md" "$dir/plan.md"
    rm -rf -- "$stage"
    echo installed > "$out/$name.plan.txt"
    (cd -- "$dir" && sha256sum plan.md | tee plan.sha256)
    exit 0
  fi
  rm -rf -- "$stage"
  if [ "$code" -eq 0 ]; then echo no-plan > "$out/$name.plan.txt"; echo "codex-work: the run left no plan-out/plan.md" >&2; code=1; fi
  exit "$code"
  ;;
*) die 'usage: codex-work.sh implement|plan ...' ;;
esac

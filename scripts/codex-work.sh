#!/usr/bin/env bash
# Runs a Codex plan or implement task for one ticket through scripts/codex-task.sh and verifies what it left.
# usage: scripts/codex-work.sh implement <name> <worktree> <prompt-file> [model] [effort] [minutes]
#        scripts/codex-work.sh plan <ticket> <name> <prompt-file> [model] [effort] [minutes]
# Defaults: gpt-6.2-sol; effort medium for implement, high for plan; 360 minutes. Receipts sit next to the prompt:
# codex-task's <name>.report.md and <name>.exit.txt, plus <name>.push.txt, <name>.staged.txt and <name>.scan.txt
# (implement) or <name>.plan.txt (plan: `installed` or `no-plan`).
# implement: HER_GITLEAKS must name the gitleaks binary (Codex's own push runs the pre-push hook with it). Before the run
#   the worktree must be clean, its HEAD on her/<n>-<slug> (never main), <orch>/<n> and its tmp must not be links, and
#   the remote must answer. An existing remote branch must match local HEAD, or the receipt says `pre-remote-mismatch`.
#   Codex (workspace-write, network) writes the worktree, <orch>/<n>/tmp and the repository's
#   common git dir, and commits and pushes its own ticket branch (push without -u: -u writes the common config).
#   The common config, the worktree's config.worktree, commondir and .git file and every file under hooks/ are hashed
#   before and after the run; a difference is `config-changed <files>` and nothing else is checked (git would read them).
#   Every later git call ignores them anyway: it uses the git dir found before the run, core.fsmonitor=false and
#   core.hooksPath=/dev/null, and asks $HER_REMOTE_URL (default https://github.com/bubbleofmoon/H.E.R.git) for the
#   remote head, never `origin`.
#   <name>.push.txt line 1 is the first that applies: `rewritten` (the pre-run HEAD is not an ancestor of HEAD),
#   `branch-changed <branch>`, `git-failed`, `dirty <paths>` plus `remote moved|unchanged|unknown`, `remote-moved`
#   (local HEAD unchanged but remote head moved), `no-changes` (both heads unchanged), `unpushed` or `pushed <sha>`.
#   A line `protected <paths>` follows whenever pre-run HEAD..HEAD holds commits touching .githooks/ or the scan scripts,
#   scan config or scan exceptions; a line `scan-refused` follows when scripts/scan-public.py of this checkout, run on
#   the repository's main checkout with --added-from-stdin (--revs-from-stdin when the remote had no such branch) and
#   --objects-from-stdin over those commits, refuses or cannot run (<name>.scan.txt has its output). That scan is
#   detection after the push, never a barrier. Exit 8 for `config-changed`, `rewritten`, `branch-changed`, `git-failed`,
#   `dirty`, `pre-remote-mismatch`, `remote-moved`, `unpushed` and `scan-refused`, else 0.
#   <name>.staged.txt lists the commits since the pre-run HEAD (`commit <sha> <subject>`) and the name-status of
#   every path they touched (`<status><TAB><path>`).
# plan: Codex (workspace-write) writes only <orch>/<n>/scratch and <orch>/<n>/plan-out; the plan it leaves there as a
#   regular, single-link plan.md is copied to <orch>/<n>/plan.md, and plan.sha256 holds its sha256sum line. A failed
#   install restores the old plan.md and plan.sha256.
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
  remote_url=${HER_REMOTE_URL:-https://github.com/bubbleofmoon/H.E.R.git}
  [ -d "$wt" ] && [ "$(git -C "$wt" rev-parse --show-toplevel 2> /dev/null)" = "$(realpath -- "$wt")" ] || die "not a worktree root: $wt"
  wtgit=$(git -C "$wt" rev-parse --absolute-git-dir 2> /dev/null) || die "no git dir for $wt"
  common=$(git -C "$wt" rev-parse --path-format=absolute --git-common-dir 2> /dev/null) && [ -d "$common" ] || die "no git common dir for $wt"
  [ "$(basename -- "$common")" = .git ] || die "the common git dir is not a checkout's .git: $common"
  main_root=$(dirname -- "$common")
  # Every git call from here on names the git dir found now, and turns off the settings that run programs.
  g() { git --git-dir="$wtgit" --work-tree="$wt" -c core.fsmonitor=false -c core.hooksPath=/dev/null "$@"; }
  branch_of() { g symbolic-ref --short -q HEAD 2> /dev/null; }
  branch=$(branch_of)
  [[ $branch =~ ^her/[0-9]+-[A-Za-z0-9._-]+$ ]] || die "HEAD is not a ticket branch (her/<n>-<slug>): ${branch:-detached}"
  [ -z "$(g status --porcelain --untracked-files=all)" ] || die "the worktree is not clean: $wt"
  ticket=${branch#her/}; ticket=${ticket%%-*}
  [ ! -L "$orch/$ticket" ] && [ ! -L "$orch/$ticket/tmp" ] || die "a link in the ticket folder: $orch/$ticket"
  pre=$(g rev-parse HEAD)
  pre_remote=$(g ls-remote "$remote_url" "refs/heads/$branch" 2> /dev/null | cut -f1) || die "the remote does not answer: $remote_url"
  out=$(dirname -- "$(realpath -- "$prompt")")
  rm -f -- "$out/$name.push.txt" "$out/$name.staged.txt" "$out/$name.scan.txt"
  receipt() { printf '%s\n' "$@" > "$out/$name.push.txt"; }
  if [ -n "$pre_remote" ] && [ "$pre_remote" != "$pre" ]; then
    receipt pre-remote-mismatch
    echo pre-remote-mismatch >&2
    exit 8
  fi
  mkdir -p -- "$orch/$ticket/tmp"
  [ ! -L "$orch/$ticket/tmp" ] || die "a link in the ticket folder: $orch/$ticket/tmp"
  # What git, its hooks and later sessions read: hashed before and after the run.
  snap() {
    local f p
    for f in "$common/config" "$wtgit/config.worktree" "$wtgit/commondir" "$wt/.git"; do
      if [ -L "$f" ]; then printf 'link:%s  %s\n' "$(readlink -- "$f")" "$f"
      elif [ -f "$f" ]; then sha256sum -- "$f"
      elif [ -e "$f" ]; then printf 'other  %s\n' "$f"
      fi
    done
    if [ -d "$common/hooks" ]; then
      find "$common/hooks" ! -type d -print0 | LC_ALL=C sort -z | while IFS= read -r -d '' p; do
        if [ -L "$p" ]; then printf 'link:%s  %s\n' "$(readlink -- "$p")" "$p"; else sha256sum -- "$p"; fi
      done
    fi
  }
  pre_snap=$(snap)
  TMPDIR="$orch/$ticket/tmp" "$here/codex-task.sh" "$name" "$wt" "$prompt" "$model" "$effort" workspace-write "$minutes" \
    "$orch/$ticket/tmp" "$common"
  code=$?
  post_snap=$(snap)
  if [ "$post_snap" != "$pre_snap" ]; then
    changed=$(comm -3 <(sort <<< "$pre_snap") <(sort <<< "$post_snap") | sed -e 's/^[[:space:]]*//' -e 's/^[^ ]*  //' | sort -u | paste -sd' ' -)
    receipt "config-changed $changed"
    echo "config-changed $changed" >&2
    exit 8
  fi
  [ "$code" -eq 0 ] || exit "$code"
  lines=() rc=0
  fail_git() {
    receipt "${lines[0]:-git-failed}"
    echo "${lines[0]:-codex-work: a git call failed}" >&2
    exit 8
  }
  now=$(branch_of)
  head=$(g rev-parse HEAD) || fail_git
  if ! g merge-base --is-ancestor "$pre" "$head" 2> /dev/null; then
    lines=(rewritten); rc=8
  elif [ "$now" != "$branch" ]; then
    lines=("branch-changed ${now:-detached}"); rc=8
  fi
  range="$pre..$head"
  list=$(g rev-list "$range") || fail_git
  commits=$(g log --format='commit %H %s' "$range") || fail_git
  names=$(g -c core.quotepath=off log -m --no-renames --name-status --format= "$range") || fail_git
  { [ -z "$commits" ] || printf '%s\n' "$commits"; printf '%s\n' "$names" | sed '/^$/d' | LC_ALL=C sort -u; } > "$out/$name.staged.txt"
  paths_file="$out/$name.paths.tmp"
  g log -m --no-renames --name-only -z --format= "$range" > "$paths_file" || { rm -f -- "$paths_file"; fail_git; }
  held=
  while IFS= read -r -d '' p; do
    case "${p,,}" in
      .githooks/*|scripts/check-repo.py|scripts/scan-public.py|scripts/tests/test_scan_public.py|.gitleaks.toml|.public-scan.json) held="$held${held:+ }$p" ;;
    esac
  done < <(LC_ALL=C sort -zu "$paths_file")
  rm -f -- "$paths_file"
  if [ -z "${lines[0]:-}" ]; then
    stray=$(g -c core.quotepath=off status --porcelain --untracked-files=all --no-renames) || fail_git
    stray=$(printf '%s\n' "$stray" | sed '/^$/d' | cut -c4- | paste -sd' ' -)
    remote_ok=1
    remote=$(g ls-remote "$remote_url" "refs/heads/$branch" 2> /dev/null | cut -f1) || remote_ok=0
    if [ -n "$stray" ]; then
      if [ "$remote_ok" -eq 0 ]; then moved=unknown
      elif [ "$remote" != "$pre_remote" ]; then moved=moved
      else moved=unchanged
      fi
      lines=("dirty $stray" "remote $moved"); rc=8
    elif [ "$head" = "$pre" ] && [ "$remote_ok" -eq 0 ]; then fail_git
    elif [ "$head" = "$pre" ] && [ "$remote" != "$pre_remote" ]; then lines=(remote-moved); rc=8
    elif [ "$head" = "$pre" ]; then lines=(no-changes)
    elif [ "$remote_ok" -eq 0 ] || [ "$remote" != "$head" ]; then lines=(unpushed); rc=8
    else lines=("pushed $head")
    fi
  fi
  [ -z "$held" ] || lines+=("protected $held")
  if [ -n "$list" ]; then
    scan_mode=--added-from-stdin; [ -n "$pre_remote" ] || scan_mode=--revs-from-stdin
    py=python3; command -v python3 > /dev/null 2>&1 || py=python
    if ! (
        cd -- "$main_root" || exit 1
        export GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=core.fsmonitor GIT_CONFIG_VALUE_0=false GIT_CONFIG_KEY_1=core.hooksPath GIT_CONFIG_VALUE_1=/dev/null
        printf '%s\n' "$list" | "$py" -I "$here/scan-public.py" --root "$main_root" "$scan_mode" || exit 1
        printf '%s\n' "$list" | "$py" -I "$here/scan-public.py" --root "$main_root" --objects-from-stdin || exit 1
      ) > "$out/$name.scan.txt" 2>&1; then
      lines+=(scan-refused); rc=8
    fi
  fi
  receipt "${lines[@]}"
  for line in "${lines[@]:1}"; do echo "$line" >&2; done
  case "${lines[0]}" in pushed*) echo "${lines[0]}" ;; *) echo "${lines[0]}" >&2 ;; esac
  exit "$rc"
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
  # of the sandboxed run reaches plan.md. Every step is checked; `installed` follows the last one, and a failed
  # install puts the old plan.md and plan.sha256 back (or removes both when there was no pair).
  src=$stage/plan.md
  installed=0
  bak_plan=$dir/.plan.md.bak bak_sha=$dir/.plan.sha256.bak tmp_plan=$dir/.plan.md.new tmp_sha=$dir/.plan.sha256.new
  rm -f -- "$bak_plan" "$bak_sha" "$tmp_plan" "$tmp_sha"
  if [ -f "$src" ] && [ ! -L "$src" ] && [ -s "$src" ] && [ "$(stat -c %h -- "$src")" = 1 ] && [ ! -d "$dir/plan.md" ]; then
    saved=1
    if [ -f "$dir/plan.md" ]; then cp -p -- "$dir/plan.md" "$bak_plan" || saved=0; fi
    if [ -f "$dir/plan.sha256" ]; then cp -p -- "$dir/plan.sha256" "$bak_sha" || saved=0; fi
    hash=
    if [ "$saved" -eq 1 ] && cat -- "$src" > "$tmp_plan" && hash=$(sha256sum < "$tmp_plan" | cut -d' ' -f1) && [ "${#hash}" -eq 64 ] \
       && printf '%s  plan.md\n' "$hash" > "$tmp_sha"; then
      if mv -fT -- "$tmp_plan" "$dir/plan.md" && mv -fT -- "$tmp_sha" "$dir/plan.sha256"; then
        installed=1
      else
        if [ -f "$bak_plan" ]; then mv -fT -- "$bak_plan" "$dir/plan.md"; else rm -f -- "$dir/plan.md"; fi
        if [ -f "$bak_sha" ]; then mv -fT -- "$bak_sha" "$dir/plan.sha256"; else rm -f -- "$dir/plan.sha256"; fi
      fi
    fi
  fi
  rm -f -- "$bak_plan" "$bak_sha" "$tmp_plan" "$tmp_sha"
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

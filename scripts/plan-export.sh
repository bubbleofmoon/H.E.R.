#!/usr/bin/env bash
# Exports origin/main of this checkout (git archive, no .git) into a ticket's planning scratch folder:
# /root/orch/her/<ticket>/scratch (override the root with HER_ORCH_ROOT). Prints the commit exported.
# usage: scripts/plan-export.sh <ticket> [--replace]
# A non-empty scratch or scratch-evidence folder is refused; --replace deletes both first.
# It does not fetch: run `git fetch origin main` first when the tip may have moved.
set -uo pipefail
ticket=${1:-} replace=${2:-}
if ! [[ $ticket =~ ^[0-9]+$ ]] || { [ -n "$replace" ] && [ "$replace" != --replace ]; } || [ $# -gt 2 ]; then
  echo "usage: plan-export.sh <ticket number> [--replace]" >&2
  exit 2
fi
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
dir=${HER_ORCH_ROOT:-/root/orch/her}/$ticket
scratch=$dir/scratch evidence=$dir/scratch-evidence
commit=$(git -C "$repo" rev-parse --verify --quiet 'origin/main^{commit}') || { echo "plan-export: origin/main not found in $repo" >&2; exit 1; }
for folder in "$scratch" "$evidence"; do
  if [ -n "$(ls -A -- "$folder" 2>/dev/null)" ]; then
    if [ "$replace" != --replace ]; then
      echo "plan-export: $folder is not empty; pass --replace to delete scratch and scratch-evidence first" >&2
      exit 1
    fi
  fi
done
if [ "$replace" = --replace ]; then rm -rf -- "$scratch" "$evidence"; fi
mkdir -p -- "$scratch"
if ! git -C "$repo" archive --format=tar "$commit" | tar -x -C "$scratch"; then
  rm -rf -- "$scratch"
  echo "plan-export: git archive failed" >&2
  exit 1
fi
echo "exported $commit to $scratch"

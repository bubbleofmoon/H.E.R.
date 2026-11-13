#!/usr/bin/env bash
# CI: runs the given her test targets one after another (single-threaded, like the Linux host rules) and
# writes a log plus a numeric exit receipt per target under $CI_EVIDENCE. Exits non-zero if any target failed.
# Targets: `unit` (the bin unit tests) or an integration test name from her/tests/. A target that does not
# exist on this commit is reported as skipped.
# usage: scripts/ci/run-tests.sh <target>...
set -uo pipefail
ev=${CI_EVIDENCE:-ci-evidence}
mkdir -p "$ev"
export HER_TASK_SCHEDULER_FILE="$(cd "$ev" && pwd)/task-scheduler.xml"
cd "$(dirname "$0")/../../her"
failed=()
for t in "$@"; do
  if [ "$t" = unit ]; then
    cmd=(cargo test --locked --bin her -- --test-threads=2)
  elif [ -f "tests/$t.rs" ]; then
    case "$t" in
      release_install) cmd=(cargo test --locked --features parity-clock,release-test-keys --test "$t" -- --test-threads=1) ;;
      session_turn_recovery_e2e|dependency_recovery_e2e) cmd=(cargo test --locked --features parity-clock --test "$t" -- --test-threads=1) ;;
      *) cmd=(cargo test --locked --test "$t" -- --test-threads=1) ;;
    esac
  else
    echo "skipped" > "../$ev/$t.exit.txt"; echo "== $t: not present, skipped"; continue
  fi
  echo "== $t: ${cmd[*]}"
  "${cmd[@]}" > "../$ev/$t.log" 2>&1
  code=$?
  echo "$code" > "../$ev/$t.exit.txt"
  grep -E '^test result' "../$ev/$t.log" | tail -1
  [ "$code" -eq 0 ] || { failed+=("$t"); tail -40 "../$ev/$t.log"; }
done
[ ${#failed[@]} -eq 0 ] || { echo "FAILED: ${failed[*]}"; exit 1; }

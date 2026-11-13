#!/bin/bash
# Linux: runs one cargo command with the machine-wide Rust *build* slot held only while compiling,
# and writes receipts.
# usage: scripts/cargo-slot.sh <ticket> <worktree> <evidence-dir> <run-name> <cargo subcommand> [args...]
# Receipts: <evidence-dir>/<run-name>.stdout.log, .stderr.log, .exit.txt (numeric cargo exit code).
# `test`: the build (`--no-run`) happens under the build slot; the built test executables then run directly
# outside it, limited to QQBOT_TEST_SLOTS concurrent runs (default 3). Other subcommands hold the build slot
# for the whole command. Waits for slots and only ever removes locks it created. `--offline -j 2`
# go right after the subcommand, so `test --test x -- --test-threads=1` works.
# Overrides: QQBOT_BUILD_SLOT, QQBOT_TEST_SLOTS, HER_TARGET_ROOT, RUSTC_WRAPPER.
ticket=$1 wt=$2 ev=$3 name=$4; shift 4
lock=${QQBOT_BUILD_SLOT:-/root/orch/build-slot.lock}
test_slots=${QQBOT_TEST_SLOTS:-3}
target_root=${HER_TARGET_ROOT:-/root/rb-targets}
mkdir -p "$ev" "$(dirname "$lock")"
cd "$wt/her" || exit 2
# sccache shares compiled crates across tickets; it needs CARGO_INCREMENTAL=0.
export RUSTC_WRAPPER=${RUSTC_WRAPPER-/root/.cargo/bin/sccache} SCCACHE_DIR=${SCCACHE_DIR:-/root/.cache/sccache} SCCACHE_CACHE_SIZE=${SCCACHE_CACHE_SIZE:-20G} CARGO_INCREMENTAL=0
[ -x "$RUSTC_WRAPPER" ] || unset RUSTC_WRAPPER
export CARGO_TARGET_DIR=$target_root/$ticket
cargo=/root/.cargo/bin/cargo
held=""
trap 'give' EXIT
# A lock records "<ticket> <run> <time> token=<random>". The owner proves it is alive with a heartbeat: while
# holding, it touches owner.txt every 60 s. PIDs are not used, because Codex sandboxes run in their own PID
# namespace and cannot see host processes. A waiter breaks a lock only when its heartbeat is older than 300 s, or
# when it has had no owner record for over 60 s (owner died between mkdir and writing it); a second lock
# (<lock>.break) lets only one waiter break it at a time. Release removes a lock only if it is still ours.
token="$$-$RANDOM-$RANDOM-$(date +%s%N)"
beat=""
stale() {
  local now; now=$(date +%s)
  [ -d "$1" ] || return 1
  if [ -e "$1/owner.txt" ]; then
    [ "$(stat -c %Y "$1/owner.txt" 2>/dev/null || echo "$now")" -lt "$(( now - 300 ))" ]
  else
    [ "$(stat -c %Y "$1" 2>/dev/null || echo "$now")" -lt "$(( now - 60 ))" ]
  fi
}
grab() {
  if mkdir "$1" 2>/dev/null; then
    held=$1; echo "$ticket $name $(date -Is) token=$token" > "$1/owner.txt"
    ( while sleep 60; do grep -qs "token=$token" "$1/owner.txt" && touch "$1/owner.txt" || exit 0; done ) &
    beat=$!
    return 0
  fi
  if stale "$1" && mkdir "$1.break" 2>/dev/null; then
    stale "$1" && { echo "cargo-slot: breaking stale lock $1 ($(cat "$1/owner.txt" 2>/dev/null || echo no owner))" >&2; rm -rf "$1"; }
    rmdir "$1.break"
  fi
  return 1
}
take() { until grab "$1"; do sleep 10; done; }
give() {
  [ -n "$beat" ] && { kill "$beat" 2>/dev/null; wait "$beat" 2>/dev/null; beat=""; }
  [ -n "$held" ] && grep -qs "token=$token" "$held/owner.txt" && rm -rf "$held"
  held=""
}
take_test_slot() {
  while :; do
    for i in $(seq 1 "$test_slots"); do grab "$lock.test$i" && return; done
    sleep 10
  done
}

sub=$1; shift
# `cargo test [OPTIONS] [TESTNAME] [-- ARGS]`: options go to the build; a bare TESTNAME filter and everything
# after `--` go to the test executables.
build_args=() test_args=() filters=() seen_sep="" want_value=""
for a in "$@"; do
  if [ -n "$seen_sep" ]; then test_args+=("$a"); continue; fi
  if [ "$a" = "--" ]; then seen_sep=1; continue; fi
  if [ -n "$want_value" ]; then build_args+=("$a"); want_value=""; continue; fi
  case "$a" in
    --test|--bin|--example|--bench|--features|-F|--package|-p|--target|--target-dir|--manifest-path|-j|--jobs|--profile|--color|--message-format|--config|-Z|--exclude)
      build_args+=("$a"); want_value=1 ;;
    -*) build_args+=("$a") ;;
    *) filters+=("$a") ;;
  esac
done

if [ "$sub" = test ]; then
  # Compile under the build slot and collect the test executables cargo built, then run those executables
  # directly outside the slot: a second `cargo test` could recompile there if sources changed meanwhile.
  take "$lock"
  nice -n 10 "$cargo" test --offline -j 2 --no-run --message-format=json-render-diagnostics "${build_args[@]}" \
    > "$ev/$name.build.json" 2> "$ev/$name.build.log"
  code=$?
  give
  if [ "$code" -eq 0 ]; then
    mapfile -t exes < <(python3 -I -c '
import json, sys
for line in open(sys.argv[1], encoding="utf-8"):
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") == "compiler-artifact" and m.get("profile", {}).get("test") and m.get("executable"):
        print(m["executable"])
' "$ev/$name.build.json")
    take_test_slot
    : > "$ev/$name.stdout.log"; : > "$ev/$name.stderr.log"
    for exe in "${exes[@]}"; do
      echo "     Running $exe" >> "$ev/$name.stderr.log"
      CARGO_MANIFEST_DIR="$wt/her" nice -n 10 "$exe" "${filters[@]}" "${test_args[@]}" >> "$ev/$name.stdout.log" 2>> "$ev/$name.stderr.log"
      c=$?; [ "$c" -ne 0 ] && code=$c
    done
    [ ${#exes[@]} -gt 0 ] || { echo "cargo-slot: no test executable was built" >> "$ev/$name.stderr.log"; code=2; }
    give
  else
    cp "$ev/$name.build.log" "$ev/$name.stderr.log"; : > "$ev/$name.stdout.log"
  fi
  rm -f "$ev/$name.build.json"
else
  take "$lock"
  nice -n 10 "$cargo" "$sub" --offline -j 2 "$@" > "$ev/$name.stdout.log" 2> "$ev/$name.stderr.log"
  code=$?
  give
fi
echo $code > "$ev/$name.exit.txt"
exit $code

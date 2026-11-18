#!/usr/bin/env bash
# Runs one Codex CLI task (implementation, fix, gate or review) and writes receipts next to the prompt:
#   <name>.report.md (last message), <name>.out.log, <name>.err.log, <name>.exit.txt (numeric code, "timeout",
# or "no-report" / "quota" when no report was written; the latter two exit non-zero).
# usage: scripts/codex-task.sh <name> <worktree> <prompt-file> [model] [effort] [sandbox] [timeout-minutes]
# Defaults: gpt-6.2-astra, max, workspace-write, 360 (terra failed once, 2026-10-31; Claude Opus is the fallback).
# workspace-write gets loopback/network access (tests and parity runners bind local ports) and write access to
# the build/cache/lock directories. Codex cannot write git metadata: it leaves changes uncommitted and the
# coordinator commits them. Sandbox `none` is the default (user decision 2026-11-03) and runs without
# Codex's sandbox: workers are told never to commit, and pushes are made to fail by
# pointing every remote's push URL at a non-existent path through GIT_CONFIG_* for this process tree.
set -uo pipefail
name=$1 workdir=$2 prompt=$(realpath -- "$3") model=${4:-gpt-6.2-astra} effort=${5:-max} sandbox=${6:-none} minutes=${7:-360}
out=$(dirname -- "$prompt")
rm -f -- "$out/$name.report.md" "$out/$name.exit.txt"
args=(exec --skip-git-repo-check -m "$model" -c "model_reasoning_effort=\"$effort\"" -C "$workdir" --color never -o "$out/$name.report.md")
if [ "$sandbox" = none ]; then
  args+=(--dangerously-bypass-approvals-and-sandbox)
  export GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=remote.origin.pushurl GIT_CONFIG_VALUE_0=/nonexistent/push-disabled \
    GIT_CONFIG_KEY_1=push.default GIT_CONFIG_VALUE_1=nothing
else
  args+=(-s "$sandbox")
fi
if [ "$sandbox" = workspace-write ]; then
  args+=(-c 'sandbox_workspace_write.network_access=true')
  for d in /root/orch /root/rb-targets /root/.cache/sccache /root/.cargo; do args+=(--add-dir "$d"); done
fi
nice -n 10 timeout --kill-after=60 "$((minutes * 60))" codex "${args[@]}" - < "$prompt" > "$out/$name.out.log" 2> "$out/$name.err.log"
code=$?
if [ "$code" -eq 124 ] || [ "$code" -eq 137 ]; then
  echo timeout > "$out/$name.exit.txt"
elif [ ! -s "$out/$name.report.md" ]; then
  # A run that left no report reviewed nothing: never a pass, whatever codex exited with.
  if grep -q 'hit your usage limit' "$out/$name.err.log"; then echo quota > "$out/$name.exit.txt"; else echo no-report > "$out/$name.exit.txt"; fi
  [ "$code" -eq 0 ] && code=1
else
  echo "$code" > "$out/$name.exit.txt"
fi
exit "$code"

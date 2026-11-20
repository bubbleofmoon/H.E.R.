#!/usr/bin/env bash
# usage: codex-task.sh <name> <checkout> <prompt-file> [model] [effort] [read-only|bypass] [minutes]
# Read-only review and investigation use the Linux launcher. Bypass is for codex-work implement.
# Receipts are written beside the prompt.
set -uo pipefail
if [ "$#" -lt 3 ] || [ "$#" -gt 7 ]; then
  echo "codex-task: expected a name, checkout, prompt and optional model, effort, sandbox and timeout" >&2
  exit 64
fi
name=$1 workdir=$2 prompt=$(realpath -- "$3") model=${4:-gpt-6.2-sol} effort=${5:-high} sandbox=${6:-read-only} minutes=${7:-360}
case "$sandbox" in
read-only)
  here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
  exec python3 -I "$here/readonly-task.py" codex review "$name" "$workdir" "$prompt" "$model" "$effort" "$minutes"
  ;;
bypass) ;;
*) echo "codex-task: use read-only for reviews or codex-work plan for writable scratch" >&2; exit 64 ;;
esac
out=$(dirname -- "$prompt")
rm -f -- "$out/$name.report.md" "$out/$name.exit.txt"
args=(exec --skip-git-repo-check -m "$model" -c "model_reasoning_effort=\"$effort\"" -C "$workdir" --dangerously-bypass-approvals-and-sandbox --color never -o "$out/$name.report.md")
nice -n 10 timeout --kill-after=60 "$((minutes * 60))" codex "${args[@]}" - < "$prompt" > "$out/$name.out.log" 2> "$out/$name.err.log"
code=$?
if [ "$code" -eq 124 ] || [ "$code" -eq 137 ]; then
  echo timeout > "$out/$name.exit.txt"
elif [ ! -s "$out/$name.report.md" ]; then
  # A run that left no report produced nothing: never a pass, whatever codex exited with.
  if grep -q 'hit your usage limit' "$out/$name.err.log"; then echo quota > "$out/$name.exit.txt"; else echo no-report > "$out/$name.exit.txt"; fi
  [ "$code" -eq 0 ] && code=1
else
  echo "$code" > "$out/$name.exit.txt"
fi
exit "$code"

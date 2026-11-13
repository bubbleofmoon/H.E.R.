#!/usr/bin/env bash
# PreToolUse guard for worker subagents (.claude/agents/*.md). Needs python or python3 on PATH; refuses when it cannot parse.
# Usage: agent-guard.sh <role>   role: implementer | reviewer | gate
# Reads the hook JSON on stdin; exit 2 with a reason on stderr blocks the call.
role=${1:-implementer}
input=$(cat)
PY=$(command -v python || command -v python3) || { echo "agent-guard ($role): no python on PATH; refusing." >&2; exit 2; }
fields() {
  "$PY" -I -c 'import json,sys
d=json.load(sys.stdin); t=d.get("tool_input") or {}
print(d.get("tool_name") or ""); print(json.dumps(t.get("command") or "")); print(t.get("file_path") or t.get("notebook_path") or "")' <<<"$input" 2>/dev/null
}
parsed=$(fields) || { echo "agent-guard ($role): cannot parse the hook input; refusing." >&2; exit 2; }
tool=$(sed -n 1p <<<"$parsed")
cmd=$(sed -n 2p <<<"$parsed" | "$PY" -I -c 'import json,sys; print(json.loads(sys.stdin.read()))')
path=$(sed -n 3p <<<"$parsed")

deny() { echo "agent-guard ($role): $1" >&2; exit 2; }

# Every role: the coordinator owns pushes, the integration branch, the build slot and the frozen oracle.
if [ "$tool" = Bash ]; then
  grep -Eq '(^|[;&|[:space:]])git[[:space:]]+push' <<<"$cmd" && deny "pushing is the coordinator's job; leave changes uncommitted and report."
  grep -Eq 'update-ref|branch[[:space:]]+(-f|-D|--force|-M)|checkout[[:space:]]+-B' <<<"$cmd" && grep -q 'js-rust-integration' <<<"$cmd" \
    && deny "only the coordinator moves migration/js-rust-integration."
  grep -q 'build-slot.lock' <<<"$cmd" && deny "use the build slot (scripts/cargo-slot.ps1 on Windows, scripts/cargo-slot.sh on Linux); never create or remove its lock by hand."
  grep -Eq '(>|rm |mv |cp .* |sed -i|tee ).*/root/js-baseline-01|/root/her/backups/js-rust-baseline-01' <<<"$cmd" \
    && ! grep -Eq '^[[:space:]]*(cat|ls|sha256sum|grep|head|tail|node|diff|find)[[:space:]]' <<<"$cmd" \
    && deny "the frozen JS baseline is read-only."
fi
case "$path" in
  /root/js-baseline-01/*|*/backups/js-rust-baseline-01/*) deny "the frozen JS baseline is read-only." ;;
esac

case "$role" in
  implementer)
    if [ "$tool" = Bash ]; then
      grep -Eq '(^|[;&|[:space:]])git[[:space:]]+(commit|merge|rebase|cherry-pick|revert|am|tag)([[:space:]]|$)' <<<"$cmd"         && deny "workers never commit; the coordinator merges the integration tip and commits. List changed files in your report."
    fi
    ;;
  reviewer)
    case "$tool" in Edit|Write|NotebookEdit) deny "reviews are read-only; put findings in your report." ;; esac
    if [ "$tool" = Bash ]; then
      grep -Eq '(^|[;&|[:space:]])(git[[:space:]]+(commit|merge|rebase|reset|checkout|switch|stash|restore|cherry-pick|revert|worktree|clean|tag|am|apply)|cargo|rm|mv|cp|chmod|chattr|truncate)([[:space:]]|$)' <<<"$cmd" \
        && deny "reviews are read-only (no git writes, builds or file changes); read code and git history only."
      grep -Eq '(^|[^0-9&>])>{1,2}[[:space:]]*[^&[:space:]]' <<<"$cmd" && ! grep -Eq '>{1,2}[[:space:]]*/dev/null' <<<"$cmd" \
        && deny "reviews are read-only; no output redirection to files."
    fi
    ;;
  gate)
    case "$tool" in Edit|Write|NotebookEdit)
      case "$path" in */her/src/*|*/her/tests/*|*/her-memory/src/*|*/Cargo.toml)
        deny "merge gates do not change product code or tests; classify the failure and report it." ;;
      esac ;;
    esac
    ;;
esac
exit 0

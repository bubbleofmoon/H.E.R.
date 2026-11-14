#!/usr/bin/env bash
# PreToolUse guard for worker subagents (.claude/agents/*.md); the rules and their documentation live in
# agent_guard.py beside this file (tests: scripts/tests/test_agent_guard.py).
# Usage: agent-guard.sh <role>   role: implementer | reviewer | gate. Exit 2 with a reason on stderr blocks the call.
role=${1:-implementer}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
PY=$(command -v python || command -v python3) || { echo "agent-guard ($role): no python on PATH; refusing." >&2; exit 2; }
exec "$PY" -I "$here/agent_guard.py" "$role"

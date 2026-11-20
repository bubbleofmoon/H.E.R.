#!/usr/bin/env bash
# Run the same repository checks for merge CI and pinned quick receipts.
set -euo pipefail
filter=${1:-}
if [ "${RUNNER_OS:-}" = Linux ]; then
  if ! command -v bwrap >/dev/null; then
    sudo apt-get update
    sudo apt-get install -y bubblewrap
  fi
  # GitHub's disposable Ubuntu runner can enable user namespaces for this test.
  if [ -e /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]; then
    sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
  fi
  if [ -n "$filter" ]; then
    case "$filter" in
      IsolationTests.test_worker_cannot_read_host_credentials|IsolationTests.test_worker_cannot_push_to_host_git_endpoint) ;;
      *) echo "unknown repository-check filter" >&2; exit 2 ;;
    esac
    exec python -I scripts/tests/test_readonly_agent.py "$filter"
  fi
  python -I scripts/tests/test_readonly_agent.py
elif [ -n "$filter" ]; then
  echo "repository-check filters require Linux" >&2
  exit 2
fi
python -I scripts/ci/install-gitleaks.py "$RUNNER_TEMP/her-gitleaks"
export HER_GITLEAKS="$RUNNER_TEMP/her-gitleaks/gitleaks"
if [ "$RUNNER_OS" = Windows ]; then HER_GITLEAKS="$HER_GITLEAKS.exe"; fi
python -I scripts/check-repo.py
python -I scripts/tests/test_scan_public.py
python -I scripts/tests/test_pre_push_hook.py
python -I scripts/tests/test_gh_public.py
python -I scripts/tests/test_handoff.py
python -I scripts/tests/test_check_repo.py
python -I scripts/tests/test_ci_suite.py
python -I scripts/tests/test_agent_guard.py
python -I scripts/tests/test_issue_watch.py
python -I scripts/tests/test_ci_wait.py
python -I scripts/tests/test_strip_history.py
python -I scripts/tests/test_snowluma_export.py
python -I scripts/tests/test_codex_task.py
python -I scripts/tests/test_quick_workflow.py
python -I scripts/tests/test_merge_check.py
python -I scripts/tests/test_plan_export.py
python -I scripts/scan-public.py
"$HER_GITLEAKS" dir . --config .gitleaks.toml --redact --no-banner

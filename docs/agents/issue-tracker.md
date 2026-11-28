# Issue tracker: GitHub Issues on the public repo

- **Source of truth:** `bubbleofmoon/H.E.R.` GitHub Issues (`#N` = ticket `N`; `gh issue view <n> --comments -R bubbleofmoon/H.E.R.`). Read newest comments and linked vault spec (`C:/her-vault/.scratch/...`) before claiming.
- **Public-text rule:** every issue/PR/comment body, title, or non-vocabulary label goes through `scripts/gh-public.ps1` / `scripts/gh-public.sh` (`scan-public.py` + `gitleaks`). Never run bare `gh issue create|comment|edit --body/--body-file`, `gh pr create|comment`, or `gh api -f body=...`. Never `@`-mention usernames in GitHub comments. Refusals print `KIND path:line` only; fix text instead of widening exceptions. Name vault/orchestration paths (`C:/her-vault/.scratch/...`, `C:/her-orch/<ticket>/`) by path only; never quote logs, chats, session ids, tokens or account numbers.
- **Claiming & closing:** ready = open, no `in-progress`, all native `blocked_by` dependencies closed. Claim via `gh issue edit <n> -R bubbleofmoon/H.E.R. --add-label in-progress` and keep through merge (release if pausing idle). Done = PR merged + scanned closing comment linking PR and its `/checks` tab (never raw numeric Actions run IDs, which trigger the 7+ digit scan), `KNOWN RED` lines and evidence paths, removing `in-progress` (partial merge keeps issue open noting remainder).
- **CI:** hosted `.github/workflows/checks.yml` gates merges (`docs/agents/rust-validation.md`); local gates own native/elevated/production checks. Fork PRs need maintainer approval; never attach self-hosted runners.

## Pull requests and merging

Coordinator only:
1. Branch `her/<n>-<slug>` from `origin/main` in `C:/her-wt/<n>`. Pre-PR: `git fetch origin`, run `python -I scripts/merge-check.py`, merge `origin/main`.
2. Push via hooks only if remote branch lacks exact head; create PR via `scripts/gh-public.ps1 --body-file <body.md> pr create --repo bubbleofmoon/H.E.R. --title "<n>: <summary>"` (body uses `Refs #<n>`, not `Closes`, lists plan/report paths and any added known-red).
3. Gate on two-axis `code-review` + hosted `checks` on head commit + required native checks.
4. Merge only when head contains current `origin/main` and all non-skipped `checks` jobs (`changes`, `scans`, `checks-gate`, plus `lint` and `dsh-contract` when `code=true`) are `SUCCESS` (`tests-windows` skips on PRs and runs on `main` pushes; ignore `quick`; docs-only PRs matching Markdown outside `her/src/`, `her/tests/`, `her-memory/src/`, `her-memory/tests/`, `her-ui/` set `code=false` and still require `changes`/`scans`/`checks-gate` `SUCCESS`). Lint findings are report-only; unlisted failures must be fixed or proven flake via 1 `gh run rerun <run-id> --failed -R bubbleofmoon/H.E.R.` (keep both links).
5. From `C:/her`: `gh pr merge <pr> -R bubbleofmoon/H.E.R. --merge --delete-branch --match-head-commit <gated head sha>` (never `--subject`, `--body`, `--admin`, `--auto`), then `git pull --ff-only`, `git worktree remove <worktree>`, `git branch -d her/<n>-<slug>`.
6. Fix a red `main` CI run before next merge; close issue as above.

## Ready queries

Read-only helper (Git Bash / POSIX Bash; stops on API error):

```bash
ready_issues() {
  local candidates n blockers
  candidates=$(gh api --method GET --paginate repos/bubbleofmoon/H.E.R./issues \
    -f state=open -f per_page=100 \
    --jq '.[] | select(.pull_request == null) | select(all(.labels[]; .name != "in-progress")) | .number') || return
  for n in $candidates; do
    blockers=$(gh api --paginate \
      "repos/bubbleofmoon/H.E.R./issues/$n/dependencies/blocked_by" \
      --jq '.[] | select(.state != "closed") | .number') || return
    if [ -z "$blockers" ]; then printf '#%s\n' "$n"; fi
  done
}

ready_issues
```

## Wayfinding operations

- **Relation/state ops (numbers, ids, fixed vocabulary labels only):** `gh api` / `gh issue edit --add-label|--remove-label|--add-assignee`, sub-issues (`repos/bubbleofmoon/H.E.R./issues/<map>/sub_issues -f sub_issue_id=<id>`), `blocked_by` (`issues/<n>/dependencies/blocked_by -f issue_id=<blocker>`), close/merge/rerun, vocabulary labels (`in-progress`, `windows`, `linux`, `wayfinder:*`). Reads (`GET`/`view`) publish nothing.
- **Published text:** bodies, titles, non-vocabulary labels, milestones must go through `scripts/gh-public.{ps1,sh}` (`issue|pr create|comment` with 1 body file; never edit issue bodies via CLI).
- **Maps & tickets:** map = `wayfinder:map`; sub-issues carry exactly one of `wayfinder:{research,prototype,grilling,task}` (map/tickets carry no triage, `linux`, or `in-progress` label; add `windows` only when work must run on Windows/Rig). Claim by assigning (`@me`); frontier = open unassigned sub-issues with all blockers closed. Resolve with scanned resolution comment + close + scanned map-pointer comment (`- #<ticket> <one-line outcome> (<date>)`). Map decisions read from newest map comments (if body rewrite needed, post scanned replacement comment for owner manual browser edit). Public `research/<slug>` branches (never merged) stay free of vault material.

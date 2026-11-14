# Issue tracker: GitHub Issues on the public repo

- HER tickets live as GitHub Issues on `bubbleofmoon/H.E.R.` (public; `gh issue view <n> --comments -R bubbleofmoon/H.E.R.`). Issue `#N` is ticket `N`; the issue is authoritative for state, discussion and blockers (native GitHub issue dependencies). Read `baton.md` first in a fresh session, then the newest handoff addressed to your side; its scope takes precedence over older issue text.
- Every comment, issue body and pull-request body goes through `scripts/gh-public.ps1` / `scripts/gh-public.sh`, which run the public scan over the body file and call `gh` only when it is clean. A bare `gh issue comment`/`issue create`/`pr comment` is not allowed: it bypasses the scan. A refusal prints each finding's kind and line number, never the matched text; rewrite the text rather than adding an exception.
- A ticket states what it needs in public and points elsewhere for the rest: specs, raw logs, validation notes, chat or session samples and production incident detail stay in the vault (`C:/her-vault/.scratch/...`) or under `C:/her-orch/<ticket>/`, and the issue names those files by path only. Never quote a log line, a conversation or an account number into a ticket.
- Host labels: `windows` needs the Windows machine (native desktop/session checks, elevated scheduler work, production steps, Windows-only implementation and its gates); `linux` is everything else. A coordinator starts implementation only on issues with its own host label and `baton:<side>`. A baton on an issue for the other host can request an answer, not implementation there.
- A ticket is ready to claim when it is open, has the coordinator's host label and baton, has no `in-progress` claim, and every blocker is closed. The coordinator rechecks readiness and claims it with `gh issue edit <n> -R bubbleofmoon/H.E.R. --add-label in-progress` before launching a worker. Keep the claim through implementation, review, fixes and the merge gate; a fresh session inspects the existing worktree, workers and reports before resuming a claimed issue. Release the claim when pausing with no worker running or handing the work to another coordinator. Workers never claim or change issue, pull-request or CI state; their only GitHub writes are pushes to their ticket branch and `quick.yml` runs (`implementer-protocol.md`).
- Done means the coordinator closes the issue as completed after merging its pull request, with a comment linking the pull request, the green CI run, any KNOWN RED lines and the evidence paths; remove `in-progress` and the baton. A partial merge leaves the issue open with a handoff recording what remains (including work blocked on another issue).
- Merge checks are the hosted runners (`.github/workflows/checks.yml`, `rust-validation.md` "GitHub-hosted CI"): every ticket merges through a pull request, and its CI run is linked from the issue. Local gates keep the native, elevated and production checks the hosted runners cannot run, and stay authoritative for those. Fork pull requests need a maintainer approval before their workflow runs, and no self-hosted runner is ever attached to this repo.

## Pull requests and merging

Only a coordinator runs these steps.

1. The ticket branch `her/<n>-<slug>` is cut from `origin/main` (worktree `/root/her-wt/<n>` or `C:/her-wt/<n>`). Before opening or updating the pull request, `git fetch origin` and merge `origin/main` into it.
2. `git push -u origin her/<n>-<slug>`, then from the ticket worktree: `scripts/gh-public.sh --body-file <body.md> pr create --repo bubbleofmoon/H.E.R. --title "<n>: <summary>"` (`.ps1` on Windows). gh takes the current branch as head and `main` as base. The body says `Refs #<n>` (not `Closes`; the coordinator closes the issue), names plan and report paths, and lists any known-red entry the branch adds. Every later push to the branch reruns CI.
3. Gate with `merge-gate`: the CI result of the head commit plus the native checks the ticket needs.
4. Merge only when every `checks` workflow check on the head commit concluded `SUCCESS` (a job green only through listed known red counts as green), except the `tests-windows` checks, which are `SKIPPED` on a pull request because they run on pushes to `main`; `quick` workflow runs never count either way and the head contains the current `origin/main`. If `main` moved, merge it in, push and wait for the new run. An unlisted failure is fixed, or shown to be a flake by one rerun of the failed jobs (`gh run rerun <run-id> --failed -R bubbleofmoon/H.E.R.`, keep both run links).
5. From the main checkout: `gh pr merge <pr> -R bubbleofmoon/H.E.R. --merge --delete-branch --match-head-commit <gated head sha>`. This makes a merge commit whose message comes from the scanned title; with `-R` gh deletes only the remote branch. Never pass `--subject`, `--body`, `--admin` or `--auto`. Then `git pull --ff-only` in the main checkout, `git worktree remove <worktree>` and `git branch -d her/<n>-<slug>`.
6. The push to `main` runs CI again; a red `main` is fixed before the next merge. Close the issue as in "Done" above.

## Ready queries

Run this read-only helper in Bash (Linux or Git Bash on Windows). It paginates both the host/baton queue and each issue's blockers, excludes claims and pull requests, and prints only issues whose blockers are all closed. An API error stops the query; it is not an empty blocker list. Recheck the issue and newest handoff before claiming, since the queue can change.

```bash
ready_issues() {
  local side=$1 candidates n blockers
  candidates=$(gh api --method GET --paginate repos/bubbleofmoon/H.E.R./issues \
    -f state=open -f labels="$side,baton:$side" -f per_page=100 \
    --jq '.[] | select(.pull_request == null) | select(all(.labels[]; .name != "in-progress")) | .number') || return
  for n in $candidates; do
    blockers=$(gh api --paginate \
      "repos/bubbleofmoon/H.E.R./issues/$n/dependencies/blocked_by" \
      --jq '.[] | select(.state != "closed") | .number') || return
    if [ -z "$blockers" ]; then printf '#%s\n' "$n"; fi
  done
}

ready_issues windows
ready_issues linux
```

Read the queue when a session starts and after each handoff; there is no watcher process. Keep your baton while working so a new session can find the work.

## Wayfinding operations

Two kinds of write, and the rule differs between them:

- A **relation or state** operation carries numbers, ids and fixed vocabulary names only: adding a sub-issue, adding or removing a `blocked_by` dependency, assigning, closing, merging a pull request with the command in "Pull requests and merging", rerunning a CI job, and adding or removing one of this repository's own labels (`in-progress`, `windows`, `linux`, `baton:*`, `wayfinder:*`). `gh api` and `gh issue edit --add-label/--remove-label/--add-assignee` are the tools for those, and the wrappers refuse `gh api` precisely because the same command can also publish text. A read (`gh api --method GET`, `gh issue view`) publishes nothing.
- **Published text** is anything a person reads on the issue: an issue, PR or comment body, a title, a label name outside the vocabulary, a milestone title. Text goes through `scripts/gh-public.ps1` / `scripts/gh-public.sh` and nowhere else. The wrappers scan the body file plus every `--title` and `--label` value and call `gh` only for a clean result; they support `issue|pr create|comment` with one body file, so a bare `gh issue edit --body`/`--body-file`, `gh issue create --body`, `gh api -f body=…` and any editor or prompt path stay forbidden.

- A map is one GitHub issue labelled `wayfinder:map`; its tickets are sub-issues of it (`gh api repos/bubbleofmoon/H.E.R./issues/<map>/sub_issues -f sub_issue_id=<issue id>`: two ids, no text), each with exactly one of `wayfinder:research`, `wayfinder:prototype`, `wayfinder:grilling`, `wayfinder:task`. Map and tickets carry no triage, `windows`/`linux` or `in-progress` label, except a task ticket whose work must run on the Windows host or the Rig, which also gets `windows` and, when handed over, `baton:windows`.
- Blocking uses the native issue dependencies (`gh api repos/bubbleofmoon/H.E.R./issues/<n>/dependencies/blocked_by -f issue_id=<blocker id>`). Claim a ticket by assigning it (`gh issue edit <n> --add-assignee @me`); the frontier is the map's open, unassigned sub-issues whose blockers are all closed.
- Resolve a ticket with a resolution comment through `scripts/gh-public`, then close it (completed). Record its place in the map as a **map-pointer comment on the map issue**, also through `scripts/gh-public`: one line, `- #<ticket> <one-line outcome> (<date>)`, in the body file. Map decisions are read from the map's newest comments, so no issue body is ever edited. The wrappers support create and comment only, and that is deliberate: never publish a body edit with a bare `gh issue edit`. If a map body genuinely has to be rewritten, the coordinator posts the complete replacement text as a scanned comment that says it replaces the body, and the body is changed by hand in the browser, which is the owner's action, not a worker's and not a coordinator's command line.
- Research findings go on a pushed `research/<slug>` branch (never merged), linked from the ticket. Keep them free of vault material: the branch is public.

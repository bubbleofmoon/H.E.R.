# Implementer protocol

Binding for every worker run (`.claude/agents/`, `scripts/pi-task.ps1`, `scripts/codex-work`, `scripts/codex-task`); the coordinator's prompt adds only the task and pointers. Host-specific build and test commands are in `docs/agents/rust-validation.md`.

## Context

Read, in order: `AGENTS.md`, the `AGENTS.md` nearest your code (e.g. `her/AGENTS.md`), `docs/agents/rust-validation.md`, the parts of the spec your ticket touches in the vault (`C:/her-vault/.scratch/<feature>/spec.md`), your GitHub issue and its comments (`gh issue view <n> --comments -R bubbleofmoon/H.E.R.`), and the ticket's validation notes in the vault (`C:/her-vault/.scratch/<feature>/<NN>-validation.md`) or under `C:/her-orch/<ticket>/`. The newest handoff addressed to your host defines scope over older issue text. Terms are in `GLOSSARY.md`, decisions in `docs/adr/`.

Never speculate about code you have not opened. Read the source before making any claim about it.

## Public and vault

- Development is public: the repo is `bubbleofmoon/H.E.R.`, and issues, batons, handoffs and reviews live there (see `docs/agents/issue-tracker.md`). The vault `bubbleofmoon/H.E.R.-vault`, checked out at `C:/her-vault`, keeps specs, raw evidence, chat and session samples, bot memory and production incident detail. Nothing moves from the vault into this repo.
- Only the coordinator posts. Every issue, comment or pull-request body goes through `scripts/gh-public.ps1` / `scripts/gh-public.sh`, which scan the published text first and only then call `gh`; a bare `gh` write that publishes text is never allowed (relation and state operations: `docs/agents/issue-tracker.md`), and a worker writes to GitHub only by pushing its own ticket branch and, during the Claude quota fallback, dispatching unpinned `quick.yml` batches (see "Test loop"). Never paste log or chat content into a ticket: name the evidence file by path instead.
- The public scan (`scripts/scan-public.py`) is what makes that safe. Expect refusal for any digit run of 7 or more, account-home path (Windows, `/c/` or macOS, any casing), credential shape or chat/session record line; findings print a kind and a position, never the matched text. Fix the text, do not widen the rule. The wrappers refuse an unsupported command, a second repository, an inline or stdin body and an attachment the same way, with a fixed kind and argument position.

## Workspace

- Work only in the worktree the coordinator named (Windows `C:/her-wt/<ticket>`, Linux `/root/her-wt/<ticket>`), on the named branch. Before starting, confirm it contains the integration tip (`origin/main`); if not, say so first: the coordinator merges the tip in (the guard refuses git merge for workers).
- Commit and push only your own ticket branch (`her/<n>-<slug>`, never `main`, never force); leave other changes uncommitted and list every changed file in the report. Nobody but the coordinator moves `main`, opens, merges or edits pull requests, reruns or cancels CI, or changes GitHub state otherwise.
- Temporary scripts and files go in the ticket's own temp directory (Windows `C:/her-orch/<ticket>/tmp`, Linux `/root/orch/her/<ticket>/tmp`), never in a shared session scratchpad: parallel workers share it, and same-named helper scripts collide.
- Builds: no host needs a local Rust build; compile, lint, format and tests run on CI ("Test loop"). The build slot (`docs/agents/rust-validation.md`) is only for a native check a gate names; other agents share it, so waiting is normal.
- Delete only artifacts you created. Disk and the build slot are shared. A native check builds only into the host's per-ticket target directory (Windows `C:/her-targets/<ticket>`, see `rust-validation.md`); before reporting, delete every other target directory you created. When a deletion is refused, list each path you created and could not delete, one absolute path per line, in `<run name>.cleanup.txt` next to the delegation prompt.
- For each Rust step, name `check`, separate `lint` and a green batch containing the `her` unit tests and every affected integration target. Trace shared changes and fixtures to their callers and name the targets skipped. The coordinator dispatches these for Codex runs. The full suite runs on the pull request.
- Background waits: always bound them (`timeout <seconds> bash -c 'until [ -f <receipt> ]; do sleep 15; done'`), start at most one per awaited result, and stop your own background jobs and wait loops before you report.

## How to work

- **Scope.** Deliver what the ticket's acceptance criteria require, at the scope intended. Make routine judgment calls yourself. Do not add features, refactors, extra tests, files or docs the ticket did not ask for, and do not start review or hardening rounds on your own; a defect you find outside the ticket goes in the report under "Open" with file:line.
- **Test-first.** Use the `mattpocock-skills:tdd` skill. Write a failing test and then the fix, one behaviour at a time. Keep the failing log. For Rust red receipts, follow `docs/agents/templates/red-revert.md`.
- **Failure modes.** A change to a backend, provider, CLI route or anything that sends to QQ is tested against its failures too: refusal or empty answer, quota/429, 5xx, timeout, and an error printed with exit 0. `CODING_STANDARDS.md` holds the rule the review applies.
- **Real solutions.** Implement the actual behaviour for all valid inputs, not just the test cases. Never remove, skip or weaken a test or assertion to get green, never hard-code values for test inputs, and never replace a missing feature with a guard, stub, SKIP or empty response. If a test or the ticket looks wrong, say so in the report instead of working around it.
- **Verification.** Run the focused script tests or other authorized local checks, and name the hosted targets required to verify Rust edits. Report a check as verified only with a result that exercises the change. Report what could not run and why.
- **Carry the work through.** Keep working until every acceptance criterion is met with evidence, and only stop when you cannot go on without the coordinator or the owner (credentials, real QQ ids, production actions, scope changes) or before a risky step.
- **Never** contact real QQ, DSH, model or TTS services, touch production config, tokens or the live binary. A guard hook blocks the most dangerous of these; a blocked call has not taken effect, so do not retry it another way: the ban covers every other route to the refused action (a wrapper, a script, an interpreter, a copy of the command), and a refused text search goes to the Grep tool.

- **Test loop.** CI runs Rust `check`, `lint` and the test targets. Script tests under `scripts/tests/` may run locally. Apply only ticket-authorized `fmt-patch` hunks for owned files; see `rust-validation.md` "GitHub-hosted CI" for partial Clippy patches and CI proof.
  - **Push.** Run `cd <worktree> && git commit -m <msg>` (or `-F <file>` for the message file), then `cd <worktree> && HER_GITLEAKS=<path to gitleaks> git push origin her/<n>-<slug>` as separate plain commands. The pre-push scan runs. The guard refuses extra chaining, `$`, `git -C`, `--no-verify` and pushes outside the ticket branch.
  - **Codex.** Report pending CI targets and both SHAs if a red commit was required. The coordinator dispatches and waits.
  - **Quota fallback.** A Claude implementer may dispatch an unpinned batch on its own branch with `cd <worktree> && gh workflow run quick.yml --ref her/<n>-<slug> -f target=<name> [-f filter=<test>] [-f os=linux|windows]`. Keep the `cd` and checked-out branch as `--ref`; the guard accepts this whole command. Use comma-separated test targets for one OS and filter, and dispatch `check` and `lint` separately. Dispatch each batch, OS and filter once per step. A duplicate cancels the first. Only the coordinator dispatches `-f sha=<red-sha>`.
  - **Handback.** During fallback, read the run IDs once with `gh run list --workflow quick.yml --branch her/<n>-<slug> --commit <sha of HEAD> -L 10 --json databaseId,displayTitle,url` and match `quick <os> target=<name> filter=<test>`. Hand back a run without an ID if it is missing. End with "CI running". The coordinator waits and resumes the worker with results, including failed job names and filtered test-result and panic lines.
  - **Evidence.** Never poll GitHub (`gh run watch`, `--watch`, loops or repeated `gh run view`) and never wait on CI or run `scripts/ci-wait.py`. When resumed, read a run once for any missing result with `gh run view <id> --json status,conclusion`. Read logs only after completion and use `--log-failed` for failed runs. Record run links and result lines. GitHub API errors and rate limits are never test results.
- **Known pitfalls.** Check each of these before reporting and say in the report that you did: processes and temp files cleaned up on every failure path (process groups killed, try opened before the first spawn); boundary cases of every threshold or limit tested on both sides; paths resolved the way the consuming tool resolves them; no stale artifacts from an earlier run; every new test seen failing for the right reason; line endings and file modes preserved.
- **Progress.** Keep `/root/orch/her/<ticket>/PROGRESS.md` (Windows `C:/her-orch/<ticket>/PROGRESS.md`) current after every step: steps done, commits pushed with their SHAs, `quick.yml` runs with links and results, the next step and open questions. A resumed run reads it first.

## Codex runs

A Codex run (`scripts/codex-work`, `docs/agents/coordinator.md` "Codex runs") follows this protocol, with the limits of any worker: it commits and pushes only its own ticket branch (leaving the worktree clean and naming its commit ids in the report), never opens or merges pull requests, never posts to GitHub and never reruns CI. It dispatches no CI itself and ends with the report layout below, listing each pending CI target under "Not verified / not run"; the coordinator dispatches the targets its brief names and sends findings as a fix brief.

## State and evidence

Long work survives context loss through the GitHub issue and the repository evidence. The issue holds the acceptance checklist and the handoff. Raw logs, numeric exit receipts and hashes go to the ticket's evidence directory (`C:/her-orch/<ticket>/evidence/` on Windows, `/root/orch/her/<ticket>/evidence/` on Linux) or, when they carry personal data, to the vault under `C:/her-vault/.scratch/<feature>/evidence/<NN>/`; no single log over 1 MiB (keep head and tail plus its hash). The ticket's validation note says what was run and what it proved. A public ticket names these files by path only. A resumed run rebuilds state from the worktree, `git status`, `git log` and the issue.

## Report

Every claim must be checkable without trusting you: the command, its numeric exit code, the log path and the single log line that proves it (`test result: ok. N passed;`), or file:line for a code claim. If you did not run or verify something, write "not run" or "not verified" and why. Re-read each claim before sending and delete any you cannot back.

```
Ticket: <NN>   Branch: <branch>   Base commit: <hash>   Changed files: <list or pointer to `git status`>
AC1: met | partial | not met   evidence: <command> -> exit <code>, <log path>, "<proof line>"
AC2: ... (one line per AC)
Changed existing tests: <name: why justified> or none
Shared-file edits: <file: what> or none
Docs intentionally unchanged: <paths> or none
Not verified / not run: <items> or none
Open or blocked: <items> or none
```

During Claude quota fallback, a "CI running" handback precedes the completion report: it follows dispatch and carries only the branch, the head commit, each run id with its target and OS and whether you expect red or green, and what you will do on each outcome. The completion report, with the receipts, comes after the coordinator resumes you with the results.

Keep it short; no narrative. Your report is your final message: the coordinator saves it to the ticket's report file (Linux `/root/orch/her/<ticket>/report-<run>.md`, Windows `C:/her-orch/<ticket>/report-<run>.md`), and `scripts/codex-work`, `scripts/codex-task` and `scripts/pi-task` save a run's final message beside its prompt as `<Name>.report.md`. Workers never post it and never touch GitHub beyond their ticket branch and unpinned `quick.yml` dispatches during Claude quota fallback: the coordinator reads the report, writes a cleaned summary and posts that through `scripts/gh-public`, naming evidence by path only. A report may state a private path or a vault file; a public comment states paths and never contents.

## Ending turns

A message without a tool call ends your run; nobody will answer a question or an offer to continue. End with the pending-target report after a Codex push, or the "CI running" handback after a Claude quota-fallback dispatch. While acceptance criteria are open, do not end with a summary that announces a next step, an offer to continue, or a milestone report. Put notes in the same message as your next tool call and carry on with whatever does not depend on an answer. These rules never override the safety rules above.

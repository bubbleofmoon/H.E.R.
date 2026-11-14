# Implementer protocol

Binding for every worker run (`.claude/agents/`, `scripts/pi-task.ps1`, `scripts/codex-task.ps1`); the coordinator's prompt adds only the task and pointers. Host-specific build and test commands are in `docs/agents/rust-validation.md`.

## Context

Read, in order: `AGENTS.md`, the `AGENTS.md` nearest your code (e.g. `her/AGENTS.md`), `docs/agents/rust-validation.md`, the parts of the spec your ticket touches in the vault (`C:/her-vault/.scratch/<feature>/spec.md`), your GitHub issue and its comments (`gh issue view <n> --comments -R bubbleofmoon/H.E.R.`), and the ticket's validation notes in the vault (`C:/her-vault/.scratch/<feature>/<NN>-validation.md`) or under `C:/her-orch/<ticket>/`. The newest handoff addressed to your host defines scope over older issue text. Terms are in `GLOSSARY.md`, decisions in `docs/adr/`.

Never speculate about code you have not opened. Read the source before making any claim about it.

## Public and vault

- Development is public: the repo is `bubbleofmoon/H.E.R.`, and issues, batons, handoffs and reviews live there (see `docs/agents/issue-tracker.md`). The vault `bubbleofmoon/H.E.R.-vault`, checked out at `C:/her-vault`, keeps specs, raw evidence, chat and session samples, bot memory and production incident detail. Nothing moves from the vault into this repo.
- Only the coordinator posts. Every issue, comment or pull-request body goes through `scripts/gh-public.ps1` / `scripts/gh-public.sh`, which scan the published text first and only then call `gh`; a bare `gh` write that publishes text is never allowed (relation and state operations: `docs/agents/issue-tracker.md`), and a worker writes to GitHub only by pushing its own ticket branch and triggering `quick.yml` (see "Test loop"). Never paste log or chat content into a ticket: name the evidence file by path instead.
- The public scan (`scripts/scan-public.py`) is what makes that safe. Expect refusal for any digit run of 7 or more, account-home path (Windows, `/c/` or macOS, any casing), credential shape or chat/session record line; findings print a kind and a position, never the matched text. Fix the text, do not widen the rule. The wrappers refuse an unsupported command, a second repository, an inline or stdin body and an attachment the same way, with a fixed kind and argument position.

## Workspace

- Work only in the worktree the coordinator named (Windows `C:/her-wt/<ticket>`, Linux `/root/her-wt/<ticket>`), on the named branch. Before starting, confirm it contains the integration tip (`origin/main`); if not, merge the tip in (existing branch) or reset onto it (fresh branch).
- Commit and push only your own ticket branch (`her/<n>-<slug>`, never `main`, never force); leave other changes uncommitted and list every changed file in the report. Nobody but the coordinator moves `main`, opens, merges or edits pull requests, reruns or cancels CI, or changes GitHub state otherwise.
- Temporary scripts and files go in the ticket's own temp directory (Windows `C:/her-orch/<ticket>/tmp`, Linux `/root/orch/her/<ticket>/tmp`), never in a shared session scratchpad: parallel workers share it, and same-named helper scripts collide.
- Builds: only through the build-slot procedure in `docs/agents/rust-validation.md`. Other agents share the slot, so waiting is normal; do non-build work meanwhile and run long commands in the background or with long timeouts.
- Delete only artifacts you created. Disk and the build slot are shared. Build only into the host's per-ticket target directory (Windows `C:/her-targets/<ticket>`, see `rust-validation.md`); before reporting, delete every other target directory you created. When a deletion is refused, list each path you created and could not delete, one absolute path per line, in `<run name>.cleanup.txt` next to the delegation prompt.
- Before reporting a ticket or fix as done, run the `her` unit tests and every integration target your change could affect (single-threaded, the synthetic-clock feature where the rules say), and name the targets you skipped. The full suite runs in CI on the ticket's pull request; an implementer runs it locally only when the delegation prompt asks.
- Background waits: always bound them (`timeout <seconds> bash -c 'until [ -f <receipt> ]; do sleep 15; done'`), start at most one per awaited result, and stop your own background jobs and wait loops before you report.

## How to work

- **Scope.** Deliver what the ticket's acceptance criteria require, at the scope intended. Make routine judgment calls yourself. Do not add features, refactors, extra tests, files or docs the ticket did not ask for, and do not start review or hardening rounds on your own; a defect you find outside the ticket goes in the report under "Open" with file:line.
- **Test-first.** Use the `mattpocock-skills:tdd` skill: a failing test that shows the gap, then the fix, one behaviour at a time. Keep the failing run's log as evidence.
- **Failure modes.** A change to a backend, provider, CLI route or anything that sends to QQ is tested against its failures too: refusal or empty answer, quota/429, 5xx, timeout, and an error printed with exit 0. `CODING_STANDARDS.md` holds the rule the review applies.
- **Real solutions.** Implement the actual behaviour for all valid inputs, not just the test cases. Never remove, skip or weaken a test or assertion to get green, never hard-code values for test inputs, and never replace a missing feature with a guard, stub, SKIP or empty response. If a test or the ticket looks wrong, say so in the report instead of working around it.
- **Verification.** When you change code that can be run, built or tested, run a real check that exercises the change: the ticket's focused tests, the build, or an integration check. A syntax-only check, or a check command that failed to start, does not count. Only if no real check can run here, say which one you did not run and why instead of reporting the change as done.
- **Carry the work through.** Keep working until every acceptance criterion is met with evidence, and only stop when you cannot go on without the coordinator or the owner (credentials, real QQ ids, production actions, scope changes) or before a risky step.
- **Never** contact real QQ, DSH, model or TTS services, touch production config, tokens or the live binary. A guard hook blocks the most dangerous of these; a blocked call has not taken effect, so do not retry it another way.

- **Test loop.** Run the one target you are changing locally only when it is fast; otherwise commit and push your ticket branch, each as its own plain command (`cd <worktree> && git commit -m <msg>` or `-F <file>`, then `cd <worktree> && HER_GITLEAKS=<path to gitleaks> git push -u origin her/<n>-<slug>`; the pre-push scan runs, and the guard refuses chaining, `$`, `git -C`, `--no-verify` and any other push), then `cd <worktree> && gh workflow run quick.yml --ref her/<n>-<slug> -f target=<name> [-f filter=<test>] [-f os=windows]`, then `gh run list --workflow quick.yml --branch her/<n>-<slug> -L 1` and `gh run watch <id> --exit-status --interval 60`; quote the run link and the result lines in the report. The full suite runs on the pull request. Poll CI at most once a minute; parallel workers share one GitHub API limit (5000 an hour), and a 403 rate-limit answer is never a test result.
- **Known pitfalls.** Check each of these before reporting and say in the report that you did: processes and temp files cleaned up on every failure path (process groups killed, try opened before the first spawn); boundary cases of every threshold or limit tested on both sides; paths resolved the way the consuming tool resolves them; no stale artifacts from an earlier run; every new test seen failing for the right reason; line endings and file modes preserved.
- **Self-review.** Before reporting, run Codex read-only on your branch, at most two rounds:
  - Write the prompt to `/root/orch/her/<ticket>/review/codex-self-<k>.md` (Windows `C:/her-orch/<ticket>/review/`). Its first lines are `Head: <sha>` and `Base: <sha of origin/main>`. It asks for a two-axis review (spec vs ticket, correctness defects) of `git diff <base>...<head>` in the worktree only, and names no vault path.
  - Run exactly `cd <worktree> && /root/her/scripts/codex-task.sh <ticket>-self-<k> <worktree> <prompt> gpt-6.1-sol high read-only [minutes]`. On Windows: `powershell -NoProfile -File C:/her/scripts/codex-task.ps1 -Name <ticket>-self-<k> -WorkDir <worktree> -PromptFile <prompt> -Model gpt-6.1-sol -Effort high -Sandbox read-only [-TimeoutMinutes <1-999>]`.
  - The guard allows only this form: the integration checkout's script, no other model, effort or sandbox, nothing chained.
  - Start it with the Bash tool's background option and wait for its `.exit.txt` with a bounded wait.
  - Write no `<name>.cleanup.txt`.
  - Verify each finding in the code, and fix the real ones, each swept for its pattern across your change.
  - Codex is capped at three rounds per ticket, the gate's included.

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

Keep it short; no narrative. Your report is your final message: the coordinator saves it to the ticket's report file (Linux `/root/orch/her/<ticket>/report-<run>.md`, Windows `C:/her-orch/<ticket>/report-<run>.md`), and `scripts/codex-task` and `scripts/pi-task` save a run's final message beside its prompt as `<Name>.report.md`. Workers never post it and never touch GitHub beyond their ticket branch and `quick.yml` runs: the coordinator reads the report, writes a cleaned summary and posts that through `scripts/gh-public`, naming evidence by path only. A report may state a private path or a vault file; a public comment states paths and never contents.

## Ending turns

A message without a tool call ends your run; nobody will answer a question or an offer to continue. While acceptance criteria are open, do not end with a summary that announces a next step, an offer to continue, or a milestone report. Put notes in the same message as your next tool call and carry on with whatever does not depend on an answer. These rules never override the safety rules above.

# Baton: who acts next on a ticket

Binding for both coordinators (Windows and Linux). Tickets are GitHub Issues on the public repo `bubbleofmoon/H.E.R.` (`docs/agents/issue-tracker.md`); this file says who must act on an issue next and how a fresh session picks up. Hand off with `scripts/handoff.sh` / `scripts/handoff.ps1`.

## Labels

- `baton:windows`: the Windows coordinator acts next. `baton:linux`: the Linux coordinator acts next. `baton:owner`: a decision only the owner (the administrator) can make.
- At most one baton label per issue. No baton means nobody owes an action (a closed issue, or an open one still blocked).
- `windows` / `linux` say where a ticket can be done; the baton says who acts now. Linux holds the queue, sizes and plans; Windows gets only finished work items (`docs/agents/coordinator.md`). A side may hold the baton of a ticket labelled for the other machine only to answer a question on it.

## Handing over

1. Write the comment to a body file whose first line is `**Handoff → windows**`, `**Handoff → linux**` or `**Handoff → owner**`.
2. Run `scripts/handoff.sh <issue> <linux|windows|owner> <body-file>` (`scripts/handoff.ps1` on Windows). It changes nothing and exits non-zero when the heading does not match the side, the issue is not open, the repository has no `baton:<side>` label or the public scan refuses the body. Otherwise it posts the comment through `scripts/gh-public` first, so the receiver never wakes to a stale comment, then one `gh issue edit` removes every other `baton:*` label and adds `baton:<side>`. Exit 4 means `gh` failed after the scan passed. If it failed posting the comment, the comment may or may not be on the issue and the baton did not move: check the issue first. If it failed moving the label, the comment is posted: run the `gh issue edit` line it prints.

The comment must stand on its own; the receiver reads nothing else first:

- **State:** where the work stands (what is merged, what is in a worktree or branch, what failed).
- **Commit:** the exact commit (and branch or PR) the receiver starts from.
- **Do:** what the receiver should do.
- **Done when:** what counts as finished, and who gets the baton back.
- **Links:** spec section, related issues, evidence or report paths (paths only — never their contents).

Hand over when: your side merged a ticket that unblocks one of the other side's; you need the other side to do something on an issue (merge first, verify on its platform, relabel, answer a question); you finish what the other side asked; or the owner must decide (`owner`, with the question, the options and your recommendation). When the owner answers, whoever sees the answer passes the baton to the side that acts on it.

Keep your own baton while you work on an issue, and before ending a session hand off to your own side with a comment that records the current state, so a fresh session of your side can resume from it. Shared or broad changes (build scripts, CI, `AGENTS.md`, docs indexes, test fixtures, large merges) are announced on the issue that caused them, with a handoff when the other side must act.

## A fresh session

A new or cleared session never acts on chat memory alone. It:

1. reads this file (and `AGENTS.md` if not loaded);
2. lists the open issues carrying its own baton label: `gh issue list -R bubbleofmoon/H.E.R. --label baton:<side> --state open`, and the ready queue with the helper in `docs/agents/issue-tracker.md`;
3. on each, does what the newest handoff comment addressed to it asks, starting from the commit it names (checking worktrees, running workers and their reports against that state);
4. passes the baton when done, with `scripts/handoff`.

Read the queue when a session starts and after each handoff, and keep the watcher running in between (see "Watcher"). The owner's queue is the issues carrying `baton:owner`.

## Watcher

`python -I scripts/issue_watch.py --machine linux` (or `windows`, `owner`), started hidden in the background, wakes a coordinator when its baton lands. Once a minute it asks GitHub for open issues on `bubbleofmoon/H.E.R.` (`--repo` for another repository) carrying `baton:<machine>`, with a conditional request: when nothing changed GitHub answers 304, which costs nothing against the rate limit. It prints `BATON #<n> <title> <url>` and exits when the label lands on an issue, newly or again after leaving; comments, pushes and progress elsewhere never wake it. It exits after every event, so start it again after acting on the baton, recording its PID and creation time like every background process. It does not compute readiness or claims. Its state file is `~/.her-baton-watch-<machine>-<owner>-<repo>.json`, one per repository. `--once` polls once and prints the newly held issues or `no batons`. Exit 2 is an API failure or a malformed `--repo`: read the message before restarting.

## Shared ground rules

- Only coordinators merge, and only through a pull request whose CI is green on a head that contains the current `origin/main` (`issue-tracker.md` "Pull requests and merging").
- Production, its config, tokens, tasks and data are touched only from the Windows machine and only with the owner's go-ahead at the time. Production incident detail stays in the vault.
- Never write `C:/qqbot`, `C:/qqbot-rust`, `memory.db` or `~/.dsh` from either machine. Production is read-only for development and every production edit is the owner’s own action under the vault operating procedures (`docs/agents/production-ops.md`). Tests never contact real QQ, DSH, model or TTS services.
- Never `@`-mention in comments: that notifies real GitHub accounts.

# Baton and single-coordinator operation

Development runs on a single Windows Antigravity coordinator against `bubbleofmoon/H.E.R.` (`docs/agents/issue-tracker.md`). Multi-host Linux↔Windows baton ping-pong is retired.

## Owner batons (`baton:owner`)

- Add `baton:owner` when an open issue is blocked on an administrator decision (or production action).
- Post a concise scanned comment via `scripts/gh-public.ps1` stating the **question**, **options**, and **recommendation**.
- Once the owner answers, remove `baton:owner` and resume or close the issue.
- Open issues without `baton:owner` are either ready for the coordinator (all blockers closed) or blocked on another issue.

## Shared ground rules

- Merge only through a pull request with green `checks` CI on a head containing current `origin/main` (`docs/agents/issue-tracker.md` "Pull requests and merging").
- Production is read-only from development: never write `C:/qqbot` (until #67 also `C:/qqbot-rust`, `C:/her-orch/relay`, `~/.dsh`) or `memory.db`, and never contact real QQ, DSH, model or TTS services (`docs/agents/production-ops.md`).
- Never `@`-mention usernames in GitHub comments.

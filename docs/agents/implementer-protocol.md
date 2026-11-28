# Implementer protocol

Binding for Antigravity subagents working on a ticket. Host-specific build and test rules live in `docs/agents/rust-validation.md`.

## Context

Read, in order: `AGENTS.md`, the `AGENTS.md` nearest your code (such as `her/AGENTS.md`), `docs/agents/rust-validation.md`, any matching vault spec under `C:/her-vault/.scratch/<feature>/spec.md`, the GitHub issue and its comments (`gh issue view <n> --comments -R bubbleofmoon/H.E.R.`), `GLOSSARY.md`, and relevant ADRs under `docs/adr/`.

Read the source before making any claim about it.

## Public and vault

- Development is public on `bubbleofmoon/H.E.R.`; specs, raw evidence, chat/session samples, memory data and production incident detail stay in `C:/her-vault` (`bubbleofmoon/H.E.R.-vault`). Move nothing from the vault into this repository except owner-approved persona text (ADR 0002).
- Only the coordinator posts to GitHub, opens or merges pull requests, or reruns/cancels CI. All issue/PR/comment text goes through `scripts/gh-public.ps1` (`scan-public.py` + `gitleaks`). Name vault or evidence files by path only; never paste logs, chats, account-home paths, credentials or 7+ digit runs into public text.

## Workspace and checks

- Work only in the assigned worktree (`C:/her-wt/<ticket>`) on branch `her/<n>-<slug>` cut from `origin/main` (never commit to `main`, never force-push). Keep temporary scripts/files in `C:/her-orch/<ticket>/tmp`, never inside the checkout.
- No host needs a full local Rust build; hosted `checks.yml` compiles, lints, formats and runs Rust test shards. Run script tests (`scripts/tests/*.py`) locally; run native Windows Rust checks only when required for a Windows-native gate through `scripts/cargo-slot.ps1` into `C:/her-targets/<ticket>`.
- Never poll GitHub (`gh run watch`, `--watch`, loops or repeated `gh run view`) or run `scripts/ci-wait.py` from a worker.

## How to work

- **Scope:** deliver the ticket's acceptance criteria cleanly; record out-of-scope defects found along the way in your report with `file:line`.
- **Test-first (`tdd`):** write a failing test first, observe the failure for the right reason, then implement the fix (`docs/agents/rust-validation.md` / `CODING_STANDARDS.md`).
- **Failure modes (`CODING_STANDARDS.md`):** test backend, provider, CLI and QQ-send changes against refusal/empty answers, quota/429, 5xx, timeout and exit-0 error output; errors go to logs or owner alerts, never to group or non-owner chats.
- **Real solutions:** implement the full behavior for all valid inputs. Never skip/weaken tests, hardcode test cases or replace missing behavior with stubs.
- **Safety:** never contact real QQ, DSH, model or TTS services, and never write production roots (`C:/qqbot`, `C:/qqbot-rust`, `C:/her-orch/relay`, `~/.dsh`, `memory.db`).
- **Known pitfalls check:** confirm resource/process/temp cleanup on failure paths, boundary conditions on both sides of thresholds, resolved paths, no stale artifacts, `#[cfg(test)]` vs non-test visibility, all callers (including `#[cfg(windows)]`) updated, line endings/file modes preserved, and clean plain commits.
- **Pre-review self-check:** before reporting ready: (1) grep every sibling caller of any changed admission, token or mode guard; (2) re-capture a failing red receipt (`cargo-slot.ps1 -ExpectFail` or equivalent) whenever test fixtures or assertions changed; (3) run `npm --prefix her-ui run build` and persist `ui-build.log` under `C:/her-orch/<ticket>/evidence/` whenever `her-ui/` changed.

## Report

Every claim in a worker report must be backed by an exact command, exit code, log path and proof line (or `file:line` for code reads). Mark anything unrun as `not run` / `not verified` with the reason.

```text
Ticket: <NN>   Branch: <branch>   Base commit: <hash>   Changed files: <list>
AC1: met | partial | not met   evidence: <command> -> exit <code>, <log path>, "<proof line>"
Changed existing tests: <name: why justified> or none
Shared-file edits: <file: what> or none
Not verified / not run: <items> or none
Open or blocked: <items> or none
```

Keep `C:/her-orch/<ticket>/PROGRESS.md` updated during multi-step work so any resumed run picks up cleanly.

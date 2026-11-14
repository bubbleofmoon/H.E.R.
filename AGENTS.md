# HER

QQ bot bridge in Rust (`her/`, `her-memory/`) (ADR 0004). Terms: `GLOSSARY.md`. Specs and validation evidence live in the vault (`docs/agents/issue-tracker.md`).

Keep files used by the Rust runtime, build, tests, CI or agent workflow. Retain active Windows launchers until #12/#20 replace them and DSH support until #78 retires it. Node fixture tooling stays under `her/tests/fixtures/` and `scripts/`; memory data and `her-memory/` remain protected. Retention decisions: `docs/adr/0001-cleanup-retention.md`. `her-memory` probe diagnostics report metadata only; stored model text never enters its output.

## Public and vault

- This repository, `bubbleofmoon/H.E.R.`, is where development happens: code, docs, agent docs, CI, issues, batons and handoffs. Every ticket merges through a pull request gated by the hosted checks workflow (CI first: `docs/agents/issue-tracker.md` "Pull requests and merging").
- `bubbleofmoon/H.E.R.-vault`, checked out at `C:/her-vault` (Linux `/root/her-vault`), is the vault: specs, raw validation evidence, `.scratch/`, bot memory, chat and session samples, the patched copy of SnowLuma's non-commercial code and production incident detail stay there. Nothing moves from the vault into this repository, and this repository's history never carries vault material.
- A public ticket, comment or commit names vault files by path only. Post every issue, comment or pull-request body through `scripts/gh-public.ps1` / `scripts/gh-public.sh`: they accept only `gh issue|pr create|comment` on this repository with one body file, scan that file and any published title or label, and call `gh` only when all of it is clean, so an inline body, a stdin body, a second body file, an attachment or an editor flow never reaches GitHub. `.githooks/pre-push` runs the same scan over the tree and over every blob the push publishes, including a dirty other branch or an ancestor a later commit deletes.
- The scan refuses secrets, digit runs of 7 or more without a reviewed exception, account-home paths and lines that look like quoted chat or session records. It prints a finding's kind and position, never the matched text. Fix the text; do not widen a rule.
- Production is never a development target: never write `C:/qqbot`, `C:/qqbot-rust` or `~/.dsh`, and never contact real QQ, DSH, model or TTS services from development.

## DOX

- An AGENTS.md binds its subtree; read the ones on the path to what you edit. When a contract, workflow or preference changes, update the nearest doc in the same change and delete stale text. Document current contracts only, no history or decision dates (git has those).
- Every AGENTS.md has a sibling CLAUDE.md containing only `@AGENTS.md`.

## User Preferences

- Start every background process hidden; never open a foreground console.
- Write agent-facing docs (AGENTS.md, `docs/agents/`) in English.

## Subagent workflow

- Coordinator rules (sizing, the planner, review routing, the merge flow and the Windows coordinator's duties) are in `docs/agents/coordinator.md`; only coordinator sessions read it.
- Hard limits: workers commit to and push only their own ticket branch (`her/<n>-<slug>`, never `main`, never force) and may trigger and read the quick CI job for it; they never open, merge or edit pull requests, rerun or cancel CI, post to GitHub, hand off or touch build slot locks by hand; planners write only their plan file and their scratch export, and never build or run code; reviews never write; reviewers and planners run git only as plain read commands; gates never change product code or tests. The guard stops plain attempts and is not a sandbox: python is unrestricted, so these limits hold whether or not a call is blocked. Claude agents enforce this via `.claude/hooks/agent-guard.sh` (rules in `agent_guard.py`, tested by `scripts/tests/test_agent_guard.py`); Codex runs also never write git metadata or outside the worktree and build/cache/lock dirs, and never touch production, `C:/qqbot` or `C:/qqbot-rust`. Stop only processes you started, by recorded PID and creation time, never by name or pattern.
- Ticket material for a run (prompts, plans, reports, receipts, evidence, carry patches) lives under `/root/orch/her/<ticket>/` on Linux and `C:/her-orch/<ticket>/` on Windows, never in the checkout.
- A cut-off run continues as a fresh run reading the worktree, issue, newest handoff and validation notes.

## Agent skills

- Issues: GitHub Issues on the public repo `bubbleofmoon/H.E.R.` (issue `#N` = ticket `N`, labels `windows`/`linux`), see `docs/agents/issue-tracker.md` for claims, the comment rule and ready queries; who acts next: `docs/agents/baton.md` (a fresh session reads it first). Specs and validation evidence stay in the vault or under `C:/her-orch/<ticket>/`; GitHub owns ticket state and blockers.
- Domain docs: single-context, `GLOSSARY.md` + `docs/adr/`, see `docs/agents/domain.md`.
- Rust fixture timing and validation evidence: `docs/agents/rust-validation.md`.

## Lifecycle entry points

- For Windows launchers, desktop Run stores, manual GUI supervisor startup, process handling and task deployment, read `docs/agents/windows-lifecycle.md`.
- For supervisor ownership, desired state, dependency startup/adoption, pinned installs, approved upgrades and interrupted-transaction recovery, release checks/install and update policy, Rust backup jobs and restore, job dispatch or task storage, read `docs/agents/lifecycle-tasks.md`.

## Child DOX Index

- `her/AGENTS.md`: Crate layout and pointers to the bridge contracts.
- `docs/adr/0001-cleanup-retention.md`: Repository retention.
- `docs/adr/0002-public-repo-polyform-noncommercial.md`: Public development repo, vault and license.
- `docs/agents/baton.md`: Who acts next (baton labels, handoff comments); a fresh session starts here.
- `docs/agents/coordinator.md`: Coordinator workflow.
- `docs/agents/issue-tracker.md`: GitHub Issues, claims, the scanned-text rule (published text versus numeric relation and state operations), wayfinding operations and ready queries.
- `docs/agents/implementer-protocol.md`: Worker context, evidence and reports.
- `docs/agents/windows-lifecycle.md`: Windows launchers and scheduler fixtures.
- `docs/agents/rust-validation.md`: Build slots and validation evidence.
- `docs/agents/production-ops.md`: Public production rules; procedures live in the vault.
- `CODING_STANDARDS.md`: Review rules.
- `docs/agents/runtime-paths.md`: Runtime selection, guards and configuration.
- `docs/agents/social-state.md`: Social state and console authority.
- `docs/agents/sessions-recovery.md`: Inbound and turn durability.
- `docs/agents/harness.md`: Harness and conversation storage.
- `docs/agents/prompts-modes.md`: Prompts and participation modes.
- `docs/agents/mcp-memory.md`: MCP and memory access.
- `docs/agents/media-transport.md`: Media and voice delivery.
- `docs/agents/dependency-recovery.md`: DSH and dependency recovery and alerts.
- `docs/agents/lifecycle-tasks.md`: Supervisor lifecycle, health check, jobs and backups.
- `docs/agents/domain.md`: Glossary and ADR conventions.
- `docs/release-smoke.md`: Release window and tray checks.
- `CONTRIBUTING.md`: Setup, repository hooks, the public scan and license notices.

Index entries are short fixed phrases naming the topic; change one only when the file's topic changes, never to list new details (details belong in the file itself).

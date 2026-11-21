# HER

QQ bot bridge in Rust (`her/`, `her-memory/`) (ADR 0004). Terms: `GLOSSARY.md`. Specs and validation evidence live in the vault (`docs/agents/issue-tracker.md`).

Keep files used by the Rust runtime, build, tests, CI or agent workflow; DSH support stays until #78 retires it, and memory data is protected (`docs/adr/0001-cleanup-retention.md`).

## Public and vault

- This repository, `bubbleofmoon/H.E.R.`, is where development happens: code, docs, agent docs, CI, issues, batons and handoffs. Every ticket merges through a pull request gated by the hosted checks workflow (CI first: `docs/agents/issue-tracker.md` "Pull requests and merging").
- `bubbleofmoon/H.E.R.-vault`, checked out at `C:/her-vault` (Linux `/root/her-vault`), is the vault: specs, raw validation evidence, `.scratch/`, bot memory, chat and session samples, the patched copy of SnowLuma's non-commercial code and production incident detail stay there. No vault material moves into this repository or its history, except the owner-approved default persona text (ADR 0002). Coordinators commit to the vault with the repository-level git identity set in the vault checkout by the owner, on both hosts: user name `bubbleofmoon` and the owner's GitHub noreply address.
- A public ticket, comment or commit names vault files by path only. Post every issue, comment or pull-request body through `scripts/gh-public.sh` / `.ps1`, never a bare `gh` write (`docs/agents/issue-tracker.md`); `.githooks/pre-push` scans everything a push publishes.
- The scan refuses secrets, digit runs of 7 or more without a reviewed exception, account-home paths and lines that look like quoted chat or session records. It prints a finding's kind and position, never the matched text. Fix the text; do not widen a rule.
- Production is never a development target: never write the production root `C:/qqbot` (until the owner's move, #67, also `C:/qqbot-rust`, `C:/her-orch/relay` and `~/.dsh`), and never contact real QQ, DSH, model or TTS services from development. The one exception is the #67 move, which the Windows coordinator runs on the owner's relayed go (`docs/agents/production-ops.md`).

## DOX

- An AGENTS.md binds its subtree; read the ones on the path to what you edit. When a contract, workflow or preference changes, update the nearest doc in the same change and delete stale text. Document current contracts only, no history or decision dates (git has those).
- Every AGENTS.md has a sibling CLAUDE.md containing only `@AGENTS.md`.

## User Preferences

- Start every background process hidden and keep consoles closed; open a window only for an app the owner asked to open.
- Write agent-facing docs (AGENTS.md, `docs/agents/`) in English.

## Agent workflow

- Coordinator rules (model routing, two-axis review, merge flow and GitHub API budget) are in `docs/agents/coordinator.md`.
- Ticket work runs on `her/<n>-<slug>` branches cut from `origin/main` in `C:/her-wt/<ticket>` (never push directly to `main`, never force-push). Merge only through pull requests gated by hosted `checks.yml`.
- Never poll GitHub. Wait for CI only through `scripts/ci-wait.py` (`docs/agents/coordinator.md` "GitHub API budget").
- No host needs a local Rust build. Hosted CI compiles, lints, formats and tests Rust. Native checks a gate names follow `docs/agents/rust-validation.md`. Apply only ticket-authorized `fmt-patch` hunks for owned files and follow that doc for partial Clippy patches and CI proof. Local rustfmt and editor tooling are optional.
- A case called "bounded" or "unlikely" is a finding: fix it or name a follow-up.
- Stop only processes you started, by recorded PID and creation time, never by name or pattern. Never edit a script a run may be using: write a temporary file beside it and move it into place.
- Ticket run material (prompts, plans, reports, receipts, evidence, `PROGRESS.md`) lives under `C:/her-orch/<ticket>/`, never in the checkout.

## Agent skills

- Issues: GitHub Issues on the public repo `bubbleofmoon/H.E.R.` (issue `#N` = ticket `N`), see `docs/agents/issue-tracker.md` and `docs/agents/baton.md`. GitHub owns ticket state and blockers.
- Domain docs: single-context, `GLOSSARY.md` + `docs/adr/`, see `docs/agents/domain.md`.

## Child DOX Index

- `her/AGENTS.md`: Crate layout and pointers to the bridge contracts.
- `docs/adr/0001-cleanup-retention.md`: Repository retention.
- `docs/adr/0002-public-repo-polyform-noncommercial.md`: Public development repo, vault and license.
- `docs/agents/baton.md`: Single-coordinator operation, owner batons and shared ground rules.
- `docs/agents/coordinator.md`: Coordinator workflow, model selection and GitHub API budget.
- `docs/agents/issue-tracker.md`: GitHub Issues, claims, the scanned-text rule (published text versus numeric relation and state operations), wayfinding operations and ready queries.
- `docs/agents/implementer-protocol.md`: Subagent worker context, evidence and reports.
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

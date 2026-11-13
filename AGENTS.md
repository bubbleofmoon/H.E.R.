# HER

QQ bot bridge in Rust (`her/`, `her-memory/`) (ADR 0004). Terms: `GLOSSARY.md`. Specs and validation evidence live in the vault (`docs/agents/issue-tracker.md`).

Keep files used by the Rust runtime, build, tests, CI or agent workflow. Retain active Windows launchers until #12/#20 replace them and DSH support until #78 retires it. Node fixture tooling stays under `her/tests/fixtures/` and `scripts/`; memory data and `her-memory/` remain protected. Retention decisions: `docs/adr/0001-cleanup-retention.md`. `her-memory` probe diagnostics report metadata only; stored model text never enters its output.

## Public and vault

- This repository, `bubbleofmoon/H.E.R.`, is where development happens: code, docs, agent docs, CI, issues, batons and handoffs. Every merge runs the hosted checks workflow.
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

- The coordinator is the Linux main session (Opus) and never implements: it picks ready issues (open, unclaimed, its host label and baton, blockers closed), claims them (`docs/agents/issue-tracker.md`), sizes, investigates by reading, plans and routes them, relays plans, caps fix rounds, owns every owner question, checks reports by their receipts (exit files, test-result lines, diff), runs the merge gates, merges, pushes, keeps the ticket's handoff current and closes the issue after merge. Few status turns while waiting.
- Size every ticket before routing and record the size on the ticket. Decided: can the exact change be written now (if not, `?`)? Blast radius: files and call sites (grep the symbol), crates crossed. Loop length: a single Linux test target is short; database, migration, release/pack or Windows-native loops are long. Risk area: auth and tokens, keys and release signing, ACLs and isolation, deletes or moves of user data, migrations, production.

  | Size | Signals | Route |
  |---|---|---|
  | XS | decided, 1–2 files, one Linux test target | `ticket-implementer` on Haiku with an exact coordinator brief |
  | S | ≤ ~4 files in one crate, short loop, small design call | `ticket-implementer` on Sonnet |
  | M/L | several crates, long loop or real design | Opus plan (`Plan` agent, `model: opus`) → Haiku executes; a part the plan cannot make exact goes back to the planner, never to Sonnet |
  | ? | cause or approach unknown | investigate (Sonnet, Opus if hard), then size the resulting plan |

  Record every run with `scripts/agent_usage.py record --agent <id> --ticket <n> --size <size> --host <linux|windows>` (ledger `.scratch/her-supervisor/agent-usage.jsonl`, private run data that stays untracked and is never published); after merge compare the predicted size with the measured cost and adjust the borders. The baseline and the weekly verdict live in the vault (`C:/her-vault/.scratch/her-supervisor/agent-usage.md`).
- Planner (M/L only): reads the real code, returns a plan and stops (keep its context for revisions); it never builds or tests, and writes full code only where one line cannot carry the change. Judgement parts (races, ordering, design calls) are written as exact edits too, so Haiku executes the whole plan; Haiku stops and hands back where the plan does not hold, and the planner revises it. Plan format for any handoff: (1) root cause or goal with evidence (file:line, command output); (2) the exact change per file:line, as find → replace when mechanical, or exact steps plus the commit SHA; (3) the first test to write and see fail, then the rest.
- Review, never by the planner or implementer: Codex in every merge gate (`gpt-6.1-sol` at `high` via `scripts/codex-task.ps1` / `.sh`, hidden, never `gpt-6-astra`); add a `ticket-reviewer` on Fable for risk areas, on Opus for other S–M/L work. Reviews check the diff and that it follows the plan; verify each finding in the code before it becomes a required fix. Gates are `merge-gate`; reviews and gates apply `CODING_STANDARDS.md`.
- Flow: merge the integration tip into the ticket branch → execute → review → fix round if needed → gate → coordinator merges into `migration/js-rust-integration`. Gate the integration tip itself after several merges. Before fixing a failing test, search open issues and the other host's branches for its name.
- Hard limits: workers never commit, push or move the integration branch, never post to GitHub, never touch build-slot locks by hand; reviews never write; gates never change product code or tests. Claude agents enforce this via `.claude/hooks/agent-guard.sh`; Codex runs also never write git metadata or outside the worktree and build/cache/lock dirs, and never touch production, `C:/qqbot` or `C:/qqbot-rust`. Stop only processes you started, by recorded PID and creation time, never by name or pattern.
- A cut-off run continues as a fresh run reading the worktree, issue, newest handoff and validation notes.

Windows (32 GB, 4 cores / 8 threads, also runs production):

- Executes only items Linux hands with `baton:windows` in the plan format (an XS/S brief, an M/L plan, an investigation run, a native or production check, or a question), same rules otherwise: it runs the item as written (executor, investigation runs), commits the run's changes to the ticket branch, pushes the branch, and runs only the native Windows checks a gate asks for (Windows-only tests, junctions, ACLs, production hotfix gates); it hands back with `baton:linux` when done or when the plan does not hold (a `?` is resolved on Linux).
- One worker session at a time; each item gets a fresh coordinator session that starts from and ends with a handoff comment.
- Builds via `C:/her/scripts/cargo-slot.ps1 -WorkTree <worktree>` (the integration checkout's copy): two build slots, two test slots (`docs/agents/rust-validation.md`). pi (`scripts/pi-task.ps1`) is paused.
- Ticket material for a run (prompts, receipts, evidence, carry patches) lives under `C:/her-orch/<ticket>/`, never in the checkout.

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

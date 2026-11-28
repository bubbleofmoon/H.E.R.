# Coordinator

Binding for the single Windows Antigravity coordinator (`C:/her`, 32 GB, 4 cores / 8 threads, also hosts production). Core safety rules live in `AGENTS.md` and `docs/agents/production-ops.md`.

## Role and flow

1. Pick ready open issues (`docs/agents/issue-tracker.md`), claim `in-progress`, cut `her/<n>-<slug>` from `origin/main` in `C:/her-wt/<n>`.
2. Implement directly or via Antigravity subagents (`docs/agents/implementer-protocol.md`), test-first (`tdd` / `diagnosing-bugs`).
3. Gate every diff with two-axis `code-review`: Standards (`CODING_STANDARDS.md` via `research` subagent) + Spec/Rust-contract (`scripts/codex-task.ps1 -Sandbox read-only`, or a separate `research` subagent with `Model: "pro"` when Codex hits quota/unavailability).
4. For `her-ui/` edits, run `npm --prefix her-ui run typecheck`, `npm --prefix her-ui run build` (persist `ui-build.log`), inline-style `git grep`, `python scripts/check-repo.py --eol-base origin/main`, and `python scripts/scan-public.py --root .`.
5. Merge `origin/main` (`python -I scripts/merge-check.py` pre-commit), push cleanly, open PR via `scripts/gh-public.ps1` from the branch worktree (no `--base`/`--head`), wait via `scripts/ci-wait.py`, run required native/elevated checks via `scripts/cargo-slot.ps1`, merge, clean worktree and close issue.

## Model and review routing

- **Default coordinator & planner:** `Orchestrator (Neon)` (`Gemini 4 Neon` for planning/slicing; fast execution model for routine steps).
- **Tiered planning & adversarial checks:**
  - **Tier 1 (scoped):** Neon plans alone; Standards (`research`) + Spec (`codex-task.ps1 -Model gpt-6.2-sol -Effort high -Sandbox read-only`, else `research` `Model: "pro"`).
  - **Tier 2 (cross-cutting/multi-module):** Neon gathers read-only grep inventory and drafts plan; adversarial red-team pass with a fresh `research` (`Model: "pro"`) reviewer and `codex-task.ps1` (`gpt-6.2-sol`, CI/UI/rollback/scope), returning `file:line` deltas only.
  - **Tier 3 (greenfield/architecture forks):** blind planners (`gemini-4-neon-high`, independent subagent planners, and `codex-task.ps1` `gpt-6.2-sol` when quota allows) in parallel on identical prompt + inventory, writing separate files (`@coordinator DONE: <path>`), merged by Neon.
- **Deep single-model:** `Gemini 4 Neon (High/XHigh)` for deep backend bugs, UI architecture, or Rust concurrency/lifecycle.
- **Review cap:** verify findings in code before requiring fixes; file non-blocking medium/low items as follow-up issues (new tickets).

## Staged tracks (`implement-spec`)

- Staged multi-ticket track specs and the index live at `C:/her-vault/.scratch/tracks/INDEX.md` (`<NN>-<slug>/spec.md`).
- When picking up a track with `implement-spec`: cut integration branch `her/<track-slug>` from `origin/main` in `C:/her-wt/<track-slug>`, run per-ticket implementer subagents in sibling worktrees (`tdd`), merge into the integration branch, gate with two-axis `code-review`, open PR via `scripts/gh-public.ps1` (`Refs #...`), wait via `scripts/ci-wait.py`, merge on green `checks.yml`, clean worktrees, and close each resolved issue via `scripts/gh-public.ps1` + `gh issue close`.

## Native checks

Hosted CI runs Linux/Windows compile/lint/fmt/test shards. Run native checks locally only for Windows-specific gates (`HER-Elevated-Test`, scheduler/ACL, release proofs, production-move rehearsals) via `C:/her/scripts/cargo-slot.ps1 -WorkTree <worktree>` (`docs/agents/rust-validation.md`). Never create/delete slot locks manually.

## GitHub API budget

- Limits: 5,000 REST calls/h/account + GraphQL + secondary burst limits (403/429).
- Wait for CI only via background `scripts/ci-wait.py` from `C:/her` (never poll with `gh run watch`, `--watch`, loops, or repeated `gh run view`):
  - PR: `python -I scripts/ci-wait.py --workflow checks.yml --event pull_request --sha <head sha> --timeout 7200`
  - Dispatched or rerun: `python -I scripts/ci-wait.py [--workflow checks.yml] --label <n>-<run> --timeout 7200 --run <id> [--min-attempt <n>] [...]` (`--workflow` defaults to `quick.yml`, so pass `--workflow checks.yml` for `checks.yml` runs, and `--min-attempt` when waiting past an earlier finished attempt)
- `ci-wait.py` caches 1 run list/workflow in `%LOCALAPPDATA%\her-ci-wait` (~100s min refresh, conditional requests, backs off on low budget, stops on rate-limit reset). Exits: `0` green, `1` red, `3` cancelled/other, `2` usage, `4` timeout / `5` GitHub error (restart wait).
- Read `statusCheckRollup` once pre-merge (`issue-tracker.md`); prefer GraphQL/single list queries; `gh api rate_limit` is free; keep `ci-wait.py` intervals `>= 100s`.

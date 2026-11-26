# Coordinator

Binding for the single Windows Antigravity coordinator (`C:/her`, 32 GB, 4 cores / 8 threads, also hosts production). Core safety rules live in `AGENTS.md` and `docs/agents/production-ops.md`.

## Role and flow

1. Pick ready open issues (`docs/agents/issue-tracker.md`) whose blockers are closed, claim with `in-progress`, and cut `her/<n>-<slug>` from `origin/main` in `C:/her-wt/<n>`.
2. Implement directly or through Antigravity subagents (`docs/agents/implementer-protocol.md`), driving tests first (`tdd` / `diagnosing-bugs`).
3. Review every ticket diff with dual-axis `code-review` (Standards against `CODING_STANDARDS.md` via `research` subagent + Spec/Rust-contract review via `scripts/codex-task.ps1 -Sandbox read-only` or parallel Spec subagent).
4. For `her-ui/` changes, run local UI checks (`npm --prefix her-ui run typecheck`, `npm --prefix her-ui run build`, inline-style `git grep`) alongside `python scripts/check-repo.py` and `python scripts/scan-public.py --root .`.
5. Integrate current `origin/main` (run `python -I scripts/merge-check.py` before committing the merge), push with `HER_GITLEAKS=C:/her-orch/tools/gitleaks-8.30.0/gitleaks.exe`, open the PR via `scripts/gh-public.ps1`, wait for hosted checks with `scripts/ci-wait.py`, run required Windows native/elevated checks via `scripts/cargo-slot.ps1`, merge, clean up the worktree and close the issue.

## Model and review routing

- **Default coordinator:** `Orchestrator (Neon)` (plans on Gemini 4 Neon, fast execution).
- **Spec/contract second opinion:** `scripts/codex-task.ps1 -Model gpt-6.2-astra -Effort high -Sandbox read-only` for cross-file state/backend contract audits alongside Antigravity Standards review.
- **Deep single-model work:** use `Gemini 4 Neon (High/XHigh)` for deep backend bug audits, UI/navigation architecture, or subtle Rust concurrency/lifecycle fixes.
- **Review cap:** verify each finding in code before requiring fixes, and file non-blocking medium/low polish as follow-up issues (`#362` or dedicated follow-up tickets).

## Native checks

- Hosted CI compiles, lints, formats and runs Linux/Windows test shards. Run native checks locally only for Windows-specific gates (elevated tests via `HER-Elevated-Test`, scheduler/ACL checks, release proofs, production-move rehearsals) using `C:/her/scripts/cargo-slot.ps1 -WorkTree <worktree>` (`docs/agents/rust-validation.md`). Never create or delete slot locks by hand.

## GitHub API budget

- REST limit is 5,000 calls/hour per account across all tools, plus GraphQL limits and secondary burst limits (403/429).
- Wait for CI only through `scripts/ci-wait.py` run in the background from `C:/her` (never poll with `gh run watch`, `--watch`, loops or repeated `gh run view`):
  - Pull request checks: `python -I scripts/ci-wait.py --workflow checks.yml --event pull_request --sha <head sha> --timeout 7200`
  - Dispatched runs: `python -I scripts/ci-wait.py --label <n>-<run> --timeout 7200 --run <id> [...]`
- `ci-wait.py` caches one run list per workflow in `%LOCALAPPDATA%\her-ci-wait` (~100 s minimum refresh, conditional requests, slows under low budget, stops on rate-limit reset). Exits: `0` green, `1` red, `3` cancelled/other, `2` usage, `4` timeout / `5` GitHub error (neither is a result — restart wait).
- Read `statusCheckRollup` once before merging (`issue-tracker.md`). Prefer GraphQL or single list queries, check `gh api rate_limit` before bursts (does not spend quota), and keep `ci-wait.py` intervals at least 100 s.

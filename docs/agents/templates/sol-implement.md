# Sol implement brief

Reach for this to start `codex-work implement` for an XS/S ticket or one step of an M/L plan. Fill the angle-bracket fields, delete the lines that do not apply and save the prompt as `/root/orch/her/<n>/prompts/<name>.md` (`docs/agents/coordinator.md` "Codex runs").

```
Implement ticket <n>, "<title>", in <worktree> on branch <her/n-slug>. Base: <sha of origin/main>.
Follow docs/agents/implementer-protocol.md. Commit only the files the ticket needs, leave the worktree clean, push with `git push origin <branch>` (no `-u`). Push only your ticket branch, never main, never force, never `--no-verify`; the pre-push hook runs. Put the commit ids in the final message.
Task: <plan path and the step to run, or the exact change and its acceptance criteria, one per line>.
Reuse: <the crate or source the change reuses, or none>.
Red receipts: <each new Rust test, its target and expected assertion or panic, and the one-line change that turns it red (docs/agents/templates/red-revert.md); script tests run locally and show their red run>.
CI after this step: `check` for every Rust edit, separate `lint`, and one green batch with `unit` and every affected `her/tests/*.rs` target per OS. Trace shared code and fixture changes to their callers. The coordinator dispatches. Run no Cargo or CI.
When a red receipt is required, commit the real change and then the red mutations. Push both commits together and report both SHAs.
Final message: the protocol's report layout; list the CI targets above under "Not verified / not run".
```

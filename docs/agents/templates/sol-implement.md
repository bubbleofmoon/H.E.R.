# Sol implement brief

Reach for this to start `codex-work implement` for a ticket (its own short plan in `PROGRESS.md`, then the change), the next step of a ticket larger than one run, or one step of a risk-area plan. Fill the angle-bracket fields, delete the lines that do not apply and save the prompt as `/root/orch/her/<n>/prompts/<name>.md` (`docs/agents/coordinator.md` "Codex runs").

```
Implement ticket <n>, "<title>", in <worktree> on branch <her/n-slug>. Base: <sha of origin/main>.
Follow docs/agents/implementer-protocol.md. Commit only the files the ticket needs, leave the worktree clean, push with `git push origin <branch>` (no `-u`). Push only your ticket branch, never main, never force, never `--no-verify`; the pre-push hook runs. Put the commit ids in the final message.
Task: <the ticket's acceptance criteria and, for a later step, the step recorded in PROGRESS.md; for a risk-area plan, the plan path and the step>. Write the short plan in PROGRESS.md first; if the ticket is larger than one run, implement its first step now and record the rest; stop after the plan only for an owner decision, and name it.
Branch state: <the coordinator has merged the current origin/main into the branch, or merge it first>. A red receipt built on a tree that did not compile is recreated on the fixed tree.
Reuse: <the crate or source the change reuses, or none>.
Keep code used outside tests available outside `#[cfg(test)]`. For each changed signature, identify and check every caller, including tests and `#[cfg(windows)]` code. Name every existing test whose expected value or matched page text changes.
Red receipts: <each new Rust test, its target and expected assertion or panic, and the one-line change that turns it red (docs/agents/templates/red-revert.md); script tests run locally and show their red run>.
CI after this step: `check` for every Rust edit, separate `lint`, and one green batch with `unit` and every affected `her/tests/*.rs` target per OS. Trace shared code and fixture changes to their callers. The coordinator dispatches. Run no Cargo or CI.
When a red receipt is required, commit the real change and then the red mutations. Push both commits together and report both SHAs.
Final message: the protocol's report layout; list the CI targets above under "Not verified / not run".
```

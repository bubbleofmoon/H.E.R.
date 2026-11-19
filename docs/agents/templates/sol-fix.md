# Sol fix brief

Reach for this to start `codex-work implement` for a fix round after a review or a red CI run. Fill the angle-bracket fields and save the prompt as `/root/orch/her/<n>/prompts/<name>.md` (`docs/agents/coordinator.md` "Codex runs").

```
Fix attempt <k> for ticket <n> in <worktree> on branch <her/n-slug>. Head: <sha>.
Follow docs/agents/implementer-protocol.md. Commit only the files the fix needs, leave the worktree clean, push with `git push origin <branch>` (no `-u`). Push only your ticket branch, never main, never force, never `--no-verify`; the pre-push hook runs. Put the commit ids in the final message.
Findings: <report path, or the failed job names with their test-result and panic lines>.
Fix each REQUIRED finding after verifying it in the code; answer a finding you reject with file:line evidence. Change nothing else.
Red receipts: <each test the fix adds or changes, its target and expected assertion or panic, and the change that turns it red>.
CI after this step: `check` for every Rust edit, separate `lint`, and one green batch with `unit` and every affected `her/tests/*.rs` target per OS. Trace shared code and fixture changes to their callers. The coordinator dispatches. Run no Cargo or CI.
When a red receipt is required, push the real and red commits together and report both SHAs.
Final message: the protocol's report layout, one line per finding (fixed, rejected with evidence).
```

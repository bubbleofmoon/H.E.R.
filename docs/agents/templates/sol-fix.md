# Sol fix brief

Reach for this to start `codex-work implement` for a fix round after a review or a red CI run. Fill the angle-bracket fields and save the prompt as `/root/orch/her/<n>/prompts/<name>.md` (`docs/agents/coordinator.md` "Codex runs").

```
Fix round <k> of 3 for ticket <n> in <worktree> on branch <her/n-slug>. Head: <sha>.
Follow docs/agents/implementer-protocol.md. Leave every change uncommitted; the wrapper commits and pushes.
Findings: <report path, or the failed job names with their test-result and panic lines>.
Fix each REQUIRED finding after verifying it in the code; answer a finding you reject with file:line evidence. Change nothing else.
Red receipts: <each test the fix adds or changes and the change that turns it red>.
CI the coordinator will dispatch after this run: <targets>. Run no cargo and dispatch no CI.
Final message: the protocol's report layout, one line per finding (fixed, rejected with evidence).
```

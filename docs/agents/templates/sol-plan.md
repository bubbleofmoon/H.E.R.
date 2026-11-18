# Sol plan brief

Reach for this to start `codex-work plan` (M/L tickets) or to send a plan revision. Fill the angle-bracket fields, delete the lines that do not apply and save the prompt as `/root/orch/her/<n>/prompts/<name>.md` (`docs/agents/coordinator.md` "Codex runs").

```
Plan ticket <n>, "<title>", for an executor that follows the plan exactly. Base: <sha of origin/main>.
Read: docs/agents/coordinator.md "Planner" (plan format, known pitfalls, build-file rules), issue <n> with its comments, <spec and validation paths>.
Scratch export: /root/orch/her/<n>/scratch (a copy of the base, no .git): write every code edit and new test there.
Write the plan to /root/orch/her/<n>/plan-out/plan.md; the wrapper installs it.
Size and executor runs: <XS|S|M|L>; split the work into runs of one coherent step each.
Reuse: name the crate or source each step reuses (CONTRIBUTING.md "Reusing code").
Red receipts: for each new Rust test, name the one-line change that turns it red and the failure it shows (docs/agents/templates/red-revert.md).
CI the coordinator will dispatch after each run: <check, lint, unit, targets>.
Risk area: <none | the "Threats" section is required: failure cases for quota, large or old data, restart midway, concurrency, abusable inputs, the upgrade path>.
Revision of the previous plan: <the plan's path and the findings to fix, or none>.
Final message: at most ten lines (steps, size, open questions, scratch files changed).
```

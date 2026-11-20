# Sol plan brief

Reach for this to start `codex-work plan` (M/L tickets) or to send a plan revision. Fill the angle-bracket fields, delete the lines that do not apply and save the prompt as `/root/orch/her/<n>/prompts/<name>.md` (`docs/agents/coordinator.md` "Codex runs").

For a standalone Linux launcher brief, use `/inputs/issue.md` and name any other staged inputs. Use `/repo` for history and `/scratch` and `/plan-out` for edits and plan output.

```
Plan ticket <n>, "<title>", for an executor that follows the plan exactly. Base: <sha of origin/main>.
Read: docs/agents/coordinator.md "Planner" (plan format, known pitfalls, build-file rules), issue <n> with its comments, <spec and validation paths>.
Scratch export: /root/orch/her/<n>/scratch (a copy of the base, no .git): write every code edit and new test there.
Write the plan to /root/orch/her/<n>/plan-out/plan.md; the wrapper installs it.
Size and executor runs: <XS|S|M|L>; split the work into runs of one coherent step each.
Reuse: name the crate or source each step reuses (CONTRIBUTING.md "Reusing code").
Red receipts: for each new Rust test, name the target, one-line change that turns it red, and expected assertion or panic (docs/agents/templates/red-revert.md).
CI after this step: `check` for every Rust edit, separate `lint`, and one green batch with `unit` and every affected `her/tests/*.rs` target per OS. Trace shared code and fixture changes to their callers.
Callers and tests: name every caller of a changed signature (including tests and `#[cfg(windows)]` code) and every existing test whose expected value or matched page text changes. Keep any code used outside tests available outside `#[cfg(test)]`.
Plan inputs: record the base and files relied on. Check the scratch export's line endings and file modes. For each JSON boundary change name the producer, consumer and exact expected shape, including missing and null fields where applicable.
Risk area: <none | the "Threats" section is required: failure cases for quota, large or old data, restart midway, concurrency, abusable inputs, the upgrade path>.
Revision of the previous plan: <the plan's path and the findings to fix, or none>.
Final message: at most ten lines (steps, size, open questions, scratch files changed).
```

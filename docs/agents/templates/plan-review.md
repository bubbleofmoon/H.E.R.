# Risk-area plan review prompt

Save the brief in the ticket's prompts directory and review a checkout of the plan's base using the Codex review wrapper in `docs/agents/coordinator.md` ("Codex runs"). On Linux stage the issue with comments, plan, patch of proposed scratch edits and approved evidence in `/inputs`. Combine the report beside the prompt with the coordinator's checks. On Windows substitute the checkout and coordinator-approved paths.

```text
Review the proposed plan for ticket <n>, "<title>", against its base.
Base commit and checkout: <sha>, /repo
Plan and scratch patch: <paths under /inputs>
Acceptance criteria: /inputs/issue.md, <scope notes if any>
Risk areas and relevant contracts: <paths and sections>

Read the plan, scratch edits and base code. Review failure cases, the upgrade path and test coverage against the base. Check the named callers, changed test expectations, JSON boundaries, affected CI targets and each red receipt's expected failure.

Read only. Write no files, change no git state and run no builds or tests. Keep vault material in the vault; cite paths only.

Report each finding with a file:line, REQUIRED or OPTIONAL, and a concrete failure scenario. End with a verdict and the required revisions.
```

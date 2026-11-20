# Risk-area plan review prompt

Save the filled prompt in the ticket's prompts directory. Run `scripts/codex-task.sh <name> <worktree-or-scratch> <prompt> gpt-6.2-astra high read-only`. Read `<name>.report.md` beside the prompt and combine its findings with the coordinator's checks into one revision brief.

For a standalone Linux launcher brief, name `/repo` as the checkout and include the issue with comments, plan and approved evidence among the `/inputs` files.

```text
Review the proposed plan for ticket <n>, "<title>", against its base.
Base commit and checkout: <sha and path>
Plan and scratch export: <paths>
Acceptance criteria: <one per line>
Risk areas and relevant contracts: <paths and sections>

Read the plan, scratch edits and base code. Review failure cases, the upgrade path and test coverage against the base. Check the named callers, changed test expectations, JSON boundaries, affected CI targets and each red receipt's expected failure.

Read only. Write no files, change no git state and run no builds or tests. Keep vault material in the vault; cite paths only.

Report each finding with a file:line, REQUIRED or OPTIONAL, and a concrete failure scenario. End with a verdict and the required revisions.
```

Head: <head sha>
Base: <sha of origin/main>
Round: <n> of 3

Review ticket <ticket number>, "<title>", in <worktree path> on branch <branch>. Read only: write no file and run no git command that changes anything.

Previous report: <path of the last round's report, or none>
Diff: `git diff <base>...<head>`
Acceptance criteria: <the ticket's list, one per line>
Plan: <path of the plan the change follows, or none>

Check, in this order:
1. Spec: each acceptance criterion is met by the diff, at the scope the ticket intends.
2. Correctness: defects in the changed code, including the failure modes `CODING_STANDARDS.md` names.
3. Tests: each new test fails without its fix, for the reason the ticket names.
4. Round 2 or 3: each REQUIRED finding of the previous report is fixed or answered, and the fix introduced no new defect.

Report REQUIRED findings only. Each has file:line and a concrete failure scenario (input, then wrong result), and you verified it in the code first. Leave out style, preferences and suggestions.

End with one last line: PASS (no REQUIRED finding) or FIXES REQUIRED.

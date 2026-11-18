# Gate prompt template

Reach for this for each round of a merge gate, whether Codex or a `ticket-reviewer` runs it. Round 1 runs at effort `high` over the whole ticket diff; rounds 2 and 3 run at `medium` over the fix only. Fill the angle-bracket fields, keep the lines for the round and delete the other round's, and save the prompt as the gate's prompt file.

```
Head: <head sha>
Base: <sha of origin/main>
Round: <n> of 3

Review ticket <ticket number>, "<title>", in <worktree path> on branch <branch>. Read only: write no file and run no git command that changes anything.

Acceptance criteria: <the ticket's list, one per line>
Plan: <path of the plan the change follows, or none>
Previous report: <path of the last round's report, or none>

Scope (round 1): `git diff <base>...<head>`, the whole ticket.
Scope (rounds 2 and 3): `git diff <previous head>..<head>` and the code it touches, plus the previous report's findings. Leave the rest of the ticket diff alone.

Check (round 1):
1. Spec: each acceptance criterion is met by the diff, at the scope the ticket intends.
2. Correctness: defects in the changed code, including the failure modes `CODING_STANDARDS.md` names.
3. Tests: each new test fails without its fix, for the reason the ticket names.

Check (rounds 2 and 3):
1. Each REQUIRED finding of the previous report is fixed or answered.
2. The fix introduced no defect (correctness as in round 1) and keeps every acceptance criterion met.
3. Each test the fix adds or changes fails without its change.

Report REQUIRED findings only. Each has file:line and a concrete failure scenario (input, then wrong result), and you verified it in the code first. Leave out style, preferences and suggestions.

End with one last line: PASS (no REQUIRED finding) or FIXES REQUIRED.
```

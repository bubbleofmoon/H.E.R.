# Code review prompt

The first round covers the full ticket diff and goes to the Claude reviewer. Follow-up rounds cover the fix diff and unresolved required findings. Save the brief and start the reviewer described in `docs/agents/coordinator.md` ("Review and gates"). The report lands beside the prompt.

On Linux use `/repo` for the committed checkout and `/inputs` for the issue, plan, prior report and other staged inputs. On Windows substitute the checkout and coordinator-approved paths.

```
Head: <head sha>
Base: <sha of origin/main>
Round: <n> of 3

Review ticket <ticket number>, "<title>", in /repo on branch <branch>. Read only: write no file and run no git command that changes anything.

Acceptance criteria: /inputs/issue.md, <scope notes if any>
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

Protected paths: <the `protected` paths of the Codex run's receipt, or none>; review each in full.

Report every finding with file:line, a severity (REQUIRED or OPTIONAL) and a concrete failure scenario (input, then wrong result), verified in the code first. Be concise.

End with the verdict, which depends only on the REQUIRED findings: mergeable (none), mergeable after listed fixes or not mergeable, then the numbered required fixes.
```

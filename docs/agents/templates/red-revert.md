# Red-revert template

Reach for this when a plan needs each new test's red receipt on CI (`CODING_STANDARDS.md` "Tests"): the real change goes in first, a red commit on top breaks it with named one-line mutations, and the green commit reverts exactly those mutations. The planner fills the angle-bracket fields.

```
Real change: <commit sha, or "the commit before the red commit">   Target: <quick.yml target>   OS: <linux|windows>

Mutations (red commit, one line each; the mutated line ends with the comment `RED-<id>`)
| id | file:line | find -> replace (one line) | test it must break |
| M1 | <path:line> | <original line> -> <mutated line> | <test name> |

Expected failures of the red run
| test | failure it must show (assertion or panic text) |
| <test name> | <text> |

Steps
1. Make the real change with its tests and commit it. Done when its sha is in the header above.
2. Apply every mutation and make the red commit. Done when `grep -rn 'RED-' <changed files>` lists exactly the mutated lines and each matches its row.
3. Push and dispatch `<target>` on the red commit (the coordinator dispatches `-f sha=<red commit>` once later commits exist). Done when the failing tests are exactly those in the table with the listed messages: no other failure, no compile error, no test that still passes.
4. Make the green commit: restore each find text. Done when `grep -rn 'RED-' <changed files>` prints nothing and `git diff <real change sha> HEAD` prints nothing.
5. Push and dispatch `<target>` again. Done when it passes; hand back both run ids with the matched failures.
```

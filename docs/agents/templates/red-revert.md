# Red-revert template

Reach for this when a plan needs each new test's red receipt on CI (`CODING_STANDARDS.md` "Tests"): the real change goes in first, a red commit on top breaks it with named one-line mutations, and the coordinator restores the exact real tree in a green commit. The plan run fills the angle-bracket fields.

```
Real change: <commit sha, or "the commit before the red commit">   Targets: <comma-separated quick.yml targets>   OS: <linux|windows>

Mutations (red commit, one line each; the mutated line ends with the comment `RED-<id>`)
| id | file:line | find -> replace (one line) | test it must break |
|---|---|---|---|
| M1 | <path:line> | <original line> -> <mutated line> | <test name> |

Expected failures of the red run
| target | test | failure it must show (assertion or panic text) |
|---|---|---|
| <target> | <test name> | <text> |

Steps
Worker
1. Make the real change with its tests and commit it. Record the real SHA.
2. Apply the mutations and make the red commit. Check that `grep -rn 'RED-' <changed files>` lists exactly the named mutations. Record the red SHA. Push the real and red commits together and hand back both SHAs.

Coordinator
3. Dispatch one red batch per OS on the ticket ref with `-f sha=<red-sha>`. In the clean ticket worktree at the red SHA, restore with `git checkout <real-sha> -- .`, commit, and require `git diff --exit-code <real-sha> HEAD` to pass.
4. Push through the hooks and start green CI while red CI runs. Include every affected target in the green test batch and dispatch `check` and `lint` separately.
5. Record both run links, the exact red failures and green passes before accepting the step. Each named test must fail at its named assertion or panic. A fixture or compile error cannot establish red. Record any extra cross-effect failures and confirm those tests pass on the green tree.
```

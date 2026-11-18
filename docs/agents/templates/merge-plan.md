# Merge plan template

Reach for this when the coordinator merges `origin/main` into a ticket branch and a conflict is semantic (both sides changed the same behaviour, not just adjacent lines). The planner fills the angle-bracket fields, writes each resolved file to `scratch/merge/<path>` in the ticket's scratch export, and saves the plan with the steps below.

```
Merge: <branch> at <ours sha> <- origin/main at <theirs sha>   Merge base: <base sha>
Conflicted files: <path>, <path>

## <path>
- Ours changed: <what, file:line>
- Theirs changed: <what, file:line>
- Resolution: <how both behaviours survive, and why>
- Resolved file: scratch/merge/<path>          (or) Exact edits: <find -> replace, one per hunk>
- Tests: <test name, from ours | theirs>, ...   (every name from both sides, once)
- Docs: <sentence added or changed by ours | theirs>, ...   (every one, once)

Steps (run in <worktree>)
1. The coordinator runs `git merge --no-commit --no-ff origin/main`. Done when `git diff --name-only --diff-filter=U` prints exactly the conflicted files above.
2. Copy each `scratch/merge/<path>` over `<path>`. Done when `cmp scratch/merge/<path> <path>` exits 0 for each.
3. Apply the exact edits. Done when each find text is gone and each replace text is present once.
4. Docs files both sides edited keep every sentence either side added or changed, once, combined sentence by sentence. Done when each sentence listed under Docs appears exactly once.
5. Done when `grep -rn '^<<<<<<<\|^>>>>>>>' <conflicted files>` prints nothing and `python3 -I scripts/merge-check.py` exits 0 (test-count union and brace balance for every Rust file both sides changed).
6. `git add` the conflicted files and make one plain `git commit` (the merge commit). Done when `git rev-list --parents -n 1 HEAD` prints three SHAs: the merge, ours and theirs.
7. Push, then dispatch `check` and the test targets the merged files affect (`implementer-protocol.md` "Test loop"). Done when every run id is handed back.

Known pitfalls: <ticket-specific list>
```

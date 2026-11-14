# Contributing to H.E.R.

Read AGENTS.md and the relevant docs/agents contract before changing code.
Enable the repository hooks once per clone:

```sh
git config core.hooksPath .githooks
python -I scripts/check-repo.py
python -I scripts/scan-public.py
```

`scripts/scan-public.py` is the one public guard: it checks secrets, digit runs of 7
or more, account-home paths (Windows, `/c/` and macOS spellings, any casing) and quoted
chat or session records over the whole tree, over a text on stdin (`--stdin`) and over the
tracked blobs of the revisions a push publishes (`--revs-from-stdin`) and the commit and tag
messages of the objects it publishes (`--objects-from-stdin`). `.githooks/pre-push` runs the
tree scan, then the blob scan and the message scan for every ref being pushed, so a clean
checkout does not excuse a dirty branch, tag or ancestor, and a clean tree does not excuse a
dirty commit or annotated-tag message. Every issue, comment or pull-request body
goes through `scripts/gh-public.ps1` or `scripts/gh-public.sh`, which scan the body file and
any published title or label and call `gh` only when all of it is clean; they accept only
`gh issue|pr create|comment` on the public repository with one real body-file option, and
refuse an inline body, a stdin body, a second body file, `-F`/`-f` fields, attachments,
`--web`, `--editor`, `--fill` and any other command. The arguments are parsed once and an
option keeps its own value, so a title that looks like a flag (`--title --body-file=notes.md`)
is title text and never the required body source; `gh` then receives arguments rebuilt from
what was parsed and scanned, so it publishes exactly that text and is never left prompting
for a body. A title or label is scanned whole, including every line of a multiline value;
the PowerShell wrapper refuses a multiline title or label instead, because Windows PowerShell
5.1 cannot forward one intact. A refusal prints a fixed kind and the argument position,
never a supplied value.

Install Rust stable, Python 3, Node.js 24 with its bundled npm and gitleaks 8.30.0. Node supports the synthetic test fixtures. See docs/agents/rust-validation.md for builds
and the complete unit/integration suite; use isolated fixtures and no real services.
The checks workflow defines Windows and Linux validation and runs on hosted runners
for every pull request and every push to main; it is the merge gate for everything it runs,
and local gates keep only the native, elevated and production checks the hosted runners cannot run.
On the shared development host, use the cargo-slot scripts for every Rust command.

The guard tests itself: `python -I scripts/tests/test_scan_public.py`,
`python -I scripts/tests/test_pre_push_hook.py` and `python -I scripts/tests/test_gh_public.py`, `python -I scripts/tests/test_check_repo.py`
run against synthetic samples and a stub `gh`, and need gitleaks on `PATH` or `HER_GITLEAKS`.

Fixtures use short synthetic account identifiers. Numeric scan exceptions must
name a specific file/value and explain the non-account meaning. Secret scan
exceptions must be limited to a documented public protocol constant.

This project is source-available under PolyForm Noncommercial 1.0.0. Preserve
LICENSE and the Required Notice in NOTICE when redistributing. Publishing a
repository, release or production change requires the administrator's go-ahead.

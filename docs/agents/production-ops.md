# Production operations

Public rules only. Live layout, log/report formats, console/exe-swap procedures, incidents and owner edits stay in vault `bubbleofmoon/H.E.R.-vault` (`C:/her-vault/docs/agents/production-ops.md`). Public text names vault paths only.

## Public rules

- **Read-only boundary:** production root is `C:/qqbot` (plus archived pre-move folders until #127). Never write them or `memory.db`, change scheduled tasks/services/tokens/releases/configs there, or stop/start/replace production processes from development.
- **Database safety:** read production DBs from file metadata or an owner-approved copy only — never open live DBs in place (`mode=ro` creates/leaves `-wal`/`-shm`; `immutable=1` reuses existing ones).
- **Owner authority:** every production action requires the owner's go at execution time (`docs/agents/baton.md`); agents prepare commands/checks only (`C:/her-orch/ops/stage-release.ps1` stages local `main-<sha>` bundles and cutover scripts per vault `docs/agents/production-ops.md`).
- **Test isolation:** tests use throwaway runtimes (`HER_INSTALL_ROOT`) and fake gateways/agent hosts/model servers; never contact real QQ, DSH, model or TTS services.
- **Evidence hygiene:** diagnosis notes, `her-cli report` output, log excerpts and receipts go to `C:/her-vault/.scratch/<feature>/evidence/<NN>/` or `C:/her-orch/<ticket>/evidence/`. Public text uses `group:<n>` / `private:<n>` placeholders and never quotes logs, chats, session IDs, tokens or account numbers (`scripts/gh-public` enforces).
- **Production move (`#67`, `scripts/production-move.ps1 -Production`):** admin PowerShell under owner account: `-Mode Inventory` (non-interactive read-only; blocked checks report `skipped: needs <check>`), `-Mode Move` (`-DigestModel <id>` sets digest model), `-Mode Rollback -Production -StateFile <evidence>\state.json`, `-Mode OperatorFolders` (copies ops scripts to `C:\qqbot\ops`).
  - Direct owner go: Windows coordinator may run `Inventory` anytime; production `Move`/`Rollback`/`OperatorFolders`, release installs, and lifecycle changes require the owner's direct go in the active Windows session.
  - Development tests only with `-Rehearsal` via `scripts/lifecycle/test-production-move.ps1` under a temp root (`state.json` goes to vault; console/`inventory.json` mask profile as `<profile>`; production rollback paths must remain usable outside account profile).

## Crate contracts

`her-cli report` reads selected production diagnostic files without config loading, service calls, locks or writes. Preserve explicit runtime selection, install-root guard, shared captured-length streaming reads, and metadata-only text/JSON output. Collector timing carries bridge turn and submitted-attempt identities; summaries require a final-attempt match. Formats and window semantics live in the vault document.

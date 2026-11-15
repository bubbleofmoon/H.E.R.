# Production operations

Public rules only. Production operating procedures are not published here: they live in the
vault, repository `bubbleofmoon/H.E.R.-vault`, file `docs/agents/production-ops.md`, checked
out at `C:/her-vault`. That document holds the live layout, the log files and report
formats, the console and exe-swap procedures, and every incident and owner-edit record. A
public issue, comment or commit names those vault paths and nothing else.

## Public rules

- Production lives in one root, `C:/qqbot` (the install root, `docs/agents/runtime-paths.md`);
  until the owner's move (#67) brings them inside, `C:/qqbot-rust`, the relay folder
  `C:/her-orch/relay` and `~/.dsh` are production too. Production is read-only for
  development: never write any of them or `memory.db`; never change a scheduled task, a
  service, a token, a release directory or a configuration file there. Development work
  never stops, starts or replaces a production process.
- A production database is read from file metadata only, or from a copy taken with the
  owner's go; it is never opened in place, in any mode. `mode=ro` on a WAL database creates
  `-wal` and `-shm` beside it and leaves them after close, and `immutable=1` reuses existing
  ones, so no open mode is safe against a live database.
- Every production action is the owner's, taken with the owner's go at the time it happens
  (`docs/agents/baton.md`). An agent prepares the command and the check, and reports what it
  would do; it never runs it. A go relayed through a handoff does not change this: a
  production-action handoff carries the exact command and check for the owner to run. The
  one exception is the #67 move below.
- Tests never touch production. They run against throwaway runtimes and fake QQ gateways,
  agent hosts and model servers, and never contact real QQ, DSH, model or TTS services.
  `HER_INSTALL_ROOT` points a test at a throwaway installation, never at a live one.
- Production incident detail stays in the vault: diagnosis notes, `her-cli report` output,
  log excerpts and receipts go to `C:/her-vault/.scratch/<feature>/evidence/<NN>/` or
  `C:/her-orch/<ticket>/evidence/`. A public text names those files by path, writes
  conversation keys as `group:<n>` or `private:<n>` placeholders, and never quotes a log
  line, a conversation, a session id, a token or an account number. `scripts/gh-public`
  refuses log-, chat- and account-shaped text; a refusal means the material belongs in the
  vault, not that the rule needs widening.
- The `her-cli report` contract that product code must keep is under Crate contracts below; its
  file list, window semantics and format details are in the vault document.
- The move into the install root (#67) is `scripts/production-move.ps1` with `-Production`,
  from an administrator PowerShell under the owner's account: `-Mode Inventory` (read-only
  checks) first, then `-Mode Move` (`-DigestModel <id>` also points the memory digest at the
  relay), and `-Mode Rollback -Production -StateFile <evidence>\state.json` returns to the
  untouched originals. `-Mode OperatorFolders` copies the ops scripts into `C:\qqbot\ops`
  afterwards. For this move only, the Windows coordinator runs the script for the owner:
  Inventory at any time, non-interactively, and Move, OperatorFolders and a manual Rollback
  only on the owner's go. A go counts only in one form: the owner gives it in the Linux
  coordinator's session, and the Linux coordinator relays it in a handoff that quotes the
  owner's words and names the release. The Windows coordinator then writes a one-time go
  file directly in `C:\her-orch\67\go\`, holding the single line `MOVE <release>`,
  `ROLLBACK <release>` or `COPY`, at most 2 hours before the run, and passes it as
  `-OwnerGoFile`; the script reads and deletes it before the first change, so it serves one
  run only. The same go covers the move's post-move checks and the persona import and
  assignment through the console. The script's own automatic rollback needs no go. Without
  `-OwnerGoFile`, Move, Rollback and OperatorFolders need an interactive session and the
  typed word (`MOVE`, `ROLLBACK`, `COPY`). Every other production action stays the owner's.
  Development runs the script only with `-Rehearsal`, through
  `scripts/test-production-move.ps1` under a temporary root. Its evidence folder holds
  production detail (`state.json` keeps real paths for the rollback) and goes to the vault;
  the console and `inventory.json` show the profile root as `<profile>`. In production the
  evidence folder may not lie under the account profile, so printed rollback commands keep
  usable paths.

## Crate contracts

`her-cli report` reads selected production diagnostic files without config loading, service calls, locks or writes. Preserve its explicit runtime selection, install-root guard, shared captured-length streaming reads and metadata-only output in both text and JSON. Collector timing carries bridge turn and submitted-attempt identities; summaries require a final-attempt match. Formats, window semantics and source availability: the vault document named above (public rules only here).

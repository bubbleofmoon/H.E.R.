# Production operations

Public rules only. Production operating procedures are not published here: they live in the
vault, repository `bubbleofmoon/H.E.R.-vault`, file `docs/agents/production-ops.md`, checked
out at `C:/her-vault`. That document holds the live layout, the log files and report
formats, the console and exe-swap procedures, and every incident and owner-edit record. A
public issue, comment or commit names those vault paths and nothing else.

## Public rules

- Production is read-only for development. Never write `C:/qqbot`, `C:/qqbot-rust`,
  `memory.db` or `~/.dsh`; never change a scheduled task, a service, a token, a release
  directory or a configuration file there. Development work never stops, starts or replaces
  a production process.
- Every production action is the owner's, taken with the owner's go at the time it happens
  (`docs/agents/baton.md`). An agent prepares the command and the check, and reports what it
  would do; it never runs it.
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

## Crate contracts

`her-cli report` reads selected production diagnostic files without config loading, service calls, locks or writes. Preserve its explicit runtime selection, install-root guard, shared captured-length streaming reads and metadata-only output in both text and JSON. Collector timing carries bridge turn and submitted-attempt identities; summaries require a final-attempt match. Formats, window semantics and source availability: the vault document named above (public rules only here).

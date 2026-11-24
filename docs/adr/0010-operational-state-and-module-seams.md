# SQLite-first operational state and dedicated module seams

Status: accepted

New companion features store operational state in SQLite (WAL, `synchronous=FULL`) rather than introducing new JSON/JSONL files and cross-process lockfiles, and once DSH is retired (ADR 0006), durable turn lifecycle state consolidates into `data/conversations.db`. Shared cross-cutting primitives (`guarded_fs`, `social_state`, `console_auth`, `config_transaction`, `safe_http`, `loopback_http`) own their seams below the HTTP console and dependency supervision modules rather than routing non-HTTP runtime callers through `console` or upward into supervisor modules.

## Considered Options

- **Big-bang migration of every existing JSON store to SQLite upfront:** rejected. Keeping `social-v2.json` hidden behind a typed `social_state` module and `data/settings.db` v3 intact on disk delivers the caller decoupling and single-seam locality now with zero data or backup migration risk, leaving any future backing-store swap internal.
- **Keep adding per-feature JSON files and lockfiles:** rejected. Multi-file state splits require cross-file crash forensics, Windows sharing-violation retry matrices, and duplicated lock-ordering conventions across callers.

## Consequences

- `console` and `console_access` become thin HTTP adapters over `social_state`, `console_auth` and `config_transaction`; non-HTTP modules stop calling into `console::*`.
- Protected file and Windows DACL primitives live in `guarded_fs` with no upward dependency on relay/DSH supervision or `console_auth`.
- Deepening tickets replace obsolete hook-coupled unit tests with interface-level tests and delete retired `#[cfg(test)]` hooks.

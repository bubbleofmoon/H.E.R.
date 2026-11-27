# Keep only what the rewrite needs or production runs

Status: accepted

## Context

The repository keeps only what the Rust bridge, its build, tests, CI and agent workflow use.

## Decision

- Something stays only if the JS→Rust rewrite still needs it (its changes are not already on the integration branch and have not been superseded) or the running production setup uses it. Everything else is deleted outright, with no archive copy.
- Migration evidence remains in the private archive only when another maintained document cites it; the public tree contains no migration evidence.
- Unmerged work that is still needed survives as a branch ref only; it gets no worktree and is merged later under the normal validation rules.

## Consequences

- Production entry points, production data and backups are never touched by cleanup.

## Retention

Keep files used by the Rust runtime, build, tests, CI or agent workflow. The `her-memory/` library and benchmark runner follow the same source retention rules as the bridge. Memory databases, production data and the production install stay protected.

Active launchers stay until #12/#20 replace them, and DSH support stays until #13 retires it (ADR 0006).

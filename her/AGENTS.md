# HER

HER bridge and desktop app (binary-only crate). `her` uses the Windows GUI subsystem; `her-cli` uses the console subsystem. Both include `src/entry.rs`; no arguments select the WebView2 console window and tray, and `supervise` requires an explicit runtime root. `herupdate` is a short-lived GUI-subsystem helper with its own entry point, installed at the fixed install-root path. Development app launches require an explicit runtime; installed app launches select the install root's runtime and register desktop startup in the selected Run store. Package all three images together: launchers and desktop health checks require the console twin for CLI capture and the GUI image for resident startup. Unit tests use `--bin her`, integration tests use `--test <target>`.

Read the topic doc before changing its implementation:

- Runtime selection, install-root and development guards, configuration, containment, ACLs, process boundaries and fixture sockets: `docs/agents/runtime-paths.md`.
- Console authorization, desktop sign-in, social state, quotas, send order, audit and feedback: `docs/agents/social-state.md`.
- Desktop window, tray and Windows launchers: `docs/agents/windows-lifecycle.md`.
- Inbound journaling, owner-private gap/interrupted-event notices, delivery confirmation, queued-crash recovery, unknown sends, opening/recovery notice claims, sessions, turn recovery, completion and cancellation arbitration, and usage: `docs/agents/sessions-recovery.md`.
- Harness admission, streamed tool execution, handler settlement after cancellation, uncertain-effect pauses, model image content, engine selection, canonical conversation storage, reset and administrator resolution: `docs/agents/harness.md`.
- Roles, prompt delivery, participation modes, interactions, slang learning and readiness probes: `docs/agents/prompts-modes.md`.
- MCP protocol, QQ send confirmation, auxiliary servers, generated DSH config and memory permissions: `docs/agents/mcp-memory.md`.
- Media acquisition, normalization, stickers, required voice duration validation (SILK uses a 100 ms per-packet upper bound) and delivery: `docs/agents/media-transport.md`.
- DSH credentials, cancellable RPC, retry/failover, relay health and alerts: `docs/agents/dependency-recovery.md`.
- Releases and installs, dependency startup, ownership, recovery and stop; supervisor lifecycle, health checks, scheduled tasks, digest, backups, the Schedule store and page and task storage: `docs/agents/lifecycle-tasks.md`.
- Build slots, isolated targets, fixture clocks, independent QQ login retry-spacing assertions, pending-interaction publication waits and verification evidence: `docs/agents/rust-validation.md`.
- Public production rules and the `her-cli report` contract (procedures live in the vault): `docs/agents/production-ops.md`.
- Signed release dependency notices: `THIRD_PARTY_NOTICES.md`.

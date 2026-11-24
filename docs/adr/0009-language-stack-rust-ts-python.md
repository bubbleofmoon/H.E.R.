# Rust core, TypeScript console UI, and Python for local AI and evals

Status: accepted

HER keeps three languages, each confined to one role: Rust for the core bridge, supervisor, harness, memory store and Windows desktop app (`her/`, `her-memory/`); TypeScript, React and Tailwind for the embedded console UI (`her-ui/`, ADR 0008); and Python for local Rig AI workers (STT, TTS, embeddings) behind loopback HTTP and offline evaluation/tooling scripts.

## Considered Options

- **Port the core to Go:** rejected. On a greenfield start Go would give faster local builds and simpler stdlib networking, but rewriting a hardened 170k-line Rust codebase would forfeit months of companion work, re-buy solved Windows/outbox edge cases, lose Rust's compile-time enum exhaustiveness and RAII lock/permit safety in fail-closed state machines, and trade bundled `rusqlite` and `wry`/`tao`/`tray-icon` for rougher Windows SQLite/GUI ergonomics.
- **Move the core to Python:** rejected. Packaging a signed, self-updating, hidden-startup Windows daemon and CLI in Python adds runtime bloat, startup latency and dynamic-typing risk in rarely hit crash-recovery paths; Python stays at the Rig AI and eval boundary where it excels.

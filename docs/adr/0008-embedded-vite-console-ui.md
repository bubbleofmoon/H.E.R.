# Embedded Vite, React and Tailwind web console

Status: accepted

HER serves its redesigned web console as a static single-page application built with Vite, React, TypeScript and Tailwind CSS from `her-ui/` and embedded into the `her` binary at compile time via `her/build.rs`. The runtime binary has no Node.js dependency, serves the app under a strict Content-Security-Policy (`script-src 'self'`, `style-src 'self'`) through `console_front`, and no longer reads a runtime HTML file from disk.

Compiled build output (`her-ui/dist/`) is gitignored rather than committed: CI workflows and release builds run `npm ci && npm run build` in `her-ui/` before invoking Cargo, and set `HER_REQUIRE_UI_DIST=1` so release and UI-test builds fail if the frontend bundle is absent. Pure backend `cargo test` runs without a frontend build fall back to a minimal embedded placeholder so Rust iteration stays fast and independent of Node.js.

## Considered Options

- **Zero-build vanilla HTML, CSS and JS:** Rejected after prototyping. At 138+ console functions across 5 workspaces, typed components, shared settings-registry editors, sticky dirty-state bars and a Chinese/pinyin `Ctrl+K` command palette become fragile and slow to maintain without a component build step, and Node.js is already installed on both Linux and Windows CI runners.
- **Commit `her-ui/dist/` into Git:** Rejected. Minified bundles bloat diffs, risk source-versus-bundle drift, and conflict with the public repository scan (`non-text` and digit-run checks).
- **Invoke `npm run build` inside `her/build.rs`:** Rejected. Shelling out to Node during Cargo builds slows Rust-only edits and breaks offline or Rust-only test runs.
- **Rust-rendered UI (WASM or server-side HTML templates):** Rejected. Slow compile loops for visual layout work with no UX advantage inside the local browser and WebView2 console.

## Consequences

- Release builds, UI-test builds and hosted CI run a frontend build step in `her-ui/` ahead of Cargo (`HER_REQUIRE_UI_DIST=1`).
- Frontend dependencies must use permissive licenses compatible with ADR 0002, be recorded in `THIRD_PARTY_NOTICES.md`, and work without inline scripts or styles under the strict CSP.
- The console UI test harness (`#316`) runs against the embedded Vite build rather than a zero-build HTML shell.

# Console UI (`her-ui`)

Vite, React, TypeScript and Tailwind CSS v4 SPA embedded into `her` via `her/build.rs` (`her-ui/dist/`).

## Contracts

- **Strict CSP (`console_front.rs`):** Zero inline `style=` or `style={{...}}` attributes in components or `index.html`. Use CSS classes and tokens in `src/index.css`.
- **Desktop WebView safety (`her/src/desktop.rs`):** New-window requests (`target="_blank"`) are denied and navigation is restricted to the desktop loopback host. Show copyable canonical browser URLs (`canonicalOrigin`) rather than opening canonical setup links inside the WebView.
- **Terminology (`GLOSSARY.md`):** Use `管理员密码`, `控制台令牌` and `浏览器会话`; avoid non-glossary variants and never add raw token-paste login forms.
- **Cross-mount state:** Shared settings baselines, write fences and in-flight save locks across pages/tabs or remounts live in `src/api/` or `src/stores/`, not component-local refs.
- **Local checks:** Before pushing UI changes, run:
  - `npm --prefix her-ui run typecheck`
  - `npm --prefix her-ui run build`
  - `git grep -n -E 'style=\{' -- her-ui/src` (expect 0 matches)
  - `python scripts/check-repo.py`
  - `python scripts/scan-public.py --root .`

## Layout

- `src/api/`: Fetch wrappers, `ApiRequestError`, auth helpers and shared write coordination.
- `src/stores/`: React Context store (`consoleStore.tsx`) for shell auth, live polling, navigation, drawer and modal/toast state.
- `src/layouts/` and `src/components/`: Navigation shell, primitives, inspectors and shared widgets.
- `src/pages/`: Route views and tab components.
- `src/utils/`: Formatting helpers.

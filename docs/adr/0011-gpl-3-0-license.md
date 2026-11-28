# Adopt GNU General Public License v3.0 (GPL-3.0-only)

Status: accepted

Amends the license decision in [ADR 0002](0002-public-repo-polyform-noncommercial.md). The public-vs-vault split (`bubbleofmoon/H.E.R.` vs `bubbleofmoon/H.E.R.-vault`), the public-scan policy (`scripts/scan-public.py`), and `NOTICE` attribution remain in force.

## Decision

License `bubbleofmoon/H.E.R.` (`her`, `her-memory`, `her-ui`, and supporting scripts and docs) under `GPL-3.0-only`:

- `LICENSE` carries the canonical GNU GPL v3.0 text.
- `her/Cargo.toml`, `her-memory/Cargo.toml`, and `her-ui/package.json` declare `GPL-3.0-only`.
- `NOTICE` carries copyright attribution and points to `LICENSE` and `her/THIRD_PARTY_NOTICES.md`.

## Consequences

- Redistribution and derivative works must remain under GPL-3.0 and preserve copyright and license notices.
- Under GPL-3.0 terms, commercial use is permitted as long as reciprocal copyleft obligations are met.
- Existing permissively licensed dependencies in `her/THIRD_PARTY_NOTICES.md` (MIT, Apache-2.0, BSD, ISC, Zlib, CC0) remain compatible with GPL-3.0-only; non-commercial, proprietary, or unlicensed external code (including upstream SnowLuma source and native addons) still cannot be copied or ported into this repository.

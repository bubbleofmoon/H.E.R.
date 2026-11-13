# HER

HER is a QQ companion. She lives in QQ private chats and groups through a OneBot gateway ([SnowLuma](https://github.com/SnowLuma/SnowLuma)), talks through an agent host (DeepSeek Harness, being replaced by HER's own harness), and is run by one supervisor with a local admin console.

> **Status: in development.** The Rust rewrite and the supervisor are being finished ahead of the production switchover. Memory, persona and schedule upgrades (phase 1a) are specified but not built. Nothing here is a finished release.

## What she does

- **Chats like a group member, not a bot.** Participation modes decide when she reads, waits, replies or stays quiet; she can lurk, catch up late and start topics.
- **Remembers.** A local SQLite memory with full-text search, access scopes per conversation, a nightly digest and backups.
- **Runs herself.** One supervisor starts and watches the bridge and its dependencies (SnowLuma, the agent host, an optional model relay) at pinned versions, restarts them with backoff, alerts the administrator privately, and updates by trial with automatic rollback.
- **Is administered from a console** (desktop window and tray on Windows): participation modes, approvals, tokens and costs, dependency versions and updates, backups and restore.

## Features

### Chatting and participation
- Three ways to run a conversation: **normal chat** (answers allowed messages in order), the **admin agent** (the administrator's private assistant, which relays the model's questions and approval requests and expires unanswered ones) and **simulation** (she reads, speaks, waits and chooses when to wake while messages keep arriving).
- Allow and deny lists for groups and private chats (deny wins); a silent mode that answers only the administrator; a console pause that keeps storing messages but stops her acting.
- One session and one active turn per conversation; mode, role or access changes retire outdated work before it runs.
- Durable inbound journal: duplicates are dropped, unsubmitted work is recovered after a crash, and work whose outcome is unknown pauses for the administrator instead of being replayed blindly (with optional automatic abandonment).
- Outages of the agent host queue a bounded amount of work per conversation and release it after recovery.

### Waking up
- Triggers: every private message, @mention or quote, her nickname or group card, configured keywords, chosen members, pokes, a random chance per message, and an active mode that reads everything.
- She sets her own sleep and wake conditions; finite sleep wakes at its deadline, infinite sleep keeps only the chosen triggers. Wake batching, per-minute and per-hour wake quotas, a reply check after she speaks, and randomised proactive openings in idle conversations.
- Safety rails: a hard turn cap, bounded reminders when a turn ends without a closeout, and repair of wake settings that would leave her asleep forever. The administrator can wake a conversation from the console.

### Replying and sending
- Text, quote replies, @mentions, multi-bubble bursts with automatic or fixed gaps, and pokes; per-bubble length, items-per-request and per-minute/hour send limits, with permissions rechecked before every bubble.
- A typing delay proportional to length, and waits that let a burst of messages finish before she answers.
- A durable send outbox: every bubble's outcome is recorded, partial sends are reported, and a send whose delivery is unknown is never resent automatically.
- An optional outbound guard that blocks path- and credential-like text; warnings for awkward splits and too many bubbles.

### Stickers, images and voice
- Incoming images, QQ faces, stickers, video, files, voice, cards and forwarded records are described to the model; images reach vision models (animations sampled to a few frames), with size, pixel and count budgets and private-network URLs refused.
- Forwarded records can be opened, including nested previews, once they were seen in the conversation.
- Favourite stickers sync from QQ into a searchable local library with her own notes and tags; she can look at, send and collect stickers (collecting has its own rate limit).
- Voice (off by default): she sends prepared recordings from a library, with length, size and rate limits, optional loudness adjustment and delivery checks; the administrator can send recordings from the console.

### Memory
- Long-term memories (facts, topics, thoughts, impressions, events, preferences) in SQLite with Chinese and English full-text search, deduplication, a topic index and access rules for who may read what across conversations; the model and the administrator can forget or archive rows.
- Lightweight per-conversation memory of active topics, pending thoughts and impressions of members, shown in each briefing.
- Group slang learning: candidates are collected and researched, and only meanings the administrator confirms reach the prompt.
- A literal blacklist removes chosen text from everything the model sees while storage keeps the original.
- Nightly and pressure-triggered consolidation writes durable memories before old sessions are archived.

### Persona and prompts
- Role cards per conversation, group, default or globally, edited in the console, with sections for each mode.
- A saved background for each conversation; one full briefing per session generation, then short updates with only what changed.
- Model and effort selection per conversation with fallback candidates; agent-host preset editing with backups.

### Administrator commands and alerts
- Private commands: reset or start a new session, status, switch or clear the role, silent/active, abandon or retry a paused turn, and approve a HER update or a dependency upgrade.
- Ordinary members' management commands are refused before reaching the model.
- Private alerts for model and backend failures, dependency outages and recoveries, lost QQ login, interrupted turns, inbound gaps, failed or missed backups, task drift and update rollbacks, each deduplicated and rate-limited.

### Console
- A local web console with token or desktop sign-in: status and sessions, conversation state, prompts and backgrounds, simulation settings, sending, the send outbox, media and stickers, voice, memory, slang, roles, models and presets, access and security, an audit log with undo, paused turns and inbound gaps, tool logs and feedback, token and cost charts, tasks and run history, releases, dependencies, and backups and restore.
- On Windows, a desktop window and tray icon that start with the user's logon.

### Supervisor, updates and backups
- One supervisor keeps HER running, stopped or in maintenance as asked, watches its own heartbeat and runs scheduled tasks with history and cancellation (missed slots are not replayed).
- It starts, adopts and heals SnowLuma, an optional model relay and the agent host in order, at pinned versions in private folders, with backoff and verified restarts; dependencies survive an ordinary HER restart.
- Updates: HER checks its release feed every six hours (off, notify or auto); a release is verified, installed beside the current one, trialled and rolled back automatically if the health check fails, and automatic updates wait until she is idle. Dependency upgrades are discovered too but always need the administrator's approval.
- Verified backups of memory, conversation history, configuration and state, kept for seven days by default; restore shows a preview first and rolls back if HER does not come back healthy.
- Read-only reports of turn timing, pauses, failures and token use; a full health check that covers every dependency and the QQ login.

### Harness (in progress)
- HER's own agent loop against any OpenAI-compatible streaming endpoint, chosen per conversation instead of the agent host: stored conversation history, HER's tools, step, token and time limits, and crash recovery that never repeats a send. Simulation mode, summaries and full failover are still being built.

### Tools the model can use
- QQ: status, groups and members, history, unread and recent messages with read receipts, message details, active members, sending (text, replies, bursts, pokes, stickers, voice), waiting for messages, wake settings, images, forwarded records, stickers and their notes, lightweight memory, slang, feedback, and her own portrait.
- Long-term memory: write, search, index, forget and access rules. Administrator only: read and change selected settings.
- Web search and fetching public pages, and a read-only gateway status check.

## Planned

Human reply timing enforced in code (held sends and a typing indicator), a guard against repeated sends, real QQ faces in replies, quieter wake rules, four-layer memory with per-person impression cards and hybrid recall, a persona library with a global rule set, one schedule page for every timed item, signed releases, and the switch of simulation mode to HER's own harness.

## Layout

| Path | What it is |
| --- | --- |
| `her/` | The Rust bridge, supervisor and desktop app: `her` (GUI), `her-cli` (console twin) and `herupdate` (update helper) |
| `her-memory/` | Memory tooling |
| `scripts/` | Build-slot, validation and fixture helpers |
| `docs/` | Agent-facing contracts (`docs/agents/`), decisions (`docs/adr/`) and release checks |
| `GLOSSARY.md` | The project's vocabulary |
| `AGENTS.md`, `CODING_STANDARDS.md` | Rules for anyone (human or agent) changing the code |

## Building and testing

Stable Rust; Windows (MSVC) is the production target, and Linux runs the same test suite. Builds go through the build slots described in [docs/agents/rust-validation.md](docs/agents/rust-validation.md). In short, from `her/`:

```
cargo test --bin her <filter>            # unit tests
cargo test --test <integration-target>   # integration tests
```

Tests use throwaway runtimes and fake QQ gateways, agent hosts and model servers. They never contact real QQ accounts, models, TTS services or a real installation.

## Installing and updating

An installation lives under one install root (default `C:\HER`) with side-by-side releases, a current and a last-good release, and a runtime folder for configuration and data. The update policy is off, notify or auto; updates install beside the running release, are trialled and roll back automatically if the health check fails. Dependency upgrades always need the administrator's approval. Details: [docs/agents/lifecycle-tasks.md](docs/agents/lifecycle-tasks.md) and [docs/agents/windows-lifecycle.md](docs/agents/windows-lifecycle.md).

## What this repository is, and what stays in the vault

This is where HER development happens: code, tests, docs, agent rules, CI, issues and
handoffs all live here, and the merge check is the hosted workflow in
[`.github/workflows/checks.yml`](.github/workflows/checks.yml).

One private companion repository, the vault (`bubbleofmoon/H.E.R.-vault`), keeps what must
not be published: the specifications that quote production detail, raw validation evidence
and logs, chat and session samples, bot memory, configuration and tokens, the patched copy
of SnowLuma's non-commercial code, and production incident records. Real account and group
numbers live only there; every number in this repository's fixtures is invented.

A public ticket names vault files by path and quotes none of their content. Specs and
evidence for a ticket are named as `C:/her-vault/.scratch/<feature>/...` or
`C:/her-orch/<ticket>/evidence/...`. Nothing moves from the vault into this repository, and
this repository's history never carries vault material: `scripts/scan-public.py` is the guard
that enforces it, run over the tree by CI and `.githooks/pre-push`, over every commit a push
publishes, and over every issue or pull-request body by `scripts/gh-public`.

Machine-specific operator scripts are not published either: they stay with the host that runs
them, and the public rule they follow is in [docs/agents/production-ops.md](docs/agents/production-ops.md).

## License

[PolyForm Noncommercial 1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0) (decision: [ADR 0002](docs/adr/0002-public-repo-polyform-noncommercial.md)). Source-available, not open source: commercial use is not allowed, and redistribution must keep the license and the notices.

Third-party notices: [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

# Briefing once per session generation; wakes carry deltas; big sessions rotate before their cache expires

A reserved2 wake used to resend the conversation's full notes every time (~22k characters against well under 1k of new messages). Every copy stayed in the agent session's history, sessions grew to 500-700k tokens per model call, each step took 10-14 s, and the bot echoed the repeated rules in chat. We now give the **briefing** (role, behaviour and sending rules, stickers, memory items, participation context) in full once per **session generation**, which begins at session start, after a **session rotation**, or after a completed compaction, and otherwise send a **wake delta**: why it woke, the new messages, current state, and replacements for briefing parts that changed. A session at or over its cap (500k tokens by default, per model) is rotated while the bot sleeps, one minute before its provider's measured **cache life** runs out (Antigravity/Gemini through the relay: 5 minutes): HER runs the memory digest while the cache is still warm, archives the session, and the next wake starts fresh. DSH's own compaction stays only as a mid-turn safety net above the cap. The same rules bind the DSH route and our own harness (ADR 0006).

## Considered options

- **Keep resending the full notes each wake**: always fresh, but it is what made sessions huge and the bot repetitive.
- **Full briefing every N wakes as a refresher**: guards against drift, but brings the repetition back.
- **Compact at idle (digest, then a summary)**: keeps a summary of old turns, but costs a second whole-session call; HER already keeps the bot's short-term state (topics, thoughts, impressions, wake config) outside the agent session and puts it in every briefing, so a summary adds little.
- **Rotate whenever a sleep will outlast the cache, at any size**: cheapest calls, but drops agent-side history for small sessions too; the owner chose to keep sessions under the cap.

## Consequences

- HER must know which generation a session is in (actual DSH session, agent epoch, briefing revision, rotations and compaction events), not a "first wake sent" flag claimed before delivery.
- Cache life is a per-provider setting measured from the token log (cached share of the next call against the gap); a provider change needs a new measurement.
- A briefing change is delivered as an explicit replacement or removal, so a rule can change without waiting for a rotation.
- Older chat nuance survives only through memory and HER's own short-term state, so the memory digest runs before every rotation.

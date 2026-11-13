#!/usr/bin/env python3
"""Token usage and API-equivalent cost of Claude Code subagent runs.

Reads the subagent transcripts Claude Code writes under
~/.claude/projects/<project>/<session>/subagents/agent-<id>.jsonl (+ .meta.json).

  python3 -I scripts/agent_usage.py runs   [--since 2026-11-12] [--agent <id>]
  python3 -I scripts/agent_usage.py summary [--since ...] [--until ...] [--by role|model|ticket]
  python3 -I scripts/agent_usage.py runs --main --dir ~/.claude/projects/<project>   (coordinator sessions)
  python3 -I scripts/agent_usage.py record --agent <id> --ticket 150 --size M --role exec [--host linux] [--outcome merged]

`record` appends one line to the ledger (.scratch/her-supervisor/agent-usage.jsonl),
which is how the sizing rule's predicted size meets the measured cost. Windows runs
the same script on its own transcripts and records with --host windows.
Costs are API list-price estimates (USD), not the subscription bill.
"""
import argparse, glob, json, os, re, sys
from datetime import datetime, timezone

# $/MTok: input, output, cache read. Cache writes: 1.25x input (5m), 2x input (1h).
PRICES = {
    "opus-5-5": (4.0, 20.0, 0.20), "opus-5": (5.0, 25.0, 0.50), "opus-4": (5.0, 25.0, 0.50),
    "sonnet-5-5": (2.0, 10.0, 0.10), "sonnet-5": (2.0, 10.0, 0.20), "sonnet-4": (3.0, 15.0, 0.30),
    "haiku-5-5": (0.10, 0.50, 0.01), "haiku-4": (1.0, 5.0, 0.10),
    "fable-5-2": (8.0, 40.0, 0.20), "fable-5": (10.0, 50.0, 1.00),
}
LEDGER = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".scratch", "her-supervisor", "agent-usage.jsonl")


def price(model):
    m = (model or "").replace("claude-", "")
    for key in sorted(PRICES, key=len, reverse=True):
        if m.startswith(key):
            return PRICES[key]
    return None


def default_dirs():
    return glob.glob(os.path.expanduser("~/.claude/projects/*/*/subagents"))


def load_agent(path):
    meta = {}
    mp = path[:-len(".jsonl")] + ".meta.json"
    if os.path.exists(mp):
        try:
            meta = json.load(open(mp, encoding="utf-8"))
        except ValueError:
            pass
    last = {}  # requestId -> (model, usage); streamed messages repeat usage, keep the last
    first_ts = last_ts = None
    prompt = ""
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            try:
                d = json.loads(line)
            except ValueError:
                continue
            ts = d.get("timestamp")
            if ts:
                first_ts = first_ts or ts
                last_ts = ts
            msg = d.get("message") or {}
            if not prompt and d.get("type") == "user" and isinstance(msg.get("content"), str):
                prompt = msg["content"]
            if d.get("type") == "assistant" and msg.get("usage"):
                last[d.get("requestId") or id(d)] = (msg.get("model"), msg["usage"])
    tok = {"input": 0, "output": 0, "cache_read": 0, "cache_write_5m": 0, "cache_write_1h": 0}
    cost, models = 0.0, set()
    for model, u in last.values():
        models.add(model)
        cc = u.get("cache_creation") or {}
        w1h = cc.get("ephemeral_1h_input_tokens", 0) or 0
        w5m = cc.get("ephemeral_5m_input_tokens", (u.get("cache_creation_input_tokens", 0) or 0) - w1h) or 0
        t = {"input": u.get("input_tokens", 0) or 0, "output": u.get("output_tokens", 0) or 0,
             "cache_read": u.get("cache_read_input_tokens", 0) or 0, "cache_write_5m": w5m, "cache_write_1h": w1h}
        for k in tok:
            tok[k] += t[k]
        p = price(model)
        if p:
            i, o, r = p
            cost += (t["input"] * i + t["output"] * o + t["cache_read"] * r + w5m * i * 1.25 + w1h * i * 2) / 1e6
    minutes = None
    if first_ts and last_ts:
        f = datetime.fromisoformat(first_ts.replace("Z", "+00:00"))
        l = datetime.fromisoformat(last_ts.replace("Z", "+00:00"))
        minutes = round((l - f).total_seconds() / 60, 1)
    desc = meta.get("description") or prompt.strip().splitlines()[0][:80] if (meta.get("description") or prompt.strip()) else ""
    m = re.search(r"#(\d+)|[Tt]icket[ -](\d+)", (meta.get("description") or "") + " " + prompt[:400])
    return {
        "agent": re.sub(r"^agent-|\.jsonl$", "", os.path.basename(path)),
        "type": meta.get("agentType", "?"), "description": desc,
        "model": ",".join(sorted(x.replace("claude-", "") for x in models if x)) or meta.get("model", "?"),
        "effort": meta.get("effort"), "start": first_ts, "minutes": minutes,
        "ticket": (m.group(1) or m.group(2)) if m else None,
        "tokens": tok, "total_tokens": sum(tok.values()), "cost_usd": round(cost, 4),
    }


def all_agents(dirs, since=None, until=None, agent=None, main=False):
    out = []
    for d in dirs:
        pattern = "*.jsonl" if main else "agent-*.jsonl"
        for p in glob.glob(os.path.join(d, pattern)):
            if agent and agent not in os.path.basename(p):
                continue
            r = load_agent(p)
            if not r["start"]:
                continue
            if since and r["start"] < since:
                continue
            if until and r["start"] >= until:
                continue
            out.append(r)
    return sorted(out, key=lambda r: r["start"])


def role(r):
    t, d = r["type"], (r["description"] or "").lower()
    if t in ("ticket-reviewer",) or "review" in d:
        return "review"
    if t == "merge-gate" or "gate" in d:
        return "gate"
    if t == "Plan" or "plan" in d:
        return "plan"
    if "investigat" in d or t == "Explore":
        return "investigate"
    if t.startswith("ticket-implementer") or "implement" in d or "fix" in d:
        return "implement"
    return "other"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["runs", "summary", "record"])
    ap.add_argument("--dir", action="append")
    ap.add_argument("--since"); ap.add_argument("--until"); ap.add_argument("--agent")
    ap.add_argument("--by", default="role", choices=["role", "model", "ticket"])
    ap.add_argument("--ticket"); ap.add_argument("--size"); ap.add_argument("--role")
    ap.add_argument("--host", default="linux"); ap.add_argument("--outcome")
    ap.add_argument("--main", action="store_true", help="coordinator sessions (~/.claude/projects/<project>/*.jsonl) instead of subagents")
    a = ap.parse_args()
    dirs = a.dir or (glob.glob(os.path.expanduser("~/.claude/projects/*")) if a.main else default_dirs())
    rows = all_agents(dirs, a.since, a.until, a.agent, a.main)
    if a.cmd == "runs":
        print(f"{'start':16} {'agent':17} {'role':11} {'model':14} {'min':>6} {'Mtok':>7} {'$':>8}  description")
        for r in rows:
            print(f"{r['start'][:16]} {r['agent'][:17]:17} {role(r):11} {r['model'][:14]:14} {r['minutes'] or 0:6.1f} "
                  f"{r['total_tokens']/1e6:7.2f} {r['cost_usd']:8.2f}  {r['description'][:70]}")
    elif a.cmd == "summary":
        groups = {}
        for r in rows:
            k = {"role": role(r), "model": r["model"], "ticket": r["ticket"] or "-"}[a.by]
            g = groups.setdefault(k, [0, 0, 0.0, 0.0])
            g[0] += 1; g[1] += r["total_tokens"]; g[2] += r["cost_usd"]; g[3] += r["minutes"] or 0
        print(f"{a.by:12} {'runs':>5} {'Mtok':>8} {'Mtok/run':>9} {'$':>9} {'$/run':>7} {'min/run':>8}")
        for k, (n, t, c, m) in sorted(groups.items(), key=lambda kv: -kv[1][2]):
            print(f"{str(k)[:12]:12} {n:5} {t/1e6:8.1f} {t/1e6/n:9.2f} {c:9.2f} {c/n:7.2f} {m/n:8.1f}")
        n = len(rows); t = sum(r["total_tokens"] for r in rows); c = sum(r["cost_usd"] for r in rows)
        print(f"{'total':12} {n:5} {t/1e6:8.1f} {'':9} {c:9.2f}")
    else:
        if not a.agent or len(rows) != 1:
            sys.exit(f"record needs --agent matching exactly one run (matched {len(rows)})")
        r = rows[0]
        entry = {"recorded": datetime.now(timezone.utc).isoformat(timespec="seconds"), "host": a.host,
                 "ticket": a.ticket or r["ticket"], "size": a.size, "role": a.role or role(r), "outcome": a.outcome,
                 **{k: r[k] for k in ("agent", "model", "effort", "start", "minutes", "total_tokens", "cost_usd", "tokens")}}
        os.makedirs(os.path.dirname(LEDGER), exist_ok=True)
        with open(LEDGER, "a", encoding="utf-8") as fh:
            fh.write(json.dumps(entry, ensure_ascii=False) + "\n")
        print(json.dumps(entry, ensure_ascii=False))


if __name__ == "__main__":
    main()

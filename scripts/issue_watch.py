"""Wait until the baton label for one side lands on an open issue, print it and exit.

Protocol: `docs/agents/baton.md`. The watcher asks GitHub for open issues labelled `baton:<side>` once per
interval with a conditional request; an unchanged result answers 304, which costs nothing against the rate
limit and wakes nobody. It exits when an issue newly carries the label (or carries it again after losing it).
Comments, pushes and progress elsewhere never wake it. Run it in the background; its exit is the wake-up signal.

usage: python -I scripts/issue_watch.py --machine windows|linux|owner [--repo bubbleofmoon/H.E.R.-vault]
                                        [--state <file>] [--interval 60] [--once]
--once polls a single time and prints the current baton holders ("no batons" when none are new).
Exit 0 after printing, 2 on an API failure. Auth: GH_TOKEN or `gh auth token`.
"""
import argparse, http.client, json, os, subprocess, sys, time, urllib.error, urllib.request


def token():
    if os.environ.get("GH_TOKEN"):
        return os.environ["GH_TOKEN"]
    r = subprocess.run(["gh", "auth", "token"], capture_output=True, encoding="utf-8")
    if r.returncode or not r.stdout.strip():
        print("no GitHub token: run `gh auth login`", file=sys.stderr)
        sys.exit(2)
    return r.stdout.strip()


def fetch(url, tok, etag):
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {tok}", "Accept": "application/vnd.github+json",
                                               "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "her-baton-watch"})
    if etag:
        req.add_header("If-None-Match", etag)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.headers.get("ETag"), json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        if e.code == 304:
            return etag, None
        if e.code >= 500 or e.code == 429:
            print(f"GitHub API {e.code}; retrying next interval", file=sys.stderr)
            return etag, None
        print(f"GitHub API {e.code} for {url}: {e.read()[:300]!r}", file=sys.stderr)
        sys.exit(2)
    except (OSError, http.client.HTTPException) as e:  # URLError, timeouts, resets, dropped connections
        print(f"GitHub API unreachable ({e}); retrying next interval", file=sys.stderr)
        return etag, None


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--machine", required=True, choices=["windows", "linux", "owner"])
    p.add_argument("--repo", default="bubbleofmoon/H.E.R.-vault")
    p.add_argument("--state", default=None)
    p.add_argument("--interval", type=int, default=60)
    p.add_argument("--once", action="store_true")
    a = p.parse_args()
    state_path = a.state or os.path.join(os.path.expanduser("~"), f".her-baton-watch-{a.machine}.json")
    try:
        state = json.load(open(state_path, encoding="utf-8"))
    except (OSError, ValueError):
        state = {"etag": None, "holding": []}
    tok = token()
    url = f"https://api.github.com/repos/{a.repo}/issues?state=open&labels=baton:{a.machine}&per_page=100"
    while True:
        etag, issues = fetch(url, tok, state["etag"])
        state["etag"] = etag
        events = []
        if issues is not None:
            now = {i["number"]: i for i in issues if "pull_request" not in i}
            for n, i in sorted(now.items()):
                if n not in state["holding"]:
                    events.append(f"BATON #{n} {i['title']} {i['html_url']}")
            state["holding"] = sorted(now)
        with open(state_path, "w", encoding="utf-8") as f:
            json.dump(state, f)
        if events or a.once:
            print("\n".join(events) if events else "no batons")
            return 0
        time.sleep(a.interval)


if __name__ == "__main__":
    sys.exit(main())

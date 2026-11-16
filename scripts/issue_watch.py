"""Wait until the baton label for one side lands on an open issue, print it and exit.

Protocol: `docs/agents/baton.md` ("Watcher"). The watcher asks GitHub for open issues labelled `baton:<side>` once per
interval with a conditional request; an unchanged result answers 304, which costs nothing against the rate limit and
wakes nobody. It exits when an issue newly carries the label (or carries it again after losing it).
On an issue it already holds, it also exits on a new comment whose first line starts with "**Handoff → <side>"
(printed as HANDOFF #<n> <title> <comment url>); other comments, pushes and progress elsewhere never wake it.
Run it hidden in the background; its exit is the wake-up signal, so start it again after acting on the baton.

usage: python -I scripts/issue_watch.py --machine windows|linux|owner [--repo bubbleofmoon/H.E.R.]
                                        [--state <file>] [--interval 180] [--once]
The default state file is ~/.her-baton-watch-<machine>-<owner>-<repo>.json (each part %XX-encoded except letters, digits, '.' and '_'), one per repository, replaced atomically, so watching
another repository never makes this one's held issues look new.
--once polls a single time and prints the newly held issues ("no batons" when none are new).
--interval is at least 180 seconds, and every wait, a short rate-limit wait included, lasts at least one interval.
A rate limit (403/429 with Retry-After or X-RateLimit-Reset) is waited out, at most an hour at a time; a 403 naming a secondary rate limit without either header waits 180 s. Exit 0 after printing; 2 on an API failure, a --once poll or comment read GitHub did not answer, 100 or more batons, or a malformed --repo. Auth: GH_TOKEN or `gh auth token`.
"""
import argparse, http.client, json, os, re, subprocess, sys, tempfile, time, urllib.error, urllib.request

REPO = re.compile(r"([A-Za-z0-9-]+)/([A-Za-z0-9._-]+)")
RATE_LIMIT_CAP = 3600
PAGE = 100
MIN_INTERVAL = 180
HANDOFF = "**Handoff → "


def interval_arg(value):
    """--interval in whole seconds, at least MIN_INTERVAL (docs/agents/coordinator.md "GitHub API budget")."""
    seconds = int(value)
    if seconds < MIN_INTERVAL:
        raise argparse.ArgumentTypeError(f"must be at least {MIN_INTERVAL} seconds")
    return seconds


def token():
    if os.environ.get("GH_TOKEN"):
        return os.environ["GH_TOKEN"]
    r = subprocess.run(["gh", "auth", "token"], capture_output=True, encoding="utf-8")
    if r.returncode or not r.stdout.strip():
        print("no GitHub token: run `gh auth login`", file=sys.stderr)
        sys.exit(2)
    return r.stdout.strip()


def rate_limit_wait(headers, now=None):
    """Seconds until GitHub lifts a rate limit (Retry-After, or X-RateLimit-Reset once none remain); None otherwise."""
    now = time.time() if now is None else now
    retry = (headers.get("Retry-After") or "").strip()
    if re.fullmatch(r"[0-9]+", retry):
        return min(int(retry), RATE_LIMIT_CAP) + 1
    reset = (headers.get("X-RateLimit-Reset") or "").strip()
    if (headers.get("X-RateLimit-Remaining") or "").strip() == "0" and re.fullmatch(r"[0-9]+", reset):
        return max(0, min(int(reset) - int(now), RATE_LIMIT_CAP)) + 1
    return None


def fetch(url, tok, etag):
    """(etag, issues, wait). issues is None when nothing new was read. wait is None after an answer, 0 after a
    transient failure (retry after the normal interval), or the seconds until a rate limit lifts."""
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {tok}", "Accept": "application/vnd.github+json",
                                               "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "her-baton-watch"})
    if etag:
        req.add_header("If-None-Match", etag)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.headers.get("ETag"), json.loads(resp.read().decode("utf-8")), None
    except urllib.error.HTTPError as e:
        if e.code == 304:
            return etag, None, None
        try:
            body = e.read()
        except (OSError, http.client.HTTPException):
            body = b""
        if e.code in (403, 429):
            wait = rate_limit_wait(e.headers)
            if wait is None and e.code == 403 and b"rate limit" in body.lower():
                wait = MIN_INTERVAL      # a secondary rate limit without headers: GitHub asks for at least a minute
            if wait is not None:
                print(f"GitHub API rate limit ({e.code}); waiting {wait} s", file=sys.stderr)
                return etag, None, wait
        if e.code >= 500 or e.code == 429:
            print(f"GitHub API {e.code}; retrying next interval", file=sys.stderr)
            return etag, None, 0
        print(f"GitHub API {e.code} for {url}: {body[:300]!r}", file=sys.stderr)
        sys.exit(2)
    except (OSError, http.client.HTTPException) as e:  # URLError, timeouts, resets, dropped connections
        print(f"GitHub API unreachable ({e}); retrying next interval", file=sys.stderr)
        return etag, None, 0


def repo_parts(repo):
    match = REPO.fullmatch(repo)
    if not match or match.group(2) in (".", ".."):
        raise ValueError("--repo must be <owner>/<name>")
    return match.groups()


def encode(part):
    """One repository name part as written in a file name: ASCII letters, digits, '.' and '_' stay; anything else is %XX."""
    return "".join(c if (c.isascii() and c.isalnum()) or c in "._" else f"%{ord(c):02X}" for c in part)


def default_state_path(machine, repo):
    owner, name = repo_parts(repo)
    return os.path.join(os.path.expanduser("~"), f".her-baton-watch-{machine}-{encode(owner)}-{encode(name)}.json")


def write_state(path, state):
    """Replace the state file in one step, so a stop midway never leaves it half written."""
    fd, tmp = tempfile.mkstemp(prefix=".her-baton-watch-", suffix=".tmp", dir=os.path.dirname(os.path.abspath(path)))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(state, f)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def new_events(holding, issues):
    now = {i["number"]: i for i in issues if "pull_request" not in i}
    events = [f"BATON #{n} {i['title']} {i['html_url']}" for n, i in sorted(now.items()) if n not in holding]
    return events, sorted(now)


def handoff_events(repo, machine, tok, issues, held_before, counts):
    """(events, counts, complete, wait): a new comment on an already held issue whose first line starts with
    `**Handoff → <machine>`. Counts come from the listing's `comments` field; comments are read only when it grows.
    The first failed read stops all further reads in this poll; it and every unread issue keep their old counts. It
    reports complete=False so the next poll re-reads the listing. wait is the longest rate-limit wait a failed read
    asked for (0 for none)."""
    prefix = HANDOFF + machine
    events, fresh, complete, longest, stopped = [], {}, True, 0, False
    for i in sorted((i for i in issues if "pull_request" not in i), key=lambda i: i["number"]):
        n, total = i["number"], int(i.get("comments", 0))
        seen = counts.get(str(n))
        fresh[str(n)] = total
        if stopped:
            if seen is not None:
                fresh[str(n)] = seen
            continue
        if seen is None or n not in held_before or total <= seen:
            continue
        found = []
        for page in range(seen // PAGE + 1, (total - 1) // PAGE + 2):
            url = f"https://api.github.com/repos/{repo}/issues/{n}/comments?per_page={PAGE}&page={page}"
            _, items, wait = fetch(url, tok, None)
            if items is None:
                longest = max(longest, wait or 0)
                found = None
                break
            found += items[max(0, seen - (page - 1) * PAGE):]
        if found is None:
            fresh[str(n)] = seen
            complete, stopped = False, True
            continue
        found = found[:total - seen]      # comments posted after the listing wait for the next poll
        events += [f"HANDOFF #{n} {i['title']} {c['html_url']}" for c in found
                   if (c.get("body") or "").startswith(prefix)]
    return events, fresh, complete, longest


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--machine", required=True, choices=["windows", "linux", "owner"])
    p.add_argument("--repo", default="bubbleofmoon/H.E.R.")
    p.add_argument("--state", default=None)
    p.add_argument("--interval", type=interval_arg, default=MIN_INTERVAL)
    p.add_argument("--once", action="store_true")
    a = p.parse_args(argv)
    try:
        state_path = a.state or default_state_path(a.machine, a.repo)
        repo_parts(a.repo)
    except ValueError as e:
        print(e, file=sys.stderr)
        return 2
    try:
        with open(state_path, encoding="utf-8") as f:
            state = json.load(f)
    except (OSError, ValueError):
        state = {"etag": None, "holding": [], "comments": {}}
    if "comments" not in state:
        state["comments"] = {}
        state["etag"] = None      # a state file from before comment counts: re-read the listing to set them
    pause = state.get("retry_after", 0) - time.time()
    if pause > 0:
        time.sleep(pause)         # a comment read's rate limit outlives the run that hit it
    state["retry_after"] = 0
    tok = token()
    url = f"https://api.github.com/repos/{a.repo}/issues?state=open&labels=baton:{a.machine}&per_page={PAGE}"
    while True:
        etag, issues, wait = fetch(url, tok, state["etag"])
        state["etag"] = etag
        events = []
        complete, comment_wait = True, 0
        if issues is not None:
            if len(issues) >= PAGE:
                print(f"{PAGE} or more open issues and pull requests carry this baton; the watcher reads one page only",
                      file=sys.stderr)
                return 2
            held_before = set(state["holding"])
            events, state["holding"] = new_events(state["holding"], issues)
            more, state["comments"], complete, comment_wait = handoff_events(a.repo, a.machine, tok, issues,
                                                                             held_before, state["comments"])
            events += more
            if not complete:
                state["etag"] = None
                state["retry_after"] = time.time() + comment_wait if comment_wait else 0
                if comment_wait and not events:
                    wait = comment_wait
            else:
                state["retry_after"] = 0
        write_state(state_path, state)
        if wait == 0 and a.once:
            print("GitHub did not answer; no baton state was read", file=sys.stderr)
            return 2
        if a.once and not complete and not comment_wait and not events:
            print("GitHub did not answer a comment read; no handoff state was read", file=sys.stderr)
            return 2
        if wait is not None:
            time.sleep(max(wait, a.interval))
            continue
        if events or a.once:
            print("\n".join(events) if events else "no batons")
            return 0
        time.sleep(a.interval)


if __name__ == "__main__":
    sys.exit(main())

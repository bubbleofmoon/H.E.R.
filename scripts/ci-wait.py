#!/usr/bin/env python3
r"""Wait for one GitHub Actions run through one shared, cached poller per machine; print the result and exit.

Rules: docs/agents/coordinator.md "GitHub API budget". Every waiter on the machine reads one cached list of a
workflow's recent runs (actions/workflows/<file>/runs?per_page=40): at most one list call per ~100 s per workflow
file, made by whichever waiter takes the file lock first, with If-None-Match on the stored ETag (a 304 costs
nothing). One machine-wide budget file holds the account's remaining calls and any block, and is read before
every call of any kind: every 300 s under 1000 calls left, every 900 s under 200, and after a 403 or 429 (or a
refused token) no call before the reset. A green quick.yml result is cached per commit and reused; checks.yml
results are never cached (a merge reads the pull request's statusCheckRollup). Logs are fetched only for a failed
run: the names of every failed job and the filtered log of the first.

usage: python -I scripts/ci-wait.py --branch her/<n>-<slug> --target <name> [--filter <test>] [--os linux|windows]
                                    [--sha <commit>] [--timeout 3600] [--once]
       python -I scripts/ci-wait.py --workflow checks.yml --sha <commit> [--event pull_request|push] [--timeout 3600] [--once]
       python -I scripts/ci-wait.py [--workflow <file>] --run <id> [--timeout 3600]
Start a quick.yml wait before dispatching: a failed quick.yml run counts only when it was created after the wait
began (30 s for clock skew); read an older one with --run <id>. Without --sha the commit is the local branch's
(git rev-parse in the current directory). quick.yml runs are matched by their run name,
"quick <os> target=<target> filter=<filter>". --once reads the list once, never waits, and prints its age.
Exit 0 success; 1 failure or timed out (failed job names and filtered log lines follow); 3 cancelled or another
conclusion; 2 usage; 4 still running (the wait ended first, or --once found no finished run); 5 GitHub unavailable
or internal error. Exits 4 and 5 are never a CI result.
Cache and lock: ~/.cache/her-ci-wait (mode 0700), on Windows %LOCALAPPDATA%\her-ci-wait; against a local test
server the folder is her-ci-wait-test instead, unless HER_CI_WAIT_DIR names one.
Auth: GH_TOKEN, else `gh auth token`, read only by a waiter that is about to call. A local test server
(HER_GITHUB_API, http://127.0.0.1:<port> only) never gets either: it gets HER_CI_WAIT_TEST_TOKEN or a fixed dummy.
The test settings HER_CI_WAIT_DIR, HER_CI_WAIT_REFRESH and HER_CI_WAIT_TEST_TOKEN need a local test server.
A wait that ends with exit 4 or 5 names its run: start it again with --run <id>.
Tests: scripts/tests/test_ci_wait.py.
"""
import argparse
import calendar
import contextlib
import http.client
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

if os.name == "nt":
    import msvcrt
else:
    import fcntl

DEFAULT_API = "https://api.github.com"
LOCAL_API = re.compile(r"http://127\.0\.0\.1:[0-9]{1,5}")
LOCAL_TOKEN = "local-test"
TEST_SETTINGS = ("HER_CI_WAIT_DIR", "HER_CI_WAIT_REFRESH", "HER_CI_WAIT_TEST_TOKEN")
REPO = re.compile(r"([A-Za-z0-9-]+)/([A-Za-z0-9._-]+)")
WORKFLOW = re.compile(r"[A-Za-z0-9_-]+\.ya?ml")
BRANCH = re.compile(r"[A-Za-z0-9_][A-Za-z0-9._/-]*")
SHA = re.compile(r"[0-9a-f]{40}")
TARGET = re.compile(r"[A-Za-z0-9_]+(,[A-Za-z0-9_]+)*")
FILTER = re.compile(r"([A-Za-z0-9_][A-Za-z0-9_:.-]*)?")
EVENTS = ("push", "pull_request")
RUN_FIELDS = ("id", "display_title", "head_branch", "head_sha", "event", "status", "conclusion", "html_url", "created_at")
FAILED = ("failure", "timed_out", "startup_failure")
KEEP = re.compile(r"test result:|^test \S+ \.\.\. FAILED|^EXIT |^UNLISTED FAILURES|^---- .+ stdout ----|^error(\[E[0-9]+\])?:|^\(log truncated")
PANIC = re.compile(r"panicked at ")
PANIC_CONTEXT = 5
STAMP = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z ?")
MAX_LOG_LINES = 200
MAX_LOG_BYTES = 64 * 1024 * 1024
MAX_RESULTS = 500
SKEW = 30.0
LOG_KEEP_SECONDS = 7 * 24 * 3600
TMP_KEEP_SECONDS = 3600


class Unavailable(Exception):
    """GitHub gave no usable answer; never a CI result."""


class Later(Exception):
    """No log now; `until` is when a waiter may try again and `reason` says why."""

    def __init__(self, until, reason):
        super().__init__(reason)
        self.until = until
        self.reason = reason


class Ctx:
    """Where the cache lives and how often to call, fixed for one run."""

    def __init__(self, api, directory, refresh):
        self.api = api
        self.directory = directory
        self.refresh = refresh
        self.budget_path = os.path.join(directory, "budget.json")


def settings():
    """(api, directory, refresh) from the environment. The test settings need a loopback API, and a loopback API
    gets its own cache folder (her-ci-wait-test) unless HER_CI_WAIT_DIR names one."""
    api = os.environ.get("HER_GITHUB_API") or DEFAULT_API
    local = api != DEFAULT_API
    if local and not LOCAL_API.fullmatch(api):
        raise ValueError("HER_GITHUB_API may only name a local test server, http://127.0.0.1:<port>")
    if not local and any(os.environ.get(name) for name in TEST_SETTINGS):
        raise ValueError("HER_CI_WAIT_DIR, HER_CI_WAIT_REFRESH and HER_CI_WAIT_TEST_TOKEN work only with a local "
                         "test server (HER_GITHUB_API)")
    refresh = 100.0
    if os.environ.get("HER_CI_WAIT_REFRESH"):
        refresh = float(os.environ["HER_CI_WAIT_REFRESH"])
        if not refresh > 0:
            raise ValueError("HER_CI_WAIT_REFRESH must be a positive number of seconds")
    folder = "her-ci-wait-test" if local else "her-ci-wait"
    if os.environ.get("HER_CI_WAIT_DIR"):
        directory = os.environ["HER_CI_WAIT_DIR"]
    elif os.name == "nt":
        if not os.environ.get("LOCALAPPDATA"):
            raise ValueError("LOCALAPPDATA is not set")
        directory = os.path.join(os.environ["LOCALAPPDATA"], folder)
    else:
        directory = os.path.join(os.path.expanduser("~"), ".cache", folder)
    return api, directory, refresh


def interval(remaining, refresh):
    """Seconds until the next call: slower as the account's remaining calls run low."""
    if remaining is not None and remaining < 200:
        return refresh * 9
    if remaining is not None and remaining < 1000:
        return refresh * 3
    return refresh


def rate_limited_until(status, headers, now, fallback):
    """Epoch time before which nobody calls again when this answer is a rate limit; None otherwise."""
    if status not in (403, 429):
        return None
    retry = headers.get("retry-after") or ""
    if retry.isdigit():
        return now + int(retry) + 1
    if status == 429 or headers.get("x-ratelimit-remaining") == "0":
        reset = headers.get("x-ratelimit-reset") or ""
        return max(int(reset) + 1, now) if reset.isdigit() else now + fallback
    return None


def clock(t):
    return time.strftime("%H:%M:%S UTC", time.gmtime(t))


def list_state_name(repo, workflow):
    """One file per repository and workflow; the name is percent-encoded, so no two pairs share it."""
    return "runs-" + urllib.parse.quote(f"{repo}/{workflow}", safe="") + ".json"


@contextlib.contextmanager
def lock(directory, wait):
    """Hold the machine-wide lock file. With wait=False, yield False at once when another waiter holds it.
    The OS drops the lock when its holder exits, so a killed waiter never leaves it behind."""
    f = open(os.path.join(directory, "lock"), "a+b")
    held = False
    try:
        deadline = time.time() + (60 if wait else 0)
        while True:
            try:
                if os.name == "nt":
                    f.seek(0)
                    msvcrt.locking(f.fileno(), msvcrt.LK_NBLCK, 1)
                else:
                    fcntl.flock(f.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                held = True
                break
            except OSError:
                if time.time() >= deadline:
                    break
                time.sleep(0.05)
        yield held
    finally:
        if held:
            if os.name == "nt":
                f.seek(0)
                msvcrt.locking(f.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
        f.close()


def read_json(path):
    """The JSON object in path: {} when missing or corrupt (a rewrite heals it), None when it cannot be read
    (then nobody calls or writes on its account)."""
    for _ in range(40):
        try:
            with open(path, encoding="utf-8") as f:
                value = json.load(f)
        except FileNotFoundError:
            return {}
        except PermissionError:          # Windows: another waiter is replacing it
            time.sleep(0.05)
            continue
        except ValueError:
            return {}
        except OSError:
            return None
        return value if isinstance(value, dict) else {}
    return None


def write_json(path, value):
    """Replace path in one step, so a reader sees the old or the new file, never half of one."""
    tmp = f"{path}.{os.getpid()}.tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(value, f)
    for _ in range(40):
        try:
            os.replace(tmp, path)
            return
        except PermissionError:          # Windows: a reader has it open
            time.sleep(0.05)
    os.replace(tmp, path)


def prune(directory, now):
    """Drop cached logs older than a week and temporary files a killed writer left (called under the lock)."""
    for name in os.listdir(directory):
        keep = LOG_KEEP_SECONDS if name.startswith("log-") else TMP_KEEP_SECONDS if name.endswith(".tmp") else None
        if keep is None:
            continue
        path = os.path.join(directory, name)
        try:
            if now - os.path.getmtime(path) > keep:
                os.remove(path)
        except OSError:
            pass


def token(api):
    """For GitHub: GH_TOKEN, else `gh auth token`. For a local test server: HER_CI_WAIT_TEST_TOKEN or a fixed
    dummy, never GH_TOKEN or the gh login, so a real token never goes to a test server."""
    if api != DEFAULT_API:
        return os.environ.get("HER_CI_WAIT_TEST_TOKEN") or LOCAL_TOKEN
    if os.environ.get("GH_TOKEN"):
        return os.environ["GH_TOKEN"]
    try:
        r = subprocess.run(["gh", "auth", "token"], capture_output=True, encoding="utf-8", timeout=30)
    except (OSError, subprocess.SubprocessError) as e:
        raise Unavailable(f"no GitHub token ({type(e).__name__}); run `gh auth login`")
    if r.returncode or not r.stdout.strip():
        raise Unavailable("no GitHub token; run `gh auth login`")
    return r.stdout.strip()


def make_request(url, tok, etag=None):
    req = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json",
                                               "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "her-ci-wait"})
    req.add_unredirected_header("Authorization", f"Bearer {tok}")   # never forwarded to the log storage a redirect names
    if etag:
        req.add_header("If-None-Match", etag)
    return req


def request(url, tok, etag=None):
    """(status, headers, body) of one GET. HTTP errors and 304 come back as a status; network errors raise OSError."""
    try:
        with urllib.request.urlopen(make_request(url, tok, etag), timeout=30) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.headers, e.read()


def filter_log(lines):
    """Test results, FAILED and EXIT lines, error lines, and each panic header with up to five message lines."""
    kept, dropped, context = [], 0, 0
    for raw in lines:
        line = STAMP.sub("", raw.rstrip("\r\n"))
        if PANIC.search(line):
            context, wanted = PANIC_CONTEXT, True
        elif context and line.strip():
            context, wanted = context - 1, True
        else:
            context, wanted = 0, bool(KEEP.search(line))
        if wanted:
            if len(kept) < MAX_LOG_LINES:
                kept.append(line)
            else:
                dropped += 1
    if dropped:
        kept.append(f"({dropped} more lines)")
    return kept or ["(no test-result, panic or error lines in the log)"]


def capped(resp):
    """The log's lines, up to MAX_LOG_BYTES, then one marker line."""
    total = 0
    for raw in resp:
        total += len(raw)
        if total > MAX_LOG_BYTES:
            yield "(log truncated after 64 MiB)"
            return
        yield raw.decode("utf-8", "replace")


def fetch_log(url, tok):
    """(status, headers, kept lines) of a job log, filtered while it streams and capped in size."""
    try:
        with urllib.request.urlopen(make_request(url, tok), timeout=60) as resp:
            return resp.status, resp.headers, filter_log(capped(resp))
    except urllib.error.HTTPError as e:
        return e.code, e.headers, []


def blocked(budget, now):
    """Why no call of any kind may be made now, or None."""
    if (budget or {}).get("blocked_until", 0) > now:
        return budget.get("block_reason") or "rate limit"
    return None


def note_answer(ctx, budget, status, headers, url, now):
    """Record an answer's budget news machine-wide (caller holds the lock); return the block time, if any."""
    remaining = headers.get("x-ratelimit-remaining") or ""
    if remaining.isdigit():
        budget["remaining"] = int(remaining)
    until = rate_limited_until(status, headers, now, ctx.refresh * 9)
    reason = "rate limit"
    if until is None and status == 401:
        until, reason = now + ctx.refresh * 9, "token refused (401)"
    if until is not None:
        budget.update(blocked_until=until, block_reason=reason)
    if status in (401, 404):
        budget["refused"] = {"status": status, "url": url, "at": now}
    write_json(ctx.budget_path, budget)
    return until


def refresh_if_due(ctx, state_path, url):
    """Refresh one workflow's shared run list when it is due, the budget allows a call and no other waiter holds the
    lock. Returns the list state as now cached ({} when empty, None when unreadable).

    The due time is claimed on disk under the lock before the call, so a waiter killed mid-call costs the machine
    at most one call per window.
    """
    state = read_json(state_path)
    now = time.time()
    if state is None or now < state.get("next_at", 0) or blocked(read_json(ctx.budget_path), now):
        return state
    with lock(ctx.directory, wait=False) as held:
        if not held:
            return read_json(state_path)
        state, budget = read_json(state_path), read_json(ctx.budget_path)
        now = time.time()
        if state is None or budget is None or now < state.get("next_at", 0) or blocked(budget, now):
            return state
        state["next_at"] = now + interval(budget.get("remaining"), ctx.refresh)
        write_json(state_path, state)                    # the claim, before the call
        prune(ctx.directory, now)
        try:
            status, headers, body = request(url, token(ctx.api), state.get("etag"))
        except Unavailable as e:
            state["error"] = str(e)
        except (OSError, http.client.HTTPException) as e:   # unreachable, timeout, reset
            state["error"] = f"GitHub API unreachable ({type(e).__name__})"
        else:
            now = time.time()
            until = note_answer(ctx, budget, status, headers, url, now)
            if until is not None:
                state.update(next_at=until, error=budget["block_reason"])
            elif status in (200, 304):
                runs = None
                if status == 200:
                    try:
                        data = json.loads(body.decode("utf-8"))
                    except ValueError:
                        data = None
                    runs = data.get("workflow_runs") if isinstance(data, dict) else None
                if status == 200 and not isinstance(runs, list):
                    state["error"] = "GitHub API gave an unreadable run list"
                else:
                    if status == 200:
                        state["runs"] = [{field: run.get(field) for field in RUN_FIELDS} for run in runs if isinstance(run, dict)]
                        state["etag"] = headers.get("ETag")
                    state.update(fetched_at=now, next_at=now + interval(budget.get("remaining"), ctx.refresh), error=None)
            else:
                state["error"] = f"GitHub API {status}"
        write_json(state_path, state)
        return state


def check_answer(ctx, budget, status, headers, url):
    now = time.time()
    until = note_answer(ctx, budget, status, headers, url, now)
    if until is not None:
        raise Later(until, budget["block_reason"])
    if status != 200:
        raise Later(now + interval(budget.get("remaining"), ctx.refresh), f"GitHub API {status}")


def fetch_failed_log(ctx, budget, repo, run):
    try:
        tok = token(ctx.api)
        jobs_url = f"{ctx.api}/repos/{repo}/actions/runs/{run['id']}/jobs?filter=latest&per_page=100"
        status, headers, body = request(jobs_url, tok)
        check_answer(ctx, budget, status, headers, jobs_url)
        data = json.loads(body.decode("utf-8"))
        if not isinstance(data, dict) or not isinstance(data.get("jobs"), list):
            raise Later(time.time() + ctx.refresh, "unreadable job list")
        jobs = [job for job in data["jobs"] if isinstance(job, dict) and job.get("conclusion") in FAILED]
        if not jobs:
            return ["(no failed job in this run)"]
        lines = ["failed jobs: " + ", ".join(str(job.get("name")) for job in jobs), f"job {jobs[0].get('name')}:"]
        log_url = f"{ctx.api}/repos/{repo}/actions/jobs/{jobs[0]['id']}/logs"
        status, headers, kept = fetch_log(log_url, tok)
        check_answer(ctx, budget, status, headers, log_url)
        return lines + kept
    except Unavailable as e:
        raise Later(time.time() + ctx.refresh, str(e))
    except (OSError, http.client.HTTPException, ValueError) as e:
        raise Later(time.time() + ctx.refresh, f"GitHub API unreachable ({type(e).__name__})")


def failed_log(ctx, repo, run):
    """The failed jobs' names and the first one's filtered log, fetched once per run. A block or an error is
    recorded (machine-wide budget, or retry_at beside the log), so no other waiter calls before it may succeed."""
    path = os.path.join(ctx.directory, f"log-{run['id']}.json")
    url = run.get("html_url")
    with lock(ctx.directory, wait=True) as held:
        cached = read_json(path)
        if cached and "lines" in cached:
            return cached["lines"]
        budget = read_json(ctx.budget_path)
        if not held or cached is None or budget is None:
            return [f"(log not fetched: the cache is busy or unreadable; see {url})"]
        now = time.time()
        reason = blocked(budget, now)
        if reason:
            return [f"(log unavailable until {clock(budget['blocked_until'])}: {reason}; see {url})"]
        if cached.get("retry_at", 0) > now:
            return [f"(log unavailable until {clock(cached['retry_at'])}: {cached.get('reason')}; see {url})"]
        try:
            lines = fetch_failed_log(ctx, budget, repo, run)
        except Later as later:
            write_json(path, {"retry_at": later.until, "reason": later.reason})
            return [f"(log unavailable until {clock(later.until)}: {later.reason}; see {url})"]
        write_json(path, {"lines": lines})
        return lines


def remember(ctx, key, run):
    """Cache a quick.yml result per commit. A busy lock is retried; a result that still cannot be stored is said."""
    path = os.path.join(ctx.directory, "results.json")
    for _ in range(3):
        with lock(ctx.directory, wait=True) as held:
            if not held:
                continue
            results = read_json(path)
            if results is None:
                break
            results.pop(key, None)
            results[key] = {field: run.get(field) for field in RUN_FIELDS}
            while len(results) > MAX_RESULTS:
                results.pop(next(iter(results)))
            write_json(path, results)
            return
    print("ci-wait: this result was not cached (cache busy or unreadable)", file=sys.stderr, flush=True)


def report(ctx, repo, run, cached=False):
    """Print a known result and return its exit code; trouble fetching the log never changes the code."""
    conclusion = run.get("conclusion") or "unknown"
    print(f"CI {conclusion}: {run.get('display_title')} {run.get('event')} {(run.get('head_sha') or '')[:12]} "
          f"{run.get('html_url')}" + (" (cached)" if cached else ""), flush=True)
    if conclusion == "success":
        return 0
    if conclusion in FAILED:
        try:
            lines = failed_log(ctx, repo, run)
        except Exception as e:
            lines = [f"(log unavailable: internal error {type(e).__name__}; see {run.get('html_url')})"]
        for line in lines:
            print(line, flush=True)
        return 1
    return 3


def created(run):
    """Epoch seconds of created_at; 0 when it cannot be read (such a run counts as older than any wait)."""
    try:
        return calendar.timegm(time.strptime(run.get("created_at") or "", "%Y-%m-%dT%H:%M:%SZ"))
    except (ValueError, TypeError):
        return 0


def fresh_enough(run, start):
    """A quick.yml run that may be reported as this wait's result: created at most SKEW seconds before the wait."""
    return created(run) >= start - SKEW


def quick_title(os_name, target, filter_):
    return f"quick {os_name} target={target} filter={filter_}"


def find_run(runs, a, title):
    """The newest listed run this waiter waits for, or None."""
    if a.run is not None:
        hits = [run for run in runs if run.get("id") == a.run]
    else:
        hits = [run for run in runs if run.get("head_sha") == a.sha
                and (a.branch is None or run.get("head_branch") == a.branch)
                and (a.event is None or run.get("event") == a.event)
                and (title is None or (run.get("display_title") or "").strip() == title)]
    return max(hits, key=lambda run: run.get("id") or 0, default=None)


def list_age(state, now):
    fetched = (state or {}).get("fetched_at")
    return f"(run list fetched {int(now - fetched)} s ago)" if fetched else "(no run list fetched yet)"


def local_sha(branch):
    r = subprocess.run(["git", "rev-parse", "--verify", "--quiet", f"refs/heads/{branch}^{{commit}}"],
                       capture_output=True, encoding="utf-8")
    if r.returncode or not SHA.fullmatch(r.stdout.strip()):
        raise ValueError(f"no local branch {branch} here; pass --sha")
    return r.stdout.strip()


def parse(argv):
    p = argparse.ArgumentParser(description="Wait for one GitHub Actions run through the machine's shared poller.")
    p.add_argument("--repo", default="bubbleofmoon/H.E.R.")
    p.add_argument("--workflow", default="quick.yml")
    p.add_argument("--branch")
    p.add_argument("--sha")
    p.add_argument("--run", type=int)
    p.add_argument("--event", choices=EVENTS)
    p.add_argument("--target")
    p.add_argument("--filter", default="")
    p.add_argument("--os", choices=["linux", "windows"], default="linux")
    p.add_argument("--timeout", type=float, default=3600.0)
    p.add_argument("--once", action="store_true")
    return p.parse_args(argv)


def checked(a):
    """The run name to match (None for any), after refusing every malformed or contradictory option."""
    match = REPO.fullmatch(a.repo)
    if not match or match.group(2) in (".", ".."):
        raise ValueError("--repo must be <owner>/<name>")
    if not WORKFLOW.fullmatch(a.workflow):
        raise ValueError("--workflow must be a workflow file name such as quick.yml")
    if not a.timeout >= 0:
        raise ValueError("--timeout must be zero or more seconds")
    if a.run is not None:
        if a.run < 1 or a.branch or a.sha or a.target or a.filter or a.event:
            raise ValueError("--run names one run by its id; give no --branch, --sha, --event, --target or --filter")
        return None
    if a.branch is not None and (not BRANCH.fullmatch(a.branch) or ".." in a.branch):
        raise ValueError("--branch must be a branch name such as her/<n>-<slug>")
    if a.sha is not None and not SHA.fullmatch(a.sha):
        raise ValueError("--sha must be a full commit id (40 hex digits)")
    if not (a.branch or a.sha):
        raise ValueError("name the run with --branch, --sha or --run")
    if a.workflow != "quick.yml":
        if a.target or a.filter:
            raise ValueError("--target and --filter name a quick.yml run only")
        return None
    if a.event:
        raise ValueError("--event is for workflows other than quick.yml")
    if not a.target or not TARGET.fullmatch(a.target) or not FILTER.fullmatch(a.filter):
        raise ValueError("a quick.yml run needs --target <name> and at most one --filter <test>")
    return quick_title(a.os, a.target, a.filter)


def wait(argv):
    a = parse(argv)
    try:
        api, directory, refresh = settings()
        title = checked(a)
        if a.branch and not a.sha:
            a.sha = local_sha(a.branch)
        os.makedirs(directory, mode=0o700, exist_ok=True)
    except (ValueError, OSError) as e:
        print(f"ci-wait: {e}", file=sys.stderr)
        return 2
    ctx = Ctx(api, directory, refresh)
    state_path = os.path.join(directory, list_state_name(a.repo, a.workflow))
    url = f"{api}/repos/{a.repo}/actions/workflows/{a.workflow}/runs?per_page=40"
    key = f"{a.repo} {a.workflow} {a.sha} {title}" if title else None     # quick.yml only, never checks.yml
    if key:
        hit = (read_json(os.path.join(directory, "results.json")) or {}).get(key)
        if hit and hit.get("conclusion") == "success":
            return report(ctx, a.repo, hit, cached=True)
    start = time.time()
    deadline = start + (0 if a.once else a.timeout)
    said, seen = None, None
    while True:
        state = refresh_if_due(ctx, state_path, url)
        budget = read_json(ctx.budget_path) or {}
        now = time.time()
        refused = budget.get("refused") or {}
        if refused.get("status") == 401 and blocked(budget, now):
            print("GitHub unavailable: the token was refused (401); run `gh auth login`. Not a result.", flush=True)
            return 5
        if refused.get("status") == 404 and refused.get("url") == url \
                and refused.get("at", 0) > (state or {}).get("fetched_at", 0):
            print(f"ci-wait: no workflow {a.workflow} in {a.repo} (404)", file=sys.stderr)
            return 2
        runs = (state or {}).get("runs") or []
        run = find_run(runs, a, title)
        stale = None
        if run and run.get("status") == "completed":
            if run.get("conclusion") == "success" or a.run is not None or title is None or fresh_enough(run, start):
                if key:
                    try:
                        remember(ctx, key, run)
                    except Exception as e:
                        print(f"ci-wait: this result was not cached ({type(e).__name__})", file=sys.stderr, flush=True)
                code = report(ctx, a.repo, run)
                if a.once:
                    print(list_age(state, now), flush=True)
                return code
            stale = run
        if run and run.get("id") != seen:
            seen = run.get("id")
            if stale:
                print(f"ignoring {run.get('html_url')}: it finished before this wait began; read it with --run {seen}",
                      flush=True)
            else:
                print(f"waiting for {run.get('html_url')} (resume with --run {seen})", flush=True)
        if a.run is not None and run is None and runs and (state or {}).get("fetched_at", 0) >= start \
                and a.run < min(r.get("id") or 0 for r in runs):
            print(f"ci-wait: run {a.run} is not among the 40 newest runs of {a.workflow}", file=sys.stderr)
            return 2
        reason = blocked(budget, now)
        if state is None:
            problem = "GitHub unavailable (the cache cannot be read); retrying"
        elif reason:
            problem = f"GitHub unavailable until {clock(budget['blocked_until'])} ({reason}); waiting"
        elif state.get("error"):
            problem = f"GitHub unavailable ({state['error']}); retrying"
        else:
            problem = None
        if problem and problem != said:
            print(problem, flush=True)
        said = problem
        if now >= deadline:
            if a.once:
                print(list_age(state, now), flush=True)
            resume = f"; resume with --run {run.get('id')}" if run and not stale else ""
            if problem:
                print(f"GitHub unavailable: no CI result (not a failure){resume}", flush=True)
                return 5
            if stale:
                print(f"CI still running: no run of this commit since the wait began; the older one is "
                      f"{stale.get('html_url')}, read it with --run {stale.get('id')} (not a result)", flush=True)
            elif run:
                print(f"CI still running: {run.get('html_url')} (not a result){resume}", flush=True)
            else:
                print("CI still running: no matching run yet (not a result)", flush=True)
            return 4
        time.sleep(min(ctx.refresh / 10, deadline - now))


def main(argv=None):
    try:
        return wait(argv)
    except SystemExit:
        raise
    except BaseException as e:           # never exit 1, which means "CI failure"
        try:
            print(f"ci-wait: internal error ({type(e).__name__}); not a result", flush=True)
        except BaseException:
            pass
        return 5


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Run a coordinator-staged Linux review, investigation or plan."""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import stat
import subprocess
import sys
import tempfile

SCRIPTS = Path(__file__).resolve().parent
CHECKOUT = SCRIPTS.parent
MAX_LOG = 1024 * 1024
BRIEF = "brief.md"


def required_env(name):
    value = os.environ.get(name)
    if not value:
        raise ValueError(f"{name} is required")
    return value


def regular_file(path):
    info = path.lstat()
    return stat.S_ISREG(info.st_mode) and info.st_nlink == 1


def stage_inputs(source, prompt, destination):
    destination.mkdir()
    for path in source.iterdir():
        if path.name == BRIEF or not regular_file(path):
            raise ValueError("approved inputs must be single-link regular files and omit brief.md")
        shutil.copyfile(path, destination / path.name)
    shutil.copyfile(prompt, destination / BRIEF)


def client_args(args):
    if args.provider == "codex":
        return ["exec", "--skip-git-repo-check", "-m", args.model or "gpt-6.2-sol",
                "-c", f"model_reasoning_effort={json.dumps(args.effort or 'high')}",
                "--dangerously-bypass-approvals-and-sandbox", "--color", "never",
                "--json", "-o", "/tmp/her-report.md", "-"]
    plan = args.mode == "plan"
    role = "planner" if plan else "ticket-reviewer"
    _, _, instructions = (CHECKOUT / f".claude/agents/{role}.md").read_text(encoding="utf-8").split("---\n", 2)
    tools = "Read,Grep,Glob,Bash" + (",Write,Edit" if plan else "")
    command = ["--print", "--output-format", "json", "--no-session-persistence",
               "--strict-mcp-config", "--setting-sources", "", "--append-system-prompt", instructions,
               "--model", args.model or "opus", "--effort", args.effort or ("high" if plan else "medium"),
               "--tools", tools, "--allowedTools", tools, "--add-dir", "/inputs"]
    return command + (["/repo", "/plan-out"] if plan else [])


def read_report(provider, log):
    with log.open("rb") as source:
        raw = source.read(MAX_LOG + 1)
    if len(raw) > MAX_LOG:
        return ""
    try:
        result = raw.decode()
        if provider == "claude":
            envelope = json.loads(result)
            if (not isinstance(envelope, dict) or envelope.get("type") != "result"
                    or envelope.get("subtype") != "success" or envelope.get("is_error") is not False):
                return ""
            result = envelope.get("result")
        return result.strip() if isinstance(result, str) else ""
    except (ValueError, RecursionError):
        return ""


def has_api_error(log):
    with log.open("rb") as source:
        return any(line.lstrip().startswith(b"API Error:") for line in source)


def exit_receipt(out, code, report, api_error):
    if code == 124:
        receipt = "timeout"
    elif code == 137:
        receipt = "killed"
    elif code == 75:
        receipt = "quota"
    elif not report or code == 69 or api_error:
        receipt = "no-report"
    else:
        receipt = str(code)
    out("exit.txt").write_text(receipt + "\n")
    if code == 0 and receipt == "0":
        out("report.md").write_text(report + "\n", encoding="utf-8")
        return 0
    return code or 1


def write_log(log, data):
    remaining = max(0, MAX_LOG - log.tell())
    log.write(data[:remaining])
    return len(data) > remaining


def capture_logs(command, stdout, stderr):
    overflow = False
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(child.stdout, selectors.EVENT_READ, stdout)
                ready.register(child.stderr, selectors.EVENT_READ, stderr)
                while ready.get_map():
                    for event, _ in ready.select():
                        data = os.read(event.fd, 65536)
                        if data:
                            overflow |= write_log(event.data, data)
                        else:
                            ready.unregister(event.fd)
            return child.wait(), overflow
        finally:
            if child.poll() is None:
                child.kill()


def validate_scratch(scratch):
    def fail(error):
        raise error

    if not stat.S_ISDIR(scratch.lstat().st_mode):
        raise ValueError("scratch must be a directory")
    for directory, dirs, files in os.walk(scratch, onerror=fail):
        for name in dirs + files:
            path = Path(directory) / name
            mode = path.lstat().st_mode
            if (name.casefold() == ".git" or mode & (stat.S_ISUID | stat.S_ISGID)
                    or not (stat.S_ISDIR(mode) or regular_file(path))):
                raise ValueError("scratch contains Git metadata or an unsafe file")


def install_plan(stage, ticket):
    plan = stage / "plan-out/plan.md"
    if not regular_file(plan) or not plan.stat().st_size:
        raise ValueError("plan-out/plan.md must be a nonempty single-link regular file")
    scratch = stage / "scratch"
    validate_scratch(scratch)
    with tempfile.TemporaryDirectory(prefix=".install-", dir=ticket) as directory:
        transfer = Path(directory)
        plan_copy = transfer / "plan.md"
        shutil.copyfile(plan, plan_copy)
        with plan_copy.open("rb") as source:
            digest = hashlib.file_digest(source, "sha256").hexdigest()
        digest_line = f"{digest}  plan.md"
        sha = transfer / "plan.sha256"
        sha.write_text(digest_line + "\n", encoding="ascii")
        outputs = {"scratch": scratch, "plan.md": plan_copy, "plan.sha256": sha}
        for name in outputs:
            target = ticket / name
            if os.path.lexists(target) and (target.is_symlink() or not (
                    target.is_dir() if name == "scratch" else regular_file(target))):
                raise ValueError(f"unsafe coordinator destination: {name}")
        saved = {}
        installed = []
        try:
            for name, source in outputs.items():
                target = ticket / name
                if target.exists():
                    prior = transfer / (name + ".old")
                    target.rename(prior)
                    saved[target] = prior
                source.rename(target)
                installed.append((target, source))
        except BaseException:
            for target, source in reversed(installed):
                target.rename(source)
            for target, prior in saved.items():
                prior.rename(target)
            raise
    print(digest_line)


def run(args, out, ticket):
    inputs = Path(required_env("HER_READONLY_INPUTS"))
    base = required_env("HER_READONLY_BASE")
    if not re.fullmatch(r"[a-f0-9]{40}", base):
        raise ValueError("HER_READONLY_BASE must be a full commit ID")
    executable = shutil.which(args.provider)
    if not executable:
        raise ValueError(f"trusted native {args.provider} executable is unavailable")
    repo = CHECKOUT if ticket else Path(args.source).resolve(strict=True)
    head = base
    if not ticket:
        root, head = subprocess.check_output(
            ["/usr/bin/git", "-C", str(repo), "rev-parse", "--show-toplevel", "HEAD^{commit}"],
            env={"PATH": "/usr/bin:/bin", "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"},
            text=True).strip().splitlines()
        if Path(root) != repo:
            raise ValueError("review source must be a checkout root")
    with tempfile.TemporaryDirectory(prefix=".readonly-", dir=ticket or args.prompt.parent) as directory:
        temporary = Path(directory)
        stage_inputs(inputs, args.prompt, temporary / "approved")
        stage = temporary / "run"
        command = ["/usr/bin/python3", "-I", str(SCRIPTS / "readonly-agent.py"),
                   "--repo", str(repo), "--head", head, "--base", base,
                   "--inputs", str(temporary / "approved"), "--stage", str(stage),
                   "--mode", args.mode, "--provider", args.provider, "--exec", executable,
                   "--prompt-file", BRIEF, "--timeout", str(args.minutes * 60),
                   "--capture-result", "--", *client_args(args)]
        with out("out.log").open("wb") as stdout, out("err.log").open("wb") as stderr:
            code, overflow = capture_logs(command, stdout, stderr)
        if overflow:
            print("readonly-task: worker log exceeded the capture limit", file=sys.stderr)
        report = "" if overflow else read_report(args.provider, out("out.log"))
        api_error = has_api_error(out("err.log"))
        code = exit_receipt(out, code, report, api_error)
        if code or not ticket:
            return code
        try:
            install_plan(stage, ticket)
        except (OSError, ValueError) as error:
            out("plan.txt").write_text("no-plan\n")
            print(f"readonly-task: {error}", file=sys.stderr)
            return 1
        out("plan.txt").write_text("installed\n")
        return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("provider", choices=("codex", "claude"))
    parser.add_argument("mode", choices=("review", "plan"))
    parser.add_argument("name")
    parser.add_argument("source", help="review checkout or planning ticket number")
    parser.add_argument("prompt", type=Path)
    parser.add_argument("model", nargs="?")
    parser.add_argument("effort", nargs="?")
    parser.add_argument("minutes", nargs="?", type=int, default=360)
    args = parser.parse_args()
    if not sys.platform.startswith("linux"):
        parser.error("Linux is required")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*", args.name) or not 1 <= args.minutes <= 999:
        parser.error("invalid run name or timeout")
    args.prompt = args.prompt.resolve()
    if not args.prompt.is_file():
        parser.error("prompt file is unavailable")

    ticket = None
    if args.mode == "plan":
        if not re.fullmatch(r"[1-9][0-9]*", args.source):
            parser.error("a positive ticket number is required")
        ticket = Path(os.environ.get("HER_ORCH", "/root/orch/her")) / args.source
        if ticket.is_symlink() or not ticket.is_dir() or not args.prompt.is_relative_to(ticket.resolve()):
            parser.error("the prompt must belong to an existing ticket directory")
        ticket = ticket.resolve()

    def out(suffix):
        return args.prompt.parent / f"{args.name}.{suffix}"

    try:
        with contextlib.ExitStack() as resources:
            if ticket:
                lock = os.open(ticket, os.O_RDONLY | os.O_DIRECTORY)
                resources.callback(os.close, lock)
                # Acquire before receipt and staging writes. No other coordinator lock is acquired.
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            receipts = [out(suffix) for suffix in ("report.md", "exit.txt", "out.log", "err.log", "plan.txt")]
            if args.prompt in receipts:
                raise ValueError("prompt cannot be a receipt path")
            for receipt in receipts:
                receipt.unlink(missing_ok=True)
            try:
                return run(args, out, ticket)
            except (OSError, ValueError, subprocess.CalledProcessError) as error:
                with out("err.log").open("ab") as log:
                    write_log(log, f"readonly-task: {error}\n".encode())
                out("exit.txt").write_text("no-report\n")
                raise
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"readonly-task: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

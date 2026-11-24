#!/usr/bin/env python3
"""Run repository checks and all scripts/tests/test_*.py suites in parallel.

Enforces completeness against scripts/tests/test_*.py on disk so a newly added
test suite cannot go unrun (exits 2 on mismatch).
"""
from __future__ import annotations

import argparse
from collections.abc import Sequence
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_TIMEOUT = 300

TEST_SUITES: tuple[str, ...] = (
    "test_agent_guard.py",
    "test_check_repo.py",
    "test_checks_workflow.py",
    "test_ci_suite.py",
    "test_ci_wait.py",
    "test_codex_task.py",
    "test_gh_public.py",
    "test_handoff.py",
    "test_issue_watch.py",
    "test_merge_check.py",
    "test_plan_export.py",
    "test_pre_push_hook.py",
    "test_quick_workflow.py",
    "test_run_scans.py",
    "test_scan_public.py",
    "test_snowluma_export.py",
    "test_strip_history.py",
    "test_watch_stream.py",
)

SUITES: tuple[str, ...] = (
    "scripts/check-repo.py",
    *(f"scripts/tests/{name}" for name in TEST_SUITES),
)


@dataclass(frozen=True)
class SuiteResult:
    rel: str
    returncode: int
    elapsed: float
    stdout: str
    stderr: str
    timed_out: bool = False


def discover_test_suites(tests_dir: Path) -> list[str]:
    """Return sorted test_*.py filenames present directly under tests_dir."""
    if not tests_dir.is_dir():
        return []
    return sorted(path.name for path in tests_dir.glob("test_*.py") if path.is_file())


def check_completeness(
    root: Path,
    test_suites: Sequence[str] = TEST_SUITES,
) -> list[str]:
    """Return problem descriptions if TEST_SUITES and disk test_*.py files diverge."""
    problems: list[str] = []
    seen: set[str] = set()
    for name in test_suites:
        if name in seen:
            problems.append(f"scripts/tests/{name}: listed more than once in TEST_SUITES")
        seen.add(name)

    on_disk = set(discover_test_suites(root / "scripts" / "tests"))
    for name in sorted(on_disk - seen):
        problems.append(f"scripts/tests/{name}: not in TEST_SUITES")
    for name in sorted(seen - on_disk):
        problems.append(f"scripts/tests/{name}: listed in TEST_SUITES but missing")
    return problems


def _decode_output(data: str | bytes | None) -> str:
    if data is None:
        return ""
    if isinstance(data, bytes):
        return data.decode("utf-8", errors="replace")
    return data


def _run_one_suite(rel: str, root: Path, timeout: float) -> SuiteResult:
    path = root / rel
    started = time.monotonic()
    try:
        done = subprocess.run(
            [sys.executable, "-I", str(path)],
            cwd=root,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            check=False,
        )
        return SuiteResult(
            rel=rel,
            returncode=done.returncode,
            elapsed=time.monotonic() - started,
            stdout=done.stdout,
            stderr=done.stderr,
        )
    except subprocess.TimeoutExpired as exc:
        stderr = _decode_output(exc.stderr)
        if stderr and not stderr.endswith("\n"):
            stderr += "\n"
        stderr += f"TIMEOUT: killed after {timeout}s\n"
        return SuiteResult(
            rel=rel,
            returncode=124,
            elapsed=time.monotonic() - started,
            stdout=_decode_output(exc.stdout),
            stderr=stderr,
            timed_out=True,
        )
    except OSError as exc:
        return SuiteResult(
            rel=rel,
            returncode=127,
            elapsed=time.monotonic() - started,
            stdout="",
            stderr=f"{type(exc).__name__}: {exc}\n",
        )


def run_suites(
    suites: Sequence[str],
    root: Path,
    *,
    jobs: int | None = None,
    timeout: float = DEFAULT_TIMEOUT,
) -> int:
    """Execute suites concurrently and return 0 iff every suite exits 0."""
    if not suites:
        print("Ran 0 suites in 0.00s; failures: none", flush=True)
        return 0

    worker_cap = jobs if jobs is not None else min(8, os.cpu_count() or 4)
    max_workers = max(1, min(len(suites), worker_cap))
    started = time.monotonic()

    with ThreadPoolExecutor(max_workers=max_workers) as pool:
        futures = [pool.submit(_run_one_suite, rel, root, timeout) for rel in suites]
        results = [future.result() for future in futures]

    failed: list[str] = []
    for res in results:
        if res.returncode == 0:
            print(f"PASS {res.rel} ({res.elapsed:.2f}s)", flush=True)
            continue
        failed.append(res.rel)
        print(f"FAIL {res.rel} (exit {res.returncode}, {res.elapsed:.2f}s)", flush=True)
        for label, text in (("stdout", res.stdout), ("stderr", res.stderr)):
            if text:
                print(f"  --- {label} ---", flush=True)
                for line in text.splitlines():
                    print(f"  {line}", flush=True)

    total = time.monotonic() - started
    summary = ", ".join(failed) or "none"
    print(f"Ran {len(results)} suites in {total:.2f}s; failures: {summary}", flush=True)
    return 1 if failed else 0


def main(argv: list[str] | None = None) -> int:
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="replace")

    parser = argparse.ArgumentParser(description="Run repository checks and script test suites in parallel.")
    parser.add_argument("--list", action="store_true", help="Print the ordered suite list and exit")
    parser.add_argument("--jobs", type=int, default=None, help="Maximum concurrent suites")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT, help="Per-suite timeout in seconds")
    args = parser.parse_args(argv)

    if args.jobs is not None and args.jobs < 1:
        parser.error("--jobs must be >= 1")
    if args.timeout <= 0:
        parser.error("--timeout must be > 0")

    problems = check_completeness(ROOT)
    if not (ROOT / "scripts" / "check-repo.py").is_file():
        problems.append("scripts/check-repo.py: missing on disk")
    if problems:
        print("\n".join(problems), file=sys.stderr, flush=True)
        return 2

    if args.list:
        for rel in SUITES:
            print(rel)
        return 0

    return run_suites(SUITES, ROOT, jobs=args.jobs, timeout=args.timeout)


if __name__ == "__main__":
    sys.exit(main())

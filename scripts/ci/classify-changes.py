#!/usr/bin/env python3
"""Classify changed repository paths for .github/workflows/checks.yml.

Prints one classification line and writes `code=true` or `code=false` to
`--github-output` when given. Fails open (`code=true`) whenever git refs,
commit objects, diffs, or paths cannot be verified cleanly.
"""
from __future__ import annotations

import argparse
from pathlib import Path
import re
import subprocess
import sys

SHA = re.compile(r"^[0-9a-fA-F]{40}$")
ZERO_SHA = "0" * 40
CODE_PATH_PREFIXES = (
    "her/src/",
    "her/tests/",
    "her-memory/src/",
    "her-memory/tests/",
    "her-ui/",
)


def is_doc_only_path(path: str) -> bool:
    """Return True only for Markdown documentation outside code, test, and UI trees."""
    normalized = path.replace("\\", "/")
    if not normalized or normalized.startswith("/"):
        return False
    return normalized.lower().endswith(".md") and not normalized.startswith(CODE_PATH_PREFIXES)


def _has_commit(repo: Path, sha: str) -> bool:
    if not SHA.fullmatch(sha) or sha == ZERO_SHA:
        return False
    done = subprocess.run(
        ["git", "-C", str(repo), "cat-file", "-e", f"{sha}^{{commit}}"],
        capture_output=True,
        check=False,
    )
    return done.returncode == 0


def classify_repo_diff(repo: Path, base: str, head: str) -> tuple[bool, str]:
    """Return `(code, summary)` for `base..head` in `repo`, failing open to `(True, reason)`."""
    if not _has_commit(repo, base):
        return True, "code=true (base-unavailable)"
    if not _has_commit(repo, head):
        return True, "code=true (head-unavailable)"

    diff = subprocess.run(
        ["git", "-C", str(repo), "diff", "--no-renames", "--name-only", "-z", base, head],
        capture_output=True,
        check=False,
    )
    if diff.returncode != 0:
        return True, "code=true (diff-failed)"

    raw_paths = [entry for entry in diff.stdout.split(b"\0") if entry]
    if not raw_paths:
        return True, "code=true (empty-diff)"

    paths: list[str] = []
    for raw in raw_paths:
        try:
            paths.append(raw.decode("utf-8"))
        except UnicodeDecodeError:
            return True, "code=true (non-utf8-path)"

    non_doc = [path for path in paths if not is_doc_only_path(path)]
    if non_doc:
        return True, f"code=true ({len(non_doc)}/{len(paths)} non-doc files; first={non_doc[0]})"
    return False, f"code=false (docs-only: {len(paths)} files)"


def write_github_output(output_path: Path, code: bool) -> None:
    """Append `code=true|false` to the GitHub Actions output file."""
    value = "true" if code else "false"
    with output_path.open("a", encoding="utf-8", newline="\n") as handle:
        handle.write(f"code={value}\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Classify changed paths for CI path filtering.")
    parser.add_argument("--repo", required=True, type=Path, help="Path to git checkout")
    parser.add_argument("--base", default="", help="Base commit SHA")
    parser.add_argument("--head", default="", help="Head commit SHA")
    parser.add_argument("--github-output", type=Path, default=None, help="Path to GITHUB_OUTPUT")
    args = parser.parse_args(argv)

    try:
        code, summary = classify_repo_diff(args.repo, args.base.strip(), args.head.strip())
    except Exception:
        code, summary = True, "code=true (internal-error)"

    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(errors="replace")
    print(f"changes: {summary}")
    if args.github_output is not None:
        write_github_output(args.github_output, code)
    return 0


if __name__ == "__main__":
    sys.exit(main())

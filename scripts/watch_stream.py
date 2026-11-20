#!/usr/bin/env python3
"""Follow a baton watcher's events file and report the watcher's health.

`scripts/issue_watch.py` prints one event and exits by design. On the Windows host a keeper
archives that output into an append-only events file and restarts the watcher; on Linux the keeper
does the same. This script streams that file from a persisted offset, so a Monitor session that
expires and is re-armed prints neither old events again nor loses the ones printed while it was
away, and it prints the watcher's health when it changes.

It starts with one status line, then prints each new event line and each health change (watcher
down or back, watcher state stale or fresh). It follows the recorded watcher pid, not a process
name; a pid file written by the launcher is the source of truth.

Run: python -I scripts/watch_stream.py --events <file> [--pidfile <file>] [--state <file>]
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import time

sys.stdout.reconfigure(encoding='utf-8', errors='replace')

PID_PATTERN = re.compile(r'pid\s+(\d+)')
DEFAULT_INTERVAL = 20.0
DEFAULT_STALE_AFTER = 1200.0


def read_pid(path):
    """The pid recorded in a launcher's pid file, or None."""
    try:
        text = Path(path).read_text(encoding='utf-8-sig')
    except OSError:
        return None
    match = PID_PATTERN.search(text)
    return int(match.group(1)) if match else None


def pid_alive(pid):
    """True or False when the platform can answer, None when it cannot.

    On Windows the shell's `kill -0` cannot see native Windows process ids, and Python's
    os.kill would terminate the process for a signal it does not support, so the check asks
    tasklist instead.
    """
    if pid is None:
        return None
    if os.name == 'nt':
        try:
            done = subprocess.run(['tasklist', '/FI', 'PID eq ' + str(pid), '/NH'],
                                  capture_output=True, text=True, timeout=10)
        except (OSError, subprocess.SubprocessError):
            return None
        if done.returncode != 0:
            return None
        return re.search(r'\s' + str(pid) + r'\s', done.stdout) is not None
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return None
    return True


def read_offset(path):
    """The byte offset a previous run printed up to, 0 when it cannot be read."""
    try:
        value = int(Path(path).read_text(encoding='ascii').strip())
    except (OSError, ValueError):
        return 0
    return max(0, value)


def write_offset(path, offset):
    """Save the offset by atomic replacement; a failed save is reported and leaves the old one."""
    path = Path(path)
    temporary = path.with_name(path.name + '.tmp')
    try:
        temporary.write_text(str(offset) + '\n', encoding='ascii')
        os.replace(temporary, path)
    except OSError as error:
        print('watch: cannot save the offset %s (%s)' % (path, error), file=sys.stderr)


def read_new(path, offset):
    """(complete lines after offset, new offset). A file shorter than offset was truncated."""
    try:
        size = os.path.getsize(path)
    except OSError:
        return [], offset
    if size < offset:
        offset = 0
    if size == offset:
        return [], offset
    with open(path, 'rb') as handle:
        handle.seek(offset)
        data = handle.read(size - offset)
    end = data.rfind(b'\n')
    if end < 0:
        return [], offset
    consumed = data[:end + 1]
    # The keep script writes Windows line endings and a byte-order mark at the file start.
    text = consumed.decode('utf-8-sig' if offset == 0 else 'utf-8', 'replace')
    return [line.rstrip('\r') for line in text.splitlines()], offset + end + 1


def state_age(path, now=None):
    """Seconds since the state file was written, or None when it cannot be read."""
    try:
        mtime = os.stat(path).st_mtime
    except OSError:
        return None
    return max(0.0, (now if now is not None else time.time()) - mtime)


def state_event(stale, age, stale_after):
    """(stale, 'stale' | 'fresh' | None): the state file's transition, if it has one.

    A state file that cannot be read is not a recovery: only a known, fresh age is.
    """
    if age is None:
        return stale, None
    if age > stale_after:
        return True, None if stale else 'stale'
    return (False, 'fresh') if stale else (False, None)


def status_line(args, offset, pid, alive, age):
    watcher = 'none' if pid is None else str(pid)
    if args.pidfile is not None:
        watcher += ' alive' if alive else ' not running' if alive is False else ' unknown'
    state = '' if age is None else ' state %.0fs old' % age
    return 'watch: offset %d watcher %s%s' % (offset, watcher, state)


def follow(args):
    """Print new events and health changes until --once has made one pass."""
    path = args.offset or args.events.with_name(args.events.name + '.offset')
    offset = read_offset(path)
    pid = read_pid(args.pidfile) if args.pidfile is not None else None
    alive = pid_alive(pid) if args.pidfile is not None else None
    age = state_age(args.state) if args.state is not None else None
    print(status_line(args, offset, pid, alive, age), flush=True)
    down = alive is False
    stale = age is not None and age > args.stale_after
    while True:
        lines, offset = read_new(args.events, offset)
        for line in lines:
            print(line, flush=True)
        write_offset(path, offset)
        if args.pidfile is not None:
            pid = read_pid(args.pidfile)
            alive = pid_alive(pid)
            if alive is False and not down:
                print('watch: watcher down pid %s' % pid, flush=True)
                down = True
            elif alive and down:
                print('watch: watcher back pid %s' % pid, flush=True)
                down = False
        if args.state is not None:
            age = state_age(args.state)
            stale, event = state_event(stale, age, args.stale_after)
            if event == 'stale':
                print('watch: state stale %.0fs %s' % (age, args.state), flush=True)
            elif event == 'fresh':
                print('watch: state fresh again', flush=True)
        if args.once:
            return 0
        time.sleep(args.interval)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--events', required=True, type=Path,
                        help='append-only file the keep script writes watcher output to')
    parser.add_argument('--pidfile', type=Path,
                        help="the launcher's pid file; without it the watcher is not followed")
    parser.add_argument('--state', type=Path,
                        help="the watcher's state file; its age reports a stalled poll")
    parser.add_argument('--offset', type=Path,
                        help='where the printed offset is kept (default: <events>.offset)')
    parser.add_argument('--interval', type=float, default=DEFAULT_INTERVAL,
                        help='seconds between reads (default: %(default)s)')
    parser.add_argument('--stale-after', type=float, default=DEFAULT_STALE_AFTER,
                        help='seconds without a state write before it counts as stale (default: %(default)s)')
    parser.add_argument('--once', action='store_true',
                        help='read once, print what is new, and exit')
    args = parser.parse_args(argv)
    if args.interval <= 0:
        parser.error('--interval must be positive')
    if args.stale_after < 0:
        parser.error('--stale-after cannot be negative')
    return follow(args)


if __name__ == '__main__':
    sys.exit(main())
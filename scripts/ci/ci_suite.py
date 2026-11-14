"""Helpers for scripts/ci/run-tests.py: the known-red list, libtest log counts and the shard split.

Known-red list (scripts/ci/known-red.txt), one entry per line, whitespace-separated:
    <os> <target> <test> #<ticket>
os: linux | windows | any. target: a run-tests.py target name (unit or an her/tests stem). test: the
libtest name exactly as printed in `test <name> ... FAILED`; there are no wildcards, so a crash, compile
error, timeout or count mismatch is never known. ticket: the open issue that tracks the failure. Blank
lines and lines starting with '#' are ignored; any other malformed line is an error.
"""
import re

OS_NAMES = ('linux', 'windows', 'any')
TEST_FAILURE_EXIT = 101  # cargo test's exit code when tests failed; anything else is a crash or a harness failure
FAILED_TEST = re.compile(r'^test (\S+) \.\.\. FAILED$')
PASSED_TEST = re.compile(r'^test (\S+) \.\.\. ok$')
FAILED_SUMMARY = re.compile(r'^test result: FAILED\. \d+ passed; (\d+) failed;')
ANY_SUMMARY = re.compile(r'^test result: \w+\. (\d+) passed; (\d+) failed;')
# Minutes per target on the hosted Windows runner; unlisted targets weigh DEFAULT_WEIGHT.
WEIGHTS = {'session_turn_recovery_e2e': 17, 'chat_mode_e2e': 12, 'unit': 12, 'supervisor_dependencies': 9,
           'release_install': 7, 'console_runtime_routes': 4, 'dependency_recovery_e2e': 4, 'health_check': 3,
           'harness_e2e': 2, 'mcp_core_e2e': 2, 'inbound_durable_e2e': 2, 'scripts': 2, 'dependency_versions': 1.5,
           'persona_prompt_e2e': 1.5}
DEFAULT_WEIGHT = 0.5


class ListError(ValueError):
    pass


def parse_known_red(text):
    entries = []
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith('#'):
            continue
        fields = line.split()
        if len(fields) != 4:
            raise ListError(f'line {number}: expected <os> <target> <test> #<ticket>')
        os_name, target, test, ticket = fields
        if os_name not in OS_NAMES:
            raise ListError(f'line {number}: os must be linux, windows or any')
        if not re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9_:.-]*', test):
            raise ListError(f'line {number}: test must be one libtest name (no wildcards)')
        if not re.fullmatch(r'#[1-9][0-9]*', ticket):
            raise ListError(f'line {number}: ticket must be #<number>')
        entries.append({'os': os_name, 'target': target, 'test': test, 'ticket': ticket})
    return entries


def for_os(entries, os_name):
    return [entry for entry in entries if entry['os'] in (os_name, 'any')]


def failed_tests(lines):
    """Names of the failing tests in a libtest log, or None when the log does not account for
    the failure: no single FAILED summary, or its count differs from the named failures."""
    names = [m.group(1) for m in map(FAILED_TEST.match, lines) if m]
    counts = [int(m.group(1)) for m in map(FAILED_SUMMARY.match, lines) if m]
    if not names or len(counts) != 1 or counts[0] != len(names):
        return None
    return names


def passed_tests(lines):
    return {m.group(1) for m in map(PASSED_TEST.match, lines) if m}


def tests_run(lines):
    """Tests that ran (passed plus failed) across every libtest summary in the log."""
    return sum(int(m.group(1)) + int(m.group(2)) for m in map(ANY_SUMMARY.match, lines) if m)


def classify(target, exit_code, failed, entries):
    """(known, unknown) for one target. known: [(test, entry)]; unknown: test names, or
    ['<exit N>'] when the failure is not a plain test failure (compile error, crash, timeout)."""
    if not exit_code:
        return [], []
    if failed is None or exit_code != TEST_FAILURE_EXIT:
        return [], [f'<exit {exit_code}>']
    listed = {entry['test']: entry for entry in entries if entry['target'] == target}
    return [(t, listed[t]) for t in failed if t in listed], [t for t in failed if t not in listed]


def parse_shard(spec):
    match = re.fullmatch(r'([1-9][0-9]*)/([1-9][0-9]*)', spec or '')
    if not match or int(match.group(1)) > int(match.group(2)):
        raise ValueError('--shard must be K/N with 1 <= K <= N')
    return int(match.group(1)), int(match.group(2))


def shard(items, index, count, key=lambda item: item):
    """Items of part `index` (1-based) of `count`, split greedily heaviest first; input order kept."""
    weight = lambda position: WEIGHTS.get(key(items[position]), DEFAULT_WEIGHT)
    loads = [0.0] * count
    owner = {}
    for position in sorted(range(len(items)), key=lambda p: (-weight(p), key(items[p]))):
        slot = min(range(count), key=lambda s: (loads[s], s))
        loads[slot] += weight(position)
        owner[position] = slot
    return [item for position, item in enumerate(items) if owner[position] == index - 1]

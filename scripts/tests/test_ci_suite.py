#!/usr/bin/env python3
"""Tests for scripts/ci/ci_suite.py and the run-tests.py options that use it (known red, shard, only, filter).

Unit cases import ci_suite. End-to-end cases (Linux only) run scripts/ci/run-tests.py with a fake
`cargo` first on PATH: a POSIX shell script that prints libtest-style lines, exits 0 or 101 for the
target named by FAKE_FAIL_TARGET, never builds anything and appends its arguments to FAKE_CARGO_LOG.
Cases that check classification select targets with --only, so the scripts target (real test
scripts) never runs; the shard case runs the full list, where the scripts target fails at once
because the fake build leaves no binaries.
Run: python -I scripts/tests/test_ci_suite.py
"""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
sys.path.insert(0, str(ROOT / 'scripts' / 'ci'))
import ci_suite  # noqa: E402

RUN_TESTS = ROOT / 'scripts' / 'ci' / 'run-tests.py'
FAKE_CARGO = '''#!/bin/sh
if [ -n "$FAKE_CARGO_LOG" ]; then printf '%s\\n' "$*" >> "$FAKE_CARGO_LOG"; fi
if [ "$1" = build ]; then exit 0; fi
target=unit
previous=
for argument in "$@"; do
  if [ "$previous" = --test ]; then target=$argument; fi
  previous=$argument
done
if [ "$target" = "$FAKE_FAIL_TARGET" ]; then
  if [ -z "$FAKE_FAIL_TEST" ]; then exit 101; fi
  echo "test ok_one ... ok"
  echo "test $FAKE_FAIL_TEST ... FAILED"
  echo "failures:"
  echo "test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
  exit 101
fi
if [ -n "$FAKE_NO_MATCH" ]; then
  echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.01s"
  exit 0
fi
echo "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s"
exit 0
'''
LIST_HEADER = '# Known red for the fake run.\n'


class KnownRedList(unittest.TestCase):
    def test_parses_entries_and_ignores_comments_and_blank_lines(self):
        entries = ci_suite.parse_known_red('# note\n\nlinux harness_e2e boom #40\nany unit x #1\n')
        self.assertEqual(entries, [
            {'os': 'linux', 'target': 'harness_e2e', 'test': 'boom', 'ticket': '#40'},
            {'os': 'any', 'target': 'unit', 'test': 'x', 'ticket': '#1'},
        ])

    def test_rejects_malformed_lines(self):
        for line in ('linux harness_e2e boom',
                     'mac harness_e2e boom #40',
                     'linux harness_e2e boom 40',
                     'linux harness_e2e boom #0',
                     'linux harness_e2e boom #x',
                     'linux harness_e2e * #40'):
            with self.subTest(line=line):
                with self.assertRaises(ci_suite.ListError):
                    ci_suite.parse_known_red(line + '\n')

    def test_for_os_keeps_host_and_any(self):
        entries = ci_suite.parse_known_red('linux a t #1\nwindows b t #2\nany c t #3\n')
        kept = ci_suite.for_os(entries, 'linux')
        self.assertEqual([entry['target'] for entry in kept], ['a', 'c'])


class FailedTests(unittest.TestCase):
    SUMMARY_2 = 'test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s'

    def test_names_failures_matching_the_summary(self):
        lines = ['test a ... FAILED', 'test b ... FAILED', self.SUMMARY_2]
        self.assertEqual(ci_suite.failed_tests(lines), ['a', 'b'])

    def test_count_mismatch_is_not_accounted(self):
        lines = ['test a ... FAILED', 'test b ... FAILED', self.SUMMARY_2.replace('2 failed', '3 failed')]
        self.assertIsNone(ci_suite.failed_tests(lines))

    def test_missing_summary_is_not_accounted(self):
        self.assertIsNone(ci_suite.failed_tests(['test a ... FAILED']))

    def test_crlf_text_parses(self):
        text = 'test a ... FAILED\r\ntest b ... FAILED\r\n' + self.SUMMARY_2 + '\r\n'
        self.assertEqual(ci_suite.failed_tests(text.splitlines()), ['a', 'b'])


class Classify(unittest.TestCase):
    def entries(self, text):
        return ci_suite.parse_known_red(text)

    def test_exit_zero_has_no_findings(self):
        self.assertEqual(ci_suite.classify('t', 0, None, []), ([], []))

    def test_all_listed_is_known(self):
        known, unknown = ci_suite.classify('t', 101, ['a', 'b'], self.entries('linux t a #1\nlinux t b #2\n'))
        self.assertEqual([(test, entry['ticket']) for test, entry in known], [('a', '#1'), ('b', '#2')])
        self.assertEqual(unknown, [])

    def test_one_unlisted_is_unknown(self):
        known, unknown = ci_suite.classify('t', 101, ['a', 'b'], self.entries('linux t a #1\n'))
        self.assertEqual([test for test, _ in known], ['a'])
        self.assertEqual(unknown, ['b'])

    def test_crash_without_names_is_unknown(self):
        self.assertEqual(ci_suite.classify('t', 101, None, self.entries('linux t a #1\n')), ([], ['<exit 101>']))

    def test_named_failures_with_an_abnormal_exit_are_unknown(self):
        self.assertEqual(ci_suite.classify('t', 3, ['a'], self.entries('linux t a #1\n')), ([], ['<exit 3>']))

    def test_entries_for_other_targets_are_ignored(self):
        self.assertEqual(ci_suite.classify('t', 101, ['a'], self.entries('linux other a #1\n')), ([], ['a']))


class Shard(unittest.TestCase):
    def test_parse_shard_accepts_k_of_n(self):
        self.assertEqual(ci_suite.parse_shard('2/5'), (2, 5))

    def test_parse_shard_rejects_bad_specs(self):
        for spec in ('0/5', '6/5', '1-5', ''):
            with self.subTest(spec=spec):
                with self.assertRaises(ValueError):
                    ci_suite.parse_shard(spec)

    def test_parts_are_disjoint_complete_ordered_and_deterministic(self):
        items = list(ci_suite.WEIGHTS) + ['x1', 'x2', 'x3']
        parts = [ci_suite.shard(items, k, 5) for k in range(1, 6)]
        self.assertEqual(sorted(sum(parts, [])), sorted(items))
        for part in parts:
            self.assertEqual(part, [item for item in items if item in part])
        self.assertEqual(parts, [ci_suite.shard(items, k, 5) for k in range(1, 6)])


@unittest.skipIf(os.name == 'nt', 'the fake cargo is a POSIX shell script')
class RunTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = Path(self._tmp.name)
        self.bin = self.tmp / 'bin'
        self.bin.mkdir()
        cargo = self.bin / 'cargo'
        cargo.write_text(FAKE_CARGO, encoding='utf-8')
        os.chmod(cargo, 0o755)
        self.cargo_log = self.tmp / 'cargo.log'

    def tearDown(self):
        self._tmp.cleanup()

    def run_tests(self, *args, known=None, fail_target='harness_e2e', fail_test='boom', no_match=False):
        env = {key: value for key, value in os.environ.items() if key not in ('GITHUB_ACTIONS', 'GITHUB_STEP_SUMMARY')}
        env.update(PATH=str(self.bin) + os.pathsep + os.environ.get('PATH', ''),
                   FAKE_FAIL_TARGET=fail_target, FAKE_FAIL_TEST=fail_test, FAKE_NO_MATCH='1' if no_match else '',
                   FAKE_CARGO_LOG=str(self.cargo_log), CARGO_TARGET_DIR=str(self.tmp / 'target'))
        extra = []
        if known is not None:
            known_file = self.tmp / 'known-red.txt'
            known_file.write_text(LIST_HEADER + known, encoding='utf-8')
            extra = ['--known-red', str(known_file)]
        return subprocess.run([sys.executable, '-I', str(RUN_TESTS), '--evidence', str(self.tmp / 'ev'), *extra, *args],
                              env=env, cwd=str(ROOT), capture_output=True, text=True, encoding='utf-8',
                              errors='replace', timeout=600)

    @staticmethod
    def run_names(output):
        return re.findall(r'^RUN (\S+):', output, re.M)

    def test_listed_failure_is_known_red_and_passes(self):
        result = self.run_tests('--only', 'harness_e2e', known='linux harness_e2e boom #40\n')
        out = result.stdout
        self.assertEqual(result.returncode, 0, out + result.stderr)
        self.assertIn('KNOWN RED (1', out)
        self.assertIn('harness_e2e::boom #40', out)
        self.assertIn('UNLISTED FAILURES (0)', out)

    def test_now_passing_names_only_listed_tests_seen_passing(self):
        result = self.run_tests('--only', 'harness_e2e', known='linux harness_e2e ok_one #40\nlinux harness_e2e other #41\n')
        out = result.stdout
        self.assertEqual(result.returncode, 1, out + result.stderr)
        self.assertIn('UNLISTED FAILURES (1', out)
        self.assertIn('harness_e2e::boom', out)
        now_passing = out[out.index('KNOWN RED NOW PASSING'):out.index('UNLISTED FAILURES')]
        self.assertIn('harness_e2e::ok_one #40', now_passing)
        self.assertNotIn('other', now_passing)

    def test_now_passing_is_empty_when_the_target_crashed(self):
        result = self.run_tests('--only', 'harness_e2e', known='linux harness_e2e ok_one #40\n', fail_test='')
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('KNOWN RED NOW PASSING (0)', result.stdout)

    def test_failure_without_known_red_list_fails(self):
        result = self.run_tests('--only', 'harness_e2e')
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_crash_is_never_known(self):
        result = self.run_tests('--only', 'harness_e2e', known='linux harness_e2e boom #40\n', fail_test='')
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('harness_e2e::<exit 101>', result.stdout)

    def test_malformed_known_red_list_exits_2_before_any_run(self):
        result = self.run_tests('--only', 'harness_e2e', known='linux harness_e2e boom\n')
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn('KNOWN-RED LIST ERROR', result.stdout)
        self.assertNotIn('RUN ', result.stdout)

    def test_only_runs_build_and_the_named_target(self):
        result = self.run_tests('--only', 'harness_e2e')
        self.assertEqual(self.run_names(result.stdout), ['build', 'harness_e2e'], result.stdout + result.stderr)

    def test_only_unknown_name_exits_2_listing_valid_names(self):
        result = self.run_tests('--only', 'nope')
        self.assertEqual(result.returncode, 2)
        self.assertIn('nope', result.stdout + result.stderr)
        self.assertIn('harness_e2e', result.stdout + result.stderr)

    def test_only_with_shard_exits_2(self):
        result = self.run_tests('--only', 'harness_e2e', '--shard', '1/5')
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)

    def test_filter_reaches_the_cargo_invocation_before_the_separator(self):
        result = self.run_tests('--only', 'harness_e2e', '--filter', 'boom', fail_target='')
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('--test harness_e2e boom --', self.cargo_log.read_text(encoding='utf-8'))

    def test_filter_on_the_scripts_target_exits_2(self):
        result = self.run_tests('--only', 'scripts', '--filter', 'boom')
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)

    def test_filter_matching_no_test_fails(self):
        result = self.run_tests('--only', 'harness_e2e', '--filter', 'nothing', fail_target='', no_match=True)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('--filter nothing: matched no tests', result.stdout)

    def test_empty_shard_is_an_error(self):
        result = self.run_tests('--only', 'harness_e2e', '--shard', '')
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)

    def test_empty_only_is_an_error(self):
        self.assertEqual(self.run_tests('--only', '').returncode, 2)

    def test_shards_cover_every_target_once_and_always_build(self):
        full = self.run_names(self.run_tests(fail_target='', fail_test='').stdout)
        self.assertIn('scripts', full)
        expected = sorted(name for name in full if name != 'build')
        union = []
        for k in range(1, 6):
            names = self.run_names(self.run_tests('--shard', f'{k}/5', fail_target='', fail_test='').stdout)
            self.assertEqual(names[0], 'build', f'shard {k}/5')
            union.extend(name for name in names if name != 'build')
        self.assertEqual(sorted(union), expected)


class Counts(unittest.TestCase):
    def test_passed_tests_names_only_ok_lines(self):
        self.assertEqual(ci_suite.passed_tests(['test a ... ok', 'test b ... FAILED', 'test c ... ignored']), {'a'})

    def test_tests_run_sums_passed_and_failed_over_every_summary(self):
        lines = ['test result: ok. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s',
                 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s']
        self.assertEqual(ci_suite.tests_run(lines), 3)


if __name__ == '__main__':
    unittest.main()

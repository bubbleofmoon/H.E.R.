#!/usr/bin/env python3
"""Tests for scripts/merge-check.py.

Each case builds a throwaway git repository with a base commit and two branches that both change
src/lib.rs, merges them (cleanly, or by resolving a conflict by hand) and runs the script on the
result: in the worktree with the merge in progress, or with explicit --ours/--theirs/--base revs.
A merge that keeps every test and every brace passes; a dropped test, a dropped or added brace
fails with the file and the numbers. Run: python -I scripts/tests/test_merge_check.py
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
SCRIPT = ROOT / 'scripts' / 'merge-check.py'
GIT_ENV = {
    'GIT_CONFIG_NOSYSTEM': '1', 'GIT_TERMINAL_PROMPT': '0',
    'GIT_AUTHOR_NAME': 'Merge Test', 'GIT_AUTHOR_EMAIL': 'merge.test@example.invalid',
    'GIT_COMMITTER_NAME': 'Merge Test', 'GIT_COMMITTER_EMAIL': 'merge.test@example.invalid',
}

BASE = '''pub fn one() -> u8 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_one() {
        assert_eq!(one(), 1);
    }

    // ours-marker

    // theirs-marker
}
'''


def add_test(text, marker, name, body='assert_eq!(one(), 1);'):
    return text.replace(f'    // {marker}\n',
                        f'    #[test]\n    fn {name}() {{\n        {body}\n    }}\n')


class MergeCheck(unittest.TestCase):
    def setUp(self):
        self.repo = Path(tempfile.mkdtemp(prefix='merge-check-test-'))
        self.addCleanup(shutil.rmtree, self.repo, ignore_errors=True)
        self.git('init', '-q', '-b', 'main')
        (self.repo / 'src').mkdir()
        self.write('src/lib.rs', BASE)
        self.write('notes.md', 'notes\n')
        self.write('src/only_ours.rs', '#[test]\nfn a() {}\n')
        self.git('add', '.')
        self.git('commit', '-q', '-m', 'base')
        self.base = self.git('rev-parse', 'HEAD')
        self.git('checkout', '-q', '-b', 'theirs')
        self.write('src/lib.rs', add_test(BASE, 'theirs-marker', 'theirs_test'))
        self.write('notes.md', 'notes\ntheirs\n')
        self.git('commit', '-q', '-am', 'theirs')
        self.git('checkout', '-q', 'main')
        self.write('src/lib.rs', add_test(BASE, 'ours-marker', 'ours_test'))
        self.write('src/only_ours.rs', '#[test]\nfn a() {}\n#[test]\nfn b() {}\n')
        self.git('commit', '-q', '-am', 'ours')

    def git(self, *args, check=True):
        done = subprocess.run(['git', *args], cwd=self.repo, env={**os.environ, **GIT_ENV},
                              capture_output=True, text=True)
        if check and done.returncode:
            self.fail(f'git {args}: {done.stderr}')
        return done.stdout.strip()

    def write(self, path, text):
        (self.repo / path).write_text(text, encoding='utf-8', newline='\n')

    def read(self, path):
        return (self.repo / path).read_text(encoding='utf-8')

    def check(self, *args):
        done = subprocess.run([sys.executable, '-I', str(SCRIPT), *args], cwd=self.repo,
                              capture_output=True, text=True)
        return done.returncode, done.stdout + done.stderr

    def start_merge(self):
        self.git('merge', '--no-commit', '--no-ff', 'theirs')

    def test_clean_merge_in_progress_passes(self):
        self.start_merge()
        code, out = self.check()
        self.assertEqual(code, 0, out)
        self.assertIn('src/lib.rs', out)
        self.assertNotIn('only_ours', out)
        self.assertNotIn('notes.md', out)

    def test_dropped_test_fails_with_both_counts(self):
        self.start_merge()
        self.write('src/lib.rs', self.read('src/lib.rs').replace(
            '    #[test]\n    fn theirs_test() {\n        assert_eq!(one(), 1);\n    }\n', ''))
        code, out = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn('src/lib.rs', out)
        self.assertIn('tests: working 2, expected 3', out)

    def test_dropped_closing_brace_fails_with_both_counts(self):
        self.start_merge()
        text = self.read('src/lib.rs')
        self.write('src/lib.rs', text[:text.rindex('}')])
        code, out = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn('braces: working 1, expected 0', out)

    def test_extra_brace_fails(self):
        self.start_merge()
        self.write('src/lib.rs', self.read('src/lib.rs') + '}\n')
        code, out = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn('braces: working -1, expected 0', out)

    def test_braces_in_literals_present_on_every_side_do_not_count(self):
        base = BASE.replace('assert_eq!(one(), 1);', 'assert_eq!(format!("{"), "{");')
        self.git('checkout', '-q', '-f', self.base)
        self.write('src/lib.rs', base)
        self.git('commit', '-q', '-am', 'literal brace base')
        new_base = self.git('rev-parse', 'HEAD')
        self.git('checkout', '-q', '-b', 'theirs2')
        self.write('src/lib.rs', add_test(base, 'theirs-marker', 'theirs_test', 'let _ = "}}{";'))
        self.git('commit', '-q', '-am', 'theirs2')
        self.git('checkout', '-q', '-b', 'ours2', new_base)
        self.write('src/lib.rs', add_test(base, 'ours-marker', 'ours_test', 'let _ = "{";'))
        self.git('commit', '-q', '-am', 'ours2')
        self.git('merge', '--no-commit', '--no-ff', 'theirs2')
        code, out = self.check()
        self.assertEqual(code, 0, out)

    def test_conflict_resolved_by_hand_is_checked(self):
        self.git('checkout', '-q', '-f', self.base)
        self.write('src/lib.rs', BASE)
        self.git('checkout', '-q', '-b', 'c-theirs')
        self.write('src/lib.rs', BASE.replace('    // ours-marker\n', '    // ours-marker\n    #[test]\n    fn t() {}\n'))
        self.git('commit', '-q', '-am', 'c-theirs')
        self.git('checkout', '-q', '-b', 'c-ours', self.base)
        self.write('src/lib.rs', BASE.replace('    // ours-marker\n', '    // ours-marker\n    #[test]\n    fn o() {}\n'))
        self.git('commit', '-q', '-am', 'c-ours')
        self.git('merge', '--no-commit', '--no-ff', 'c-theirs', check=False)
        self.assertIn('<<<<<<<', self.read('src/lib.rs'))
        resolved = BASE.replace('    // ours-marker\n',
                                '    // ours-marker\n    #[test]\n    fn o() {}\n    #[test]\n    fn t() {}\n')
        self.write('src/lib.rs', resolved)
        code, out = self.check()
        self.assertEqual(code, 0, out)
        self.write('src/lib.rs', resolved.replace('    #[test]\n    fn t() {}\n', ''))
        code, out = self.check()
        self.assertEqual(code, 1, out)
        self.assertIn('tests: working 2, expected 3', out)

    def test_explicit_revs_without_a_merge_in_progress(self):
        self.git('merge', '--no-ff', '-m', 'merge', 'theirs')
        code, out = self.check('--ours', 'HEAD^1', '--theirs', 'HEAD^2', '--base', self.base)
        self.assertEqual(code, 0, out)
        self.write('src/lib.rs', self.read('src/lib.rs').replace('    #[test]\n    fn ours_test() {\n'
                                                                  '        assert_eq!(one(), 1);\n    }\n', ''))
        code, out = self.check('--ours', 'HEAD^1', '--theirs', 'HEAD^2', '--base', self.base)
        self.assertEqual(code, 1, out)
        self.assertIn('src/lib.rs', out)

    def test_no_merge_and_no_revs_is_a_usage_error(self):
        code, out = self.check()
        self.assertEqual(code, 2, out)

    def test_partial_revs_are_a_usage_error(self):
        code, out = self.check('--ours', 'HEAD')
        self.assertEqual(code, 2, out)


if __name__ == '__main__':
    unittest.main()

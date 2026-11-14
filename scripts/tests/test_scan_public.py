#!/usr/bin/env python3
"""Synthetic-only tests for scripts/scan-public.py (the public guard).

Every sample here is invented: fake 9-digit numbers, a fake GitHub token shape, a
fake user path and a fake turn-ledger line. Run: python -I scripts/tests/test_scan_public.py
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
SCAN = ROOT / 'scripts' / 'scan-public.py'


def gitleaks():
    """The pinned secret scanner: HER_GITLEAKS, then PATH (CI installs it first)."""
    override = os.environ.get('HER_GITLEAKS')
    if override and Path(override).is_file():
        return override
    return shutil.which('gitleaks')


REQUIRES_SCANNER = unittest.skipUnless(gitleaks(), 'gitleaks not found: set HER_GITLEAKS or run scripts/ci/install-gitleaks.py')

CLEAN_TEXT = """# Handoff

Base commit 8d7c372c, branch cutover. The scan runs in tree and text mode.
Two build slots and two test slots exist on the shared Windows host.
"""

# Each sample is assembled from parts so this file stays clean under the guard it
# tests; every value is invented, never copied from a real conversation or machine.
FAKE_NUMBER = '123456' + '789'                                    # 9 invented digits
FAKE_TOKEN = 'ghp_' + 'Ab1cD2eF3gH4iJ5kL6m' + 'N7oP8qR9sT0uV2wX3'  # invented token shape
FAKE_PATH_WIN = 'C:' + chr(92) + 'Users' + chr(92) + 'fakeuser' + chr(92) + 'x.txt'
FAKE_PATH_MINGW = '/c/Users' + '/fakeuser/x.txt'
# A macOS account home is the same kind of personal value as a Windows one: the folder
# under /Users names the person who owns the machine, in any spelling or casing.
FAKE_PATH_MAC = '/Users' + '/fakeuser/x.txt'
FAKE_PATH_MAC_LOWER = '/users' + '/fakeuser/x.txt'
FAKE_PATH_MAC_UPPER = '/US' + 'ERS/FAKEUSER/X.TXT'
FAKE_PATH_MAC_JSON = '{"path":' + ' "/User' + 's/FakeUser/x.txt"}'
FAKE_LEDGER = '{"k":' + '"begun","turn":"t1","key":"group:1","inbound":[]}'
FAKE_LIVE = '{"ts":' + '1,"key":"private:2","predicate":"notTaken"}'


def run(args, text=None, env=None):
    merged = dict(os.environ)
    found = gitleaks()
    if found:
        merged['HER_GITLEAKS'] = found
    merged.update(env or {})
    proc = subprocess.run([sys.executable, '-I', str(SCAN)] + args, input=text,
                          capture_output=True, text=True, encoding='utf-8', env=merged)
    return proc.returncode, (proc.stdout or '') + (proc.stderr or '')


KINDS = ('digit', 'secret', 'user-path', 'chat-record', 'non-text', 'secret-scan', 'history')


def kind_lines(output):
    return [line for line in output.splitlines() if line.split(' ')[0] in KINDS]


class TextMode(unittest.TestCase):
    def kind_lines(self, output):
        return kind_lines(output)

    @REQUIRES_SCANNER
    def test_clean_text_passes(self):
        code, out = run(['--stdin'], CLEAN_TEXT)
        self.assertEqual(code, 0, out)

    @REQUIRES_SCANNER
    def test_digit_run_refused(self):
        code, out = run(['--stdin'], 'the base is ' + FAKE_NUMBER + ' now\n')
        self.assertEqual(code, 1, out)
        self.assertTrue(self.kind_lines(out), out)
        self.assertIn('digit', out)
        self.assertNotIn(FAKE_NUMBER, out)

    def test_token_refused(self):
        code, out = run(['--stdin'], 'set the key to ' + FAKE_TOKEN + '\n')
        self.assertEqual(code, 1, out)
        self.assertIn('secret', out)
        self.assertNotIn(FAKE_TOKEN, out)

    @REQUIRES_SCANNER
    def test_user_path_refused(self):
        for sample in (FAKE_PATH_WIN, FAKE_PATH_MINGW):
            code, out = run(['--stdin'], 'log written to ' + sample + '\n')
            self.assertEqual(code, 1, f'{sample}: {out}')
            self.assertIn('user-path', out)
            self.assertNotIn('fakeuser', out)

    @REQUIRES_SCANNER
    def test_mac_user_path_refused(self):
        for sample in (FAKE_PATH_MAC, FAKE_PATH_MAC_LOWER, FAKE_PATH_MAC_UPPER, FAKE_PATH_MAC_JSON):
            code, out = run(['--stdin'], 'notes at ' + sample + '\n')
            self.assertEqual(code, 1, f'{sample}: {out}')
            self.assertIn('user-path', out)
            self.assertNotIn('fakeuser', out.lower())
            self.assertNotIn('/Users', out)

    @REQUIRES_SCANNER
    def test_mac_user_path_finding_is_metadata_only(self):
        # The refusal must name a kind and a position, never the supplied path or its parts.
        code, out = run(['--stdin'], 'prefix ' + FAKE_PATH_MAC_UPPER + ' suffix\n')
        self.assertEqual(code, 1, out)
        self.assertEqual(self.kind_lines(out), ['user-path <stdin>:1'], out)

    @REQUIRES_SCANNER
    def test_turn_ledger_record_refused(self):
        code, out = run(['--stdin'], FAKE_LEDGER + '\n')
        self.assertEqual(code, 1, out)
        self.assertIn('chat-record', out)

    @REQUIRES_SCANNER
    def test_turn_field_refused(self):
        code, out = run(['--stdin'], 'the record was "turn":"a1" verbatim\n')
        self.assertEqual(code, 1, out)
        self.assertIn('chat-record', out)

    @REQUIRES_SCANNER
    def test_live_log_record_refused(self):
        code, out = run(['--stdin'], FAKE_LIVE + '\n')
        self.assertEqual(code, 1, out)
        self.assertIn('chat-record', out)

    @REQUIRES_SCANNER
    def test_finding_names_kind_and_line_only(self):
        code, out = run(['--stdin'], 'x\n' + FAKE_LEDGER + '\n')
        self.assertEqual(code, 1, out)
        line = self.kind_lines(out)[0]
        self.assertIn('<stdin>:2', line.replace('\r', ''), line)

    def test_missing_gitleaks_exits_3(self):
        code, out = run(['--stdin'], CLEAN_TEXT, {'HER_GITLEAKS': str(ROOT / 'no-such-gitleaks')})
        self.assertEqual(code, 3, out)
        self.assertIn('secret-scan', out)


class TreeMode(unittest.TestCase):
    def tree(self, files):
        tmp = Path(tempfile.mkdtemp(prefix='public-scan-'))
        shutil.copy(ROOT / '.public-scan.json', tmp / '.public-scan.json')
        shutil.copy(ROOT / '.gitleaks.toml', tmp / '.gitleaks.toml')
        for name, text in files.items():
            path = tmp / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding='utf-8')
        return tmp

    def tearDown(self):
        if getattr(self, 'tmp', None):
            shutil.rmtree(self.tmp, ignore_errors=True)
            self.tmp = None

    @REQUIRES_SCANNER
    def test_clean_tree_passes(self):
        self.tmp = self.tree({'docs/a.md': CLEAN_TEXT, 'scripts/b.py': 'X = 1\n'})
        code, out = run(['--root', str(self.tmp)])
        self.assertEqual(code, 0, out)

    @REQUIRES_SCANNER
    def test_tree_refuses_number_path_and_chat(self):
        self.tmp = self.tree({
            'docs/a.md': 'session ' + FAKE_NUMBER + '\n\n' + FAKE_LEDGER + '\n\n' + FAKE_PATH_WIN + '\n'})
        code, out = run(['--root', str(self.tmp)])
        self.assertEqual(code, 1, out)
        for kind in ('digit', 'chat-record', 'user-path'):
            self.assertIn(kind, out)
        self.assertNotIn(FAKE_NUMBER, out)
        self.assertNotIn('fakeuser', out)

    @REQUIRES_SCANNER
    def test_tree_refuses_secret_in_script(self):
        self.tmp = self.tree({'scripts/deploy.ps1': '$key = "' + FAKE_TOKEN + '"\n'})
        code, out = run(['--root', str(self.tmp)])
        self.assertEqual(code, 1, out)
        self.assertIn('secret', out)
        self.assertNotIn(FAKE_TOKEN, out)

    @REQUIRES_SCANNER
    def test_tree_user_path_applies_to_scripts(self):
        self.tmp = self.tree({'scripts/build.ps1': "$env:CARGO_HOME = '" + FAKE_PATH_WIN + "'\n"})
        code, out = run(['--root', str(self.tmp)])
        self.assertEqual(code, 1, out)
        self.assertIn('user-path', out)

    @REQUIRES_SCANNER
    def test_tree_refuses_mac_user_path_in_any_case(self):
        self.tmp = self.tree({
            'docs/a.md': FAKE_PATH_MAC + '\n',
            'notes/b.md': FAKE_PATH_MAC_UPPER + '\n',
            'src/c.json': FAKE_PATH_MAC_JSON + '\n',
            'src/case_variant.json': '   "text": "/US' + 'ERS/FAKEUSER/X",\n',
            'src/product_regex.rs': 'Regex::new(r#"(?i)(?:/home/' + '|/Users/|/etc/)[^\\s\'"<>|]*"#)\n',
        })
        code, out = run(['--root', str(self.tmp)])
        self.assertEqual(code, 1, out)
        self.assertEqual(len(kind_lines(out)), 4, out)
        self.assertNotIn('fakeuser', out.lower())
        self.assertNotIn('product_regex', out)


@REQUIRES_SCANNER
class AddedMode(unittest.TestCase):
    DIRTY = 'session ' + FAKE_NUMBER + '\n'

    def setUp(self):
        self.repo = Path(tempfile.mkdtemp(prefix='public-scan-added-'))
        for name in ('.gitleaks.toml', '.public-scan.json'):
            shutil.copy(ROOT / name, self.repo / name)
        self.git('init', '-q', '-b', 'main')
        self.git('config', 'user.name', 'Guard Test')
        self.git('config', 'user.email', 'guard.test@example.invalid')
        self.git('config', 'commit.gpgsign', 'false')

    def tearDown(self):
        shutil.rmtree(self.repo, ignore_errors=True)

    def git(self, *args):
        proc = subprocess.run(['git', '-C', str(self.repo), *args], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        return proc.stdout.strip()

    def commit(self, files, message):
        for rel, text in files.items():
            path = self.repo / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding='utf-8')
        self.git('add', '--', *files)
        self.git('commit', '-qm', message)
        return self.git('rev-parse', 'HEAD')

    def added(self, *commits, env=None):
        return run(['--root', str(self.repo), '--added-from-stdin'], '\n'.join(commits) + '\n', env)

    def test_a_merge_scans_what_differs_from_each_parent(self):
        self.commit({'docs/a.md': 'clean\n'}, 'base')
        self.git('checkout', '-qb', 'side')
        self.commit({'docs/d.md': self.DIRTY}, 'side')
        self.git('checkout', '-q', 'main')
        self.commit({'docs/e.md': 'clean\n'}, 'main')
        self.git('merge', '-q', '--no-ff', '-m', 'merge', 'side')
        code, out = self.added(self.git('rev-parse', 'HEAD'))
        self.assertEqual(code, 1, out)
        self.assertIn('digit docs/d.md', out)
        self.assertIn('1 commits, 0 whole trees', out)

    def test_a_mode_only_change_scans_the_blob(self):
        self.commit({'a.sh': 'echo clean\n'}, 'base')
        self.git('update-index', '--chmod=+x', 'a.sh')
        self.git('commit', '-qm', 'mode only')
        code, out = self.added(self.git('rev-parse', 'HEAD'))
        self.assertEqual(code, 0, out)
        self.assertIn('1 commits, 0 whole trees, 1 blobs', out)

    def test_an_unreadable_parent_scans_the_whole_tree(self):
        # Coverage of the cat-file reading of parents: a missing parent object is unreadable, never absent.
        base = self.commit({'docs/a.md': 'clean\n'}, 'base')
        tip = self.commit({'docs/b.md': 'clean\n'}, 'tip')
        (self.repo / '.git' / 'objects' / base[:2] / base[2:]).unlink()     # the parent commit object only
        code, out = self.added(tip)
        self.assertEqual(code, 0, out)
        self.assertIn('1 commits, 1 whole trees', out)

    @unittest.skipIf(os.name == 'nt', 'the fake git is a shell script')
    def test_unreadable_diff_tree_output_is_a_finding(self):
        self.commit({'docs/a.md': 'clean\n'}, 'base')
        tip = self.commit({'docs/b.md': 'clean\n'}, 'tip')
        bin_dir = self.repo / 'fake-bin'
        bin_dir.mkdir()
        fake = bin_dir / 'git'
        fake.write_text('#!/bin/sh\nif [ "$3" = diff-tree ]; then printf \':100644 x\\000\'; exit 0; fi\n'
                        f'exec {shutil.which("git")} "$@"\n', encoding='utf-8')
        fake.chmod(0o755)
        code, out = self.added(tip, env={'PATH': str(bin_dir) + os.pathsep + os.environ.get('PATH', '')})
        self.assertEqual(code, 1, out)
        self.assertIn('diff-tree output could not be read', out)

    def test_no_commits_scans_nothing(self):
        code, out = run(['--root', str(self.repo), '--added-from-stdin'], '\n')
        self.assertEqual(code, 0, out)
        self.assertIn('0 commits', out)


if __name__ == '__main__':
    unittest.main(verbosity=2)

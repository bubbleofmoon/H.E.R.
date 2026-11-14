#!/usr/bin/env python3
"""Isolated-repository tests for .githooks/pre-push (the push guard).

Each case builds a throwaway repository in a temporary directory, writes the ref lines git
would feed the hook, and runs the hook directly: no remote, no fetch and no push ever
happens. The samples are invented (an assembled 9-digit run, an assembled fake account-home
path, an assembled token shape); the assertions are about exit codes, refusal kinds and the
absence of the matched text. Run: python -I scripts/tests/test_pre_push_hook.py
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
HOOK = ROOT / '.githooks' / 'pre-push'
COPY = ('.gitleaks.toml', '.public-scan.json', 'scripts/scan-public.py',
        'scripts/tests/test_scan_public.py', '.githooks/pre-push')

DIRTY = 'session ' + '123456' + '789\n'
DIRTY_PATH = '/Users' + '/fakeuser/x.txt\n'
CACHE_DIRTY = 'graphify cache ' + '987654' + '321\n'

FAKE_TOKEN = 'ghp_' + 'Ab1cD2eF3gH4iJ5kL6m' + 'N7oP8qR9sT0uV2wX3'  # invented token shape

GIT_ENV = {
    'GIT_AUTHOR_NAME': 'Guard Test', 'GIT_AUTHOR_EMAIL': 'guard.test@example.invalid',
    'GIT_COMMITTER_NAME': 'Guard Test', 'GIT_COMMITTER_EMAIL': 'guard.test@example.invalid',
    'GIT_CONFIG_NOSYSTEM': '1', 'GIT_TERMINAL_PROMPT': '0',
}


def gitleaks():
    override = os.environ.get('HER_GITLEAKS')
    if override and Path(override).is_file():
        return override
    return shutil.which('gitleaks')


REQUIRES_SCANNER = unittest.skipUnless(gitleaks(), 'gitleaks not found: set HER_GITLEAKS or run scripts/ci/install-gitleaks.py')
BASH = shutil.which('bash') or shutil.which('sh')
REQUIRES_BASH = unittest.skipUnless(BASH, 'bash is not available to run the hook')
TEMP_ROOT = str(Path(tempfile.gettempdir()))


def scrub(text):
    """Captured output with the machine's temporary root replaced.

    The throwaway repositories live under the host temp folder, which sits inside the account
    home, and test output is copied into evidence logs: the findings a test asserts on are
    kinds and positions, so quoting them never needs the host path.
    """
    if not text:
        return text
    return text.replace(TEMP_ROOT, '<temp>').replace(TEMP_ROOT.replace('\\', '/'), '<temp>')


class PushHook(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix='pre-push-'))
        self.repo = self.tmp / 'repo'
        for rel in COPY:
            target = self.repo / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / rel, target)
        self.git('init', '-q', '-b', 'main')
        self.git('config', 'user.name', 'Guard Test')
        self.git('config', 'user.email', 'guard.test@example.invalid')
        self.git('config', 'commit.gpgsign', 'false')

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def git(self, *args):
        env = dict(os.environ)
        env.update(GIT_ENV)
        proc = subprocess.run(['git', '-C', str(self.repo), *args], capture_output=True, text=True, env=env)
        self.assertEqual(proc.returncode, 0,
                         f'git {" ".join(args)}: {scrub(proc.stdout)}{scrub(proc.stderr)}')
        return proc.stdout.strip()

    def write(self, rel, text):
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8')

    def commit(self, message):
        self.git('add', '-A')
        self.git('commit', '-qm', message)
        return self.git('rev-parse', 'HEAD')

    def head(self, ref='HEAD'):
        return self.git('rev-parse', ref)

    def scan_objects(self, *oids):
        """Runs the message pass directly, so a receipt can name the pass that refused."""
        env = dict(os.environ)
        env.update(GIT_ENV)
        if gitleaks():
            env['HER_GITLEAKS'] = gitleaks()
        proc = subprocess.run([BASH, str(ROOT / 'scripts' / 'scan-public.py'), '--objects-from-stdin'],
                              cwd=self.repo, input='\n'.join(oids) + '\n',
                              capture_output=True, text=True, encoding='utf-8', env=env)
        return scrub((proc.stdout or '') + (proc.stderr or ''))

    def push(self, *lines, extra=None):
        """Run the hook exactly as git does: repository as cwd, ref lines on stdin; `extra` overrides variables."""
        env = dict(os.environ)
        env.update(GIT_ENV)
        if gitleaks():
            env['HER_GITLEAKS'] = gitleaks()
        env.update(extra or {})
        proc = subprocess.run([BASH, str(HOOK), 'origin', 'https://example.invalid/guard.git'],
                              cwd=self.repo, input='\n'.join(lines) + '\n',
                              capture_output=True, text=True, encoding='utf-8', env=env)
        return proc.returncode, scrub((proc.stdout or '') + (proc.stderr or ''))

    def ref(self, name, remote_sha='0' * 40):
        return f'refs/heads/{name} {self.head(name)} refs/heads/{name} {remote_sha}'

    def tag(self, name, message):
        self.git('tag', '-a', '-m', message, name)
        return self.git('for-each-ref', '--format=%(objectname)', f'refs/tags/{name}')

    def tag_ref(self, name, oid, remote_sha='0' * 40):
        # The ref line carries the annotated tag object, exactly as git feeds it to the hook.
        return f'refs/tags/{name} {oid} refs/tags/{name} {remote_sha}'

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_clean_push_passes(self):
        self.write('docs/a.md', 'the scan runs in tree, text and history mode\n')
        self.commit('clean')
        code, out = self.push(self.ref('main'))
        self.assertEqual(code, 0, out)

    # --- published messages, not only blobs (R2-4) ---------------------------------
    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_clean_annotated_tag_message_passes(self):
        self.write('docs/a.md', 'clean note\n')
        self.commit('clean')
        oid = self.tag('v0-1-0', 'release notes are clean prose')
        code, out = self.push(self.tag_ref('v0-1-0', oid))
        self.assertEqual(code, 0, out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_dirty_new_commit_message_is_refused_although_every_file_is_clean(self):
        self.write('docs/a.md', 'clean note\n')
        self.write('docs/b.md', 'clean note too\n')
        sha = self.commit('release notes ' + '123456' + '789 and a profile path ' + '/Users' + '/fakeuser/x')
        code, out = self.push(f'refs/heads/main {sha} refs/heads/main {"0" * 40}')
        self.assertEqual(code, 1, out)
        self.assertIn('digit', out)
        self.assertIn('user-path', out)
        self.assertIn('commit ', self.scan_objects(sha))        # the message pass reports it
        self.assertNotIn('123456' + '789', out)
        self.assertNotIn('fakeuser', out.lower())

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_dirty_annotated_tag_message_is_refused_although_every_file_is_clean(self):
        self.write('docs/a.md', 'clean note\n')
        self.commit('clean')
        oid = self.tag('v0-2-0', 'tagged build ' + '987654' + '321')
        code, out = self.push(f'refs/tags/v0-2-0 {oid} refs/tags/v0-2-0 {"0" * 40}')
        self.assertEqual(code, 1, out)
        self.assertIn('digit', out)
        self.assertNotIn('987654' + '321', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_secret_in_a_new_commit_message_is_refused_although_every_file_is_clean(self):
        # The gitleaks rules scan messages as they scan blobs: a clean tree with a token in a
        # commit message is published prose, and the token is refused without being shown.
        self.write('docs/a.md', 'clean note\n')
        sha = self.commit('release note with a token ' + FAKE_TOKEN)
        code, out = self.push(f'refs/heads/main {sha} refs/heads/main {"0" * 40}')
        self.assertEqual(code, 1, out)
        self.assertIn('secret commit', out)
        self.assertNotIn(FAKE_TOKEN, out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_secret_in_an_annotated_tag_message_is_refused_although_every_file_is_clean(self):
        self.write('docs/a.md', 'clean note\n')
        self.commit('clean')
        oid = self.tag('v0-3-0', 'tagged build with a token ' + FAKE_TOKEN)
        code, out = self.push(f'refs/tags/v0-3-0 {oid} refs/tags/v0-3-0 {"0" * 40}')
        self.assertEqual(code, 1, out)
        self.assertIn('secret tag', out)
        self.assertNotIn(FAKE_TOKEN, out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_tag_of_a_tag_is_refused(self):
        # The outer tag's own message is clean, but the inner tag object is published too and its
        # message would leave unscanned: a tag whose target is a tag is refused.
        self.write('docs/a.md', 'clean note\n')
        self.commit('clean')
        self.git('tag', '-a', '-m', 'tagged build ' + '987654' + '321', 'inner-tag')
        self.git('tag', '-a', '-m', 'clean', 'outer-tag', 'inner-tag')
        oid = self.git('for-each-ref', '--format=%(objectname)', 'refs/tags/outer-tag')
        code, out = self.push(self.tag_ref('outer-tag', oid))
        self.assertNotEqual(code, 0, out)
        self.assertIn('tag of a tag', out)
        self.assertNotIn('987654' + '321', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_an_already_published_commit_message_is_not_rescanned(self):
        # Only what the push adds is published prose: a base commit already on the remote is
        # not in the outgoing object set, and the push of the clean tip stays clean.
        self.write('docs/a.md', 'clean note\n')
        base = self.commit('base is on the remote ' + '556677' + '889')
        self.write('docs/b.md', 'clean note\n')
        tip = self.commit('clean tip')
        code, out = self.push(f'refs/heads/main {tip} refs/heads/main {base}')
        self.assertEqual(code, 0, out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_dirty_alternate_ref_is_refused_although_the_checkout_is_clean(self):
        self.write('docs/a.md', 'clean note\n')
        self.commit('clean main')
        self.git('checkout', '-qb', 'other')
        self.write('docs/b.md', DIRTY)
        self.commit('dirty other branch')
        self.git('checkout', '-q', 'main')                       # the working tree is clean again
        code, out = self.push(self.ref('main'), self.ref('other'))
        self.assertEqual(code, 1, out)
        self.assertIn('digit', out)
        self.assertNotIn('123456' + '789', out)          # the matched run is never shown

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_dirty_ancestor_with_a_clean_tip_is_refused(self):
        self.write('docs/a.md', DIRTY_PATH)
        self.commit('dirty ancestor')
        self.write('docs/a.md', 'clean note\n')
        self.commit('clean tip')
        code, out = self.push(self.ref('main'))
        self.assertEqual(code, 1, out)
        self.assertIn('user-path', out)
        self.assertNotIn('fakeuser', out.lower())

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_dirty_ancestor_after_an_already_published_base_is_refused(self):
        self.write('docs/a.md', 'clean note\n')
        base = self.commit('base is already on the remote')
        self.git('checkout', '-qb', 'work')
        self.write('docs/b.md', DIRTY)
        self.commit('dirty work')
        self.write('docs/b.md', 'clean note\n')
        self.commit('clean tip again')
        code, out = self.push(self.ref('work', remote_sha=base))
        self.assertEqual(code, 1, out)
        self.assertIn('digit', out)
        self.assertNotIn('123456' + '789', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_tracked_blob_under_a_cache_like_path_is_scanned(self):
        # The tree scan skips this folder locally; committing a blob under it publishes it.
        self.write('docs/a.md', 'clean note\n')
        self.write('graphify-out/notes.txt', CACHE_DIRTY)
        self.commit('tracked blob under a cache-like path')
        code, out = self.push(self.ref('main'))
        self.assertEqual(code, 1, out)
        self.assertIn('digit', out)
        self.assertNotIn('987654' + '321', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_uninspectable_outgoing_ref_fails_closed(self):
        self.write('docs/a.md', 'clean note\n')
        self.commit('clean')
        code, out = self.push(f'refs/heads/main {"f" * 40} refs/heads/main {"0" * 40}')
        self.assertEqual(code, 1, out)
        self.assertIn('cannot be inspected', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_ref_deletion_publishes_nothing(self):
        self.write('docs/a.md', 'clean note\n')
        self.commit('clean')
        code, out = self.push(f'refs/heads/main {"0" * 40} refs/heads/main {self.head("main")}')
        self.assertEqual(code, 0, out)

    # --- a branch that moves forward publishes only what its new commits add -----------
    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_one_file_branch_update_scans_that_one_file(self):
        self.write('docs/a.md', 'clean note\n')
        base = self.commit('base is on the remote')
        self.write('docs/b.md', 'clean note too\n')
        tip = self.commit('one new file')
        code, out = self.push(f'refs/heads/main {tip} refs/heads/main {base}')
        self.assertEqual(code, 0, out)
        self.assertIn('Public scan (added): 0 findings', out)
        self.assertIn('1 commits, 0 whole trees, 1 blobs', out)
        self.assertIn('Public scan (history): 0 findings (none); 0 revisions', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_dirty_blob_in_a_branch_update_is_refused(self):
        def build():
            self.write('docs/b.md', DIRTY)
            self.commit('dirty new file')
        code, out = self.branch_update(build)
        self.assertEqual(code, 1, out)
        self.assertIn('the public scan refused the blobs this push adds', out)
        self.assertIn('1 commits, 0 whole trees', out)
        self.assertNotIn('123456' + '789', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_deletion_only_update_scans_no_blob(self):
        self.write('docs/a.md', 'clean note\n')
        self.write('docs/b.md', 'clean note too\n')
        base = self.commit('base is on the remote')
        self.git('rm', '-q', 'docs/b.md')
        tip = self.commit('delete a file')
        code, out = self.push(f'refs/heads/main {tip} refs/heads/main {base}')
        self.assertEqual(code, 0, out)
        added = [line for line in out.splitlines() if line.startswith('Public scan (added)')]
        self.assertTrue(added, out)
        self.assertIn('0 blobs', added[0])

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_force_push_keeps_the_whole_tree_scan(self):
        self.write('docs/a.md', 'clean note\n')
        base = self.commit('base')
        self.git('checkout', '-qb', 'old')
        self.write('docs/r.md', 'clean note\n')
        remote = self.commit('remote history, sibling of the local one')
        self.git('checkout', '-q', 'main')
        self.write('docs/l.md', 'clean note\n')
        local = self.commit('local history')
        code, out = self.push(f'refs/heads/main {local} refs/heads/main {remote}')
        self.assertEqual(code, 0, out)
        self.assertNotIn('Public scan (history): 0 findings (none); 0 revisions', out)
        self.assertIn('Public scan (added): 0 findings (none); 0 commits', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_new_ref_keeps_the_whole_tree_scan(self):
        self.write('docs/a.md', 'clean note\n')
        tip = self.commit('clean')
        code, out = self.push(f'refs/heads/main {tip} refs/heads/main {"0" * 40}')
        self.assertEqual(code, 0, out)
        self.assertNotIn('Public scan (history): 0 findings (none); 0 revisions', out)
        self.assertIn('Public scan (added): 0 findings (none); 0 commits', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_an_unknown_remote_sha_keeps_the_whole_tree_scan(self):
        self.write('docs/a.md', 'clean note\n')
        tip = self.commit('clean')
        code, out = self.push(f'refs/heads/main {tip} refs/heads/main {"e" * 40}')
        self.assertEqual(code, 0, out)
        self.assertNotIn('Public scan (history): 0 findings (none); 0 revisions', out)
        self.assertIn('Public scan (added): 0 findings (none); 0 commits', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_an_unrelated_root_merged_into_an_update_is_scanned_whole(self):
        self.write('docs/a.md', 'clean note\n')
        base = self.commit('base is on the remote')
        self.git('checkout', '-q', '--orphan', 'side')
        self.write('docs/c.md', DIRTY)
        self.commit('side root holds a dirty file')
        self.git('rm', '-q', 'docs/c.md')
        self.commit('side removes the dirty file')
        self.git('checkout', '-q', 'main')
        self.git('merge', '-q', '--allow-unrelated-histories', '-m', 'merge side', 'side')
        tip = self.head()
        code, out = self.push(f'refs/heads/main {tip} refs/heads/main {base}')
        self.assertEqual(code, 1, out)
        self.assertIn('digit', out)
        self.assertIn('1 whole trees', out)
        self.assertNotIn('123456' + '789', out)

    # --- a branch that moves forward scans only what its new commits add ---------------
    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_branch_pushed_to_a_tag_keeps_the_whole_tree_scan(self):
        self.write('docs/a.md', 'clean note\n')
        base = self.commit('base')
        self.write('docs/b.md', 'clean note too\n')
        tip = self.commit('tip')
        code, out = self.push(f'refs/heads/main {tip} refs/tags/v1 {base}')
        self.assertEqual(code, 0, out)
        self.assertIn('Public scan (added): 0 findings (none); 0 commits', out)
        self.assertNotIn('Public scan (history): 0 findings (none); 0 revisions', out)

    def branch_update(self, build, seed=None):
        """Base on main, the update on branch `work`, then main checked out again: the working tree is clean,
        so only the scans of what the push publishes can refuse it."""
        self.write('docs/a.md', 'clean note\n')
        for rel, text in (seed or {}).items():
            self.write(rel, text)
        base = self.commit('base is on the remote')
        self.git('checkout', '-qb', 'work')
        build()
        tip = self.head()
        self.git('checkout', '-q', 'main')
        return self.push(f'refs/heads/work {tip} refs/heads/work {base}')

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_dirty_file_added_and_deleted_in_an_update_is_refused(self):
        def build():
            self.write('docs/d.md', DIRTY)
            self.commit('add a dirty file')
            self.git('rm', '-q', 'docs/d.md')
            self.commit('delete it again')
        code, out = self.branch_update(build)
        self.assertEqual(code, 1, out)
        self.assertIn('the public scan refused the blobs this push adds', out)
        self.assertIn('2 commits, 0 whole trees', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_dirty_modification_cleaned_later_is_refused(self):
        def build():
            self.write('docs/a.md', DIRTY)
            self.commit('dirty edit')
            self.write('docs/a.md', 'clean note again\n')
            self.commit('clean it')
        code, out = self.branch_update(build)
        self.assertEqual(code, 1, out)
        self.assertIn('2 commits, 0 whole trees', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_merge_whose_side_added_and_removed_a_dirty_file_is_refused(self):
        def build():
            self.git('checkout', '-qb', 'side')
            self.write('docs/d.md', DIRTY)
            self.commit('side adds a dirty file')
            self.git('rm', '-q', 'docs/d.md')
            self.commit('side removes it')
            self.git('checkout', '-q', 'work')
            self.write('docs/e.md', 'clean note\n')
            self.commit('work moves on')
            self.git('merge', '-q', '--no-ff', '-m', 'merge side', 'side')
        code, out = self.branch_update(build)
        self.assertEqual(code, 1, out)
        self.assertIn('the public scan refused the blobs this push adds', out)
        self.assertIn('0 whole trees', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_rename_that_makes_a_record_line_count_is_refused(self):
        record = 'const SAMPLE: &str = r#"{"turn' + '":"t1"}"#;\n'          # clean in .rs, a record in .md

        def build():
            self.git('mv', 'src/x.rs', 'docs/x.md')
            self.commit('rename to markdown')
        code, out = self.branch_update(build, seed={'src/x.rs': record})
        self.assertEqual(code, 1, out)
        self.assertIn('chat-record', out)
        self.assertIn('0 whole trees', out)

    @REQUIRES_SCANNER
    @REQUIRES_BASH
    def test_a_replaced_blob_is_scanned_as_pushed(self):
        def build():
            self.write('docs/d.md', DIRTY)
            self.commit('dirty file')
            dirty = self.git('rev-parse', 'HEAD:docs/d.md')
            (self.tmp / 'clean.txt').write_text('clean note\n', encoding='utf-8')
            clean = self.git('hash-object', '-w', str(self.tmp / 'clean.txt'))
            self.git('replace', dirty, clean)
        code, out = self.branch_update(build)
        self.assertEqual(code, 1, out)
        self.assertIn('digit', out)

    @REQUIRES_BASH                                                       # no scanner needed
    def test_a_missing_scanner_is_its_own_refusal(self):
        self.write('docs/a.md', 'clean note\n')
        tip = self.commit('clean')
        code, out = self.push(f'refs/heads/main {tip} refs/heads/main {"0" * 40}',
                              extra={'HER_GITLEAKS': str(self.tmp / 'no-such-gitleaks')})
        self.assertEqual(code, 3, out)
        self.assertIn('refusal=scanner-missing', out)


if __name__ == '__main__':
    unittest.main(verbosity=2)

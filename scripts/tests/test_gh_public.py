#!/usr/bin/env python3
"""Stub-gh tests for scripts/gh-public.ps1 and scripts/gh-public.sh (the posting guard).

Every case runs a wrapper with a fake `gh` on PATH that records its arguments to the file
named by GH_STUB_MARK instead of contacting GitHub, so a case that reaches gh is visible and
a case that must refuse cannot publish anything. The bodies are synthetic (an assembled
9-digit run, an assembled fake account-home path) and the assertions cover both the exit code
and the absence of the supplied value in every output stream.
Run: python -I scripts/tests/test_gh_public.py
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
PUBLIC_REPO = 'bubbleofmoon/' + 'H.E.R.'
WRAPPER_FILES = {'sh': ROOT / 'scripts' / 'gh-public.sh', 'ps1': ROOT / 'scripts' / 'gh-public.ps1'}

CLEAN_BODY = 'The scan runs over the body file before gh is called.\n'
DIRTY_BODY = 'session ' + '123456' + '789\n'
DIRTY_TITLE = 'ticket ' + '987654' + '321\n'
# R2-2: a continuation line is where a line-oriented field scan loses text.
MULTILINE_DIRTY = 'a clean first line\n' + '987654' + '321 in a continuation line\ntail'
MULTILINE_CLEAN = 'a clean first line\na clean continuation line\nthe last line'
# R2-3: a value that looks like a flag is still only a value.
FLAG_SHAPED = '--body-file=' + 'body.md'
FAKE_HOME_DIR = 'C:' + chr(92) + 'Users' + chr(92) + 'fakeuser'

BASH = shutil.which('bash') or shutil.which('sh')
POWERSHELL = shutil.which('powershell') or shutil.which('powershell.exe')
# A host runs the wrappers it can start: bash for the .sh, PowerShell for the .ps1.
KINDS = [kind for kind, path in WRAPPER_FILES.items()
         if path.is_file() and ((kind == 'sh' and BASH) or (kind == 'ps1' and POWERSHELL))]
REQUIRES_HOST = unittest.skipUnless(KINDS, 'neither bash nor powershell is available')


def gitleaks():
    override = os.environ.get('HER_GITLEAKS')
    if override and Path(override).is_file():
        return override
    return shutil.which('gitleaks')


REQUIRES_SCANNER = unittest.skipUnless(gitleaks(), 'gitleaks not found: set HER_GITLEAKS or run scripts/ci/install-gitleaks.py')
TEMP_ROOT = str(Path(tempfile.gettempdir()))


def scrub(text):
    """A captured output with the machine's temporary root replaced.

    A failure message that quotes an output must not carry the host account home (the temp
    path is inside it), because test output lands in evidence logs; the kinds, positions and
    exit codes a test asserts on are untouched.
    """
    if not text:
        return text
    return text.replace(TEMP_ROOT, '<temp>').replace(TEMP_ROOT.replace('\\', '/'), '<temp>')


class WrapperCase(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix='gh-public-'))
        bin_dir = self.tmp / 'bin'
        bin_dir.mkdir()
        self.mark = self.tmp / 'gh-invoked.txt'
        (bin_dir / 'gh').write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$@" > "$GH_STUB_MARK"\nexit ${GH_STUB_EXIT:-0}\n', encoding='utf-8')
        (bin_dir / 'gh').chmod(0o755)  # bash skips a non-executable stub and would run the real gh
        (bin_dir / 'gh.cmd').write_text('@echo off\r\n>"%GH_STUB_MARK%" echo %*\r\nif defined GH_STUB_EXIT exit /b %GH_STUB_EXIT%\r\nexit /b 0\r\n', encoding='utf-8')
        self.body = self.tmp / 'body.md'
        self.body.write_text(CLEAN_BODY, encoding='utf-8')
        self.other = self.tmp / 'other.md'
        self.other.write_text(DIRTY_BODY, encoding='utf-8')
        self.env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
        self.env['GH_STUB_MARK'] = str(self.mark)
        self.env['PATH'] = str(bin_dir) + os.pathsep + self.env.get('PATH', '')
        if gitleaks():
            self.env['HER_GITLEAKS'] = gitleaks()

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_wrapper(self, kind, *args):
        wrapper = str(WRAPPER_FILES[kind])
        if kind == 'sh':
            self.assertTrue(BASH, 'bash is not available')
            command = [BASH, wrapper, *[str(a) for a in args]]
        else:
            self.assertTrue(POWERSHELL, 'powershell is not available')
            command = [POWERSHELL, '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', wrapper,
                       *[str(a) for a in args]]
        proc = subprocess.run(command, cwd=str(self.tmp), capture_output=True, text=True,
                              encoding='utf-8', errors='replace', env=self.env, timeout=600)
        return proc.returncode, scrub((proc.stdout or '') + (proc.stderr or ''))

    def assert_no_gh(self, out, kind):
        self.assertFalse(self.mark.exists(), f'{kind}: gh was invoked: {out}')

    def assert_clean_output(self, out, kind):
        # A refusal may name a kind and a position, never a supplied path, number or title.
        for value in ('fakeuser', '123456' + '789', '987654' + '321', 'Users'):
            self.assertNotIn(value, out, f'{kind}: the refusal echoed a supplied value: {out}')

    # --- the supported path -------------------------------------------------------
    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_clean_body_is_posted_once_with_the_given_arguments(self):
        for kind in KINDS:
            self.mark.unlink(missing_ok=True)
            code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'comment', '166',
                                         '--repo', PUBLIC_REPO, '--body-file', str(self.body))
            self.assertEqual(code, 0, f'{kind}: {out}')
            self.assertTrue(self.mark.exists(), kind)
            posted = self.mark.read_text(encoding='utf-8')
            self.assertIn('issue', posted)
            self.assertIn(PUBLIC_REPO, posted)
            self.assertIn('body.md', posted)

    # --- body sources that were never scanned -------------------------------------
    @REQUIRES_HOST
    def test_alternative_and_duplicate_body_flags_never_reach_gh(self):
        cases = [
            ['--body=inline text'],                                                  # --body
            ['--body', 'inline'],                                                    # --body split
            ['--body=' + DIRTY_BODY.strip()],                                        # --body with content
            ['--body-file=' + str(self.other)],                                      # equals form, other file
            ['--body-file=-'],                                                       # stdin
            ['--body-file', str(self.other)],                                        # second, different file
            ['-F', 'body=@' + str(self.other)],                                      # -F uploads a file
            ['-f', 'body=' + DIRTY_BODY.strip()],                                    # -f inline field
            ['--attach-file', str(self.body)],                                       # attachment upload
            ['--upload-file', str(self.body)],                                       # attachment upload
            ['--fill'],                                                              # gh drafts the body itself
            ['--web'],                                                               # the browser posts unscanned text
            ['--editor'],                                                            # the editor posts unscanned text
            ['--body-file', '-'],                                                    # stdin, bash spelling
        ]
        for kind in KINDS:
            for extra in cases:
                if kind == 'ps1' and extra == ['--body-file', '-']:
                    continue      # powershell -File does not pass a lone - through as a value
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'comment', '166',
                                             '--repo', PUBLIC_REPO, *extra)
                self.assertEqual(code, 2, f'{kind} {extra}: {out}')
                self.assert_no_gh(out, f'{kind} {extra}')
                self.assert_clean_output(out, f'{kind} {extra}')

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_unscanned_published_text_fields_are_scanned(self):
        for kind in KINDS:
            self.mark.unlink(missing_ok=True)
            code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'create',
                                         '--repo', PUBLIC_REPO, '--title', DIRTY_TITLE.strip())
            self.assertEqual(code, 1, f'{kind}: {out}')
            self.assert_no_gh(out, f'{kind} dirty title')
            self.assertIn('refusal=scan-findings', out)
            self.mark.unlink(missing_ok=True)
            code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'create',
                                         '--repo', PUBLIC_REPO, '--title', 'clean title', '--label', DIRTY_TITLE.strip())
            self.assertEqual(code, 1, f'{kind}: {out}')
            self.assert_no_gh(out, f'{kind} dirty label')

    @REQUIRES_HOST
    def test_unsupported_commands_never_reach_gh(self):
        cases = [['repo', 'edit'], ['api', 'repos/x/y'], ['release', 'create', 'v1'],
                 ['issue', 'delete', '166'], ['issue', 'edit', '166'], ['gh', 'issue', 'comment', '166'],
                 ['browse'], [], ['issue'], ['issue', 'comment']]
        for kind in KINDS:
            for group in cases:
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, '--body-file', str(self.body), *group,
                                             '--repo', PUBLIC_REPO, '--body-file', str(self.body))
                self.assertEqual(code, 2, f'{kind} {group}: {out}')
                self.assert_no_gh(out, f'{kind} {group}')

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_a_dirty_body_still_refuses(self):
        for kind in KINDS:
            self.mark.unlink(missing_ok=True)
            code, out = self.run_wrapper(kind, '--body-file', str(self.other), 'issue', 'comment', '166',
                                         '--repo', PUBLIC_REPO, '--body-file', str(self.other))
            self.assertEqual(code, 1, f'{kind}: {out}')
            self.assert_no_gh(out, kind)
            self.assertIn('digit', out)
            self.assert_clean_output(out, kind)

    # --- repository selectors ------------------------------------------------------
    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_repository_selectors_are_normalised_and_enforced(self):
        accepted = [['--repo', PUBLIC_REPO], ['--repo=' + PUBLIC_REPO], ['-R', PUBLIC_REPO],
                    ['-R' + PUBLIC_REPO], ['-R=' + PUBLIC_REPO],
                    ['--repo', PUBLIC_REPO, '-R', PUBLIC_REPO]]
        refused = [['--repo', 'bubbleofmoon/' + 'other'], ['--repo=' + 'someone/' + 'H.E.R.'],
                   ['-R', 'setjoin/' + 'H.E.R.'], ['-Rsetjoin/' + 'H.E.R.'],
                   ['--repo', PUBLIC_REPO, '--repo', 'someone/' + 'H.E.R.'],
                   ['--repo', PUBLIC_REPO, '-R', 'someone/' + 'H.E.R.'],
                   ['--repo', ''], []]
        for kind in KINDS:
            for selectors in accepted:
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'comment', '166',
                                             *selectors, '--body-file', str(self.body))
                self.assertEqual(code, 0, f'{kind} {selectors}: {out}')
                self.assertIn(PUBLIC_REPO, self.mark.read_text(encoding='utf-8'))
            for selectors in refused:
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'comment', '166',
                                             *selectors, '--body-file', str(self.body))
                self.assertEqual(code, 2, f'{kind} {selectors}: {out}')
                self.assert_no_gh(out, f'{kind} {selectors}')
                self.assertIn('refusal=repository-selector', out)
                self.assertNotIn('someone', out)

    @REQUIRES_HOST
    def test_foreign_repository_urls_never_reach_gh(self):
        urls = ['https://github.com/someone/' + 'H.E.R./issues/166',
                'https://github.com/setjoin/' + 'H.E.R./issues/166',
                'github.com/other/repo/issues/1']
        for kind in KINDS:
            for url in urls:
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'comment', url,
                                             '--repo', PUBLIC_REPO, '--body-file', str(self.body))
                self.assertEqual(code, 2, f'{kind} {url}: {out}')
                self.assert_no_gh(out, f'{kind} {url}')
                self.assertIn('refusal=repository-url', out)
                self.assertNotIn('someone', out)
            self.mark.unlink(missing_ok=True)
            code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'comment',
                                         'https://github.com/' + PUBLIC_REPO + '/issues/166',
                                         '--repo', PUBLIC_REPO, '--body-file', str(self.body))
            self.assertEqual(code, 2, out)  # a URL is not an issue number for gh issue comment
            self.assert_no_gh(out, 'public url positional')

    # --- refusal diagnostics -------------------------------------------------------
    @REQUIRES_HOST
    def test_missing_body_file_never_echoes_the_supplied_value(self):
        hidden = FAKE_HOME_DIR + chr(92) + 'missing body.md'
        for kind in KINDS:
            # the wrapper-level --body-file value is missing
            self.mark.unlink(missing_ok=True)
            code, out = self.run_wrapper(kind, '--body-file', hidden, 'issue', 'comment', '166',
                                         '--repo', PUBLIC_REPO, '--body-file', hidden)
            self.assertEqual(code, 2, f'{kind}: {out}')
            self.assert_no_gh(out, f'{kind} missing wrapper body')
            self.assert_clean_output(out, f'{kind} missing wrapper body')
            self.assertIn('refusal=missing-file', out)
            self.assertIn('argument-position=2', out)
            # the gh-level --body-file value is missing while the wrapper-level one exists
            self.mark.unlink(missing_ok=True)
            code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'comment', '166',
                                         '--repo', PUBLIC_REPO, '--body-file', hidden)
            self.assertEqual(code, 2, f'{kind}: {out}')
            self.assert_no_gh(out, f'{kind} missing gh body')
            self.assert_clean_output(out, f'{kind} missing gh body')
            self.assertIn('refusal=missing-file', out)
            self.assertIn('argument-position=9', out)

    # --- multiline published text (R2-2) ------------------------------------------
    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_multiline_title_and_label_text_is_scanned_losslessly(self):
        """A finding may not hide on a continuation line of a title or a label."""
        for kind in KINDS:
            for field in ('--title', '--label'):
                extra = ['--title', 'clean title'] if field == '--label' else []
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'create',
                                             '--repo', PUBLIC_REPO, *extra, field, MULTILINE_DIRTY)
                if kind == 'ps1':
                    # PowerShell 5.1 splits a newline-containing argument into several
                    # arguments, so this wrapper refuses the whole field instead of scanning
                    # less than gh would publish. Refusal is before gh either way.
                    self.assertEqual(code, 2, f'{kind} multiline {field}: {out}')
                    self.assertIn('refusal=unsupported-argument', out, f'{kind} multiline {field}')
                else:
                    self.assertEqual(code, 1, f'{kind} multiline {field}: {out}')
                    self.assertIn('refusal=scan-findings', out, f'{kind} multiline {field}')
                    self.assertIn(f'field={field[2:]}', out, f'{kind} multiline {field}')
                self.assert_no_gh(out, f'{kind} multiline {field}')
                self.assert_clean_output(out, f'{kind} multiline {field}')

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_a_clean_multiline_field_is_forwarded_in_full(self):
        """A clean multiline field is forwarded whole, or refused: never forwarded half-scanned.

        bash keeps a newline inside one argument, so the .sh wrapper forwards the whole field.
        Windows PowerShell 5.1 cannot carry a newline inside one native argument, so the .ps1
        wrapper refuses such a field rather than publishing less than it scanned.
        """
        for kind in KINDS:
            self.mark.unlink(missing_ok=True)
            code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'create',
                                         '--repo', PUBLIC_REPO, '--title', MULTILINE_CLEAN,
                                         '--label', MULTILINE_CLEAN)
            if kind == 'ps1':
                self.assertEqual(code, 2, f'{kind}: {out}')
                self.assert_no_gh(out, f'{kind} multiline forwarding')
                self.assertIn('refusal=unsupported-argument', out)
                continue
            self.assertEqual(code, 0, f'{kind}: {out}')
            self.assertTrue(self.mark.exists(), kind)
            posted = self.mark.read_text(encoding='utf-8')
            for line in MULTILINE_CLEAN.splitlines():
                self.assertIn(line, posted, f'{kind}: a field line never reached gh')

    # --- a value is never an option (R2-3) -----------------------------------------
    @REQUIRES_HOST
    def test_a_flag_shaped_field_value_cannot_stand_in_for_the_body(self):
        """Only a real body option is a body source; a title or label that looks like one is text."""
        strict = [                                                    # must be a body-source refusal
            ['issue', 'create', '--repo', PUBLIC_REPO, '--title', FLAG_SHAPED],
            ['issue', 'create', '--repo', PUBLIC_REPO, '--label', FLAG_SHAPED],
        ]
        any_refusal = [                                               # refused, kind not fixed
            ['issue', 'create', '--repo', PUBLIC_REPO, '--title', '--body-file', 'body.md'],
            ['issue', 'create', '--repo', PUBLIC_REPO, '--label', '--body-file', 'body.md'],
            ['issue', 'comment', '166', '--repo', PUBLIC_REPO, '--label', '--body-file', 'body.md'],
            ['issue', 'create', '--repo', PUBLIC_REPO, '--title', '--label'],
        ]
        for kind in KINDS:
            for group in strict:
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, *group)
                self.assertEqual(code, 2, f'{kind} {group}: {out}')
                self.assert_no_gh(out, f'{kind} {group}')
                self.assertIn('refusal=missing-body-file', out, f'{kind} {group}')
            for group in any_refusal:
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, *group)
                self.assertEqual(code, 2, f'{kind} {group}: {out}')
                self.assert_no_gh(out, f'{kind} {group}')
                self.assertIn('refusal=', out, f'{kind} {group}')
            if kind == 'sh':      # powershell -File does not pass a lone -- through as a value
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, 'issue', 'create', '--repo', PUBLIC_REPO, '--title', '--')
                self.assertEqual(code, 2, out)
                self.assert_no_gh(out, 'lone -- title value')
                self.assertIn('refusal=missing-body-file', out)

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_a_field_value_that_looks_like_a_flag_stays_text(self):
        """With a real body present, a flag-shaped title is forwarded as the title it is."""
        for kind in KINDS:
            self.mark.unlink(missing_ok=True)
            code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'create',
                                         '--repo', PUBLIC_REPO, '--title', FLAG_SHAPED)
            self.assertEqual(code, 0, f'{kind}: {out}')
            self.assertTrue(self.mark.exists(), kind)
            posted = self.mark.read_text(encoding='utf-8')
            self.assertIn('issue', posted)
            self.assertIn('body.md', posted)
            self.assertIn('--title', posted)
            # the flag-shaped value is title text; the file it names is never read as a body
            self.assertNotIn(DIRTY_BODY.strip(), posted, f'{kind}: an unscanned file became a body')

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_create_without_a_title_is_refused(self):
        """gh would prompt for a title that was never scanned, so create refuses a call without --title."""
        for kind in KINDS:
            for group in (['issue', 'create'], ['pr', 'create']):
                self.mark.unlink(missing_ok=True)
                code, out = self.run_wrapper(kind, '--body-file', str(self.body), *group,
                                             '--repo', PUBLIC_REPO)
                self.assertEqual(code, 2, f'{kind} {group}: {out}')
                self.assert_no_gh(out, f'{kind} {group}')
                self.assertIn('refusal=missing-title', out, f'{kind} {group}')

    @REQUIRES_HOST
    def test_a_missing_scanner_is_its_own_refusal(self):
        for kind in KINDS:
            self.mark.unlink(missing_ok=True)
            self.env['HER_GITLEAKS'] = str(self.tmp / 'no-such-gitleaks')
            code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'comment', '166',
                                         '--repo', PUBLIC_REPO)
            self.assertEqual(code, 3, f'{kind}: {out}')
            self.assertIn('refusal=scanner-missing', out)
            self.assertNotIn('refusal=scan-findings', out)
            self.assert_no_gh(out, kind)

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_a_gh_failure_after_a_clean_scan_exits_4(self):
        for kind in KINDS:
            self.mark.unlink(missing_ok=True)
            self.env['GH_STUB_EXIT'] = '1'
            code, out = self.run_wrapper(kind, '--body-file', str(self.body), 'issue', 'comment', '166',
                                         '--repo', PUBLIC_REPO)
            self.assertEqual(code, 4, f'{kind}: {out}')
            self.assertIn('refusal=gh-failed', out)
            self.assertTrue(self.mark.exists(), kind)


if __name__ == '__main__':
    unittest.main(verbosity=2)

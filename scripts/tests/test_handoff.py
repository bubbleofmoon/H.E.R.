#!/usr/bin/env python3
"""Stub-gh tests for scripts/handoff.sh and scripts/handoff.ps1 (the one-step handoff).

Every case runs a launcher with a fake `gh` first on PATH. The fake records each call to a log
instead of contacting GitHub and answers the issue and label reads from environment settings,
so a refused handoff shows up as a call that never reached `issue comment` or `issue edit`. The
ps1 cases run under Windows PowerShell where it exists, else under pwsh. The bodies are
synthetic: the digit run is assembled from parts.
Run: python -I scripts/tests/test_handoff.py
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent.parent
PUBLIC_REPO = 'bubbleofmoon/' + 'H.E.R.'
ARROW = chr(0x2192)
DIRTY = 'session ' + '123456' + '789\n'
SCRIPTS = {'sh': ROOT / 'scripts' / 'handoff.sh', 'ps1': ROOT / 'scripts' / 'handoff.ps1'}
BASH = shutil.which('bash') or shutil.which('sh')
POWERSHELL = next((found for found in map(shutil.which, ('powershell', 'powershell.exe', 'pwsh')) if found), None)
KINDS = [kind for kind in SCRIPTS if (kind == 'sh' and BASH) or (kind == 'ps1' and POWERSHELL)]
REQUIRES_HOST = unittest.skipUnless(KINDS, 'neither bash nor PowerShell is available')
TEMP_ROOT = str(Path(tempfile.gettempdir()))

GH_STUB = '''"""Fake gh for the handoff tests: logs each call and answers the reads from the environment."""
import json
import os
import sys

argv = sys.argv[1:]
with open(os.environ['GH_STUB_LOG'], 'a', encoding='utf-8') as log:
    log.write(json.dumps(argv) + '\\n')
fail = [item.strip() for item in os.environ.get('GH_STUB_FAIL', '').split(';') if item.strip()]
if ' '.join(argv[:2]) in fail:
    sys.exit(1)


def names(value):
    return [item for item in value.split(',') if item]


if argv[:2] == ['issue', 'view']:
    print(os.environ.get('GH_STUB_STATE', 'OPEN'))
    for label in names(os.environ.get('GH_STUB_ISSUE_LABELS', '')):
        print(label)
elif argv[:2] == ['label', 'list']:
    for label in names(os.environ.get('GH_STUB_REPO_LABELS', 'baton:linux,baton:windows,baton:owner')):
        print(label)
sys.exit(0)
'''


def gitleaks():
    override = os.environ.get('HER_GITLEAKS')
    if override and Path(override).is_file():
        return override
    return shutil.which('gitleaks')


REQUIRES_SCANNER = unittest.skipUnless(gitleaks(), 'gitleaks not found: set HER_GITLEAKS or run scripts/ci/install-gitleaks.py')


def scrub(text):
    """Captured output with the machine's temporary root replaced (the temp folder sits in the account home)."""
    if not text:
        return text
    return text.replace(TEMP_ROOT, '<temp>').replace(TEMP_ROOT.replace('\\', '/'), '<temp>')


class HandoffCase(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix='handoff-'))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        bin_dir = self.tmp / 'bin'
        bin_dir.mkdir()
        (bin_dir / 'gh_stub.py').write_text(GH_STUB, encoding='utf-8')
        (bin_dir / 'gh').write_text('#!/usr/bin/env bash\nexec "$GH_STUB_PY" "$(dirname "$0")/gh_stub.py" "$@"\n',
                                    encoding='utf-8', newline='\n')
        (bin_dir / 'gh').chmod(0o755)  # bash skips a non-executable stub and would run the real gh
        (bin_dir / 'gh.cmd').write_text('@"%GH_STUB_PY%" "%~dp0gh_stub.py" %*\r\n', encoding='utf-8')
        self.log = self.tmp / 'gh-calls.jsonl'
        self.body = self.tmp / 'body.md'
        self.env = {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}
        for var in ('GH_STUB_FAIL', 'GH_STUB_STATE', 'GH_STUB_ISSUE_LABELS', 'GH_STUB_REPO_LABELS'):
            self.env.pop(var, None)
        self.env['GH_STUB_PY'] = sys.executable
        self.env['GH_STUB_LOG'] = str(self.log)
        self.env['PATH'] = str(bin_dir) + os.pathsep + self.env.get('PATH', '')
        if gitleaks():
            self.env['HER_GITLEAKS'] = gitleaks()

    def write_body(self, side, text='The handoff text is clean.\n'):
        self.body.write_text(f'**Handoff {ARROW} {side}**\n\n{text}', encoding='utf-8')

    def run_handoff(self, kind, *args):
        script = str(SCRIPTS[kind])
        if kind == 'sh':
            command = [BASH, script, *[str(a) for a in args]]
        else:
            command = [POWERSHELL, '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', script,
                       *[str(a) for a in args]]
        proc = subprocess.run(command, cwd=str(self.tmp), capture_output=True, text=True,
                              encoding='utf-8', errors='replace', env=self.env, timeout=600)
        return proc.returncode, scrub((proc.stdout or '') + (proc.stderr or ''))

    def calls(self):
        if not self.log.exists():
            return []
        return [json.loads(line) for line in self.log.read_text(encoding='utf-8').splitlines() if line.strip()]

    def writes(self):
        return [call for call in self.calls() if call[:2] in (['issue', 'comment'], ['issue', 'edit'])]

    def reset_log(self):
        self.log.unlink(missing_ok=True)

    # --- refusals: nothing is written -------------------------------------------------
    @REQUIRES_HOST
    def test_a_bad_side_changes_nothing(self):
        self.write_body('windows')
        for kind in KINDS:
            for side in ('linuxx', 'Linux', 'baton:linux'):
                self.reset_log()
                code, out = self.run_handoff(kind, 80, side, self.body)
                self.assertEqual(code, 2, f'{kind} {side}: {out}')
                self.assertIn('handoff: refusal=', out, f'{kind} {side}: {out}')
                self.assertEqual(self.calls(), [], f'{kind} {side}')

    @REQUIRES_HOST
    def test_a_repository_without_the_baton_label_changes_nothing(self):
        self.write_body('owner')
        self.env['GH_STUB_REPO_LABELS'] = 'baton:linux,baton:windows'
        for kind in KINDS:
            self.reset_log()
            code, out = self.run_handoff(kind, 80, 'owner', self.body)
            self.assertEqual(code, 2, f'{kind}: {out}')
            self.assertIn('refusal=unknown-label', out, kind)
            self.assertEqual(self.writes(), [], kind)

    @REQUIRES_HOST
    def test_a_heading_for_another_side_changes_nothing(self):
        self.write_body('windows')
        for kind in KINDS:
            self.reset_log()
            code, out = self.run_handoff(kind, 80, 'linux', self.body)
            self.assertEqual(code, 2, f'{kind}: {out}')
            self.assertIn('refusal=heading', out, kind)
            self.assertEqual(self.calls(), [], kind)

    @REQUIRES_HOST
    def test_a_closed_or_unreadable_issue_changes_nothing(self):
        self.write_body('linux')
        base = dict(self.env)
        cases = [({'GH_STUB_STATE': 'CLOSED'}, 'issue-not-open'),
                 ({'GH_STUB_FAIL': 'issue view'}, 'issue-read'),
                 ({'GH_STUB_FAIL': 'label list'}, 'label-read')]
        for kind in KINDS:
            for extra, reason in cases:
                self.env = dict(base, **extra)
                self.reset_log()
                code, out = self.run_handoff(kind, 80, 'linux', self.body)
                self.assertEqual(code, 2, f'{kind} {reason}: {out}')
                self.assertIn(f'refusal={reason}', out, f'{kind} {reason}')
                self.assertEqual(self.writes(), [], f'{kind} {reason}')
        self.env = base

    @REQUIRES_HOST
    def test_bad_issue_numbers_and_usage_change_nothing(self):
        self.write_body('linux')
        for kind in KINDS:
            for args in (['x', 'linux', self.body], ['0', 'linux', self.body], ['080', 'linux', self.body],
                         [80, 'linux']):
                self.reset_log()
                code, out = self.run_handoff(kind, *args)
                self.assertEqual(code, 2, f'{kind} {args}: {out}')
                self.assertIn('handoff: refusal=', out, f'{kind} {args}')
                self.assertEqual(self.calls(), [], f'{kind} {args}')

    @REQUIRES_HOST
    def test_a_missing_scanner_changes_nothing(self):
        self.write_body('windows')
        self.env['HER_GITLEAKS'] = str(self.tmp / 'no-such-gitleaks')
        for kind in KINDS:
            self.reset_log()
            code, out = self.run_handoff(kind, 80, 'windows', self.body)
            self.assertEqual(code, 3, f'{kind}: {out}')
            self.assertIn('labels unchanged', out, kind)
            self.assertEqual(self.writes(), [], kind)

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_a_refused_body_changes_nothing(self):
        self.body.write_text(f'**Handoff {ARROW} windows**\n\n{DIRTY}', encoding='utf-8')
        for kind in KINDS:
            self.reset_log()
            code, out = self.run_handoff(kind, 80, 'windows', self.body)
            self.assertEqual(code, 1, f'{kind}: {out}')
            self.assertEqual(self.writes(), [], kind)
            self.assertNotIn('123456' + '789', out, kind)

    # --- the handoff itself: comment first, then the label ---------------------------------
    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_a_handoff_posts_then_moves_the_baton(self):
        self.write_body('windows')
        self.env['GH_STUB_ISSUE_LABELS'] = 'linux,baton:linux,in-progress'
        for kind in KINDS:
            self.reset_log()
            code, out = self.run_handoff(kind, 80, 'windows', self.body)
            self.assertEqual(code, 0, f'{kind}: {out}')
            writes = self.writes()
            self.assertEqual(len(writes), 2, f'{kind}: {writes}')
            comment, edit = writes
            self.assertEqual(comment[:3], ['issue', 'comment', '80'], kind)
            self.assertIn(PUBLIC_REPO, comment, kind)
            self.assertEqual(edit[:3], ['issue', 'edit', '80'], kind)
            self.assertIn('--add-label', edit, kind)
            self.assertIn('baton:windows', edit, kind)
            self.assertIn('--remove-label', edit, kind)
            self.assertIn('baton:linux', edit, kind)
            self.assertFalse(any('in-progress' in arg for arg in edit), kind)

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_every_other_baton_is_removed(self):
        self.write_body('windows')
        self.env['GH_STUB_ISSUE_LABELS'] = 'baton:linux,baton:owner,baton:windows'
        for kind in KINDS:
            self.reset_log()
            code, out = self.run_handoff(kind, 80, 'windows', self.body)
            self.assertEqual(code, 0, f'{kind}: {out}')
            edit = self.writes()[1]
            self.assertEqual(edit[edit.index('--remove-label') + 1], 'baton:linux,baton:owner', kind)

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_a_label_failure_after_the_post_is_loud(self):
        self.write_body('windows')
        self.env['GH_STUB_FAIL'] = 'issue edit'
        for kind in KINDS:
            self.reset_log()
            code, out = self.run_handoff(kind, 80, 'windows', self.body)
            self.assertEqual(code, 4, f'{kind}: {out}')
            self.assertIn('gh failed while moving the label', out, kind)
            writes = self.writes()
            self.assertEqual(writes[0][:2], ['issue', 'comment'], kind)

    @REQUIRES_HOST
    @REQUIRES_SCANNER
    def test_a_failed_comment_post_moves_no_label(self):
        self.write_body('windows')
        self.env['GH_STUB_ISSUE_LABELS'] = 'baton:linux'
        self.env['GH_STUB_FAIL'] = 'issue comment'
        for kind in KINDS:
            self.reset_log()
            code, out = self.run_handoff(kind, 80, 'windows', self.body)
            self.assertEqual(code, 4, f'{kind}: {out}')
            self.assertIn('gh failed while posting the comment', out, kind)
            self.assertEqual([c[:2] for c in self.writes()], [['issue', 'comment']], f'{kind}: a label moved')

    # --- both launchers agree (no host needed) ----------------------------------------
    def test_both_scripts_refuse_the_same_kinds_and_stay_ascii(self):
        sh = SCRIPTS['sh'].read_text(encoding='utf-8')
        ps1 = SCRIPTS['ps1'].read_text(encoding='utf-8')
        self.assertEqual(set(re.findall(r'refuse ([a-z-]+)', sh)), set(re.findall(r"Refuse '([a-z-]+)'", ps1)))
        for path in SCRIPTS.values():
            path.read_bytes().decode('ascii')


if __name__ == '__main__':
    unittest.main(verbosity=2)

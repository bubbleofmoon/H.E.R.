"""Timeout receipts, owned process trees and evidence retention for native release proof."""
import ctypes as c
from ctypes import wintypes as w
import json
from pathlib import Path
import shutil
import subprocess
import fixture_paths as tempfile
import time

from fixture_process import hidden


class ProcessTree:
    """Enroll a suspended child before it can spawn; retain the job after leader exit."""

    def __init__(self, command, **kwargs):
        self.api = c.WinDLL('kernel32', use_last_error=True)
        for name, arguments, result in [
            ('CreateJobObjectW', [c.c_void_p, w.LPCWSTR], w.HANDLE),
            ('AssignProcessToJobObject', [w.HANDLE, w.HANDLE], w.BOOL),
            ('TerminateJobObject', [w.HANDLE, w.UINT], w.BOOL),
            ('SetInformationJobObject', [w.HANDLE, c.c_int, c.c_void_p, w.DWORD], w.BOOL),
            ('QueryInformationJobObject', [w.HANDLE, c.c_int, c.c_void_p, w.DWORD, c.c_void_p], w.BOOL),
            ('CloseHandle', [w.HANDLE], w.BOOL),
        ]:
            function = getattr(self.api, name)
            function.argtypes, function.restype = arguments, result
        self.job = self.api.CreateJobObjectW(None, None)
        if not self.job:
            raise c.WinError(c.get_last_error())
        try:
            # JOBOBJECT_EXTENDED_LIMIT_INFORMATION: LimitFlags is at byte 16
            # on both ABIs. Kill on last handle close also covers runner death.
            limits = c.create_string_buffer(144 if c.sizeof(c.c_void_p) == 8 else 112)
            w.DWORD.from_buffer(limits, 16).value = 0x2000
            if not self.api.SetInformationJobObject(self.job, 9, limits, len(limits)):
                raise c.WinError(c.get_last_error())
            self.process = subprocess.Popen(command, creationflags=subprocess.CREATE_NO_WINDOW | 4, **kwargs)
            if not self.api.AssignProcessToJobObject(self.job, w.HANDLE(int(self.process._handle))):
                raise c.WinError(c.get_last_error())
            resume = c.WinDLL('ntdll').NtResumeProcess
            resume.argtypes, resume.restype = [w.HANDLE], c.c_long
            if resume(w.HANDLE(int(self.process._handle))) != 0:
                raise RuntimeError('Cannot resume the enrolled fixture child')
        except BaseException:
            if hasattr(self, 'process'):
                self.process.kill()
                self.process.wait(timeout=10)
            self.api.CloseHandle(self.job)
            raise

    def stop(self):
        if not self.job:
            return
        if not self.api.TerminateJobObject(self.job, 124):
            raise c.WinError(c.get_last_error())
        deadline = time.monotonic() + 10
        while True:
            accounting = c.create_string_buffer(48)
            if not self.api.QueryInformationJobObject(self.job, 1, accounting, 48, None):
                raise c.WinError(c.get_last_error())
            if w.DWORD.from_buffer(accounting, 40).value == 0:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError('Fixture job still has active descendants')
            time.sleep(.02)
        self.process.wait(timeout=10)
        self.api.CloseHandle(self.job)
        self.job = None


class ReleaseFixture:
    def __init__(self, root, evidence, task_names=(), timeout=None, archive_exclusions=('node_modules',)):
        tempfile.development_root(root)
        tempfile.development_root(evidence)
        self.root, self.evidence = Path(root), Path(evidence)
        self.task_names, self.timeout = list(task_names), timeout
        self.archive_exclusions = set(archive_exclusions)
        self.trees = []

    def start(self, command, **kwargs):
        tree = ProcessTree(list(map(str, command)), **kwargs)
        self.trees.append(tree)
        return tree.process

    def run(self, command, label, timeout=60, override_timeout=True, **kwargs):
        command = list(map(str, command))
        (self.evidence / (label + '.command.json')).write_text(json.dumps(command))
        code = 125  # launch/cleanup failure, never a successful receipt
        try:
            with (self.evidence / (label + '.stdout.log')).open('w', encoding='utf-8') as out, \
                    (self.evidence / (label + '.stderr.log')).open('w', encoding='utf-8') as err:
                process = self.start(command, stdout=out, stderr=err, **kwargs)
                try:
                    budget = self.timeout if override_timeout and self.timeout is not None else timeout
                    code = process.wait(timeout=budget)
                except subprocess.TimeoutExpired:
                    code = 124
                    (self.evidence / (label + '.timeout.json')).write_text(json.dumps({'timeoutSeconds': budget}))
                    self.trees[-1].stop()
        finally:
            (self.evidence / (label + '.exit.txt')).write_text(str(code))
        return code

    def finish(self):
        errors = []
        try:
            for tree in reversed(self.trees):
                try:
                    tree.stop()
                except Exception as error:
                    errors.append(str(error))
            # Task Scheduler children are outside the launcher's job. Remove only
            # preselected GUID tasks, then reap pinned processes in this unique root.
            command = ['powershell.exe', '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass',
                       '-File', str(Path(__file__).with_name('release-fixture-cleanup.ps1')),
                       '-FixtureRoot', str(self.root), '-Output', str(self.evidence / 'cleanup.json'),
                       '-TaskNames', ','.join(self.task_names)]
            try:
                code = self.run(command, 'cleanup', timeout=45, override_timeout=False)
                if code != 0:
                    errors.append('Fixture task/process cleanup failed: ' + str(code))
            except Exception as error:
                errors.append(str(error))
            finally:
                # The cleanup helper has exited; its job must also be released.
                if len(self.trees):
                    try:
                        self.trees[-1].stop()
                    except Exception as error:
                        errors.append(str(error))
        finally:
            try:
                for file in self.root.rglob('*'):
                    if file.is_file() and file.suffix in ('.json', '.jsonl', '.log', '.txt') and not set(file.relative_to(self.root).parts) & self.archive_exclusions:
                        target = self.evidence / 'artifacts' / file.relative_to(self.root)
                        target.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copyfile(file, target)
            except Exception as error:
                errors.append('Artifact archive failed: ' + str(error))
            (self.evidence / 'finalize.json').write_text(json.dumps({'errors': errors, 'root': str(self.root)}))
            if errors:
                raise RuntimeError('Fixture retained at ' + str(self.root) + ': ' + '; '.join(errors))
            if self.root.exists():
                assert self.root.resolve().parent == Path(tempfile.gettempdir()).resolve() and self.root.name.startswith('relproof-')
                account = subprocess.check_output(['whoami'], text=True, **hidden()).strip()
                for flags in (['/grant', account + ':(OI)(CI)F', '/T', '/C'], ['/reset', '/T', '/C']):
                    subprocess.run(['icacls', str(self.root), *flags], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **hidden(), timeout=60, check=True)
                shutil.rmtree(self.root)
            print('Fixture directory removed: ' + str(self.root))

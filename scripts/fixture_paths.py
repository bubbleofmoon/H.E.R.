"""Fixture allocation delegates the production boundary to the built Rust CLI."""
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile as _tempfile
import weakref


def her_cli():
    selected = os.environ.get('HER_TEST_CLI')
    candidates = [Path(selected)] if selected else []
    for argument in sys.argv[1:]:
        candidate = Path(argument)
        if candidate.name.lower() in ('her', 'her.exe', 'her-cli', 'her-cli.exe'):
            candidates.append(candidate.with_name('her-cli.exe' if os.name == 'nt' else 'her-cli'))
    target = os.environ.get('CARGO_TARGET_DIR')
    if target:
        candidates.extend(Path(target) / mode / ('her-cli.exe' if os.name == 'nt' else 'her-cli')
                          for mode in ('debug', 'release'))
    repo = Path(__file__).resolve().parent.parent
    candidates.extend(repo / 'her/target' / mode / ('her-cli.exe' if os.name == 'nt' else 'her-cli')
                      for mode in ('debug', 'release'))
    cli = next((candidate for candidate in candidates if candidate.is_file()), None)
    if cli is None:
        raise RuntimeError('Set HER_TEST_CLI to the built her-cli before allocating fixture roots')
    return cli


def development_root(path):
    cli = her_cli()
    env = os.environ.copy()
    env.pop('HER_RUNTIME_ROOT', None)
    selected = Path(path)
    if not selected.is_absolute():
        selected = Path.cwd() / selected
    result = subprocess.run([str(cli), 'install-root', 'check-development', '--runtime-root',
                             str(selected)], env=env, stdin=subprocess.DEVNULL,
                            capture_output=True, text=True, timeout=15,
                            **({'creationflags': subprocess.CREATE_NO_WINDOW} if os.name == 'nt' else {}))
    if result.returncode:
        raise RuntimeError('fixture root refused: ' + result.stderr.strip())
    return Path(path)


def gettempdir():
    # tempfile probes candidates by writing files; check them before it can probe production.
    for candidate in ([_tempfile.tempdir] if _tempfile.tempdir else _tempfile._candidate_tempdir_list()):
        development_root(candidate)
    return str(development_root(_tempfile.gettempdir()))


def mkdtemp(suffix=None, prefix=None, dir=None):
    parent = Path(dir if dir is not None else gettempdir())
    for _ in range(100):
        path = development_root(parent / (('tmp' if prefix is None else prefix) + secrets.token_hex(8) + (suffix or '')))
        try:
            path.mkdir(mode=0o700)
            return str(path.absolute())
        except FileExistsError:
            continue
    raise FileExistsError('No unused fixture directory name')


class TemporaryDirectory(_tempfile.TemporaryDirectory):
    def __init__(self, suffix=None, prefix=None, dir=None, ignore_cleanup_errors=False, *, delete=True):
        self.name = mkdtemp(suffix, prefix, dir)
        self._ignore_cleanup_errors = ignore_cleanup_errors
        self._delete = delete
        self._finalizer = weakref.finalize(self, self._cleanup, self.name,
                                          warn_message=f'Implicitly cleaning up {self!r}',
                                          ignore_errors=ignore_cleanup_errors, delete=delete)

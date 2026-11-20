#!/usr/bin/env python3
"""Stage the exact DSH npm releases and run the bridge credential contract on Linux."""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]
PACKAGE = '@deepseek-ai/dsh'
REGISTRY = 'https://registry.npmjs.org/'
VERSIONS = ('0.2.1-rc.2', '0.2.1-rc.5')
TEST = 'dsh_credential::contract::settings_describe_cookie_contract'
READY = re.compile(rb'^dsh web: (http://127\.0\.0\.1:[1-9][0-9]{0,4})/\S*\r?$', re.MULTILINE)
STARTUP_SECONDS = 180
TEST_SECONDS = 15


def tool(name):
    path = shutil.which(name)
    if path is None:
        raise RuntimeError(f'{name} is required for dsh_contract')
    # Preserve npm's link name, which selects the npm CLI when invoked.
    return Path(path).absolute()


def stage(root, node, version):
    scratch_home = root / 'npm-home'
    scratch_home.mkdir()
    stage_dir = root / 'package'
    stage_dir.mkdir()
    (stage_dir / 'package.json').write_text('{"private":true}\n', encoding='utf-8')
    userconfig = scratch_home / 'user.npmrc'
    globalconfig = scratch_home / 'global.npmrc'
    userconfig.touch()
    globalconfig.touch()
    subprocess.run(
        [tool('npm'), 'install', '--save-exact', '--ignore-scripts', '--no-audit', '--no-fund',
         '--fetch-retries=1', '--registry', REGISTRY, '--cache', scratch_home / 'cache',
         '--userconfig', userconfig, '--globalconfig', globalconfig, '--', f'{PACKAGE}@{version}'],
        cwd=stage_dir, env={'HOME': str(scratch_home), 'PATH': f'{node.parent}:{os.defpath}'},
        stdin=subprocess.DEVNULL, check=True, timeout=600,
    )
    location = f'node_modules/{PACKAGE}'
    lock = json.loads((stage_dir / 'package-lock.json').read_text(encoding='utf-8'))
    record = lock['packages'][location]
    expected_tarball = f'{REGISTRY}{PACKAGE}/-/dsh-{version}.tgz'
    if (record['version'] != version or record['resolved'] != expected_tarball
            or not record.get('integrity', '').startswith('sha512-')):
        raise RuntimeError('npm lock did not resolve the exact registry package with integrity')
    package = (stage_dir / location).resolve(strict=True)
    if not package.is_relative_to(stage_dir):
        raise RuntimeError('installed DSH package is outside the staging directory')
    manifest = json.loads((package / 'package.json').read_text(encoding='utf-8'))
    if manifest['name'] != PACKAGE or manifest['version'] != version:
        raise RuntimeError('installed DSH package identity differs from the requested release')
    entry = (package / manifest['bin']['dsh']).resolve(strict=True)
    if not entry.is_relative_to(package) or not entry.is_file():
        raise RuntimeError('installed DSH executable is outside the staged package or missing')
    return entry


def build_test():
    command = ['cargo', 'test', '--locked', '--features', 'parity-clock', '--bin', 'her',
               '--no-run', '--message-format=json-render-diagnostics']
    build = subprocess.run(command, cwd=ROOT / 'her', stdout=subprocess.PIPE, text=True, check=True)
    for line in build.stdout.splitlines():
        message = json.loads(line)
        if (message['reason'] == 'compiler-artifact' and message['target']['name'] == 'her'
                and message['profile']['test'] and message['executable']):
            return Path(message['executable']).resolve(strict=True)
    raise RuntimeError('Cargo did not produce the her unit test executable')


def wait_for_dsh(child, log):
    deadline = time.monotonic() + STARTUP_SECONDS
    while time.monotonic() < deadline:
        status = child.poll()
        if status is not None:
            raise RuntimeError(f'DSH exited before announcing readiness (exit {status})')
        with log.open('rb') as stream:
            output = stream.read(1024 * 1024 + 1)
        if len(output) > 1024 * 1024:
            raise RuntimeError('DSH exceeded the startup output limit')
        # DSH logs an authenticated URL. Pass only its loopback origin to the test.
        match = READY.search(output[:output.rfind(b'\n') + 1])
        if match:
            return match[1].decode('ascii')
        time.sleep(0.1)
    raise RuntimeError(f'DSH did not announce a loopback listening port within {STARTUP_SECONDS} seconds')


def isolated(root, node, entry, test_binary):
    if os.getpid() != 1:
        raise RuntimeError('the DSH fixture requires its own PID namespace')
    home, work, runtime_temp = (root / name for name in ('home', 'work', 'temp'))
    for directory in (home, work, runtime_temp):
        directory.mkdir()
    env = {'HOME': str(home), 'DSH_HOME': str(home / 'dsh'), 'TMPDIR': str(runtime_temp),
           'PATH': f'{node.parent}:{os.defpath}', 'DSH_TELEMETRY_DISABLED': '1'}
    log = root / 'startup.log'
    with log.open('wb') as output, subprocess.Popen(
        [node, entry, 'web', '--host', '127.0.0.1', '--port', '0', '--no-open'],
        cwd=work, env=env, stdin=subprocess.DEVNULL, stdout=output, stderr=output,
    ) as child:
        try:
            env['HER_DSH_CONTRACT_URL'] = wait_for_dsh(child, log)
            result = subprocess.run(
                [test_binary, '--ignored', '--exact', TEST], cwd=work, env=env,
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, text=True, timeout=TEST_SECONDS,
            )
            print(result.stdout, end='', flush=True)
            result.check_returncode()
            if f'test {TEST} ... ok' not in result.stdout.splitlines():
                raise RuntimeError('the selected Rust contract test did not run')
        finally:
            child.kill()
            child.wait()
    # Exiting PID 1 also kills and reaps descendants that outlived the direct child.


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', choices=VERSIONS)
    parser.add_argument('--filter')
    parser.add_argument('--isolated', nargs=4, type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if sys.platform != 'linux':
        parser.error('the contract requires Linux namespaces')
    if args.filter is not None and args.filter not in TEST:
        parser.error(f'filter matches no tests (available: {TEST})')
    if args.isolated:
        isolated(*args.isolated)
        return 0

    node = tool('node')
    ip = tool('ip')
    unshare = tool('unshare')
    test_binary = build_test()
    failed = False
    for version in (args.version,) if args.version else VERSIONS:
        print(f'Running dsh_contract against {PACKAGE}@{version}', flush=True)
        try:
            with tempfile.TemporaryDirectory(prefix='her-dsh-contract-', dir='/tmp') as temp:
                fixture = Path(temp).resolve()
                entry = stage(fixture, node, version)
                subprocess.run(
                    [unshare, '--user', '--map-root-user', '--net', '--pid', '--fork',
                     '--kill-child', '--mount-proc', '/bin/sh', '-ec',
                     '"$1" link set lo up; shift; exec "$@"', 'dsh-contract', ip,
                     sys.executable, '-I', __file__, '--isolated', fixture, node, entry, test_binary],
                    env={}, stdin=subprocess.DEVNULL, check=True,
                    timeout=STARTUP_SECONDS + TEST_SECONDS + 15,
                )
        except (OSError, RuntimeError, ValueError, KeyError, subprocess.SubprocessError) as error:
            print(f'dsh_contract {version}: FAIL ({error})', file=sys.stderr, flush=True)
            failed = True
        else:
            print(f'dsh_contract {version}: PASS', flush=True)
    return int(failed)


if __name__ == '__main__':
    sys.exit(main())

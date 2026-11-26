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
VERSION_LIST = ROOT / 'her' / 'dsh-contract-versions.txt'
TEST = 'dsh_credential::contract::settings_describe_cookie_contract'
READY = re.compile(rb'^dsh web: (http://127\.0\.0\.1:[1-9][0-9]{0,4})/\S*\r?$', re.MULTILINE)
INTEGRITY = re.compile(r'sha512-[A-Za-z0-9+/]{86}==')
INSTALL_SECONDS = 600
STARTUP_SECONDS = 180
TEST_SECONDS = 15


def tool(name):
    path = shutil.which(name)
    if path is None:
        raise RuntimeError(f'{name} is required for dsh_contract')
    # Preserve npm's link name, which selects the npm CLI when invoked.
    return Path(path).absolute()


def read_pins():
    pins = {}
    for line in VERSION_LIST.read_text(encoding='utf-8').splitlines():
        version, _, integrity = line.partition(' ')
        if not version or version in pins or not INTEGRITY.fullmatch(integrity):
            raise ValueError('DSH contract pins must list distinct versions with SHA-512 integrity')
        pins[version] = integrity
    if not pins:
        raise ValueError('DSH contract pins must list at least one version')
    return pins


def run_npm(root, node, npm, *args):
    home = root / 'npm-home'
    subprocess.run(
        [node, npm, *args, '--omit=dev', '--no-audit', '--no-fund', '--no-update-notifier',
         '--registry', REGISTRY, f'--{PACKAGE.split("/")[0]}:registry={REGISTRY}',
         '--cache', home / 'cache', '--userconfig', home / 'user.npmrc',
         '--globalconfig', home / 'global.npmrc'],
        cwd=root / 'package',
        env={'HOME': str(home), 'TMPDIR': str(home), 'PATH': f'{node.parent}:{os.defpath}'},
        stdin=subprocess.DEVNULL, check=True, timeout=INSTALL_SECONDS,
    )


def stage(root, node, npm, version, integrity):
    scratch_home = root / 'npm-home'
    scratch_home.mkdir()
    stage_dir = root / 'package'
    stage_dir.mkdir()
    (stage_dir / 'package.json').write_text('{"private":true}\n', encoding='utf-8')
    (scratch_home / 'user.npmrc').touch()
    (scratch_home / 'global.npmrc').touch()
    # Populate the private cache before entering the loopback-only network namespace.
    run_npm(root, node, npm, 'install', '--save-exact', '--ignore-scripts', '--fetch-retries=1',
            f'{PACKAGE}@{version}')
    location = f'node_modules/{PACKAGE}'
    lock = json.loads((stage_dir / 'package-lock.json').read_text(encoding='utf-8'))
    record = lock['packages'][location]
    expected_tarball = f'{REGISTRY}{PACKAGE}/-/dsh-{version}.tgz'
    if record['version'] != version or record['resolved'] != expected_tarball:
        raise RuntimeError('npm lock did not resolve the exact registry package')
    if record.get('integrity') != integrity:
        raise RuntimeError('npm lock integrity does not match the pinned DSH integrity')


def installed_entry(stage_dir, version):
    package = (stage_dir / 'node_modules' / PACKAGE).resolve(strict=True)
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


def isolated(root, node, npm, test_binary, version):
    if os.getpid() != 1:
        raise RuntimeError('the DSH fixture requires its own PID namespace')
    # Reinstall from the verified lock so lifecycle scripts run under fixture isolation.
    run_npm(root, node, npm, 'ci', '--offline', '--ignore-scripts=false')
    entry = installed_entry(root / 'package', version)
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
    try:
        pins = read_pins()
    except (OSError, ValueError) as error:
        parser.error(str(error))
    parser.add_argument('--version', choices=pins)
    parser.add_argument('--filter')
    parser.add_argument('--isolated', nargs=4, type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if sys.platform != 'linux':
        parser.error('the contract requires Linux namespaces')
    if args.filter is not None and args.filter not in TEST:
        parser.error(f'filter matches no tests (available: {TEST})')
    if args.isolated:
        if args.version is None:
            parser.error('the isolated fixture requires a version')
        isolated(*args.isolated, args.version)
        return 0

    node = tool('node')
    npm = tool('npm')
    ip = tool('ip')
    unshare = tool('unshare')
    test_binary = build_test()
    failed = False
    for version in (args.version,) if args.version else pins:
        print(f'Running dsh_contract against {PACKAGE}@{version}', flush=True)
        try:
            with tempfile.TemporaryDirectory(prefix='her-dsh-contract-', dir='/tmp') as temp:
                fixture = Path(temp).resolve()
                stage(fixture, node, npm, version, pins[version])
                subprocess.run(
                    [unshare, '--user', '--map-root-user', '--net', '--pid', '--fork',
                     '--kill-child', '--mount-proc', '/bin/sh', '-ec',
                     '"$1" link set lo up; shift; exec "$@"', 'dsh-contract', ip,
                     sys.executable, '-I', __file__, '--version', version,
                     '--isolated', fixture, node, npm, test_binary],
                    env={}, stdin=subprocess.DEVNULL, check=True,
                    timeout=INSTALL_SECONDS + STARTUP_SECONDS + TEST_SECONDS + 15,
                )
        except (OSError, RuntimeError, ValueError, KeyError, subprocess.SubprocessError) as error:
            print(f'dsh_contract {version}: FAIL ({error})', file=sys.stderr, flush=True)
            failed = True
        else:
            print(f'dsh_contract {version}: PASS', flush=True)
    return int(failed)


if __name__ == '__main__':
    sys.exit(main())

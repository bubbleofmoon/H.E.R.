#!/usr/bin/env python3
"""Install a pinned Node distribution including npm for dependency fixtures without relying on the repository workspace path."""
import hashlib
from pathlib import Path
import platform
import sys
import tarfile
import urllib.request
import zipfile

version = '24.11.0'
system = platform.system()
archives = {
    'Windows': ('win-x64.zip', '1054540bce22b54ec7e50ebc078ec5d090700a77657607a58f6a64df21f49fdd'),
    'Linux': ('linux-x64.tar.gz', 'b3c071cdf47aab867c3b2aa287257df12ec5d7c962bf922b32fd33226c4295fd'),
}
suffix, expected = archives[system]
name = f'node-v{version}-{suffix}'
out = Path(sys.argv[1]).resolve()
out.mkdir(parents=True, exist_ok=True)
archive = out / name
archive.write_bytes(urllib.request.urlopen(f'https://nodejs.org/dist/v{version}/{name}').read())
assert hashlib.sha256(archive.read_bytes()).hexdigest() == expected, 'Node archive checksum mismatch'
prefix = f'node-v{version}-' + ('win-x64' if system == 'Windows' else 'linux-x64')
def destination(name):
    relative = Path(name).relative_to(prefix)
    assert not relative.is_absolute() and '..' not in relative.parts, 'Unsafe archive member'
    return out / relative
if system == 'Windows':
    with zipfile.ZipFile(archive) as z:
        for member in z.infolist():
            if member.is_dir():
                continue
            target = destination(member.filename)
            target.parent.mkdir(parents=True, exist_ok=True)
            data = z.read(member)
            if not target.exists() or target.read_bytes() != data:
                target.write_bytes(data)
    node, npm = out / 'node.exe', out / 'node_modules/npm/bin/npm-cli.js'
else:
    with tarfile.open(archive) as t:
        for member in t.getmembers():
            if not member.isfile():
                continue
            target = destination(member.name)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(t.extractfile(member).read())
            target.chmod(member.mode)
    # Keep the standard installation layout expected by npm_cli() fixtures.
    for name, entry in [('npm', 'npm-cli.js'), ('npx', 'npx-cli.js')]:
        link = out / 'bin' / name
        relative = '../lib/node_modules/npm/bin/' + entry
        if link.is_symlink():
            assert str(link.readlink()) == relative, 'Unexpected existing npm link'
        else:
            assert not link.exists(), 'Refusing to replace an existing npm entry'
            link.symlink_to(relative)
    node, npm = out / 'bin/node', out / 'lib/node_modules/npm/bin/npm-cli.js'
assert node.is_file() and npm.is_file(), 'Node/npm distribution incomplete'
print(f'Installed Node {version} and bundled npm; pinned SHA256 verified')

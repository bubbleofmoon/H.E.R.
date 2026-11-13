#!/usr/bin/env python3
"""Install pinned gitleaks, verifying the release checksum before extraction."""
import hashlib
from pathlib import Path
import platform
import sys
import tarfile
import urllib.request
import zipfile

version = '8.30.0'
system = {'Windows': 'windows', 'Linux': 'linux'}[platform.system()]
name = f'gitleaks_{version}_{system}_x64.' + ('zip' if system == 'windows' else 'tar.gz')
base = f'https://github.com/gitleaks/gitleaks/releases/download/v{version}/'
out = Path(sys.argv[1]).resolve()
out.mkdir(parents=True, exist_ok=True)
checksums = urllib.request.urlopen(base + f'gitleaks_{version}_checksums.txt').read().decode()
expected = next(line.split()[0] for line in checksums.splitlines() if line.split()[-1] == name)
archive = out / name
archive.write_bytes(urllib.request.urlopen(base + name).read())
assert hashlib.sha256(archive.read_bytes()).hexdigest() == expected, 'release checksum mismatch'
executable = 'gitleaks.exe' if system == 'windows' else 'gitleaks'
if system == 'windows':
    with zipfile.ZipFile(archive) as z:
        (out / executable).write_bytes(z.read(executable))
else:
    with tarfile.open(archive) as t:
        (out / executable).write_bytes(t.extractfile(executable).read())
    (out / executable).chmod(0o755)
print(f'Installed gitleaks {version}; SHA256 verified')

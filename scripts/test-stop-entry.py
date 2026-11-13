#!/usr/bin/env python3
"""Exercise the stop entry against a disposable fake executable, never real processes."""
import json
import os
from pathlib import Path
import subprocess
import fixture_paths as tempfile


def main():
    repo = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(prefix="her-stop-fixture-") as tmp:
        root = Path(tmp)
        runtime = root / "runtime"
        runtime.mkdir()
        fake = root / "her"
        fake.write_text("#!/usr/bin/env python3\nimport json, os, sys\nopen(os.environ['TICKET27_ARGS'], 'w').write(json.dumps(sys.argv[1:]))\n")
        fake.chmod(0o755)
        shim = root / "fixture.ps1"
        shim.write_text(r'''
param($Script, $Runtime, $Executable)
function Get-NetTCPConnection { throw 'unexpected port probe' }
function Get-CimInstance { throw 'unexpected process enumeration' }
function Stop-Process { throw 'unexpected direct process stop' }
function Remove-Item { throw 'unexpected direct removal' }
function Start-Sleep { }
& $Script -RuntimeRoot $Runtime -Executable $Executable -Confirm
exit $LASTEXITCODE
''')
        receipt = root / "args.json"
        env = os.environ.copy()
        env["TICKET27_ARGS"] = str(receipt)
        result = subprocess.run(["pwsh", "-NoProfile", "-File", str(shim), str(repo / "stop-bot.ps1"), str(runtime), str(fake)],
                                env=env, text=True, capture_output=True, timeout=30)
        assert result.returncode == 0, (result.returncode, result.stdout, result.stderr)
        args = json.loads(receipt.read_text())
        assert args[:3] == ["lifecycle", "set", "stopped"]
        assert args[args.index("--runtime-root") + 1] == str(runtime)
        assert "--confirm" in args
        print("stop entry requests a confirmed stopped state on the selected runtime")


if __name__ == "__main__":
    main()

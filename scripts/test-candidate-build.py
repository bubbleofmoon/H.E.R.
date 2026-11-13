#!/usr/bin/env python3
"""Exercise the Windows build entry with a fake toolchain and disposable artifacts on Linux."""
import os
from pathlib import Path
import subprocess
import fixture_paths as tempfile


def main():
    repo = Path(__file__).resolve().parent.parent
    with tempfile.TemporaryDirectory(prefix="her-build-fixture-") as tmp:
        root = Path(tmp)
        program_files = root / "program-files"
        vswhere = program_files / "Microsoft Visual Studio/Installer/vswhere.exe"
        vcvars = root / "visual-studio/VC/Auxiliary/Build/vcvars64.bat"
        vswhere.parent.mkdir(parents=True)
        vcvars.parent.mkdir(parents=True)
        vcvars.write_text("fixture")
        vswhere.write_text("#!/bin/sh\nprintf '%s\\n' \"$TICKET27_VS_ROOT\"\n")
        vswhere.chmod(0o755)
        live = root / "online/her.exe"
        live.parent.mkdir()
        live.write_bytes(b"online-binary")
        candidate = root / "candidate"
        shim = root / "fixture.ps1"
        shim.write_text(r'''
param($BuildScript, $Candidate)
function Start-Process {
  param($FilePath, $ArgumentList, [switch]$NoNewWindow, [switch]$PassThru, [switch]$Wait, $WindowStyle, $WorkingDirectory)
  [IO.File]::WriteAllText($env:TICKET27_TARGET_RECEIPT, [string]$env:CARGO_TARGET_DIR)
  [IO.File]::WriteAllText($env:TICKET27_WORKING_RECEIPT, [string]$WorkingDirectory)
  if ($env:TICKET27_EXIT -eq '0' -and $env:CARGO_TARGET_DIR) {
    $release = Join-Path $env:CARGO_TARGET_DIR 'release'
    [IO.Directory]::CreateDirectory($release) | Out-Null
    [IO.File]::WriteAllText((Join-Path $release 'her.exe'), 'candidate-binary')
  }
  return [pscustomobject]@{ ExitCode = [int]$env:TICKET27_EXIT }
}
& $BuildScript -Release -CandidateDir $Candidate
exit $LASTEXITCODE
''')
        env = os.environ.copy()
        env.update({
            "ProgramFiles(x86)": str(program_files),
            "TICKET27_VS_ROOT": str(root / "visual-studio"),
            "TICKET27_TARGET_RECEIPT": str(root / "target-receipt"),
            "TICKET27_WORKING_RECEIPT": str(root / "working-receipt"),
        })
        env.pop("CARGO_TARGET_DIR", None)

        native = root/'libkernel32.dll.so'
        subprocess.run(['cc','-shared','-fPIC','-x','c','-o',str(native),'-'],input=r"""
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
static char names[8][8192]; static int count, position;
static int copy_name(uint32_t *size, uint16_t *buffer) {
 size_t n=strlen(names[position]);
 if (n+1>*size) { *size=n+1; errno=234; return 0; }
 for (size_t i=0;i<=n;i++) buffer[i]=(unsigned char)names[position][i];
 return 1;
}
void *FindFirstFileNameW(const uint16_t *file,uint32_t flags,uint32_t *size,uint16_t *buffer) {
 const char *list=getenv("TICKET27_LINK_TARGETS"); FILE *f=list ? fopen(list,"r") : NULL;
 if (!f) { errno=5; return (void*)-1; }
 count=0; position=0;
 while (count<8 && fgets(names[count],8192,f)) {
  names[count][strcspn(names[count],"\r\n")]=0;
  if (names[count][0]) count++;
 }
 fclose(f);
 if (!count) { errno=5; return (void*)-1; }
 if (!copy_name(size,buffer)) return (void*)-1;
 return (void*)1;
}
int FindNextFileNameW(void *handle,uint32_t *size,uint16_t *buffer) {
 if (++position>=count) { errno=38; return 0; }
 return copy_name(size,buffer);
}
int FindClose(void *handle) { return 1; }
""",text=True,check=True,capture_output=True,timeout=20)
        env['LD_LIBRARY_PATH']=str(root)+os.pathsep+env.get('LD_LIBRARY_PATH','')
        for code in (0, 17):
            env["TICKET27_EXIT"] = str(code)
            result = subprocess.run(
                ["pwsh", "-NoProfile", "-File", str(shim), str(repo / "her/build.ps1"), str(candidate)],
                env=env, text=True, capture_output=True, timeout=30,
            )
            assert result.returncode == code, (result.returncode, result.stdout, result.stderr)
            actual = (root / "target-receipt").read_text()
            assert Path(actual) == candidate, f"build used {actual!r}, expected candidate directory {candidate}"
            assert Path((root / "working-receipt").read_text()) == repo / "her", "build must run in the bridge crate directory"
            assert live.read_bytes() == b"online-binary", "candidate build changed online binary"
            if code == 0:
                assert (candidate / "release/her.exe").read_text() == "candidate-binary"
        receipt = (root / "target-receipt").read_text()
        unsafe = subprocess.run(
            ["pwsh", "-NoProfile", "-File", str(shim), str(repo / "her/build.ps1"), str(repo / "her/target/release")],
            env=env, text=True, capture_output=True, timeout=30,
        )
        assert unsafe.returncode != 0, "the live build directory must be refused"
        assert (root / "target-receipt").read_text() == receipt, "toolchain must not start for the live target"
        alias = root / "bridge-alias"
        alias.symlink_to(repo / "her", target_is_directory=True)
        redirected = subprocess.run(
            ["pwsh", "-NoProfile", "-File", str(shim), str(repo / "her/build.ps1"), str(alias / "target/release")],
            env=env, text=True, capture_output=True, timeout=30,
        )
        assert redirected.returncode != 0, "a redirected live build directory must be refused"
        assert (root / "target-receipt").read_text() == receipt, "toolchain must not start for a redirected live target"
        dangling = root / "not-yet-existing-live-target"
        target = repo / "her/target" / root.name
        assert not target.exists()
        dangling.symlink_to(target, target_is_directory=True)
        redirected = subprocess.run(
            ["pwsh", "-NoProfile", "-File", str(shim), str(repo / "her/build.ps1"), str(dangling / "release")],
            env=env, text=True, capture_output=True, timeout=30,
        )
        assert redirected.returncode != 0, "a dangling redirect into the live build tree must be refused"
        assert (root / "target-receipt").read_text() == receipt, "toolchain must not start through a dangling redirect"
        assert live.read_bytes() == b"online-binary"
        contained = root/'candidate-contained-links'
        (contained/'release').mkdir(parents=True)
        artifact = contained/'release/her.exe'
        companion = contained/'release/cargo-artifact.exe'
        artifact.write_text('previous-candidate'); os.link(artifact, companion)
        links = root/'link-targets.txt'
        links.write_text(str(artifact)+'\n'+str(companion)+'\n')
        env['TICKET27_LINK_TARGETS'] = str(links)
        env['TICKET27_EXIT'] = '0'
        rebuilt = subprocess.run(['pwsh','-NoProfile','-File',str(shim),str(repo/'her/build.ps1'),str(contained)],
                                 env=env,text=True,capture_output=True,timeout=30)
        assert rebuilt.returncode == 0, ('contained Cargo-style artifact hardlinks must remain buildable',rebuilt.stdout,rebuilt.stderr)
        assert artifact.read_text() == companion.read_text() == 'candidate-binary'
        assert live.read_bytes() == b'online-binary'
        receipt = (root/'target-receipt').read_text()
        env['TICKET27_EXIT'] = '0'
        for kind in ('release', 'debug', 'deps', 'artifact', 'hardlink'):
            output = root / ('candidate-' + kind)
            output.mkdir()
            if kind in ('release', 'debug'):
                (output/kind).symlink_to(live.parent, target_is_directory=True)
            elif kind == 'deps':
                (output/'release').mkdir()
                (output/'release/deps').symlink_to(live.parent, target_is_directory=True)
            else:
                (output/'release').mkdir()
                if kind == 'artifact': (output/'release/her.exe').symlink_to(live)
                else:
                    os.link(live, output/'release/her.exe')
                    links.write_text(str(output/'release/her.exe')+'\n'+str(live)+'\n')
            redirected = subprocess.run(
                ['pwsh', '-NoProfile', '-File', str(shim), str(repo/'her/build.ps1'), str(output)],
                env=env, text=True, capture_output=True, timeout=30)
            assert redirected.returncode != 0, f'build accepted redirected output: {kind}'
            assert (root/'target-receipt').read_text() == receipt, f'toolchain launched for {kind}'
            assert live.read_bytes() == b'online-binary', f'build changed aliased artifact: {kind}'
        print("candidate build success and failure preserve the online binary")


if __name__ == "__main__":
    main()

"""Native identity and hidden subprocess helpers for disposable lifecycle fixtures."""
import os
from pathlib import Path
import subprocess


def hidden():
    return {'creationflags': subprocess.CREATE_NO_WINDOW} if os.name == 'nt' else {}


def identity(process):
    """Read creation FILETIME and image through the same pinned Windows process handle."""
    if os.name != 'nt':
        stat = Path(f'/proc/{process.pid}/stat').read_text()
        return stat[stat.rfind(')') + 1:].split()[19], str(Path(f'/proc/{process.pid}/exe').resolve())
    import ctypes as c
    from ctypes import wintypes as w
    api = c.WinDLL('kernel32', use_last_error=True)
    api.GetProcessTimes.argtypes = [w.HANDLE] + [c.POINTER(w.FILETIME)] * 4
    api.GetProcessTimes.restype = w.BOOL
    api.QueryFullProcessImageNameW.argtypes = [w.HANDLE, w.DWORD, w.LPWSTR, c.POINTER(w.DWORD)]
    api.QueryFullProcessImageNameW.restype = w.BOOL
    created, exited, kernel, user = (w.FILETIME() for _ in range(4))
    handle = w.HANDLE(int(process._handle))  # Popen retains this handle until collection.
    if not api.GetProcessTimes(handle, c.byref(created), c.byref(exited), c.byref(kernel), c.byref(user)):
        raise c.WinError(c.get_last_error())
    image, size = c.create_unicode_buffer(32768), w.DWORD(32768)
    if not api.QueryFullProcessImageNameW(handle, 0, image, c.byref(size)):
        raise c.WinError(c.get_last_error())
    return str((created.dwHighDateTime << 32) | created.dwLowDateTime), image.value

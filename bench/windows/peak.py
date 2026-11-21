"""Wall time and peak working set of one command over N runs (Windows; stdout goes to OUT):

    python bench/windows/peak.py N OUT cmd args...
"""
import ctypes, subprocess, sys, time, statistics
from ctypes import wintypes
class PMC(ctypes.Structure):
    _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD)] + [
        (n, ctypes.c_size_t) for n in ("PeakWorkingSetSize", "WorkingSetSize", "QuotaPeakPagedPoolUsage",
        "QuotaPagedPoolUsage", "QuotaPeakNonPagedPoolUsage", "QuotaNonPagedPoolUsage", "PagefileUsage", "PeakPagefileUsage")]
psapi = ctypes.WinDLL("psapi")
psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(PMC), wintypes.DWORD]
n, outp, cmd = int(sys.argv[1]), sys.argv[2], sys.argv[3:]
ms, ws, pf = [], [], []
for i in range(n):
    with open(outp, "wb") as out:
        t = time.perf_counter()
        p = subprocess.Popen(cmd, stdout=out, stderr=subprocess.DEVNULL)
        p.wait()
        ms.append((time.perf_counter() - t) * 1000)
    c = PMC(); c.cb = ctypes.sizeof(PMC)
    psapi.GetProcessMemoryInfo(int(p._handle), ctypes.byref(c), c.cb)
    ws.append(c.PeakWorkingSetSize / 1048576); pf.append(c.PeakPagefileUsage / 1048576)
print(f"runs={n} wall_ms median={statistics.median(ms):.0f} min={min(ms):.0f} max={max(ms):.0f} | "
      f"peak_working_set_MiB={max(ws):.1f} peak_commit_MiB={max(pf):.1f} | exit={p.returncode}")

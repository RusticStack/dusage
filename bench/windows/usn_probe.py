#!/usr/bin/env python3
"""Independent oracle for the NTFS USN journal, used to validate `dusage --watch`.

Read-only: the journal is only queried and read. Nothing here writes to the volume
except the fixture files a driver script makes.

    python usn_probe.py query  --volume C
    python usn_probe.py read   --volume C --start 0x... --limit 50 [--json out.jsonl]
    python usn_probe.py stats  --volume C --start 0x... [--end 0x...]

`read` starts at `first_usn` when --start is omitted. `stats` prints one line with
the record count, the USN range and a reason histogram; --json writes one JSON
object per record.
"""

import argparse
import ctypes
import json
import sys
from ctypes import wintypes

GENERIC_READ = 0x80000000
FILE_SHARE_ALL = 0x1 | 0x2 | 0x4
OPEN_EXISTING = 3
INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value

FILE_DEVICE_FILE_SYSTEM = 9


def ctl_code(device, function, method, access):
    return (device << 16) | (access << 14) | (function << 2) | method


FSCTL_QUERY_USN_JOURNAL = ctl_code(FILE_DEVICE_FILE_SYSTEM, 61, 0, 0)
FSCTL_READ_USN_JOURNAL = ctl_code(FILE_DEVICE_FILE_SYSTEM, 46, 3, 0)

USN_REASON = {
    0x00000001: "DATA_OVERWRITE",
    0x00000002: "DATA_EXTEND",
    0x00000004: "DATA_TRUNCATION",
    0x00000010: "NAMED_DATA_OVERWRITE",
    0x00000020: "NAMED_DATA_EXTEND",
    0x00000040: "NAMED_DATA_TRUNCATION",
    0x00000100: "FILE_CREATE",
    0x00000200: "FILE_DELETE",
    0x00000400: "EA_CHANGE",
    0x00000800: "SECURITY_CHANGE",
    0x00001000: "RENAME_OLD_NAME",
    0x00002000: "RENAME_NEW_NAME",
    0x00004000: "INDEXABLE_CHANGE",
    0x00008000: "BASIC_INFO_CHANGE",
    0x00010000: "HARD_LINK_CHANGE",
    0x00020000: "COMPRESSION_CHANGE",
    0x00040000: "ENCRYPTION_CHANGE",
    0x00080000: "OBJECT_ID_CHANGE",
    0x00100000: "REPARSE_POINT_CHANGE",
    0x00200000: "STREAM_CHANGE",
    0x00400000: "TRANSACTED_CHANGE",
    0x80000000: "CLOSE",
}

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
kernel32.CreateFileW.restype = wintypes.HANDLE
kernel32.CreateFileW.argtypes = [
    wintypes.LPCWSTR,
    wintypes.DWORD,
    wintypes.DWORD,
    ctypes.c_void_p,
    wintypes.DWORD,
    wintypes.DWORD,
    wintypes.HANDLE,
]
kernel32.DeviceIoControl.restype = wintypes.BOOL
kernel32.DeviceIoControl.argtypes = [
    wintypes.HANDLE,
    wintypes.DWORD,
    ctypes.c_void_p,
    wintypes.DWORD,
    ctypes.c_void_p,
    wintypes.DWORD,
    ctypes.POINTER(wintypes.DWORD),
    ctypes.c_void_p,
]
kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
kernel32.OpenFileById.restype = wintypes.HANDLE
kernel32.OpenFileById.argtypes = [
    wintypes.HANDLE,
    ctypes.c_void_p,
    wintypes.DWORD,
    wintypes.DWORD,
    ctypes.c_void_p,
    wintypes.DWORD,
]
kernel32.GetFinalPathNameByHandleW.restype = wintypes.DWORD
kernel32.GetFinalPathNameByHandleW.argtypes = [
    wintypes.HANDLE,
    wintypes.LPWSTR,
    wintypes.DWORD,
    wintypes.DWORD,
]

FILE_READ_ATTRIBUTES = 0x0080
FILE_FLAG_BACKUP_SEMANTICS = 0x02000000
ExtendedFileIdType = 2


class FILE_ID_DESCRIPTOR(ctypes.Structure):
    _fields_ = [("dwSize", wintypes.DWORD), ("Type", ctypes.c_int), ("FileId", ctypes.c_uint64 * 2)]


def path_of(h, file_ref):
    """Resolve a 128-bit file reference to its path through OpenFileById."""
    d = FILE_ID_DESCRIPTOR(
        ctypes.sizeof(FILE_ID_DESCRIPTOR), ExtendedFileIdType,
        (ctypes.c_uint64 * 2)(file_ref & 0xFFFFFFFFFFFFFFFF, file_ref >> 64),
    )
    fh = kernel32.OpenFileById(
        h, ctypes.byref(d), FILE_READ_ATTRIBUTES, FILE_SHARE_ALL, None,
        FILE_FLAG_BACKUP_SEMANTICS,
    )
    if fh == INVALID_HANDLE_VALUE or fh is None:
        return None
    try:
        buf = ctypes.create_unicode_buffer(1024)
        n = kernel32.GetFinalPathNameByHandleW(fh, buf, 1024, 0)
        if n == 0:
            return None
        p = buf.value
        return p[4:] if p.startswith("\\\\?\\") else p
    finally:
        kernel32.CloseHandle(fh)


class READ_USN_JOURNAL_DATA_V1(ctypes.Structure):
    _fields_ = [
        ("StartUsn", ctypes.c_int64),
        ("ReasonMask", wintypes.DWORD),
        ("ReturnOnlyOnClose", wintypes.DWORD),
        ("Timeout", ctypes.c_uint64),
        ("BytesToWaitFor", ctypes.c_uint64),
        ("UsnJournalID", ctypes.c_uint64),
        ("MinMajorVersion", wintypes.WORD),
        ("MaxMajorVersion", wintypes.WORD),
    ]


def open_volume(letter):
    path = "\\\\.\\%s:" % letter.upper()
    h = kernel32.CreateFileW(
        path, GENERIC_READ, FILE_SHARE_ALL, None, OPEN_EXISTING, 0, None
    )
    if h == INVALID_HANDLE_VALUE or h is None:
        raise OSError(ctypes.get_last_error(), "CreateFileW %s" % path)
    return h


def ioctl(h, code, in_buf, in_size, out_size):
    out = ctypes.create_string_buffer(out_size)
    ret = wintypes.DWORD(0)
    ok = kernel32.DeviceIoControl(
        h, code, in_buf, in_size, out, out_size, ctypes.byref(ret), None
    )
    if not ok:
        raise OSError(ctypes.get_last_error(), "DeviceIoControl 0x%x" % code)
    return out.raw[: ret.value]


def query(h):
    b = ioctl(h, FSCTL_QUERY_USN_JOURNAL, None, 0, 64)
    (jid, first, nxt, lowest, mx, maxsize, delta) = (
        int.from_bytes(b[0:8], "little"),
        int.from_bytes(b[8:16], "little", signed=True),
        int.from_bytes(b[16:24], "little", signed=True),
        int.from_bytes(b[24:32], "little", signed=True),
        int.from_bytes(b[32:40], "little", signed=True),
        int.from_bytes(b[40:48], "little"),
        int.from_bytes(b[48:56], "little"),
    )
    return {
        "journal_id": jid,
        "first_usn": first,
        "next_usn": nxt,
        "lowest_valid_usn": lowest,
        "max_usn": mx,
        "maximum_size": maxsize,
        "allocation_delta": delta,
    }


def parse(buf):
    """Parse USN_RECORD_V2/V3 runs. `buf` starts after the 8-byte next-USN."""
    out = []
    off = 0
    while len(buf) - off >= 8:
        length = int.from_bytes(buf[off : off + 4], "little")
        major = int.from_bytes(buf[off + 4 : off + 6], "little")
        if length < 8 or length > len(buf) - off:
            break
        rec = buf[off : off + length]
        if major == 2:
            frn = int.from_bytes(rec[8:16], "little")
            parent = int.from_bytes(rec[16:24], "little")
            usn = int.from_bytes(rec[24:32], "little", signed=True)
            base, name_len_at, name_off_at = 40, 56, 58
        elif major == 3:
            frn = int.from_bytes(rec[8:24], "little")
            parent = int.from_bytes(rec[24:40], "little")
            usn = int.from_bytes(rec[40:48], "little", signed=True)
            base, name_len_at, name_off_at = 56, 72, 74
        else:
            off += (length + 7) & ~7
            continue
        reason = int.from_bytes(rec[base : base + 4], "little")
        attrs = int.from_bytes(rec[base + 12 : base + 16], "little")
        name_len = int.from_bytes(rec[name_len_at : name_len_at + 2], "little")
        name_off = int.from_bytes(rec[name_off_at : name_off_at + 2], "little")
        name = rec[name_off : name_off + name_len].decode("utf-16-le", "replace")
        out.append(
            {
                "usn": usn,
                "major": major,
                "file_ref": frn,
                "parent_ref": parent,
                "reason": reason,
                "reason_names": [
                    n for bit, n in USN_REASON.items() if reason & bit
                ],
                "attributes": attrs,
                "name": name,
            }
        )
        off += (length + 7) & ~7
    return out


def read_all(h, journal_id, start, end=None, buf_bytes=256 * 1024, reason_mask=0xFFFFFFFF):
    """Read records from `start` up to `end` (exclusive), in one buffer at a time."""
    recs = []
    nxt = start
    polls = 0
    while True:
        req = READ_USN_JOURNAL_DATA_V1(
            nxt, reason_mask, 0, 0, 0, journal_id, 2, 3
        )
        b = ioctl(h, FSCTL_READ_USN_JOURNAL, ctypes.byref(req), ctypes.sizeof(req), buf_bytes)
        polls += 1
        if len(b) < 8:
            break
        new_next = int.from_bytes(b[0:8], "little", signed=True)
        got = parse(b[8:])
        if end is not None:
            got = [r for r in got if r["usn"] < end]
        recs.extend(got)
        if new_next <= nxt or len(b) == 8 or not got:
            nxt = max(nxt, new_next)
            break
        nxt = new_next
    return recs, nxt, polls


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["query", "read", "stats"])
    ap.add_argument("--volume", default="C")
    ap.add_argument("--start", default=None)
    ap.add_argument("--end", default=None)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--json", default=None)
    ap.add_argument("--under", default=None, help="keep only records whose parent is under this path")
    ap.add_argument("--reason-mask", default="0xffffffff", help="u32 reason mask for the read")
    a = ap.parse_args()
    start = int(a.start, 0) if a.start else None
    end = int(a.end, 0) if a.end else None

    h = open_volume(a.volume)
    try:
        info = query(h)
        if a.cmd == "query":
            print(json.dumps(info))
            return
        if start is None:
            start = info["first_usn"]
        mask = int(a.reason_mask, 0)
        recs, nxt, polls = read_all(h, info["journal_id"], start, end, reason_mask=mask)
        unresolved = 0
        if a.under:
            want = a.under.rstrip("\\").lower()
            kept, cache = [], {}
            for r in recs:
                pr = r["parent_ref"]
                if pr not in cache:
                    cache[pr] = path_of(h, pr)
                base = cache[pr]
                if base is None:
                    unresolved += 1
                    continue
                full = (base.rstrip("\\") + "\\" + r["name"]).lower()
                if full == want or full.startswith(want + "\\"):
                    kept.append(r)
            recs = kept
    finally:
        kernel32.CloseHandle(h)

    if a.json:
        with open(a.json, "w", encoding="utf-8") as f:
            for r in recs:
                f.write(json.dumps(r) + "\n")
    if a.cmd == "read":
        for r in recs[: a.limit or len(recs)]:
            print(
                "%#x %-7s %-28s parent=%#x ref=%#x"
                % (r["usn"], r["major"], "+".join(r["reason_names"]) or "-",
                   r["parent_ref"], r["file_ref"])
            )
        print("records=%d polls=%d next=%#x" % (len(recs), polls, nxt))
        return
    hist = {}
    for r in recs:
        k = "+".join(r["reason_names"]) or "-"
        hist[k] = hist.get(k, 0) + 1
    print(
        "journal_id=%#x start=%#x end=%#x records=%d polls=%d next=%#x"
        % (info["journal_id"], start, end or nxt, len(recs), polls, nxt)
    )
    if a.under:
        print("filter=under %s unresolved_parents=%d" % (a.under, unresolved))
    for k, v in sorted(hist.items(), key=lambda kv: -kv[1]):
        print("  %6d  %s" % (v, k))


if __name__ == "__main__":
    sys.exit(main())

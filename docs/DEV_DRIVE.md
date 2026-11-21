# Dev Drive and scratch trees

Roadmap item 11. Guidance only: nothing in `dusage` or `fastdel` detects, creates, formats
or trusts a Dev Drive, and no flag changes behaviour on one. A Dev Drive is a ReFS volume
with a drive letter, and the tools treat it as that.

**Not run.** No command on this page has been run on a Dev Drive or on any ReFS volume. The
harness that would do it (`bench/windows/admin.ps1`, section 5) attaches a plain ReFS VHDX,
not a Dev Drive, and its rows are still owed (`docs/ROADMAP.md`, section 5). The behaviour
below is read from the code, and each row says what remains unknown.

## When a Dev Drive is worth it

Scratch trees — `target`, `node_modules`, agent temp folders — are what `fastdel` exists to
prune, and a Dev Drive is where Microsoft suggests keeping them. Requirements, from
[Microsoft Learn](https://learn.microsoft.com/en-us/windows/dev-drive/):

- Windows 11 build 10.0.22621.2338 or later.
- 50 GB minimum.
- The designation is made at format time; an existing volume is not converted.
- `C:` is not eligible.
- `fsutil devdrv trust` needs an elevated shell.
- Performance mode needs a trusted volume with real-time protection on.

**Do not turn protection off to make deletes faster.** The one delete timing found
([microsoft/WinDev#27](https://github.com/microsoft/WinDev/issues/27)) is a 119,954-file tree
at 58 s with protection on, 57 s with the folder excluded and 45 s with protection disabled,
so Defender does not explain the delete time. This project has not measured it either: the
protection A/B in `admin.ps1` is owed. Whether a Dev Drive makes `dusage` or `fastdel`
faster is **unmeasured**.

A Dev Drive does not empty itself. The scratch trees regenerate there as they do anywhere,
so the pruning below is still needed.

## What the tools do there

| Command | What the code does | Unknown until run |
|---|---|---|
| `dusage D:\` (`auto` backend) | Asks for `FileIdExtdDirectoryInfo`; if the filesystem rejects that class it retries with `FileIdBothDirectoryInfo`; if the batched read is unsupported altogether it falls back to the standard-library walk | Which of the three a Dev Drive takes |
| `--on-disk` | Reads `AllocationSize` from the batched read. The standard-library fallback has no allocation size | Whether ReFS block cloning shows up in the number |
| `--dedup-hardlinks` | Counts each non-zero file id once; an id of zero is never treated as a duplicate | The file ids ReFS returns |
| `--streams` | Sums the named streams `FindFirstStreamW` reports | What ReFS reports |
| `--mft` | Reads the NTFS master file table. A volume that does not answer `FSCTL_GET_NTFS_VOLUME_DATA` fails the scan with the system's error | The exact error on ReFS. Do not use it on a Dev Drive |
| `--watch` | Needs an elevated shell and a USN journal on the volume (V2 and V3 records are parsed). Without either, each tick prints `no change feed (<reason>); full rescan` and rescans at the interval | Whether the journal opens on a Dev Drive |
| `fastdel D:\` | Refused: every volume root is a protected root, in every spelling | Nothing; the guard is path-based |
| `fastdel D:\work ...` | Same as on any volume: permanent, no Recycle Bin, links removed and never followed | Delete speed |
| `dusage df -T` | Prints the filesystem name Windows reports for each volume | The Windows `df` path is unverified on a real machine (`docs/ROADMAP.md`, section 5) |

## Pruning a Dev Drive

Point `fastdel` at a folder, not at the drive root, and preview first:

```powershell
fastdel D:\work --preset cargo --dry
fastdel D:\work --preset node --dry
fastdel D:\scratch --age 2h --prefix tmp- --log prune.log
```

Drop `--dry` once the plan is what you expect. Deletion is permanent.
